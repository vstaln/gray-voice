//! voice — local speech tools and an ambient voice channel.
//!
//! Tools: voice_say (TTS via say/espeak-ng/espeak/piper/spd-say) and
//! voice_listen (whisper.cpp over a mic recording via pw-record/arecord/sox).
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
             "description": "Record N seconds from the mic and transcribe with the first working STT provider (whisper.cpp, groq, xai, cf). Needs a recorder (pw-record|arecord|sox). Ambient mode: /voice on|off.",
             "parameters": {"type": "object", "properties": {
                "seconds": {"type": "integer"},
                "model": {"type": "string", "description": "whisper .bin model path"}},
             }},
        ],
        "commands": ["/voice"], "subcommands": ["voice"],
        "composer_actions": [
            {"id": "dictate", "keys": DEFAULT_HOTKEYS, "argv": ["dictate"], "busy": "listening"},
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

fn have_recorder() -> bool {
    ["pw-record", "arecord", "sox"].iter().any(|r| have(r))
}

/// Records `secs` seconds of 16 kHz mono WAV. PipeWire first: on systems
/// without the ALSA bridge, arecord can't open the default device.
fn record(out: &std::path::Path, secs: &str) -> bool {
    let ok = |c: &mut Command| c.status().map(|s| s.success()).unwrap_or(false);
    // `timeout -s INT` stops pw-record cleanly and exits 124, so judge by the file.
    if have("pw-record") && have("timeout") {
        let _ = Command::new("timeout")
            .args([
                "-s",
                "INT",
                secs,
                "pw-record",
                "--rate",
                "16000",
                "--channels",
                "1",
            ])
            .args(["--format", "s16"])
            .arg(out)
            .stdout(std::process::Stdio::null())
            .status();
        if std::fs::metadata(out).is_ok_and(|m| m.len() > 44) {
            return true;
        }
    }
    if have("arecord")
        && ok(Command::new("arecord")
            .args(["-q", "-f", "S16_LE", "-r", "16000", "-c", "1", "-d", secs])
            .arg(out))
    {
        return true;
    }
    have("sox")
        && ok(Command::new("sox")
            .args(["-d", "-r", "16000", "-c", "1", "-b", "16"])
            .arg(out)
            .args(["trim", "0", secs]))
}

/// Record until gray reports the hotkey pressed again (or released, in
/// hold mode) on stdin as `down`/`up` lines, showing a level meter on
/// stderr, which gray displays as the status line.
fn record_live(out: &std::path::Path, hold: bool, key: &str) -> bool {
    use std::io::BufRead;
    use std::time::{Duration, Instant};
    let mut cmd = if have("pw-record") {
        let mut c = Command::new("pw-record");
        c.args(["--rate", "16000", "--channels", "1", "--format", "s16"]);
        c
    } else if have("arecord") {
        let mut c = Command::new("arecord");
        c.args(["-q", "-f", "S16_LE", "-r", "16000", "-c", "1"]);
        c
    } else {
        let mut c = Command::new("sox");
        c.args(["-q", "-d", "-r", "16000", "-c", "1", "-b", "16"]);
        c
    };
    let Ok(mut rec) = cmd
        .arg(out)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if tx.send(line.trim() == "down").is_err() {
                break;
            }
        }
    });
    let hint = if hold {
        format!("release {key} to stop")
    } else {
        format!("press {key} to stop")
    };
    let start = Instant::now();
    let mut last_down = start;
    let mut levels: VecDeque<usize> = VecDeque::new();
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let mut stop = false;
        while let Ok(down) = rx.try_recv() {
            let now = Instant::now();
            if !down {
                stop |= hold;
                continue;
            }
            // In press mode a new press stops; key auto-repeat from the
            // opening press (or a held key) arrives early or in a tight stream.
            if !hold
                && now - start > Duration::from_millis(700)
                && now - last_down > Duration::from_millis(150)
            {
                stop = true;
            }
            last_down = now;
        }
        // Terminals that never report key release still auto-repeat a held
        // key, so a pause in repeats means it was let go.
        if hold && last_down.elapsed() > Duration::from_millis(800) {
            stop = true;
        }
        if start.elapsed() > Duration::from_secs(120) || !matches!(rec.try_wait(), Ok(None)) {
            stop = true;
        }
        levels.push_back(level_now(out));
        if levels.len() > 16 {
            levels.pop_front();
        }
        let meter: String = levels
            .iter()
            .map(|&i| {
                [
                    '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}',
                    '\u{2587}', '\u{2588}',
                ][i]
            })
            .collect();
        eprintln!(
            "listening {meter:\u{2581}<16} {}s  {hint}",
            start.elapsed().as_secs()
        );
        if stop {
            break;
        }
    }
    let _ = Command::new("kill")
        .args(["-INT", &rec.id().to_string()])
        .status();
    let _ = rec.wait();
    eprintln!("transcribing");
    std::fs::metadata(out).is_ok_and(|m| m.len() > 44)
}

