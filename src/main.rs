//! voice: speech in and out for gray. One /voice page picks the speech-to-text
//! and text-to-speech providers; the talk key dictates into the prompt,
//! finished answers can be read aloud, and an ambient listener can push
//! what it hears into the conversation.

use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

mod cloud;
mod page;
mod speech;

const NAME: &str = "voice";
const DEFAULT_HOTKEY: &str = "ctrl+space";
const HOTKEY_ID: &str = "plugin.voice.dictate";
const NO_RECORDER: &str = "No recorder \u{b7} install pipewire or alsa-utils";
const MODEL_MISSING: &str = "No whisper model \u{b7} enter to download";
const MODEL_FILE: &str = "ggml-base.en.bin";
const MODEL_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin";
const MODEL_BYTES: u64 = 147_951_465;
const STT_NAMES: [(&str, &str); 3] = [
    ("local", "whisper.cpp"),
    ("deepgram", "Deepgram"),
    ("groq", "Groq"),
];

fn manifest() -> Value {
    json!({
        "name": NAME, "display_name": "Voice",
        "version": env!("CARGO_PKG_VERSION"), "protocol": "1.1",
        "capabilities": ["host.turn"],
        "hooks": ["prompt/context"],
        "autostart": true,
        "tools": [
            {"name": "voice_say", "label": "Said", "preview": "{text|firstline}",
             "description": "Speak text aloud with the voice chosen in /voice.",
             "parameters": {"type": "object", "properties": {
                "text": {"type": "string"},
                "engine": {"type": "string", "description": "pin a voice: say|piper|espeak-ng|espeak|spd-say|deepgram|fish"},
                "rate": {"type": "integer", "description": "words per minute where supported"}},
             "required": ["text"]}},
            {"name": "voice_listen", "label": "Listened", "preview": "{seconds}s",
             "description": "Record N seconds from the mic and transcribe with the speech-to-text chosen in /voice.",
             "parameters": {"type": "object", "properties": {
                "seconds": {"type": "integer"}},
             }},
        ],
        "commands": ["/voice"], "subcommands": ["voice"],
        "command_descriptions": {"/voice": "speech in and out"},
        "composer_actions": [
            {"id": "dictate", "keys": [DEFAULT_HOTKEY], "argv": ["dictate"], "busy": "\u{25cf} 0:00"},
            {"id": "speak", "keys": [], "argv": ["speak-answer"], "on": "turn_end", "busy": "\u{25d6}"},
        ],
    })
}

struct Shared {
    enabled: AtomicBool,
    pump_q: Mutex<VecDeque<String>>,
    cv: Condvar,
    heard: Mutex<VecDeque<String>>,
    inflight: Mutex<HashMap<String, String>>,
    denied: AtomicBool,
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
        || in_path(bin)
}

/// Extensions a command-line binary can carry. `PATHEXT` on Windows also lists
/// shell handlers that are not programs (`.CPL`, `.MSC`, `.VBS`, `.JS`, …), so
/// appending it verbatim matched `C:\WINDOWS\system32\main.cpl` as
/// whisper.cpp's legacy `main` binary and made whisper look installed when it
/// was not — which sent the user after a model instead of whisper itself.
const RUNNABLE_EXT: [&str; 4] = [".exe", ".com", ".bat", ".cmd"];

fn in_path(bin: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    in_dirs(bin, &std::env::split_paths(&paths).collect::<Vec<_>>())
}

fn in_dirs(bin: &str, dirs: &[PathBuf]) -> bool {
    if dirs.iter().any(|d| d.join(bin).is_file()) {
        return true;
    }
    if cfg!(windows) {
        return RUNNABLE_EXT
            .iter()
            .any(|ext| dirs.iter().any(|d| d.join(format!("{bin}{ext}")).is_file()));
    }
    false
}

fn pick(cands: &[&str]) -> Option<String> {
    cands.iter().find(|b| have(b)).map(|s| s.to_string())
}

