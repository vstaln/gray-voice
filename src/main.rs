//! voice — local speech tools and an ambient voice channel.
//!
//! Tools: voice_say (TTS via say/espeak-ng/espeak/piper/spd-say) and
//! voice_listen (whisper.cpp over a mic recording via arecord/sox).
//! Ambient mode (`/voice on` or VOICE_LISTEN=1): a background listener
//! turns mic chunks into transcripts pushed via `host/run` (capability
//! `host.turn`), with `prompt/context` as the pull fallback. Everything
//! is binary-detected; absent pieces fail with install hints.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, IsTerminal, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use serde_json::{json, Value};

const NAME: &str = "voice";
const CHUNK_SECS: &str = "4";

fn manifest() -> Value {
    json!({
        "name": NAME, "display_name": "Voice",
        "version": env!("CARGO_PKG_VERSION"), "protocol": "1.1",
        "capabilities": ["host.turn"],
        "hooks": ["prompt/context"],
        "tools": [
            {"name": "voice_say",
             "description": "Speak text aloud via the first available local TTS (say/espeak-ng/espeak/piper/spd-say).",
             "parameters": {"type": "object", "properties": {
                "text": {"type": "string"},
                "engine": {"type": "string", "description": "pin an engine: say|espeak-ng|espeak|piper|spd-say"},
                "rate": {"type": "integer", "description": "words per minute where supported"}},
             "required": ["text"]}},
            {"name": "voice_listen",
             "description": "Record N seconds from the mic and transcribe with whisper.cpp (whisper-cli or main). Needs a recorder (arecord|sox) and a whisper binary+model. Ambient mode: /voice on|off.",
             "parameters": {"type": "object", "properties": {
                "seconds": {"type": "integer"},
                "model": {"type": "string", "description": "whisper .bin model path"}},
             }},
        ],
        "commands": ["/voice"], "subcommands": ["voice"],
    })
}

struct Shared {
    enabled: AtomicBool,
    pump_q: Mutex<VecDeque<String>>,        // transcripts awaiting push
    cv: Condvar,
    heard: Mutex<VecDeque<String>>,         // push-failed, drained by prompt/context
    inflight: Mutex<HashMap<String, String>>, // host/run id -> transcript
    denied: AtomicBool,                     // host.run rejected once -> pull only
    seq: AtomicU64,
    listener_started: AtomicBool,
}

fn have(bin: &str) -> bool {
    Command::new(bin).arg("--version").stdout(Stdio::null()).stderr(Stdio::null())
        .status().map(|s| s.success()).unwrap_or(false)
        || Command::new("which").arg(bin).stdout(Stdio::null()).stderr(Stdio::null())
            .status().map(|s| s.success()).unwrap_or(false)
}

fn pick(cands: &[&str]) -> Option<String> {
    cands.iter().find(|b| have(b)).map(|s| s.to_string())
}

// ── ambient listener ──────────────────────────────────────────

fn record(out: &std::path::Path, secs: &str) -> bool {
    if have("arecord") {
        Command::new("arecord")
            .args(["-q", "-f", "S16_LE", "-r", "16000", "-c", "1", "-d", secs])
            .arg(out).status()
    } else if have("sox") {
        Command::new("sox")
            .args(["-d", "-r", "16000", "-c", "1", "-b", "16"])
            .arg(out).args(["trim", "0", secs]).status()
    } else {
        return false;
    }
    .map(|s| s.success()).unwrap_or(false)
}

/// Mean |i16| over WAV data; below VOICE_RMS (default 150) the chunk is
/// room noise and whisper never runs.
fn loud(path: &std::path::Path) -> bool {
    let gate: f64 = std::env::var("VOICE_RMS").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(150.0);
    let Ok(bytes) = std::fs::read(path) else { return false };
    if bytes.len() < 400 { return false }
    let mut sum = 0u64; let mut n = 0u64;
    for c in bytes[44..].chunks_exact(2) {
        sum += i16::from_le_bytes([c[0], c[1]]).unsigned_abs() as u64;
        n += 1;
    }
    n > 0 && (sum as f64 / n as f64) > gate
}

fn transcribe(whisper: &str, wav: &std::path::Path, model: Option<&str>) -> Option<String> {
    let mut cmd = Command::new(whisper);
    if let Some(m) = model { cmd.args(["-m", m]); }
    cmd.args(["-f", &wav.display().to_string(), "-nt", "-np"]);
    let out = cmd.output().ok()?;
    if !out.status.success() { return None }
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let l = t.to_lowercase();
    let junk = l.is_empty()
        || (l.starts_with('[') && l.ends_with(']'))           // [BLANK_AUDIO] etc.
        || matches!(l.as_str(), "." | "thank you" | "thanks for watching"
            | "bye" | "you" | "(silence)" | "silence");
    (!junk).then_some(t)
}