/// Level of the last 0.1 s written to the WAV, 0..=7 on a log scale.
fn level_now(path: &std::path::Path) -> usize {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return 0;
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len < 44 + 3200 {
        return 0;
    }
    let mut buf = vec![0u8; 3200];
    if f.seek(SeekFrom::Start((len - 3200) & !1)).is_err() || f.read_exact(&mut buf).is_err() {
        return 0;
    }
    let mean = buf
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| f64::from(i16::from_le_bytes([b[0], b[1]])).abs())
        .sum::<f64>()
        / 1600.0;
    (((mean.max(1.0).ln() - 5.0) / 3.5 * 7.0).clamp(0.0, 7.0)) as usize
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
    if !have_recorder() {
        eprintln!("voice: no recorder (pw-record/arecord/sox) — listener idle");
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
    if !have_recorder() {
        return Err(
            "no recorder — install pipewire (pw-record), alsa-utils (arecord) or sox".into(),
        );
    }
    let wav = std::env::temp_dir().join(format!("voice-{}.wav", std::process::id()));
    if !record(&wav, &secs) {
        return Err(
            "recording failed — no working mic device? (pw-record/arecord/sox exited nonzero)"
                .into(),
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
    id: String,
    name: String,
    free: String,
    key: Option<String>,
    model: String,
    url: String,
}

fn stt(id: &str, name: &str, free: &str, key: bool, model: &str, url: &str) -> Stt {
    Stt {
        id: id.into(),
        name: name.into(),
        free: free.into(),
        key: key.then(|| id.to_string()),
        model: model.into(),
        url: url.into(),
    }
}

fn stt_providers() -> Vec<Stt> {
    let mut all = vec![
        stt(
            "groq",
            "Groq",
            "free 28,800 s/day",
            true,
            "whisper-large-v3-turbo",
            "https://api.groq.com/openai/v1/audio/transcriptions",
        ),
        stt(
            "xai",
            "xAI",
            "paid ~$0.20/hr",
            true,
            "grok-voice-transcribe-2.0",
            "https://api.x.ai/v1/audio/transcriptions",
        ),
        stt(
            "cloudflare",
            "Cloudflare",
            "free ~214 min/day",
            false,
            "@cf/openai/whisper-large-v3-turbo",
            "",
        ),
        stt("local", "Local", "free, offline", false, "", ""),
    ];
    for c in read_config()["custom"].as_array().into_iter().flatten() {
        let (Some(id), Some(url), Some(model)) =
            (c["id"].as_str(), c["url"].as_str(), c["model"].as_str())
        else {
            continue;
        };
        all.push(stt(id, id, "your endpoint", true, model, url));
    }
    all
}

fn find_stt(id: &str) -> Option<Stt> {
    stt_providers().into_iter().find(|p| p.id == id)
}

fn add_custom(argv: &[&str]) -> String {
    let [_, id, url, model] = argv else {
        return "usage: /voice add <name> <url> <model>   (an OpenAI-compatible /audio/transcriptions URL)".into();
    };
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        || *id == "add"
        || *id == "remove"
    {
        return "the name may use letters, digits, - and _".into();
    }
    if model.starts_with(['@', '<']) {
        return "the model name cannot start with @ or <".into();
    }
    if find_stt(id).is_some() {
        return format!("{id} already exists");
    }
    if !url.starts_with("https://")
        && !url.starts_with("http://localhost")
        && !url.starts_with("http://127.0.0.1")
    {
        return "the URL must be https (or http on localhost)".into();
    }
    let r = update_config(|c| {
        let mut list = c["custom"].as_array().cloned().unwrap_or_default();
        list.push(json!({"id": id, "url": url, "model": model}));
        c["custom"] = json!(list);
    });
    match r.and_then(|()| find_stt(id).map_or(Ok(()), |p| stt_toggle(&p, true))) {
        Ok(()) => format!("{id} added and on. Set its key in a shell: gray voice key {id}"),
        Err(e) => format!("{id}: config not saved: {e}"),
    }
}

fn remove_custom(id: &str) -> String {
    let r = update_config(|c| {
        if let Some(list) = c["custom"].as_array_mut() {
            list.retain(|e| e["id"].as_str() != Some(id));
        }
        if let Some(list) = c["stt"].as_array_mut() {
            list.retain(|e| e.as_str() != Some(id));
        }
    });
    match r {
        Ok(()) => format!("{id} removed"),
        Err(e) => format!("{id}: config not saved: {e}"),
    }
}

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
    let field = p.key.as_deref()?;
    read_keys()[field]
        .as_str()
        .map(str::to_string)
        .or_else(|| std::env::var(format!("{}_API_KEY", field.to_uppercase())).ok())
        .filter(|k| !k.is_empty())
}

fn stt_enabled(p: &Stt) -> bool {
    match read_config()["stt"].as_array() {
        Some(list) => list.iter().any(|v| v.as_str() == Some(p.id.as_str())),
        None => p
            .key
            .as_ref()
            .map_or(p.id == "local", |_| provider_key(p).is_some()),
    }
}

fn stt_toggle(p: &Stt, on: bool) -> Result<(), String> {
    update_config(|c| {
        let mut ids: Vec<String> = stt_providers()
            .iter()
            .filter(|q| stt_enabled(q))
            .map(|q| q.id.to_string())
            .collect();
        if on && !ids.contains(&p.id) {
            ids.insert(0, p.id.clone());
        }
        if !on {
            ids.retain(|i| *i != p.id);
        }
        c["stt"] = json!(ids);
    })
}

fn stt_ready() -> bool {
    stt_providers()
        .iter()
        .filter(|p| stt_enabled(p))
        .any(|p| match p.id.as_str() {
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
    let rows: Vec<Value> = stt_providers()
        .iter()
        .map(|p| {
            json!({
                "id": p.id,
                "label": p.name,
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

fn hold_to_talk() -> bool {
    read_config()["dictate_mode"] == "hold"
}

fn voice_picker(selected: Option<&str>) -> Value {
    let keys = hotkeys()
        .iter()
        .map(|k| key_label(k))
        .collect::<Vec<_>>()
        .join(" or ");
    let key = key_label(&hotkeys()[0]);
    let mode = if hold_to_talk() {
        format!("Hold {key} while you talk")
    } else {
        format!("Press {key} to start and stop")
    };
    let mut items = vec![
        json!({"id": "dictate", "label": "Dictate now", "description": format!("or press {keys} at the prompt")}),
        json!({"id": "hotkey", "label": "Hotkey", "description": format!("{keys} \u{b7} Enter, then press a new key"),
               "capture_key": "Press the new dictation key"}),
        json!({"id": "mode", "label": "Mode", "description": format!("{mode} \u{b7} Enter to switch")}),
    ];
    let providers = stt_providers();
    let off = providers.iter().filter(|p| !stt_enabled(p)).count();
    for p in providers.iter().filter(|p| stt_enabled(p)) {
        items.push(json!({"id": p.id, "label": format!("\u{25cf} {}", p.name), "description": format!("{} \u{b7} {}", key_state(p), p.free)}));
    }
    items.push(json!({"id": "more", "label": "More providers", "description": format!("{off} off \u{b7} add your own")}));
    json!({"title": "Voice", "items": items, "selected_id": selected})
}

fn providers_picker(selected: Option<&str>) -> Value {
    let mut items: Vec<Value> = stt_providers()
        .iter()
        .map(|p| {
            json!({
                "id": format!("more:{}", p.id),
                "label": format!("{} {}", if stt_enabled(p) { "\u{25cf}" } else { "\u{25cb}" }, p.name),
                "description": format!("{} \u{b7} {}", key_state(p), p.free),
            })
        })
        .collect();
    items.push(json!({"id": "add", "label": "+ Add your own", "description": "any OpenAI-compatible endpoint"}));
    json!({
        "title": "Speech to text providers",
        "items": items,
        "selected_id": selected,
        "footer": "Enter turns a provider on or off",
    })
}

fn stt_panel() -> String {
    let mut out = String::from("voice · speech to text\n");
    for p in &stt_providers() {
        let on = stt_enabled(p);
        let key = key_state(p);
        out.push_str(&format!(
            "  {} {:<10} {:<30} {:<12} {}\n",
            if on { "on " } else { "off" },
            p.id,
            p.name,
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
            &p.url,
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

/// Whisper turns room noise into a few stock phrases; a transcript that is
/// only one of these (or only punctuation) heard nothing.
fn is_silence_hallucination(t: &str) -> bool {
    let norm: String = t
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .trim()
        .to_lowercase();
    matches!(
        norm.as_str(),
        "" | "you"
            | "thank you"
            | "thanks"
            | "thank you for watching"
            | "thanks for watching"
            | "bye"
    )
}

fn stt_transcribe(wav: &std::path::Path) -> Result<String, String> {
    let t = stt_transcribe_raw(wav)?;
    if is_silence_hallucination(&t) {
        return Err("heard only room noise".into());
    }
    Ok(t)
}

fn stt_transcribe_raw(wav: &std::path::Path) -> Result<String, String> {
    let enabled: Vec<Stt> = stt_providers().into_iter().filter(stt_enabled).collect();
    let mut err = String::new();
    for p in &enabled {
        let r = match p.id.as_str() {
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
    if !have_recorder() {
        return "no recorder (pw-record/arecord/sox)".into();
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

fn gray_home() -> std::path::PathBuf {
    std::env::var_os("GRAY_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}
fn config_path() -> std::path::PathBuf {
    gray_home().join("voice").join("config.json")
}

const DEFAULT_HOTKEYS: [&str; 2] = ["ctrl+space", "f8"];
/// gray's keymap entry for the dictation action (see gray keybindings.json).
const HOTKEY_ID: &str = "plugin.voice.dictate";

/// The dictation keys: the user's keybindings.json entry, else the defaults.
fn hotkeys() -> Vec<String> {
    let user = std::fs::read_to_string(gray_home().join("keybindings.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get(HOTKEY_ID).cloned());
    let keys: Vec<String> = match user {
        Some(Value::String(k)) => vec![k],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|k| k.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    if keys.is_empty() {
        DEFAULT_HOTKEYS.iter().map(|k| k.to_string()).collect()
    } else {
        keys
    }
}

fn set_hotkey(key: &str) -> Result<(), String> {
    let path = gray_home().join("keybindings.json");
    let mut map = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Map<String, Value>>(&t).ok())
        .unwrap_or_default();
    map.insert(HOTKEY_ID.into(), json!([key]));
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&map).unwrap_or_default() + "\n",
    )
    .map_err(|e| e.to_string())
}

/// `ctrl+space` -> `Ctrl+Space`, `f8` -> `F8`.
fn key_label(key: &str) -> String {
    key.split('+')
        .map(|p| {
            let mut c = p.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect())
                .unwrap_or_default()
        })
        .collect::<Vec<String>>()
        .join("+")
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
    if !have_recorder() {
        m.push("recorder (pw-record/arecord/sox)");
    }
    m
}

fn status(sh: &Shared) -> String {
    let tts = pick(&["say", "espeak-ng", "espeak", "piper", "spd-say"]).unwrap_or("none".into());
    let stt = pick(&["whisper-cli", "whisper", "main", "whisper-cpp"]).unwrap_or("none".into());
    let rec = pick(&["pw-record", "arecord", "sox"]).unwrap_or("none".into());
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

fn set_listen(sh: &Arc<Shared>, on: bool) -> String {
    if !on {
        sh.enabled.store(false, Ordering::Relaxed);
        return match persist_enabled(false) {
            Ok(()) => "listening off".into(),
            Err(e) => format!("listening off (config not saved: {e})"),
        };
    }
    let missing = missing_listen_deps();
    if !missing.is_empty() {
        return format!("cannot listen, missing: {}", missing.join(", "));
    }
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

fn command_run(sh: &Arc<Shared>, params: &Value) -> Value {
    let argv: Vec<&str> = params["argv"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    match argv.as_slice() {
        [] => return json!({"result": {"picker": voice_picker(None)}}),
        ["--pick", "dictate"] => {
            let key = hotkeys().remove(0);
            return json!({"result": {"composer_action": key, "text": format!("dictation needs the gray prompt: press {}", key_label(&key))}});
        }
        ["--pick", pick] if pick.starts_with("hotkey:") => {
            let key = &pick["hotkey:".len()..];
            if !key.contains('+')
                && !(key.starts_with('f') && key.len() > 1 && key[1..].parse::<u8>().is_ok())
            {
                return json!({"result": {"text": format!("{} would type into the prompt; pick an F key or a combo like Ctrl+Space", key_label(key))}});
            }
            return match set_hotkey(key) {
                Ok(()) => json!({"result": {"picker": voice_picker(Some("hotkey"))}}),
                Err(e) => json!({"result": {"text": format!("hotkey not saved: {e}")}}),
            };
        }
        ["--pick", "mode"] => {
            let next = if hold_to_talk() { "press" } else { "hold" };
            if let Err(e) = update_config(|c| c["dictate_mode"] = json!(next)) {
                return json!({"result": {"text": format!("mode not saved: {e}")}});
            }
            return json!({"result": {"picker": voice_picker(Some("mode"))}});
        }
        ["--pick", "more"] => return json!({"result": {"picker": providers_picker(None)}}),
        ["--pick", "add"] | ["add"] => return json!({"result": {"text": add_custom(&["add"])}}),
        ["add", ..] => return json!({"result": {"text": add_custom(&argv)}}),
        ["remove", id] => return json!({"result": {"text": remove_custom(id)}}),
        ["--pick", pick] => {
            let (more, id) = match pick.strip_prefix("more:") {
                Some(id) => (true, id),
                None => (false, *pick),
            };
            let Some(p) = find_stt(id) else {
                return json!({"result": {"text": format!("unknown provider {id}")}});
            };
            if let Err(e) = stt_toggle(&p, !stt_enabled(&p)) {
                return json!({"result": {"text": format!("{id}: config not saved: {e}")}});
            }
            let picker = if more {
                providers_picker(Some(pick))
            } else {
                voice_picker(Some(id))
            };
            return json!({"result": {"picker": picker}});
        }
        _ => {}
    }
    let verb = argv[0];
    let text = match verb {
        "on" => set_listen(sh, true),
        "off" => set_listen(sh, false),
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
            match find_stt(id) {
                Some(p) if p.key.is_some() => {
                    format!("set the {id} key with: voice key {id} (reads one line from stdin)")
                }
                Some(_) => format!("{id} needs no key"),
                None => format!("unknown provider {id}"),
            }
        }
        verb2 if find_stt(verb2).is_some() => {
            let p = find_stt(verb2).unwrap();
            let on = params["argv"]
                .as_array()
                .and_then(|a| a.get(1))
                .and_then(Value::as_str)
                .map(|v| v != "off")
                .unwrap_or(true);
            match stt_toggle(&p, on) {
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
            if !have_recorder() {
                eprintln!("no recorder (pw-record/arecord/sox)");
                std::process::exit(1);
            }
            let secs = args.get(1).and_then(|s| s.parse::<u64>().ok());
            let wav =
                std::env::temp_dir().join(format!("voice-dictate-{}.wav", std::process::id()));
            let ok = match secs {
                Some(n) => record(&wav, &n.clamp(1, 60).to_string()),
                None => record_live(&wav, hold_to_talk(), &key_label(&hotkeys()[0])),
            };
            if !ok {
                eprintln!("recording failed");
                std::process::exit(1);
            }
            let t = if loud(&wav) {
                stt_transcribe(&wav)
            } else {
                Err("too quiet".into())
            };
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
            let Some(p) = find_stt(id) else {
                eprintln!("unknown provider {id}");
                std::process::exit(1);
            };
            let Some(field) = p.key.as_deref() else {
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
        Some(p) if find_stt(p).is_some() => {
            let s = find_stt(p).unwrap();
            let on = args.get(1).map(|v| v != "off").unwrap_or(true);
            match stt_toggle(&s, on) {
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