#[cfg(target_os = "linux")]
fn bound(cmd: &mut Command) -> std::io::Result<Child> {
    use std::os::unix::process::CommandExt;
    extern "C" {
        fn prctl(option: i32, ...) -> i32;
    }
    unsafe {
        cmd.pre_exec(|| {
            prctl(1, 9);
            Ok(())
        });
    }
    cmd.spawn()
}

#[cfg(not(target_os = "linux"))]
fn bound(cmd: &mut Command) -> std::io::Result<Child> {
    cmd.spawn()
}

fn tmp(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{tag}-{}.wav", std::process::id()))
}

fn gray_home() -> PathBuf {
    std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .or_else(|| std::env::var_os("USERPROFILE").filter(|h| !h.is_empty()))
                .map(|h| PathBuf::from(h).join(".gray"))
        })
        .unwrap_or_else(|| PathBuf::from("."))
}

fn config_path() -> PathBuf {
    gray_home().join("voice").join("config.json")
}

fn read_config() -> Value {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

fn write_private(path: &Path, data: &str) -> std::io::Result<()> {
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
    if !c.is_object() {
        c = json!({});
    }
    f(&mut c);
    let r = write_private(&cfg, &serde_json::to_string_pretty(&c).unwrap_or_default());
    let _ = lock.unlock();
    r.map_err(|e| e.to_string())
}

fn read_json(path: PathBuf) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null)
}

fn migrate() {
    let c = read_config();
    let legacy_stt = c["stt"].is_array();
    if c["migrated"].as_bool().unwrap_or(false) && !legacy_stt {
        return;
    }
    let old_keys = read_json(gray_home().join("voice").join("keys.json"));
    let pro = read_json(gray_home().join("voicepro").join("config.json"));
    let _ = update_config(|c| {
        if !c["keys"].is_object() {
            c["keys"] = json!({});
        }
        let mut found: Vec<(String, String)> = Vec::new();
        if let Some(o) = old_keys.as_object() {
            found.extend(
                o.iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))),
            );
        }
        for (id, field) in [("deepgram", "deepgram_api_key"), ("fish", "fish_api_key")] {
            if let Some(v) = pro[field].as_str() {
                found.push((id.into(), v.into()));
            }
        }
        if !c["migrated"].as_bool().unwrap_or(false) {
            for (id, v) in found {
                if !v.is_empty() && c["keys"][&id].is_null() {
                    c["keys"][&id] = json!(v);
                }
            }
            c["migrated"] = json!(true);
        }
        if let Some(list) = c["stt"].as_array() {
            let pick = list
                .iter()
                .filter_map(Value::as_str)
                .find(|id| STT_NAMES.iter().any(|(n, _)| n == id))
                .unwrap_or("local")
                .to_string();
            c["stt"] = json!(pick);
        }
    });
}

fn provider_key(id: &str) -> Option<String> {
    std::env::var(format!("{}_API_KEY", id.to_uppercase()))
        .ok()
        .or_else(|| read_config()["keys"][id].as_str().map(str::to_string))
        .filter(|k| !k.is_empty())
}

fn stt_active() -> String {
    read_config()["stt"].as_str().unwrap_or("local").to_string()
}

fn hold_to_talk() -> bool {
    read_config()["dictate_mode"] == "hold"
}