fn listener(sh: Arc<Shared>) {
    let Some(whisper) = pick(&["whisper-cli", "whisper", "main", "whisper-cpp"]) else {
        eprintln!("voice: no whisper.cpp binary (whisper-cli/main) — listener idle");
        return;
    };
    if !have("arecord") && !have("sox") {
        eprintln!("voice: no recorder (arecord/sox) — listener idle");
        return;
    }
    let model = std::env::var("VOICE_MODEL").ok()
        .or_else(|| std::env::var("WHISPER_MODEL").ok());
    while sh.enabled.load(Ordering::Relaxed) {
        let n = sh.seq.fetch_add(1, Ordering::Relaxed);
        let wav = std::env::temp_dir()
            .join(format!("voice-{}-{n}.wav", std::process::id()));
        if record(&wav, CHUNK_SECS) && loud(&wav) {
            if let Some(text) = transcribe(&whisper, &wav, model.as_deref()) {
                sh.pump_q.lock().unwrap().push_back(text);
                sh.cv.notify_one();
            }
        }
        let _ = std::fs::remove_file(&wav);
    }
}

/// Pump: transcripts -> `host/run` (string ids resolved by the reader).
/// Errors flip `denied`; pending text falls back to `heard`, which
/// `prompt/context` drains on the next turn.
fn pump(sh: Arc<Shared>, out: Arc<Mutex<std::io::Stdout>>) {
    loop {
        let text = {
            let mut q = sh.pump_q.lock().unwrap();
            loop {
                if let Some(t) = q.pop_front() { break t }
                q = sh.cv.wait(q).unwrap();
            }
        };
        if sh.denied.load(Ordering::Relaxed) {
            sh.heard.lock().unwrap().push_back(text);
            continue;
        }
        let n = sh.seq.fetch_add(1, Ordering::Relaxed);
        let wid = format!("vc-{n}");
        sh.inflight.lock().unwrap().insert(wid.clone(), text.clone());
        let mut w = out.lock().unwrap();
        let _ = writeln!(w, "{}", json!({"id": wid, "method": "host/run",
            "params": {"prompt": format!("[voice] {text}")}}));
        let _ = w.flush();
    }
}

// ── tools ─────────────────────────────────────────────────────

fn voice_say(a: &Value) -> Result<String, String> {
    let text = a["text"].as_str().ok_or("text required")?;
    let rate = a["rate"].as_u64().unwrap_or(180).to_string();
    let engine = a["engine"].as_str().map(str::to_string)
        .or_else(|| pick(&["say", "espeak-ng", "espeak", "piper", "spd-say"]));
    let Some(e) = engine else {
        return Err("no TTS engine — install espeak-ng/piper/spd-say (or macOS `say`)".into());
    };
    let st = match e.as_str() {
        "say" => Command::new("say").args(["-r", &rate, text]).status(),
        "espeak-ng" | "espeak" =>
            Command::new(&e).args(["-s", &rate, text]).status(),
        "piper" => Command::new("sh").args(["-c",
            "printf %s \"$0\" | piper --stdout | aplay -q"]).arg(text).status(),
        "spd-say" => Command::new("spd-say").arg(text).status(),
        other => return Err(format!("unknown engine {other}")),
    }.map_err(|e| format!("spawn: {e}"))?;
    if st.success() { Ok(format!("spoke via {e}")) }
    else { Err(format!("{e} exited {st}")) }
}

fn voice_listen(a: &Value) -> Result<String, String> {
    let secs = a["seconds"].as_u64().unwrap_or(5).clamp(1, 60).to_string();
    let whisper = pick(&["whisper-cli", "whisper", "main", "whisper-cpp"])
        .ok_or("no whisper.cpp binary (whisper-cli/main) found")?;
    let wav = std::env::temp_dir()
        .join(format!("voice-{}.wav", std::process::id()));
    if !record(&wav, &secs) {
        return Err("no recorder — install alsa-utils (arecord) or sox".into());
    }
    let model = a["model"].as_str().map(str::to_string)
        .or_else(|| std::env::var("VOICE_MODEL").ok())
        .or_else(|| std::env::var("WHISPER_MODEL").ok());
    let t = transcribe(&whisper, &wav, model.as_deref())
        .ok_or_else(|| "whisper produced no text".to_string());
    let _ = std::fs::remove_file(&wav);
    t
}

fn tool(name: &str, args: &Value) -> Value {
    let r = match name {
        "voice_say" => voice_say(args),
        "voice_listen" => voice_listen(args),
        t => Err(format!("unknown tool {t}")),
    };
    match r {
        Ok(c) => json!({"result": {"content": c}}),
        Err(e) => json!({"result": {"content": e, "is_error": true}}),
    }
}

fn status(sh: &Shared) -> String {
    let tts = pick(&["say","espeak-ng","espeak","piper","spd-say"]).unwrap_or("none".into());
    let stt = pick(&["whisper-cli","whisper","main","whisper-cpp"]).unwrap_or("none".into());
    let rec = pick(&["arecord","sox"]).unwrap_or("none".into());
    let model = std::env::var("VOICE_MODEL")
        .or_else(|_| std::env::var("WHISPER_MODEL"))
        .unwrap_or("(whisper default)".into());
    format!("voice\n  listening: {}\n  push: {}\n  queued: {}\n  tts: {tts}\n  stt: {stt}\n  model: {model}\n  mic: {rec}",
        if sh.enabled.load(Ordering::Relaxed) { "on" } else { "off" },
        if sh.denied.load(Ordering::Relaxed) { "pull only (host.turn denied)" }
            else { "host/run" },
        sh.heard.lock().unwrap().len())
}

