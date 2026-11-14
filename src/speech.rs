use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use crate::{have, read_config};

const BARS: [char; 8] = [
    '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}', '\u{2588}',
];

fn flag_path() -> PathBuf {
    crate::gray_home().join("voice").join("speaking")
}

pub fn is_speaking() -> bool {
    std::fs::metadata(flag_path())
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|d| d < Duration::from_millis(1500))
}

struct Flag;

impl Flag {
    fn touch(&self) {
        let p = flag_path();
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(p, b"1");
    }
}

impl Drop for Flag {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(flag_path());
    }
}

pub fn piper_model() -> Option<String> {
    std::env::var("PIPER_MODEL")
        .ok()
        .filter(|m| !m.is_empty())
        .or_else(|| read_config()["piper_model"].as_str().map(str::to_string))
}

pub fn local_engine() -> Option<String> {
    ["say", "piper", "espeak-ng", "espeak", "spd-say"]
        .iter()
        .find(|e| (**e != "piper" || piper_model().is_some()) && have(e))
        .map(|s| s.to_string())
}

pub fn tts_active() -> String {
    read_config()["tts"].as_str().unwrap_or("local").to_string()
}

pub fn read_answers() -> bool {
    read_config()["read_answers"].as_bool().unwrap_or(false)
}

fn wav_levels(wav: &[u8]) -> Vec<usize> {
    let (mut rate, mut channels, mut data) = (16_000u32, 1usize, None);
    let mut i = 12;
    while i + 8 <= wav.len() {
        let size = u32::from_le_bytes([wav[i + 4], wav[i + 5], wav[i + 6], wav[i + 7]]) as usize;
        match &wav[i..i + 4] {
            b"fmt " if i + 16 <= wav.len() => {
                channels = u16::from_le_bytes([wav[i + 10], wav[i + 11]]).max(1) as usize;
                rate = u32::from_le_bytes([wav[i + 12], wav[i + 13], wav[i + 14], wav[i + 15]]);
            }
            b"data" => {
                data = Some((i + 8, size.min(wav.len().saturating_sub(i + 8))));
                break;
            }
            _ => {}
        }
        i += 8 + size + (size & 1);
    }
    let Some((start, len)) = data else {
        return Vec::new();
    };
    let window = (rate as usize * channels * 2 / 12).max(2) & !1;
    wav[start..start + len]
        .chunks(window)
        .map(|c| {
            let sum: f64 = c
                .chunks_exact(2)
                .map(|b| f64::from(i16::from_le_bytes([b[0], b[1]])).abs())
                .sum();
            crate::level_of(sum / (c.len() / 2).max(1) as f64)
        })
        .collect()
}

fn meter(levels: &[usize], t: Duration) -> String {
    let step = (t.as_millis() / 83) as usize;
    (0..5)
        .map(|k| {
            let lvl = if levels.is_empty() {
                (3.5 + 3.5 * (t.as_secs_f64() * 7.0 + k as f64 * 1.3).sin()) as usize
            } else {
                levels
                    .get((step + k).saturating_sub(4))
                    .copied()
                    .unwrap_or(0)
            };
            BARS[lvl.min(7)]
        })
        .collect()
}

fn run_with_meter(mut child: Child, levels: &[usize], flag: &Flag) -> Result<(), String> {
    let t0 = Instant::now();
    loop {
        match child.try_wait() {
            Ok(None) => {}
            Ok(Some(st)) if st.success() => return Ok(()),
            Ok(Some(st)) => return Err(format!("player exited {st}")),
            Err(e) => return Err(e.to_string()),
        }
        flag.touch();
        if std::env::var_os("GRAY_ACTION_INSERT").is_some() {
            eprintln!("\u{25d6} {}\tesc", meter(levels, t0.elapsed()));
        }
        std::thread::sleep(Duration::from_millis(83));
    }
}