fn hotkey() -> String {
    let user = std::fs::read_to_string(gray_home().join("keybindings.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get(HOTKEY_ID).cloned());
    let first = match user {
        Some(Value::String(k)) => Some(k),
        Some(Value::Array(a)) => a.first().and_then(|k| k.as_str()).map(str::to_string),
        _ => None,
    };
    first.unwrap_or_else(|| DEFAULT_HOTKEY.to_string())
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

fn voice_gate() -> f64 {
    std::env::var("VOICE_RMS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(150.0)
}

fn wav_data_offset(path: &Path) -> Option<u64> {
    use std::io::Read;
    let mut head = vec![0u8; 512];
    let n = std::fs::File::open(path).ok()?.read(&mut head).ok()?;
    head[..n]
        .windows(4)
        .position(|w| w == b"data")
        .map(|p| p as u64 + 8)
}

fn read_range(path: &Path, from: u64, len: u64) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    if len == 0 {
        return None;
    }
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = vec![0u8; len as usize];
    f.read_exact(&mut buf).ok()?;
    Some(buf)
}

fn wav_bytes(pcm: &[u8]) -> Vec<u8> {
    let n = pcm.len() as u32;
    let mut w = Vec::with_capacity(44 + pcm.len());
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + n).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&16_000u32.to_le_bytes());
    w.extend_from_slice(&32_000u32.to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&n.to_le_bytes());
    w.extend_from_slice(pcm);
    w
}

fn level_of(mean: f64) -> usize {
    (((mean.max(1.0).ln() - 5.0) / 3.5 * 7.0).clamp(0.0, 7.0)) as usize
}

pub(crate) fn samples(b: &[u8]) -> impl Iterator<Item = i16> + '_ {
    (0..b.len() / 2).map(move |i| i16::from_le_bytes([b[i * 2], b[i * 2 + 1]]))
}

fn tail_mean(path: &Path) -> (u64, f64) {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return (0, 0.0);
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len < 44 + 3200 {
        return (len, 0.0);
    }
    let mut buf = vec![0u8; 3200];
    if f.seek(SeekFrom::Start((len - 3200) & !1)).is_err() || f.read_exact(&mut buf).is_err() {
        return (len, 0.0);
    }
    let mean = samples(&buf).map(|s| f64::from(s).abs()).sum::<f64>() / 1600.0;
    (len, mean)
}

fn loud(path: &Path) -> bool {
    let gate = voice_gate();
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    if bytes.len() < 400 {
        return false;
    }
    let mut sum = 0u64;
    let mut n = 0u64;
    for s in samples(&bytes[44..]) {
        sum += s.unsigned_abs() as u64;
        n += 1;
    }
    n > 0 && (sum as f64 / n as f64) > gate
}

fn recorder_cmd() -> Option<Command> {
    if in_path("pw-record") {
        let mut c = Command::new("pw-record");
        c.args(["--rate", "16000", "--channels", "1", "--format", "s16"]);
        Some(c)
    } else if in_path("arecord") {
        let mut c = Command::new("arecord");
        c.args(["-q", "-f", "S16_LE", "-r", "16000", "-c", "1"]);
        Some(c)
    } else if in_path("sox") {
        let mut c = Command::new("sox");
        c.args(["-q", "-d", "-r", "16000", "-c", "1", "-b", "16"]);
        Some(c)
    } else {
        None
    }
}

fn have_recorder() -> bool {
    ["pw-record", "arecord", "sox"].iter().any(|r| in_path(r))
}

struct Capture {
    rec: Option<Child>,
    path: PathBuf,
    data_at: Option<u64>,
    voiced: bool,
    quiet: u32,
    gate: f64,
}

