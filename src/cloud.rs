use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct Reply {
    pub code: u16,
    pub body: Vec<u8>,
}

fn temp(tag: &str) -> std::path::PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "voice-{tag}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

pub fn http(
    url: &str,
    headers: &[String],
    body: Option<(&Path, &str)>,
    secs: u32,
) -> Result<Reply, String> {
    if !crate::have("curl") {
        return Err("no curl".into());
    }
    let out = temp("http");
    let mut cmd = Command::new("curl");
    cmd.args(["-sS", "--max-time", &secs.to_string(), "-K", "-", "-o"])
        .arg(&out)
        .args(["-w", "%{http_code}"]);
    if let Some((path, content_type)) = body {
        cmd.args(["-X", "POST", "-H"])
            .arg(format!("Content-Type: {content_type}"))
            .arg("--data-binary")
            .arg(format!("@{}", path.display()));
    }
    let mut child = cmd
        .arg(url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("curl: {e}"))?;
    if let Some(mut si) = child.stdin.take() {
        for h in headers {
            let esc = h.replace('\\', "\\\\").replace('"', "\\\"");
            let _ = writeln!(si, "header = \"{esc}\"");
        }
    }
    let o = child.wait_with_output().map_err(|e| format!("curl: {e}"))?;
    let bytes = std::fs::read(&out).unwrap_or_default();
    let _ = std::fs::remove_file(&out);
    if !o.status.success() {
        return Err(match o.status.code() {
            Some(6 | 7 | 28 | 35 | 52 | 56) => "offline".into(),
            _ => String::from_utf8_lossy(&o.stderr)
                .lines()
                .next()
                .unwrap_or("request failed")
                .chars()
                .take(80)
                .collect(),
        });
    }
    let code = String::from_utf8_lossy(&o.stdout)
        .trim()
        .parse()
        .unwrap_or(0);
    Ok(Reply { code, body: bytes })
}

fn post(url: &str, headers: &[String], content_type: &str, body: &[u8]) -> Result<Reply, String> {
    let file = temp("body");
    std::fs::write(&file, body).map_err(|e| e.to_string())?;
    let r = http(url, headers, Some((&file, content_type)), 90);
    let _ = std::fs::remove_file(&file);
    r
}

fn detail(r: &Reply) -> String {
    if r.code == 401 || r.code == 403 {
        return "invalid key".into();
    }
    let v: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
    let msg = [
        "/err_msg",
        "/error/message",
        "/message",
        "/detail",
        "/error",
    ]
    .iter()
    .find_map(|p| v.pointer(p).and_then(Value::as_str))
    .map(str::to_string)
    .unwrap_or_default();
    if msg.is_empty() {
        format!("HTTP {}", r.code)
    } else {
        format!(
            "HTTP {}: {}",
            r.code,
            msg.chars().take(80).collect::<String>()
        )
    }
}

pub fn validate_key(id: &str, key: &str) -> Result<(), String> {
    let bearer = format!("Authorization: Bearer {key}");
    let (url, header) = match id {
        "deepgram" => (
            "https://api.deepgram.com/v1/auth/token",
            format!("Authorization: Token {key}"),
        ),
        "groq" => ("https://api.groq.com/openai/v1/models", bearer),
        "xai" => ("https://api.x.ai/v1/models", bearer),
        "fish" => ("https://api.fish.audio/wallet/self/api-credit", bearer),
        _ => return Ok(()),
    };
    let r = http(url, &[header], None, 15)?;
    if (200..300).contains(&r.code) {
        Ok(())
    } else if r.code == 401 || r.code == 403 {
        Err("invalid".into())
    } else {
        Err(format!("HTTP {}", r.code))
    }
}

pub fn deepgram_transcribe(key: &str, wav: &Path) -> Result<String, String> {
    let model = std::env::var("DEEPGRAM_STT_MODEL").unwrap_or_else(|_| "nova-3".into());
    let url = format!("https://api.deepgram.com/v1/listen?model={model}&smart_format=true");
    let bytes = std::fs::read(wav).map_err(|e| e.to_string())?;
    let r = post(
        &url,
        &[format!("Authorization: Token {key}")],
        "audio/wav",
        &bytes,
    )?;
    if !(200..300).contains(&r.code) {
        return Err(format!("Deepgram {}", detail(&r)));
    }
    let v: Value = serde_json::from_slice(&r.body).map_err(|e| e.to_string())?;
    Ok(v["results"]["channels"][0]["alternatives"][0]["transcript"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string())
}

pub fn synth(provider: &str, text: &str) -> Result<Vec<u8>, String> {
    let (name, url, headers, body) = match provider {
        "deepgram" => {
            let key = crate::provider_key("deepgram").ok_or("No Deepgram key")?;
            let model =
                std::env::var("DEEPGRAM_TTS_MODEL").unwrap_or_else(|_| "aura-2-thalia-en".into());
            (
                "Deepgram",
                format!(
                    "https://api.deepgram.com/v1/speak?model={model}&encoding=linear16&container=wav"
                ),
                vec![format!("Authorization: Token {key}")],
                json!({"text": text}),
            )
        }
        "fish" => {
            let key = crate::provider_key("fish").ok_or("No Fish Audio key")?;
            let model = std::env::var("FISH_MODEL").unwrap_or_else(|_| "s2.1-pro".into());
            (
                "Fish Audio",
                "https://api.fish.audio/v1/tts".to_string(),
                vec![
                    format!("Authorization: Bearer {key}"),
                    format!("model: {model}"),
                ],
                json!({"text": text, "format": "wav"}),
            )
        }
        other => return Err(format!("unknown voice {other}")),
    };
    let r = post(
        &url,
        &headers,
        "application/json",
        body.to_string().as_bytes(),
    )?;
    if !(200..300).contains(&r.code) {
        return Err(format!("{name} {}", detail(&r)));
    }
    if r.body.is_empty() {
        return Err(format!("{name} returned no audio"));
    }
    Ok(r.body)
}