#[cfg(not(windows))]
fn player(wav: &std::path::Path) -> Option<Command> {
    let players: [(&str, &[&str]); 5] = [
        ("afplay", &[]),
        ("paplay", &[]),
        ("aplay", &["-q"]),
        ("ffplay", &["-nodisp", "-autoexit", "-loglevel", "error"]),
        ("play", &["-q"]),
    ];
    players.iter().find(|(b, _)| have(b)).map(|(b, a)| {
        let mut c = Command::new(b);
        c.args(*a).arg(wav);
        c
    })
}

#[cfg(windows)]
fn player(wav: &std::path::Path) -> Option<Command> {
    let mut c = Command::new("powershell");
    c.args([
        "-NoProfile",
        "-Command",
        &format!(
            "(New-Object Media.SoundPlayer '{}').PlaySync()",
            wav.display().to_string().replace('\'', "''")
        ),
    ]);
    Some(c)
}

fn play_wav(wav: &[u8], flag: &Flag) -> Result<(), String> {
    let path = std::env::temp_dir().join(format!("voice-say-{}.wav", std::process::id()));
    std::fs::write(&path, wav).map_err(|e| e.to_string())?;
    let r = (|| {
        let mut cmd = player(&path).ok_or("No audio player \u{b7} install alsa-utils or ffmpeg")?;
        let child = crate::bound(
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        )
        .map_err(|e| e.to_string())?;
        run_with_meter(child, &wav_levels(wav), flag)
    })();
    let _ = std::fs::remove_file(&path);
    r
}

fn local_wav(engine: &str, text: &str, rate: &str) -> Result<Vec<u8>, String> {
    match engine {
        "piper" => {
            let model = piper_model().ok_or("No piper model \u{b7} set PIPER_MODEL")?;
            let path = std::env::temp_dir().join(format!("voice-piper-{}.wav", std::process::id()));
            let mut child = Command::new("piper")
                .args(["-m", &model, "-f"])
                .arg(&path)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| format!("piper: {e}"))?;
            if let Some(mut si) = child.stdin.take() {
                let _ = si.write_all(text.as_bytes());
            }
            let ok = child.wait().map(|s| s.success()).unwrap_or(false);
            let bytes = std::fs::read(&path).unwrap_or_default();
            let _ = std::fs::remove_file(&path);
            if ok && !bytes.is_empty() {
                Ok(bytes)
            } else {
                Err("piper failed".into())
            }
        }
        _ => {
            let o = Command::new(engine)
                .args(["--stdout", "-s", rate, "--", text])
                .output()
                .map_err(|e| format!("{engine}: {e}"))?;
            if o.status.success() && !o.stdout.is_empty() {
                Ok(o.stdout)
            } else {
                Err(format!("{engine} failed"))
            }
        }
    }
}

fn chunks(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for sentence in text.split_inclusive(['.', '!', '?', '\n']) {
        if !cur.is_empty() && cur.len() + sentence.len() > max {
            out.push(std::mem::take(&mut cur));
        }
        let mut rest = sentence;
        while rest.len() > max {
            let cut = (0..=max)
                .rev()
                .find(|i| rest.is_char_boundary(*i))
                .unwrap_or(max);
            out.push(rest[..cut].to_string());
            rest = &rest[cut..];
        }
        cur.push_str(rest);
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out.retain(|c| !c.trim().is_empty());
    out
}

pub fn say(text: &str, engine: Option<&str>, rate: Option<u64>) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("Nothing to say".into());
    }
    let rate = rate.unwrap_or(180).to_string();
    let (provider, engine) = match engine {
        Some(e @ ("deepgram" | "fish")) => (e.to_string(), None),
        Some(e) => ("local".to_string(), Some(e.to_string())),
        None => (tts_active(), None),
    };
    let flag = Flag;
    flag.touch();
    if provider == "local" {
        let e = engine
            .or_else(local_engine)
            .ok_or("No voice \u{b7} install piper or espeak-ng")?;
        match e.as_str() {
            "say" | "spd-say" => {
                let mut cmd = Command::new(&e);
                if e == "say" {
                    cmd.args(["-r", &rate]);
                } else {
                    cmd.arg("-w");
                }
                cmd.arg(text)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                let child = crate::bound(&mut cmd).map_err(|er| format!("{e}: {er}"))?;
                run_with_meter(child, &[], &flag)?;
            }
            "piper" | "espeak-ng" | "espeak" => {
                for c in chunks(text, 600) {
                    play_wav(&local_wav(&e, &c, &rate)?, &flag)?;
                }
            }
            other => return Err(format!("Unknown voice {other}")),
        }
        return Ok(e);
    }
    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Vec<u8>, String>>(1);
    let (prov, parts) = (provider.clone(), chunks(text, 1800));
    std::thread::spawn(move || {
        for part in parts {
            let r = crate::cloud::synth(&prov, &part);
            let failed = r.is_err();
            if tx.send(r).is_err() || failed {
                break;
            }
        }
    });
    for wav in rx {
        flag.touch();
        play_wav(&wav?, &flag)?;
    }
    Ok(provider)
}