impl Capture {
    fn start(path: PathBuf) -> Result<Self, String> {
        let _ = std::fs::remove_file(&path);
        let mut cmd = recorder_cmd().ok_or(NO_RECORDER)?;
        let rec = bound(
            cmd.arg(&path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        )
        .map_err(|_| "Recording failed".to_string())?;
        Ok(Capture {
            rec: Some(rec),
            path,
            data_at: None,
            voiced: false,
            quiet: 0,
            gate: voice_gate(),
        })
    }

    fn alive(&mut self) -> bool {
        matches!(self.rec.as_mut().map(Child::try_wait), Some(Ok(None)))
    }

    fn len(&self) -> u64 {
        std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }

    fn idle_len(&self) -> Option<u64> {
        (!self.voiced).then(|| self.len())
    }

    fn skip(&mut self) {
        self.data_at = Some(self.len().max(44) & !1);
        self.voiced = false;
        self.quiet = 0;
    }

    fn tick(&mut self) -> (usize, Option<Vec<u8>>) {
        let (len, mean) = tail_mean(&self.path);
        if self.data_at.is_none() {
            if len < 44 {
                return (0, None);
            }
            self.data_at = Some(wav_data_offset(&self.path).unwrap_or(44));
        }
        let at = self.data_at.unwrap_or(44);
        if mean > self.gate {
            self.voiced = true;
            self.quiet = 0;
        } else {
            self.quiet += 1;
        }
        let seg_len = len.saturating_sub(at) & !1;
        let mut seg = None;
        if (self.voiced && self.quiet >= 7 && seg_len >= 32_000) || seg_len >= 32_000 * 25 {
            seg = read_range(&self.path, at, seg_len);
            self.data_at = Some(at + seg_len);
            self.voiced = false;
        } else if !self.voiced && seg_len > 16_000 {
            self.data_at = Some(at + seg_len - 16_000);
        }
        (level_of(mean), seg)
    }

    fn stop(&mut self) {
        if let Some(mut rec) = self.rec.take() {
            let interrupted = !cfg!(windows)
                && Command::new("kill")
                    .args(["-INT", &rec.id().to_string()])
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success());
            if !interrupted {
                let _ = rec.kill();
            }
            let _ = rec.wait();
        }
    }

    fn rest(&mut self) -> Option<Vec<u8>> {
        self.stop();
        let at = self
            .data_at
            .unwrap_or_else(|| wav_data_offset(&self.path).unwrap_or(44));
        read_range(&self.path, at, self.len().saturating_sub(at) & !1).filter(|p| p.len() >= 3_200)
    }

    fn all(&mut self) -> Option<Vec<u8>> {
        self.data_at = None;
        self.rest()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_file(&self.path);
    }
}

fn whisper_bin() -> Option<String> {
    pick(&["whisper-cli", "whisper", "main", "whisper-cpp"])
}

fn whisper_model() -> Option<String> {
    let cfg = read_config();
    let explicit = std::env::var("VOICE_MODEL")
        .or_else(|_| std::env::var("WHISPER_MODEL"))
        .ok()
        .or_else(|| cfg["whisper_model"].as_str().map(str::to_string))
        .filter(|m| Path::new(m).is_file());
    if explicit.is_some() {
        return explicit;
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let dirs = [
        gray_home().join("voice").join("models"),
        home.join(".cache").join("whisper"),
        home.join("whisper.cpp").join("models"),
        home.join("src").join("whisper.cpp").join("models"),
        PathBuf::from("/usr/share/whisper.cpp/models"),
        PathBuf::from("/usr/local/share/whisper.cpp/models"),
    ];
    let prefer = [
        "base.en",
        "small.en",
        "base",
        "small",
        "tiny.en",
        "tiny",
        "medium.en",
        "medium",
    ];
    let mut found: Vec<(usize, PathBuf)> = Vec::new();
    for d in dirs {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Some(stem) = name
                .strip_prefix("ggml-")
                .and_then(|n| n.strip_suffix(".bin"))
            else {
                continue;
            };
            let rank = prefer
                .iter()
                .position(|p| *p == stem)
                .unwrap_or(prefer.len());
            found.push((rank, e.path()));
        }
    }
    found.sort_by_key(|(r, _)| *r);
    found.first().map(|(_, p)| p.display().to_string())
}

fn local_stt_problem() -> Option<String> {
    if whisper_bin().is_none() {
        Some("No whisper.cpp \u{b7} install whisper-cli".into())
    } else if whisper_model().is_none() {
        Some(MODEL_MISSING.into())
    } else {
        None
    }
}

