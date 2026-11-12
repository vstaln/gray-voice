//! voice — local speech tools and an ambient voice channel.
//!
//! Tools: voice_say (TTS via say/espeak-ng/espeak/piper/spd-say) and
//! voice_listen (whisper.cpp over a mic recording via arecord/sox).
//! Ambient mode (`/voice on` or VOICE_LISTEN=1): a background listener
//! turns mic chunks into transcripts pushed via `host/run` (capability
//! `host.turn`), with `prompt/context` as the pull fallback. Everything
//! is binary-detected; absent pieces fail with install hints.

use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, IsTerminal, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

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
             "description": "Record N seconds from the mic and transcribe with the first working STT provider (whisper.cpp, groq, xai, cf). Needs a recorder (arecord|sox). Ambient mode: /voice on|off.",
             "parameters": {"type": "object", "properties": {
                "seconds": {"type": "integer"},
                "model": {"type": "string", "description": "whisper .bin model path"}},
             }},
        ],
        "commands": ["/voice"], "subcommands": ["voice"],
        "composer_actions": [
            {"keys": ["ctrl+space", "f8"], "argv": ["dictate", "6"], "busy": "listening, then transcribing"},
        ],
    })
}

struct Shared {
    enabled: AtomicBool,
    pump_q: Mutex<VecDeque<String>>, // transcripts awaiting push
    cv: Condvar,
    heard: Mutex<VecDeque<String>>, // push-failed, drained by prompt/context
    inflight: Mutex<HashMap<String, String>>, // host/run id -> transcript
    denied: AtomicBool,             // host.run rejected once -> pull only
    seq: AtomicU64,
    listener_started: AtomicBool,
}

fn have(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
        || Command::new("which")
            .arg(bin)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
}

fn pick(cands: &[&str]) -> Option<String> {
    cands.iter().find(|b| have(b)).map(|s| s.to_string())
}

// ── ambient listener ──────────────────────────────────────────

fn record(out: &std::path::Path, secs: &str) -> bool {
    if have("arecord") {
        Command::new("arecord")
            .args(["-q", "-f", "S16_LE", "-r", "16000", "-c", "1", "-d", secs])
            .arg(out)
            .status()
    } else if have("sox") {
        Command::new("sox")
            .args(["-d", "-r", "16000", "-c", "1", "-b", "16"])
            .arg(out)
            .args(["trim", "0", secs])
            .status()
    } else {
        return false;
    }
    .map(|s| s.success())
    .unwrap_or(false)
}