fn command_run(sh: &Arc<Shared>, params: &Value) -> Value {
    let verb = params["argv"].as_array()
        .and_then(|a| a.first()).and_then(Value::as_str).unwrap_or("status");
    let text = match verb {
        "on" => {
            sh.enabled.store(true, Ordering::Relaxed);
            if !sh.listener_started.swap(true, Ordering::Relaxed) {
                let sh2 = Arc::clone(sh);
                std::thread::spawn(move || listener(sh2));
            }
            "listening on".to_string()
        }
        "off" => { sh.enabled.store(false, Ordering::Relaxed); "listening off".into() }
        "status" => status(sh),
        other => format!("voice: on|off|status (got {other})"),
    };
    json!({"result": {"text": text}})
}

// ── wire loop ─────────────────────────────────────────────────

fn sidecar() -> std::io::Result<()> {
    let sh = Arc::new(Shared {
        enabled: AtomicBool::new(std::env::var("VOICE_LISTEN")
            .map(|v| v != "0" && !v.is_empty()).unwrap_or(false)),
        pump_q: Mutex::new(VecDeque::new()), cv: Condvar::new(),
        heard: Mutex::new(VecDeque::new()),
        inflight: Mutex::new(HashMap::new()),
        denied: AtomicBool::new(false), seq: AtomicU64::new(0),
        listener_started: AtomicBool::new(false),
    });
    let out = Arc::new(Mutex::new(std::io::stdout()));
    {
        let (sh2, out2) = (Arc::clone(&sh), Arc::clone(&out));
        std::thread::spawn(move || pump(sh2, out2));
    }
    if sh.enabled.load(Ordering::Relaxed) && !sh.listener_started.swap(true, Ordering::Relaxed) {
        let sh2 = Arc::clone(&sh);
        std::thread::spawn(move || listener(sh2));
    }
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        // string-id replies to our host/run requests
        if v.get("method").is_none() {
            if let Some(id) = v["id"].as_str() {
                if let Some(text) = sh.inflight.lock().unwrap().remove(id) {
                    if v.get("error").is_some() {
                        sh.denied.store(true, Ordering::Relaxed);
                        sh.heard.lock().unwrap().push_back(text);
                    }
                }
            }
            continue;
        }
        let method = v["method"].as_str().unwrap_or("");
        match v.get("id").cloned() {
            None if method == "plugin/shutdown" => {
                sh.enabled.store(false, Ordering::Relaxed);
                return Ok(());
            }
            None => {}
            Some(id) => {
                let reply = match method {
                    "plugin/manifest" => json!({"id": id, "result": manifest()}),
                    "tool/call" => {
                        let mut r = tool(v["params"]["name"].as_str().unwrap_or(""),
                            &v["params"]["args"]);
                        r["id"] = id; r
                    }
                    "command/run" => {
                        let mut r = command_run(&sh, &v["params"]);
                        r["id"] = id; r
                    }
                    "prompt/context" => {
                        let heard: Vec<String> =
                            sh.heard.lock().unwrap().drain(..).collect();
                        let text = if heard.is_empty() { String::new() } else {
                            format!("Voice messages from the user: {}",
                                heard.join(" | "))
                        };
                        json!({"id": id, "result": {"text": text}})
                    }
                    other => json!({"id": id, "error": {"code": -32601,
                        "message": format!("method not found: {other}")}}),
                };
                let mut w = out.lock().unwrap();
                writeln!(w, "{reply}")?; w.flush()?;
            }
        }
    }
    sh.enabled.store(false, Ordering::Relaxed);
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<String> = match args.first().map(String::as_str) {
        Some(NAME) => args[1..].to_vec(), _ => args };
    match args.first().map(String::as_str) {
        Some("manifest") => println!("{}", manifest()),
        Some("sidecar") => { let _ = sidecar(); }
        Some("status") | Some("off") => println!("{}", status(&Shared {
            enabled: AtomicBool::new(false), pump_q: Mutex::new(VecDeque::new()),
            cv: Condvar::new(), heard: Mutex::new(VecDeque::new()),
            inflight: Mutex::new(HashMap::new()), denied: AtomicBool::new(false),
            seq: AtomicU64::new(0), listener_started: AtomicBool::new(false) })),
        _ => {
            if args.is_empty() {
                if std::io::stdin().is_terminal() {
                    println!("{}", status(&Shared {
                        enabled: AtomicBool::new(false),
                        pump_q: Mutex::new(VecDeque::new()), cv: Condvar::new(),
                        heard: Mutex::new(VecDeque::new()),
                        inflight: Mutex::new(HashMap::new()),
                        denied: AtomicBool::new(false), seq: AtomicU64::new(0),
                        listener_started: AtomicBool::new(false) }));
                    return;
                }
                let _ = sidecar(); return;
            }
            eprintln!("unknown verb"); std::process::exit(1);
        }
    }
}