fn stt_problem() -> Option<String> {
    let id = stt_active();
    match id.as_str() {
        "deepgram" | "groq" if provider_key(&id).is_none() => {
            let name = STT_NAMES
                .iter()
                .find(|(n, _)| *n == id)
                .map_or("", |(_, l)| l);
            Some(format!("No {name} key \u{b7} add one in /voice"))
        }
        "deepgram" | "groq" => None,
        _ => local_stt_problem(),
    }
}

fn ambient_problem() -> Option<String> {
    if !have_recorder() {
        return Some(NO_RECORDER.into());
    }
    stt_problem()
}

fn download_model(mut progress: impl FnMut(u8)) -> Result<(), String> {
    let dir = gray_home().join("voice").join("models");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let dest = dir.join(MODEL_FILE);
    let part = dir.join(format!("{MODEL_FILE}.part"));
    let mut child = Command::new("curl")
        .args(["-fsSL", "-o"])
        .arg(&part)
        .arg(MODEL_URL)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "No curl \u{b7} install curl".to_string())?;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                let got = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
                progress((got * 100 / MODEL_BYTES).min(99) as u8);
                std::thread::sleep(Duration::from_millis(300));
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    if !status.success() {
        let _ = std::fs::remove_file(&part);
        return Err("Download failed \u{b7} check your connection".into());
    }
    std::fs::rename(&part, &dest).map_err(|e| e.to_string())
}

fn whisper_transcribe(wav: &Path) -> Result<String, String> {
    let bin = whisper_bin().ok_or("No whisper.cpp \u{b7} install whisper-cli")?;
    let model = whisper_model().ok_or(MODEL_MISSING)?;
    let out = Command::new(bin)
        .args(["-m", &model, "-f"])
        .arg(wav)
        .args(["-nt", "-np"])
        .output()
        .map_err(|e| format!("whisper: {e}"))?;
    if !out.status.success() {
        return Err("whisper failed".into());
    }
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let l = t.to_lowercase();
    if l.starts_with('[') && l.ends_with(']') {
        return Ok(String::new());
    }
    Ok(t)
}

fn transcribe_groq(key: &str, wav: &Path) -> Result<String, String> {
    let mut child = Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "90",
            "-K",
            "-",
            "-X",
            "POST",
            "https://api.groq.com/openai/v1/audio/transcriptions",
            "-F",
            &format!("file=@{}", wav.display()),
            "-F",
            "model=whisper-large-v3-turbo",
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
        return Err("Groq offline".into());
    }
    if body.starts_with('{') {
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        return match v["text"].as_str() {
            Some(t) => Ok(t.to_string()),
            None => Err(match v["error"]["code"].as_str() {
                Some("invalid_api_key") => "Groq invalid key".to_string(),
                _ => format!("Groq {}", body.chars().take(80).collect::<String>()),
            }),
        };
    }
    Ok(body)
}

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
            | "silence"
    )
}

const NOISE: &str = "heard only room noise";

fn stt_transcribe(wav: &Path) -> Result<String, String> {
    let id = stt_active();
    let t = match id.as_str() {
        "deepgram" => {
            cloud::deepgram_transcribe(&provider_key("deepgram").ok_or("No Deepgram key")?, wav)
        }
        "groq" => transcribe_groq(&provider_key("groq").ok_or("No Groq key")?, wav),
        _ => whisper_transcribe(wav),
    }?;
    if is_silence_hallucination(&t) {
        return Err(NOISE.into());
    }
    Ok(t.trim().to_string())
}

fn transcribe_pcm(pcm: &[u8], tag: &str) -> Result<String, String> {
    let wav = std::env::temp_dir().join(format!("{tag}-{}.wav", std::process::id()));
    std::fs::write(&wav, wav_bytes(pcm)).map_err(|e| e.to_string())?;
    let r = if loud(&wav) {
        stt_transcribe(&wav)
    } else {
        Err("too quiet".into())
    };
    let _ = std::fs::remove_file(&wav);
    r
}

enum TestEvent {
    Level(usize, u64),
    Transcribing,
}