/// Mean |i16| over WAV data; below VOICE_RMS (default 150) the chunk is
/// room noise and whisper never runs.
fn loud(path: &std::path::Path) -> bool {
    let gate: f64 = std::env::var("VOICE_RMS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(150.0);
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    if bytes.len() < 400 {
        return false;
    }
    let mut sum = 0u64;
    let mut n = 0u64;
    for c in bytes[44..].as_chunks::<2>().0 {
        sum += i16::from_le_bytes(*c).unsigned_abs() as u64;
        n += 1;
    }
    n > 0 && (sum as f64 / n as f64) > gate
}

fn transcribe(whisper: &str, wav: &std::path::Path, model: Option<&str>) -> Option<String> {
    let mut cmd = Command::new(whisper);
    if let Some(m) = model {
        cmd.args(["-m", m]);
    }
    cmd.args(["-f", &wav.display().to_string(), "-nt", "-np"]);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let l = t.to_lowercase();
    let junk = l.is_empty()
        || (l.starts_with('[') && l.ends_with(']'))           // [BLANK_AUDIO] etc.
        || matches!(l.as_str(), "." | "thank you" | "thanks for watching"
            | "bye" | "you" | "(silence)" | "silence");
    (!junk).then_some(t)
}

fn listener(sh: Arc<Shared>) {
    if !have("arecord") && !have("sox") {
        eprintln!("voice: no recorder (arecord/sox) — listener idle");
        return;
    }
    if !stt_ready() {
        eprintln!(
            "voice: no speech-to-text (whisper.cpp or a provider — /voice <id> on) — listener idle"
        );
        return;
    }
    let mut warned = false;
    while sh.enabled.load(Ordering::Relaxed) {
        let n = sh.seq.fetch_add(1, Ordering::Relaxed);
        let wav = std::env::temp_dir().join(format!("voice-{}-{n}.wav", std::process::id()));
        if record(&wav, CHUNK_SECS) && loud(&wav) {
            match stt_transcribe(&wav) {
                Ok(text) => {
                    sh.pump_q.lock().unwrap().push_back(text);
                    sh.cv.notify_one();
                }
                Err(e) => {
                    if !warned {
                        warned = true;
                        eprintln!("voice: {e}");
                    }
                }
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
                if let Some(t) = q.pop_front() {
                    break t;
                }
                q = sh.cv.wait(q).unwrap();
            }
        };
        if sh.denied.load(Ordering::Relaxed) {
            sh.heard.lock().unwrap().push_back(text);
            continue;
        }
        let n = sh.seq.fetch_add(1, Ordering::Relaxed);
        let wid = format!("vc-{n}");
        sh.inflight
            .lock()
            .unwrap()
            .insert(wid.clone(), text.clone());
        let mut w = out.lock().unwrap();
        let _ = writeln!(
            w,
            "{}",
            json!({"id": wid, "method": "host/run",
            "params": {"prompt": format!("[voice] {text}")}})
        );
        let _ = w.flush();
    }
}

// ── tools ─────────────────────────────────────────────────────

fn voice_say(a: &Value) -> Result<String, String> {
    let text = a["text"].as_str().ok_or("text required")?;
    let rate = a["rate"].as_u64().unwrap_or(180).to_string();
    let engine = a["engine"]
        .as_str()
        .map(str::to_string)
        .or_else(|| pick(&["say", "espeak-ng", "espeak", "piper", "spd-say"]));
    let Some(e) = engine else {
        return Err("no TTS engine — install espeak-ng/piper/spd-say (or macOS `say`)".into());
    };
    let st = match e.as_str() {
        "say" => Command::new("say").args(["-r", &rate, text]).status(),
        "espeak-ng" | "espeak" => Command::new(&e).args(["-s", &rate, text]).status(),
        "piper" => Command::new("sh")
            .args(["-c", "printf %s \"$0\" | piper --stdout | aplay -q"])
            .arg(text)
            .status(),
        "spd-say" => Command::new("spd-say").arg(text).status(),
        other => return Err(format!("unknown engine {other}")),
    }
    .map_err(|e| format!("spawn: {e}"))?;
    if st.success() {
        Ok(format!("spoke via {e}"))
    } else {
        Err(format!("{e} exited {st}"))
    }
}

fn voice_listen(a: &Value) -> Result<String, String> {
    let secs = a["seconds"].as_u64().unwrap_or(5).clamp(1, 60).to_string();
    if !have("arecord") && !have("sox") {
        return Err("no recorder — install alsa-utils (arecord) or sox".into());
    }
    let wav = std::env::temp_dir().join(format!("voice-{}.wav", std::process::id()));
    if !record(&wav, &secs) {
        return Err(
            "recording failed — no working mic device? (arecord/sox exited nonzero)".into(),
        );
    }
    let model = a["model"]
        .as_str()
        .map(str::to_string)
        .or_else(|| std::env::var("VOICE_MODEL").ok())
        .or_else(|| std::env::var("WHISPER_MODEL").ok());
    let local = pick(&["whisper-cli", "whisper", "main", "whisper-cpp"]);
    let r = stt_transcribe(&wav).or_else(|e| match &local {
        Some(w) => transcribe(w, &wav, model.as_deref()).ok_or(e),
        None => Err(e),
    });
    let _ = std::fs::remove_file(&wav);
    r
}

struct Stt {
    id: &'static str,
    label: &'static str,
    free: &'static str,
    key: Option<&'static str>,
    model: &'static str,
    url: &'static str,
}

const STT: &[Stt] = &[
    Stt {
        id: "groq",
        label: "groq whisper-large-v3-turbo",
        free: "free 28,800 s/day",
        key: Some("groq"),
        model: "whisper-large-v3-turbo",
        url: "https://api.groq.com/openai/v1/audio/transcriptions",
    },
    Stt {
        id: "xai",
        label: "xai grok-voice-transcribe-2.0",
        free: "paid ~$0.20/hr",
        key: Some("xai"),
        model: "grok-voice-transcribe-2.0",
        url: "https://api.x.ai/v1/audio/transcriptions",
    },
    Stt {
        id: "cloudflare",
        label: "cf whisper-large-v3-turbo",
        free: "free ~214 min/day",
        key: None,
        model: "@cf/openai/whisper-large-v3-turbo",
        url: "",
    },
    Stt {
        id: "local",
        label: "local whisper.cpp",
        free: "free, offline",
        key: None,
        model: "",
        url: "",
    },
];

fn keys_path() -> std::path::PathBuf {
    config_path().with_file_name("keys.json")
}

fn read_keys() -> Value {
    std::fs::read_to_string(keys_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

fn write_keys(v: &Value) -> std::io::Result<()> {
    if let Some(p) = keys_path().parent() {
        std::fs::create_dir_all(p)?;
    }
    write_private(
        &keys_path(),
        &serde_json::to_string_pretty(v).unwrap_or_default(),
    )
}

fn provider_key(p: &Stt) -> Option<String> {
    let field = p.key?;
    read_keys()[field]
        .as_str()
        .map(str::to_string)
        .or_else(|| std::env::var(format!("{}_API_KEY", field.to_uppercase())).ok())
        .filter(|k| !k.is_empty())
}

fn stt_enabled(p: &Stt) -> bool {
    match read_config()["stt"].as_array() {
        Some(list) => list.iter().any(|v| v.as_str() == Some(p.id)),
        None => p.key.map_or(p.id == "local", |_| provider_key(p).is_some()),
    }
}

fn stt_toggle(p: &Stt, on: bool) -> Result<(), String> {
    update_config(|c| {
        let mut ids: Vec<String> = STT
            .iter()
            .filter(|q| stt_enabled(q))
            .map(|q| q.id.to_string())
            .collect();
        if on && !ids.iter().any(|i| i == p.id) {
            ids.insert(0, p.id.to_string());
        }
        if !on {
            ids.retain(|i| i != p.id);
        }
        c["stt"] = json!(ids);
    })
}

fn stt_ready() -> bool {
    STT.iter().filter(|p| stt_enabled(p)).any(|p| match p.id {
        "local" => pick(&["whisper-cli", "whisper", "main", "whisper-cpp"]).is_some(),
        "cloudflare" => have("cf"),
        _ => provider_key(p).is_some(),
    })
}

fn update_config(f: impl FnOnce(&mut Value)) -> Result<(), String> {
    let cfg = config_path();
    if let Some(p) = cfg.parent() {
        std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
    }
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&cfg)
        .map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| e.to_string())?;
    let mut c = read_config();
    f(&mut c);
    let r = write_private(&cfg, &serde_json::to_string_pretty(&c).unwrap_or_default());
    let _ = lock.unlock();
    r.map_err(|e| e.to_string())
}
fn stt_json() -> String {
    let rows: Vec<Value> = STT
        .iter()
        .map(|p| {
            json!({
                "id": p.id,
                "label": p.label,
                "enabled": stt_enabled(p),
                "key": match p.key {
                    Some(_) => if provider_key(p).is_some() { "set" } else { "missing" },
                    None if p.id == "cloudflare" => "cf cli",
                    None => "none needed",
                },
                "free": p.free,
                "model": p.model,
            })
        })
        .collect();
    json!({"providers": rows}).to_string()
}

fn key_state(p: &Stt) -> &'static str {
    match p.key {
        Some(_) if provider_key(p).is_some() => "key set",
        Some(_) => "no key",
        None if p.id == "cloudflare" => "cf cli",
        None => "no key needed",
    }
}