pub fn speak_answer() -> Result<(), String> {
    if !read_answers() {
        return Ok(());
    }
    let text = std::env::var("GRAY_TURN_TEXT").unwrap_or_default();
    let spoken = prose(&text);
    if spoken.is_empty() {
        return Ok(());
    }
    say(&spoken, None, None).map(|_| ())
}

fn link_end(cs: &[char], open: usize) -> Option<(usize, usize)> {
    let close = (open + 1..cs.len()).find(|&i| cs[i] == ']')?;
    if cs.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = (close + 2..cs.len()).find(|&i| cs[i] == ')')?;
    Some((close, end))
}

fn inline(s: &str) -> String {
    let cs: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c == '!' && cs.get(i + 1) == Some(&'[') {
            if let Some((_, end)) = link_end(&cs, i + 1) {
                i = end + 1;
                continue;
            }
        }
        if c == '[' {
            if let Some((close, end)) = link_end(&cs, i) {
                out.extend(&cs[i + 1..close]);
                i = end + 1;
                continue;
            }
        }
        if matches!(c, '`' | '*' | '~') {
            i += 1;
            continue;
        }
        if c == '_' && cs.get(i + 1) == Some(&'_') {
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out.split_whitespace()
        .filter(|w| !w.starts_with("http://") && !w.starts_with("https://"))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn prose(text: &str) -> String {
    let mut paras: Vec<String> = vec![String::new()];
    let mut fenced = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("```") || t.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        let table = t.starts_with('|') || (t.matches('|').count() >= 2 && t.contains("--"));
        let rule = t.len() >= 3 && t.chars().all(|c| matches!(c, '-' | '*' | '_' | '=' | ' '));
        if fenced || table || rule {
            continue;
        }
        if t.is_empty() {
            if !paras.last().is_some_and(String::is_empty) {
                paras.push(String::new());
            }
            continue;
        }
        let t = t
            .trim_start_matches('#')
            .trim_start_matches('>')
            .trim_start();
        let t = t
            .strip_prefix("- ")
            .or_else(|| t.strip_prefix("* "))
            .or_else(|| t.strip_prefix("+ "))
            .unwrap_or(t);
        let digits = t.chars().take_while(char::is_ascii_digit).count();
        let t = if digits > 0 && t[digits..].starts_with(". ") {
            &t[digits + 2..]
        } else {
            t
        };
        let mut s = inline(t);
        if s.is_empty() {
            continue;
        }
        if !s.ends_with(['.', '!', '?', ':', ';', ',']) {
            s.push('.');
        }
        let p = paras.last_mut().expect("paras is never empty");
        if !p.is_empty() {
            p.push(' ');
        }
        p.push_str(&s);
    }
    paras
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn local_label() -> String {
    local_engine().unwrap_or_else(|| "piper".into())
}