fn test_mic(mut on: impl FnMut(TestEvent)) -> Result<Option<String>, String> {
    if !have_recorder() {
        return Err(NO_RECORDER.into());
    }
    if let Some(p) = stt_problem() {
        return Err(p);
    }
    let mut cap = Capture::start(tmp("voice-test"))?;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(3) && cap.alive() {
        std::thread::sleep(Duration::from_millis(100));
        on(TestEvent::Level(
            level_of(tail_mean(&cap.path).1),
            t0.elapsed().as_millis() as u64,
        ));
    }
    let pcm = cap.all().ok_or("No mic input \u{b7} check the device")?;
    on(TestEvent::Transcribing);
    match transcribe_pcm(&pcm, "voice-test") {
        Ok(t) => Ok(Some(t)),
        Err(e) if e == NOISE => Ok(None),
        Err(e) if e == "too quiet" => Err("No sound \u{b7} check your mic".into()),
        Err(e) => Err(e),
    }
}

fn dictate_live(hold: bool, key: &str) -> Result<String, String> {
    use std::io::BufRead;
    let mut cap = Capture::start(tmp("voice-dictate"))?;
    enum KeyMsg {
        Down,
        Up,
        Stop,
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            let msg = match line.trim() {
                "down" => KeyMsg::Down,
                "stop" => KeyMsg::Stop,
                _ => KeyMsg::Up,
            };
            if tx.send(msg).is_err() {
                return;
            }
        }
        let _ = tx.send(KeyMsg::Stop);
    });
    let key = key.to_lowercase();
    let hint = if hold {
        format!("release {key}")
    } else {
        format!("{key} stop")
    };
    let (seg_tx, seg_rx) = mpsc::channel::<Vec<u8>>();
    let worker = std::thread::spawn(move || {
        let mut all = String::new();
        let mut last_err = String::from("No speech heard");
        for (n, pcm) in seg_rx.into_iter().enumerate() {
            match transcribe_pcm(&pcm, &format!("voice-seg{n}")) {
                Ok(t) => {
                    if !all.is_empty() {
                        all.push(' ');
                    }
                    all.push_str(&t.replace(['\n', '\t'], " "));
                }
                Err(e) if e == "too quiet" || e == NOISE => {}
                Err(e) => last_err = e,
            }
        }
        if all.is_empty() {
            Err(last_err)
        } else {
            Ok(all)
        }
    });
    let start = Instant::now();
    let mut last_down = start;
    let mut levels: VecDeque<usize> = VecDeque::new();
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let mut stop = false;
        while let Ok(msg) = rx.try_recv() {
            let now = Instant::now();
            match msg {
                KeyMsg::Stop => stop = true,
                KeyMsg::Up => stop |= hold,
                KeyMsg::Down => {
                    if !hold
                        && now - start > Duration::from_millis(700)
                        && now - last_down > Duration::from_millis(150)
                    {
                        stop = true;
                    }
                    last_down = now;
                }
            }
        }
        if hold && last_down.elapsed() > Duration::from_millis(800) {
            stop = true;
        }
        if start.elapsed() > Duration::from_secs(600) || !cap.alive() {
            stop = true;
        }
        let (lvl, seg) = cap.tick();
        if let Some(pcm) = seg {
            let _ = seg_tx.send(pcm);
        }
        levels.push_back(lvl);
        if levels.len() > 7 {
            levels.pop_front();
        }
        let bars = [
            '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}',
            '\u{2588}',
        ];
        let mut meter: String = levels.iter().map(|&i| bars[i]).collect();
        while meter.chars().count() < 7 {
            meter.push(bars[0]);
        }
        let secs = start.elapsed().as_secs();
        eprintln!("\u{25cf} {}:{:02} {meter}\t{hint}", secs / 60, secs % 60);
        if stop {
            break;
        }
    }
    if let Some(pcm) = cap.rest() {
        let _ = seg_tx.send(pcm);
    }
    drop(seg_tx);
    eprintln!("Transcribing\u{2026}");
    worker
        .join()
        .unwrap_or_else(|_| Err("Transcriber crashed".into()))
}