fn stt_picker(selected: Option<&str>) -> Value {
    let items: Vec<Value> = STT
        .iter()
        .map(|p| {
            json!({
                "id": p.id,
                "label": format!("{} {}", if stt_enabled(p) { "on " } else { "off" }, p.label),
                "description": format!("{} · {}", key_state(p), p.free),
            })
        })
        .collect();
    json!({
        "title": "voice · speech to text",
        "items": items,
        "selected_id": selected,
        "footer": "ctrl+space or F8 dictates · Enter turns a provider on or off · Esc closes",
    })
}

fn stt_panel() -> String {
    let mut out = String::from("voice · speech to text\n");
    for p in STT {
        let on = stt_enabled(p);
        let key = key_state(p);
        out.push_str(&format!(
            "  {} {:<10} {:<30} {:<12} {}\n",
            if on { "on " } else { "off" },
            p.id,
            p.label,
            key,
            p.free
        ));
    }
    out.push_str("\n  /voice on|off <id>   /voice key <id>   /voice test");
    out
}

fn write_private(path: &std::path::Path, data: &str) -> std::io::Result<()> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(data.as_bytes())
}

fn transcribe_api(p: &Stt, key: &str, wav: &std::path::Path) -> Result<String, String> {
    let mut child = Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "90",
            "-K",
            "-",
            "-X",
            "POST",
            p.url,
            "-F",
            &format!("file=@{}", wav.display()),
            "-F",
            &format!("model={}", p.model),
            "-F",
            "response_format=text",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("curl: {e}"))?;
    if let Some(mut si) = child.stdin.take() {
        let _ = writeln!(si, "header = \"Authorization: Bearer {key}\"");
    }
    let out = child.wait_with_output().map_err(|e| format!("curl: {e}"))?;
    let body = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let shown = if body.is_empty() { err } else { body };
        return Err(format!(
            "{} {}",
            p.id,
            shown.chars().take(200).collect::<String>()
        ));
    }
    if body.starts_with('{') {
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        return v["text"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| body.chars().take(200).collect::<String>());
    }
    Ok(body)
}

