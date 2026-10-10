//! voice — local speech tools: voice_say (TTS via say/espeak-ng/espeak/
//! piper/spd-say) and voice_listen (whisper.cpp over a mic recording via
//! arecord/sox). Everything is binary-detected; absent pieces fail with
//! install hints, not crashes.

use std::io::{BufRead, Write};
use std::process::{Command, Stdio};
use serde_json::{json, Value};

const NAME: &str = "voice";

fn manifest() -> Value {
    json!({
        "name": NAME, "display_name": "Voice",
        "version": env!("CARGO_PKG_VERSION"), "protocol": "1.1",
        "tools": [
            {"name": "voice_say",
             "description": "Speak text aloud via the first available local TTS (say/espeak-ng/espeak/piper/spd-say).",
             "parameters": {"type": "object", "properties": {
                "text": {"type": "string"},
                "engine": {"type": "string", "description": "pin an engine: say|espeak-ng|espeak|piper|spd-say"},
                "rate": {"type": "integer", "description": "words per minute where supported"}},
             "required": ["text"]}},
            {"name": "voice_listen",
             "description": "Record N seconds from the mic and transcribe with whisper.cpp (whisper-cli or main). Needs a recorder (arecord|sox) and a whisper binary+model.",
             "parameters": {"type": "object", "properties": {
                "seconds": {"type": "integer"},
                "model": {"type": "string", "description": "whisper .bin model path"}},
             }},
        ],
        "commands": ["/voice"], "subcommands": ["voice"],
    })
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
            &format!("printf %s \"$0\" | piper --stdout | aplay -q", )]).arg(text).status(),
        "spd-say" => Command::new("spd-say").arg(text).status(),
        other => return Err(format!("unknown engine {other}")),
    }.map_err(|e| format!("spawn: {e}"))?;
    if st.success() { Ok(format!("spoke via {e}")) }
    else { Err(format!("{e} exited {st}")) }
}

fn voice_listen(a: &Value) -> Result<String, String> {
    let secs = a["seconds"].as_u64().unwrap_or(5).min(60);
    let whisper = pick(&["whisper-cli", "whisper", "main", "whisper-cpp"])
        .ok_or("no whisper.cpp binary (whisper-cli/main) found")?;
    let wav = std::env::temp_dir().join(format!("gray-voice-{}.wav", std::process::id()));
    // capture
    let rec = if have("arecord") {
        Command::new("arecord").args(["-q","-f","S16_LE","-r","16000","-c","1",
            "-d", &secs.to_string()]).arg(&wav).status()
    } else if have("sox") {
        Command::new("sox").args(["-d","-r","16000","-c","1"])
            .arg(&wav).status()
    } else { return Err("no recorder — install alsa-utils (arecord) or sox".into()) };
    rec.map_err(|e| format!("recorder spawn: {e}"))?;
    // transcribe
    let mut cmd = Command::new(&whisper);
    if let Some(m) = a["model"].as_str() { cmd.args(["-m", m]); }
    if let Ok(mp) = std::env::var("WHISPER_MODEL") { if a["model"].is_null() { cmd.args(["-m", &mp]); } }
    cmd.args(["-f", &wav.display().to_string(), "-nt", "-np"]);
    let out = cmd.output().map_err(|e| format!("whisper: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let _ = std::fs::remove_file(&wav);
    if out.status.success() && !text.is_empty() { Ok(text) }
    else { Err(format!("whisper produced no text: {}",
        String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or(""))) }
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

fn status() -> String {
    let tts = pick(&["say","espeak-ng","espeak","piper","spd-say"]).unwrap_or("none".into());
    let stt = pick(&["whisper-cli","whisper","main","whisper-cpp"]).unwrap_or("none".into());
    let rec = pick(&["arecord","sox"]).unwrap_or("none".into());
    format!("voice\n  tts: {tts}\n  stt: {stt}\n  mic: {rec}")
}

fn sidecar() -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let method = v["method"].as_str().unwrap_or("");
        match v.get("id").cloned() {
            None if method == "plugin/shutdown" => return Ok(()),
            None => {}
            Some(id) => {
                let reply = if method == "plugin/manifest" {
                    json!({"id": id, "result": manifest()})
                } else if method == "tool/call" {
                    let mut r = tool(v["params"]["name"].as_str().unwrap_or(""), &v["params"]["args"]);
                    r["id"] = id; r
                } else if method == "command/run" {
                    json!({"id": id, "result": {"text": status()}})
                } else {
                    json!({"id": id, "error": {"code": -32601,
                        "message": format!("method not found: {method}")}})
                };
                writeln!(out, "{reply}")?; out.flush()?;
            }
        }
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<String> = match args.first().map(String::as_str) {
        Some("voice") => args[1..].to_vec(), _ => args };
    match args.first().map(String::as_str) {
        Some("manifest") => println!("{}", manifest()),
        Some("sidecar") => { let _ = sidecar(); }
        Some("status") => println!("{}", status()),
        _ => {
            if args.is_empty() {
                if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                    println!("{}", status()); return;
                }
                let _ = sidecar(); return;
            }
            eprintln!("unknown verb"); std::process::exit(1);
        }
    }
}