fn listener(sh: Arc<Shared>) {
    let (seg_tx, seg_rx) = mpsc::channel::<Vec<u8>>();
    let sh2 = Arc::clone(&sh);
    std::thread::spawn(move || {
        for pcm in seg_rx {
            if let Ok(text) = transcribe_pcm(&pcm, "voice-ambient") {
                sh2.pump_q.lock().unwrap().push_back(text);
                sh2.cv.notify_one();
            }
        }
    });
    let mut cap: Option<Capture> = None;
    let mut warned = false;
    let mut last_speaking: Option<Instant> = None;
    let path = tmp("voice-ambient");
    while sh.enabled.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
        if cap.as_mut().is_some_and(|c| !c.alive()) {
            cap = None;
        }
        if cap.is_none() {
            if let Some(p) = ambient_problem() {
                if !warned {
                    warned = true;
                    eprintln!("voice: {p}");
                }
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
            cap = Capture::start(path.clone()).ok();
            continue;
        }
        let c = cap.as_mut().expect("capture started");
        if speech::is_speaking() {
            last_speaking = Some(Instant::now());
        }
        if last_speaking.is_some_and(|t| t.elapsed() < Duration::from_millis(800)) {
            c.skip();
            continue;
        }
        let (_, seg) = c.tick();
        if let Some(pcm) = seg {
            let _ = seg_tx.send(pcm);
        }
        if c.idle_len().is_some_and(|l| l > 6_400_000) {
            cap = None;
        }
    }
    sh.listener_started.store(false, Ordering::Relaxed);
}

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

fn apply_listen(sh: &Arc<Shared>, on: bool) {
    sh.enabled.store(on, Ordering::Relaxed);
    if on && !sh.listener_started.swap(true, Ordering::Relaxed) {
        let sh2 = Arc::clone(sh);
        std::thread::spawn(move || listener(sh2));
    }
}

fn set_listen(sh: Option<&Arc<Shared>>, on: bool) -> Result<(), String> {
    if on {
        if let Some(p) = ambient_problem() {
            return Err(p);
        }
    }
    update_config(|c| c["listen"] = json!(on))?;
    if let Some(sh) = sh {
        apply_listen(sh, on);
    }
    Ok(())
}

fn voice_say(a: &Value) -> Result<String, String> {
    let text = a["text"].as_str().ok_or("text required")?;
    let spoke = speech::say(text, a["engine"].as_str(), a["rate"].as_u64())?;
    Ok(format!("spoke via {spoke}"))
}