fn transcribe_cf(wav: &std::path::Path) -> Result<String, String> {
    if !have("cf") {
        return Err("cf cli not found".into());
    }
    let b64 = Command::new("base64")
        .args(["-w0", &wav.display().to_string()])
        .output()
        .map_err(|e| format!("base64: {e}"))?;
    if !b64.status.success() {
        return Err("base64 failed".into());
    }
    let audio = String::from_utf8_lossy(&b64.stdout).trim().to_string();
    let body = std::env::temp_dir().join(format!("voice-cf-{}.json", std::process::id()));
    std::fs::write(
        &body,
        json!({"audio": audio, "task": "transcribe"}).to_string(),
    )
    .map_err(|e| e.to_string())?;
    let out = Command::new("cf")
        .args([
            "ai",
            "run",
            "@cf/openai/whisper-large-v3-turbo",
            "--body",
            &format!("@{}", body.display()),
        ])
        .output();
    let _ = std::fs::remove_file(&body);
    let out = out.map_err(|e| format!("cf: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let shown = if text.is_empty() { err } else { text };
        return Err(format!(
            "cf {}",
            shown.chars().take(200).collect::<String>()
        ));
    }
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    match v["result"]["text"].as_str() {
        Some(t) => Ok(t.to_string()),
        None => Err(format!("cf {}", text.chars().take(200).collect::<String>())),
    }
}

fn stt_transcribe(wav: &std::path::Path) -> Result<String, String> {
    let enabled: Vec<&Stt> = STT.iter().filter(|p| stt_enabled(p)).collect();
    let mut err = String::new();
    for p in enabled {
        let r = match p.id {
            "local" => match pick(&["whisper-cli", "whisper", "main", "whisper-cpp"]) {
                Some(w) => {
                    let model = std::env::var("VOICE_MODEL")
                        .ok()
                        .or_else(|| std::env::var("WHISPER_MODEL").ok());
                    transcribe(&w, wav, model.as_deref())
                        .ok_or_else(|| "local whisper produced no text".to_string())
                }
                None => Err("local whisper: no whisper.cpp binary".to_string()),
            },
            "cloudflare" => transcribe_cf(wav),
            _ => match provider_key(p) {
                Some(k) => transcribe_api(p, &k, wav),
                None => Err(format!("{}: no key (voice key {})", p.id, p.id)),
            },
        };
        match r {
            Ok(t) if !t.trim().is_empty() => return Ok(t.trim().to_string()),
            Ok(_) => err = format!("{}: empty transcript", p.id),
            Err(e) => err = e,
        }
    }
    if err.is_empty() {
        err = "no speech-to-text enabled (/voice <id> on)".into();
    }
    Err(err)
}

fn voice_test() -> String {
    if !have("arecord") && !have("sox") {
        return "no recorder (arecord/sox)".into();
    }
    let wav = std::env::temp_dir().join(format!("voice-test-{}.wav", std::process::id()));
    if !record(&wav, "4") {
        return "recording failed".into();
    }
    let t = stt_transcribe(&wav);
    let _ = std::fs::remove_file(&wav);
    match t {
        Ok(t) => t,
        Err(e) => format!("no text — {e}"),
    }
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

fn config_path() -> std::path::PathBuf {
    let home = std::env::var_os("GRAY_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    home.join("voice").join("config.json")
}
fn read_config() -> Value {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}
fn persist_enabled(on: bool) -> Result<(), String> {
    update_config(|c| c["listen"] = json!(on))
}

fn missing_listen_deps() -> Vec<&'static str> {
    let mut m = Vec::new();
    if !stt_ready() {
        m.push("speech-to-text (whisper.cpp or /voice <id> on)");
    }
    if !have("arecord") && !have("sox") {
        m.push("recorder (arecord/sox)");
    }
    m
}

fn status(sh: &Shared) -> String {
    let tts = pick(&["say", "espeak-ng", "espeak", "piper", "spd-say"]).unwrap_or("none".into());
    let stt = pick(&["whisper-cli", "whisper", "main", "whisper-cpp"]).unwrap_or("none".into());
    let rec = pick(&["arecord", "sox"]).unwrap_or("none".into());
    let model = std::env::var("VOICE_MODEL")
        .or_else(|_| std::env::var("WHISPER_MODEL"))
        .unwrap_or("(whisper default)".into());
    format!(
        "voice\n  listening: {}\n  push: {}\n  queued: {}\n  tts: {tts}\n  stt: {stt}\n  model: {model}\n  mic: {rec}",
        if sh.enabled.load(Ordering::Relaxed) {
            "on"
        } else {
            "off"
        },
        if sh.denied.load(Ordering::Relaxed) {
            "pull only (host.turn denied)"
        } else {
            "host/run"
        },
        sh.heard.lock().unwrap().len()
    )
}

fn command_run(sh: &Arc<Shared>, params: &Value) -> Value {
    let argv: Vec<&str> = params["argv"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    match argv.as_slice() {
        [] => return json!({"result": {"picker": stt_picker(None)}}),
        ["--pick", id] => {
            let Some(p) = STT.iter().find(|p| p.id == *id) else {
                return json!({"result": {"text": format!("unknown provider {id}")}});
            };
            if let Err(e) = stt_toggle(p, !stt_enabled(p)) {
                return json!({"result": {"text": format!("{id}: config not saved: {e}")}});
            }
            return json!({"result": {"picker": stt_picker(Some(id))}});
        }
        _ => {}
    }
    let verb = argv[0];
    let text = match verb {
        "on" => {
            let missing = missing_listen_deps();
            if !missing.is_empty() {
                format!("cannot listen — missing: {}", missing.join(", "))
            } else {
                sh.enabled.store(true, Ordering::Relaxed);
                if !sh.listener_started.swap(true, Ordering::Relaxed) {
                    let sh2 = Arc::clone(sh);
                    std::thread::spawn(move || listener(sh2));
                }
                match persist_enabled(true) {
                    Ok(()) => "listening on".to_string(),
                    Err(e) => format!("listening on (config not saved: {e})"),
                }
            }
        }
        "off" => {
            sh.enabled.store(false, Ordering::Relaxed);
            match persist_enabled(false) {
                Ok(()) => "listening off".into(),
                Err(e) => format!("listening off (config not saved: {e})"),
            }
        }
        "status" => format!("{}\n\n{}", status(sh), stt_panel()),
        "stt" | "panel" => {
            if params["argv"]
                .as_array()
                .map(|a| a.iter().any(|v| v.as_str() == Some("--json")))
                .unwrap_or(false)
            {
                stt_json()
            } else {
                stt_panel()
            }
        }
        "test" => voice_test(),
        "key" => {
            let id = params["argv"]
                .as_array()
                .and_then(|a| a.get(1))
                .and_then(Value::as_str)
                .unwrap_or("");
            match STT.iter().find(|p| p.id == id) {
                Some(p) if p.key.is_some() => {
                    format!("set the {id} key with: voice key {id} (reads one line from stdin)")
                }
                Some(_) => format!("{id} needs no key"),
                None => format!("unknown provider {id}"),
            }
        }
        verb2 if STT.iter().any(|p| p.id == verb2) => {
            let p = STT.iter().find(|p| p.id == verb2).unwrap();
            let on = params["argv"]
                .as_array()
                .and_then(|a| a.get(1))
                .and_then(Value::as_str)
                .map(|v| v != "off")
                .unwrap_or(true);
            match stt_toggle(p, on) {
                Ok(()) => format!("{verb2} {}", if on { "on" } else { "off" }),
                Err(e) => format!("{verb2}: config not saved: {e}"),
            }
        }
        other => format!("voice: on|off|status|stt|test|<provider> (got {other})"),
    };
    json!({"result": {"text": text}})
}

// ── wire loop ─────────────────────────────────────────────────

fn sidecar() -> std::io::Result<()> {
    let sh = Arc::new(Shared {
        enabled: AtomicBool::new(
            std::env::var("VOICE_LISTEN")
                .map(|v| v != "0" && !v.is_empty())
                .unwrap_or(false)
                || read_config()["listen"].as_bool().unwrap_or(false),
        ),
        pump_q: Mutex::new(VecDeque::new()),
        cv: Condvar::new(),
        heard: Mutex::new(VecDeque::new()),
        inflight: Mutex::new(HashMap::new()),
        denied: AtomicBool::new(false),
        seq: AtomicU64::new(0),
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
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
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
                if method == "tool/call" || method == "command/run" {
                    let (sh2, out2) = (Arc::clone(&sh), Arc::clone(&out));
                    let params = v["params"].clone();
                    let name = params["name"].as_str().unwrap_or("").to_string();
                    let is_command = method == "command/run";
                    std::thread::spawn(move || {
                        let mut r = if is_command {
                            command_run(&sh2, &params)
                        } else {
                            tool(&name, &params["args"])
                        };
                        r["id"] = id;
                        let mut w = out2.lock().unwrap();
                        let _ = writeln!(w, "{r}");
                        let _ = w.flush();
                    });
                    continue;
                }
                let reply = match method {
                    "plugin/manifest" => json!({"id": id, "result": manifest()}),
                    "prompt/context" => {
                        let heard: Vec<String> = sh.heard.lock().unwrap().drain(..).collect();
                        let text = if heard.is_empty() {
                            String::new()
                        } else {
                            format!("Voice messages from the user: {}", heard.join(" | "))
                        };
                        json!({"id": id, "result": {"text": text}})
                    }
                    other => json!({"id": id, "error": {"code": -32601,
                        "message": format!("method not found: {other}")}}),
                };
                let mut w = out.lock().unwrap();
                writeln!(w, "{reply}")?;
                w.flush()?;
            }
        }
    }
    sh.enabled.store(false, Ordering::Relaxed);
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<String> = match args.first().map(String::as_str) {
        Some(NAME) => args[1..].to_vec(),
        _ => args,
    };
    match args.first().map(String::as_str) {
        Some("manifest") => println!("{}", manifest()),
        Some("sidecar") => {
            let _ = sidecar();
        }
        Some("status") => println!(
            "{}",
            status(&Shared {
                enabled: AtomicBool::new(read_config()["listen"].as_bool().unwrap_or(false)),
                pump_q: Mutex::new(VecDeque::new()),
                cv: Condvar::new(),
                heard: Mutex::new(VecDeque::new()),
                inflight: Mutex::new(HashMap::new()),
                denied: AtomicBool::new(false),
                seq: AtomicU64::new(0),
                listener_started: AtomicBool::new(false)
            })
        ),
        Some("stt") | Some("panel") => {
            if args.iter().any(|a| a == "--json") {
                println!("{}", stt_json())
            } else {
                println!("{}", stt_panel())
            }
        }
        Some("test") => println!("{}", voice_test()),
        Some("on") => {
            let missing = missing_listen_deps();
            if !missing.is_empty() {
                eprintln!("cannot listen — missing: {}", missing.join(", "));
                std::process::exit(1);
            }
            if let Err(e) = persist_enabled(true) {
                eprintln!("config not saved: {e}");
                std::process::exit(1);
            }
            println!(
                "listening on (persisted — ambient mode starts when the sidecar loads; /voice on for the live session)"
            );
        }
        Some("off") => {
            if let Err(e) = persist_enabled(false) {
                eprintln!("config not saved: {e}");
                std::process::exit(1);
            }
            println!("listening off (persisted)");
        }
        Some("dictate") => {
            if !have("arecord") && !have("sox") {
                eprintln!("no recorder (arecord/sox)");
                std::process::exit(1);
            }
            let secs = args
                .get(1)
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(6)
                .clamp(1, 60)
                .to_string();
            let wav =
                std::env::temp_dir().join(format!("voice-dictate-{}.wav", std::process::id()));
            if !record(&wav, &secs) {
                eprintln!("recording failed");
                std::process::exit(1);
            }
            let t = stt_transcribe(&wav);
            let _ = std::fs::remove_file(&wav);
            match t {
                Ok(t) => println!("{t}"),
                Err(e) => {
                    eprintln!("no speech detected — {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("key") => {
            let id = args.get(1).map(String::as_str).unwrap_or("");
            let Some(p) = STT.iter().find(|p| p.id == id) else {
                eprintln!("unknown provider {id}");
                std::process::exit(1);
            };
            let Some(field) = p.key else {
                println!("{id} needs no key");
                return;
            };
            let mut line = String::new();
            let _ = std::io::stdin().lock().read_line(&mut line);
            let k = line.trim();
            if k.is_empty() {
                eprintln!("no key on stdin");
                std::process::exit(1);
            }
            let mut v = read_keys();
            v[field] = json!(k);
            if let Err(e) = write_keys(&v) {
                eprintln!("could not write {}: {e}", keys_path().display());
                std::process::exit(1);
            }
            println!("{id} key saved");
        }
        Some(p) if STT.iter().any(|s| s.id == p) => {
            let s = STT.iter().find(|s| s.id == p).unwrap();
            let on = args.get(1).map(|v| v != "off").unwrap_or(true);
            match stt_toggle(s, on) {
                Ok(()) => println!("{p} {}", if on { "on" } else { "off" }),
                Err(e) => {
                    eprintln!("config not saved: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("-h") | Some("--help") | Some("help") => println!(
            "voice — TTS/STT.\n  voice status|stt|test|dictate [secs]|on|off\n  voice key <id>   (reads the key from stdin)\n  voice <provider> [off]\n  in gray: /voice status|stt|test|on|off|<provider> [off]\n  tools: voice_say voice_listen"
        ),
        _ => {
            if args.is_empty() {
                if std::io::stdin().is_terminal() {
                    println!(
                        "{}",
                        status(&Shared {
                            enabled: AtomicBool::new(false),
                            pump_q: Mutex::new(VecDeque::new()),
                            cv: Condvar::new(),
                            heard: Mutex::new(VecDeque::new()),
                            inflight: Mutex::new(HashMap::new()),
                            denied: AtomicBool::new(false),
                            seq: AtomicU64::new(0),
                            listener_started: AtomicBool::new(false)
                        })
                    );
                    return;
                }
                let _ = sidecar();
                return;
            }
            eprintln!("unknown verb");
            std::process::exit(1);
        }
    }
}