fn voice_listen(a: &Value) -> Result<String, String> {
    let secs = a["seconds"].as_u64().unwrap_or(5).clamp(1, 60);
    if !have_recorder() {
        return Err(NO_RECORDER.into());
    }
    if let Some(p) = stt_problem() {
        return Err(p);
    }
    let mut cap = Capture::start(tmp("voice-listen"))?;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(secs) && cap.alive() {
        std::thread::sleep(Duration::from_millis(100));
    }
    let pcm = cap.all().ok_or("No mic input \u{b7} check the device")?;
    transcribe_pcm(&pcm, "voice-listen")
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

fn test_line() -> String {
    match test_mic(|_| {}) {
        Ok(Some(t)) => format!("\u{2713} Ready \u{b7} \u{201c}{t}\u{201d}"),
        Ok(None) => "\u{2713} Ready".into(),
        Err(e) => format!("\u{2717} {e}"),
    }
}

fn command_run(sh: &Arc<Shared>, params: &Value) -> Value {
    let argv: Vec<&str> = params["argv"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let text = match argv.as_slice() {
        [] => return json!({"result": {"terminal": ["page", "--gray-modal"]}}),
        ["on"] => match set_listen(Some(sh), true) {
            Ok(()) => "Ambient on".to_string(),
            Err(e) => format!("\u{2717} {e}"),
        },
        ["off"] => match set_listen(Some(sh), false) {
            Ok(()) => "Ambient off".to_string(),
            Err(e) => format!("\u{2717} {e}"),
        },
        ["say", rest @ ..] if !rest.is_empty() => match speech::say(&rest.join(" "), None, None) {
            Ok(_) => String::new(),
            Err(e) => format!("\u{2717} {e}"),
        },
        ["test"] => test_line(),
        _ => "/voice [on|off|say TEXT|test]".to_string(),
    };
    json!({"result": {"text": text}})
}

fn sidecar() -> std::io::Result<()> {
    migrate();
    let env_on = std::env::var("VOICE_LISTEN")
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(false);
    let sh = Arc::new(Shared {
        enabled: AtomicBool::new(false),
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
    {
        let sh2 = Arc::clone(&sh);
        std::thread::spawn(move || loop {
            let want = env_on || read_config()["listen"].as_bool().unwrap_or(false);
            if want != sh2.enabled.load(Ordering::Relaxed) {
                apply_listen(&sh2, want);
            } else if want && !sh2.listener_started.load(Ordering::Relaxed) {
                apply_listen(&sh2, true);
            }
            std::thread::sleep(Duration::from_secs(2));
        });
    }
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
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

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1)
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
        Some("page") => {
            migrate();
            if let Err(e) = page::run() {
                fail(&e);
            }
        }
        Some("test") => {
            migrate();
            println!("{}", test_line());
        }
        Some("on") | Some("off") => {
            migrate();
            let on = args[0] == "on";
            match set_listen(None, on) {
                Ok(()) => println!("Ambient {}", if on { "on" } else { "off" }),
                Err(e) => fail(&e),
            }
        }
        Some("say") => {
            migrate();
            if let Err(e) = speech::say(&args[1..].join(" "), None, None) {
                fail(&e);
            }
        }
        Some("speak-answer") => {
            if let Err(e) = speech::speak_answer() {
                fail(&e);
            }
        }
        Some("dictate") => {
            migrate();
            if let Some(p) = ambient_problem() {
                fail(&p);
            }
            match args.get(1).and_then(|s| s.parse::<u64>().ok()) {
                Some(n) => match voice_listen(&json!({"seconds": n})) {
                    Ok(t) => println!("{t}"),
                    Err(e) => fail(&e),
                },
                None => match dictate_live(hold_to_talk(), &key_label(&hotkey())) {
                    Ok(t) => println!("{t}"),
                    Err(e) => fail(&e),
                },
            }
        }
        _ => {
            if args.is_empty() && !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                let _ = sidecar();
                return;
            }
            fail("voice page|on|off|say TEXT|test");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::in_dirs;
    use std::path::PathBuf;

    fn dir_with(tag: &str, files: &[&str]) -> PathBuf {
        let d = std::env::temp_dir().join(format!("voice-bin-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("temp dir");
        for f in files {
            std::fs::write(d.join(f), b"").expect("write");
        }
        d
    }

    #[test]
    fn a_control_panel_applet_is_not_whisper() {
        // PATHEXT carries `.CPL` (and `.MSC`, `.VBS`, …), so appending every
        // extension it lists made `main` resolve to C:\WINDOWS\system32\main.cpl
        // and the plugin believe whisper.cpp was installed.
        let system32 = dir_with("sys", &["main.cpl", "main.CPL", "whisper-cli.CPL"]);
        assert!(!in_dirs("main", std::slice::from_ref(&system32)));
        assert!(!in_dirs("whisper-cli", &[system32]));
    }

    #[test]
    #[cfg(windows)]
    fn a_real_executable_on_the_path_is_found() {
        let bin = dir_with("bin", &["whisper-cli.exe", "whisper-cpp.bat"]);
        assert!(in_dirs("whisper-cli", std::slice::from_ref(&bin)));
        assert!(in_dirs("whisper-cpp", &[bin]));
    }
}
