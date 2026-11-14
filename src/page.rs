use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::{cursor, execute, terminal};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::Write;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{speech, TestEvent, MODEL_MISSING};

type Rgb = (u8, u8, u8);
type Line = (Vec<(String, Rgb)>, Vec<(String, Rgb)>, bool);

struct Theme {
    surface: Rgb,
    raised: Rgb,
    dim: Rgb,
    muted: Rgb,
    body: Rgb,
    bright: Rgb,
    accent: Rgb,
    success: Rgb,
    error: Rgb,
}

fn theme() -> Theme {
    let mut t = Theme {
        surface: (24, 24, 27),
        raised: (45, 45, 50),
        dim: (110, 110, 110),
        muted: (140, 140, 140),
        body: (225, 225, 225),
        bright: (240, 240, 240),
        accent: (246, 173, 126),
        success: (74, 222, 128),
        error: (239, 68, 68),
    };
    let env = std::env::var("GRAY_MODAL_THEME").unwrap_or_default();
    for pair in env.split(',') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        let v = v.trim_start_matches('#');
        if v.len() != 6 {
            continue;
        }
        let p = |i: usize| u8::from_str_radix(&v[i..i + 2], 16).ok();
        let (Some(r), Some(g), Some(b)) = (p(0), p(2), p(4)) else {
            continue;
        };
        let c = (r, g, b);
        match k {
            "surface_bg" => t.surface = c,
            "raised_bg" => t.raised = c,
            "text_dim" => t.dim = c,
            "text_muted" => t.muted = c,
            "text_body" => t.body = c,
            "text_bright" => t.bright = c,
            "accent" => t.accent = c,
            "success" => t.success = c,
            "error" => t.error = c,
            _ => {}
        }
    }
    t
}

#[derive(Clone, Copy, PartialEq)]
enum Row {
    Stt(usize),
    Read,
    Tts(usize),
    Key,
    Hold,
    Ambient,
    Test,
}

const STT: [(&str, &str); 3] = [
    ("local", "whisper.cpp"),
    ("deepgram", "Deepgram"),
    ("groq", "Groq"),
];
const TTS: [(&str, &str); 3] = [
    ("local", ""),
    ("deepgram", "Deepgram Aura"),
    ("fish", "Fish Audio"),
];

fn rows() -> Vec<Row> {
    let mut r: Vec<Row> = (0..3).map(Row::Stt).collect();
    r.push(Row::Read);
    r.extend((0..3).map(Row::Tts));
    r.extend([Row::Key, Row::Hold, Row::Ambient, Row::Test]);
    r
}

fn key_id(row: Row) -> Option<&'static str> {
    match row {
        Row::Stt(i) if i > 0 => Some(STT[i].0),
        Row::Tts(i) if i > 0 => Some(TTS[i].0),
        _ => None,
    }
}

enum Check {
    Edit,
    Busy,
    Ok(Instant),
    Bad(String),
}

struct Field {
    row: Row,
    id: &'static str,
    text: String,
    had: bool,
    check: Check,
}

#[derive(Clone)]
enum TestState {
    Idle,
    Rec(VecDeque<usize>, u64),
    Transcribing,
    Download(u8),
    Done(Result<Option<String>, String>),
}

struct Page {
    sel: usize,
    cfg: Value,
    field: Option<Field>,
    capturing: bool,
    note: Option<String>,
    test: Arc<Mutex<TestState>>,
    check_rx: Option<mpsc::Receiver<Result<(), String>>>,
    local_label: String,
    local_engine: bool,
    local_stt: Option<String>,
}

impl Page {
    fn new() -> Self {
        let mut p = Page {
            sel: 0,
            cfg: Value::Null,
            field: None,
            capturing: false,
            note: None,
            test: Arc::new(Mutex::new(TestState::Idle)),
            check_rx: None,
            local_label: String::new(),
            local_engine: false,
            local_stt: None,
        };
        p.reload();
        p.local_label = speech::local_label();
        p.local_engine = speech::local_engine().is_some();
        p.local_stt = crate::local_stt_problem();
        p
    }

    fn reload(&mut self) {
        self.cfg = crate::read_config();
    }

    fn set(&mut self, f: impl FnOnce(&mut Value)) {
        if let Err(e) = crate::update_config(f) {
            self.note = Some(format!("\u{2717} {e}"));
        }
        self.reload();
    }

    fn flag(&self, k: &str) -> bool {
        self.cfg[k].as_bool().unwrap_or(false)
    }

    fn active(&self, k: &str) -> String {
        self.cfg[k].as_str().unwrap_or("local").to_string()
    }

    fn has_key(&self, id: &str) -> bool {
        crate::provider_key(id).is_some()
    }

    fn activate(&mut self, row: Row) {
        match row {
            Row::Stt(i) => self.set(|c| c["stt"] = json!(STT[i].0)),
            Row::Tts(i) => self.set(|c| c["tts"] = json!(TTS[i].0)),
            _ => {}
        }
    }

    fn open_field(&mut self, row: Row) {
        let Some(id) = key_id(row) else { return };
        self.note = None;
        self.field = Some(Field {
            row,
            id,
            text: String::new(),
            had: self.has_key(id),
            check: Check::Edit,
        });
    }

    fn press(&mut self, row: Row) {
        self.note = None;
        match row {
            Row::Stt(0) | Row::Tts(0) => self.activate(row),
            Row::Stt(_) | Row::Tts(_) => match key_id(row) {
                Some(id) if self.has_key(id) => self.activate(row),
                _ => self.open_field(row),
            },
            Row::Read => {
                let v = !self.flag("read_answers");
                self.set(|c| c["read_answers"] = json!(v));
            }
            Row::Hold => {
                let v = if self.flag_hold() { "press" } else { "hold" };
                self.set(|c| c["dictate_mode"] = json!(v));
            }
            Row::Ambient => {
                if self.flag("listen") {
                    self.set(|c| c["listen"] = json!(false));
                } else if let Some(p) = crate::ambient_problem() {
                    self.note = Some(format!("\u{2717} {p}"));
                } else {
                    self.set(|c| c["listen"] = json!(true));
                }
            }
            Row::Key => self.capturing = true,
            Row::Test => self.start_test(),
        }
    }

    fn flag_hold(&self) -> bool {
        self.cfg["dictate_mode"] == "hold"
    }

    fn start_test(&mut self) {
        let download = {
            let st = self.test.lock().expect("test lock");
            match &*st {
                TestState::Rec(..) | TestState::Transcribing | TestState::Download(_) => return,
                TestState::Done(Err(e)) => e == MODEL_MISSING,
                _ => false,
            }
        };
        let st = Arc::clone(&self.test);
        *st.lock().expect("test lock") = TestState::Rec(VecDeque::new(), 0);
        std::thread::spawn(move || {
            let set = |s: TestState| *st.lock().expect("test lock") = s;
            if download {
                set(TestState::Download(0));
                if let Err(e) = crate::download_model(|pct| set(TestState::Download(pct))) {
                    set(TestState::Done(Err(e)));
                    return;
                }
            }
            let r = crate::test_mic(|ev| match ev {
                TestEvent::Level(l, ms) => {
                    let mut g = st.lock().expect("test lock");
                    let mut lv = match &*g {
                        TestState::Rec(lv, _) => lv.clone(),
                        _ => VecDeque::new(),
                    };
                    lv.push_back(l);
                    if lv.len() > 7 {
                        lv.pop_front();
                    }
                    *g = TestState::Rec(lv, ms);
                }
                TestEvent::Transcribing => *st.lock().expect("test lock") = TestState::Transcribing,
            });
            set(TestState::Done(r));
        });
    }

    fn submit_key(&mut self) {
        let Some(f) = &mut self.field else { return };
        if f.text.is_empty() {
            if f.had {
                let id = f.id;
                self.field = None;
                self.set(|c| {
                    if let Some(k) = c["keys"].as_object_mut() {
                        k.remove(id);
                    }
                });
                self.note = Some("Key removed".into());
            } else {
                self.field = None;
            }
            return;
        }
        let (id, key) = (f.id, f.text.clone());
        f.check = Check::Busy;
        let (tx, rx) = mpsc::channel();
        self.check_rx = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(crate::cloud::validate_key(id, &key));
        });
    }

    fn poll_check(&mut self) {
        let Some(rx) = &self.check_rx else { return };
        let Ok(r) = rx.try_recv() else { return };
        self.check_rx = None;
        let Some(f) = &mut self.field else { return };
        match r {
            Ok(()) => {
                let (id, key, row) = (f.id, f.text.clone(), f.row);
                f.check = Check::Ok(Instant::now());
                self.set(|c| {
                    if !c["keys"].is_object() {
                        c["keys"] = json!({});
                    }
                    c["keys"][id] = json!(key);
                });
                self.activate(row);
            }
            Err(e) => f.check = Check::Bad(e),
        }
    }

    fn capture_key(&mut self, k: KeyEvent) {
        self.capturing = false;
        if k.code == KeyCode::Esc {
            return;
        }
        let mods = k.modifiers;
        let name = match k.code {
            KeyCode::F(n) => format!("f{n}"),
            KeyCode::Char(' ') => "space".into(),
            KeyCode::Char(c) => c.to_ascii_lowercase().to_string(),
            KeyCode::Enter => "enter".into(),
            KeyCode::Tab => "tab".into(),
            _ => {
                self.note = Some("\u{2717} Try ctrl+space or an F key".into());
                return;
            }
        };
        let mut parts = Vec::new();
        for (m, n) in [
            (KeyModifiers::CONTROL, "ctrl"),
            (KeyModifiers::ALT, "alt"),
            (KeyModifiers::SHIFT, "shift"),
        ] {
            if mods.contains(m) {
                parts.push(n.to_string());
            }
        }
        if !(mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            || matches!(k.code, KeyCode::F(_)))
        {
            self.note = Some("\u{2717} Try ctrl+space or an F key".into());
            return;
        }
        parts.push(name);
        let key = parts.join("+");
        self.note = Some(match crate::set_hotkey(&key) {
            Ok(()) => "Restart gray to apply".into(),
            Err(e) => format!("\u{2717} {e}"),
        });
    }

    fn field_key(&mut self, k: KeyEvent) {
        let Some(f) = &mut self.field else { return };
        if matches!(f.check, Check::Busy) {
            return;
        }
        match k.code {
            KeyCode::Esc => {
                self.field = None;
                self.check_rx = None;
            }
            KeyCode::Enter => self.submit_key(),
            KeyCode::Backspace => {
                f.text.pop();
                f.check = Check::Edit;
            }
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                f.text.clear();
                f.check = Check::Edit;
            }
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                f.text.push(c);
                f.check = Check::Edit;
            }
            _ => {}
        }
    }

    fn render(&self, t: &Theme, width: usize) -> Vec<String> {
        let inner = width - 2;
        let all = rows();
        let sel = all[self.sel];
        let test = self.test.lock().expect("test lock").clone();
        let mut out: Vec<Line> = Vec::new();
        let head = |s: &str| (vec![(format!("  {s}"), t.muted)], vec![], false);
        let blank = || (vec![], vec![], false);
        let active_stt = self.active("stt");
        let active_tts = self.active("tts");
        for r in all.iter().copied() {
            if r == Row::Read {
                out.push(blank());
                out.push(head("Speak"));
            }
            if r == Row::Stt(0) {
                out.push(head("Listen"));
            }
            if r == Row::Key {
                out.push(blank());
            }
            let on = r == sel;
            let (fg, left_glyph, label, right): (Rgb, Option<bool>, String, Vec<(String, Rgb)>) =
                match r {
                    Row::Stt(i) => {
                        let id = STT[i].0;
                        let right = if i == 0 {
                            self.local_stt
                                .as_deref()
                                .map(|p| vec![(short_problem(p), t.dim)])
                                .unwrap_or_default()
                        } else if self.has_key(id) {
                            vec![]
                        } else {
                            vec![("no key".to_string(), t.dim)]
                        };
                        (t.body, Some(active_stt == id), STT[i].1.to_string(), right)
                    }
                    Row::Tts(i) => {
                        let id = TTS[i].0;
                        let label = if i == 0 {
                            self.local_label.clone()
                        } else {
                            TTS[i].1.to_string()
                        };
                        let right = if i == 0 {
                            if self.local_engine {
                                vec![]
                            } else {
                                vec![("not installed".to_string(), t.dim)]
                            }
                        } else if self.has_key(id) {
                            vec![]
                        } else {
                            vec![("no key".to_string(), t.dim)]
                        };
                        (t.body, Some(active_tts == id), label, right)
                    }
                    Row::Read => (
                        t.body,
                        None,
                        "Read answers".into(),
                        onoff(self.flag("read_answers"), t),
                    ),
                    Row::Key => (
                        t.body,
                        None,
                        "Talk key".into(),
                        if self.capturing {
                            vec![("press a key".to_string(), t.accent)]
                        } else {
                            vec![(crate::hotkey(), t.muted)]
                        },
                    ),
                    Row::Hold => (
                        t.body,
                        None,
                        "Hold to talk".into(),
                        onoff(self.flag_hold(), t),
                    ),
                    Row::Ambient => (
                        t.body,
                        None,
                        "Ambient".into(),
                        onoff(self.flag("listen"), t),
                    ),
                    Row::Test => (t.body, None, "Test mic".into(), test_right(&test, t)),
                };
            let mut left = vec![(if on { "\u{203a} " } else { "  " }.to_string(), t.accent)];
            match left_glyph {
                Some(true) => left.push(("\u{25cf} ".into(), t.accent)),
                Some(false) => left.push(("\u{25cb} ".into(), t.dim)),
                None => left.push(("  ".into(), t.body)),
            }
            left.push((label, if on { t.bright } else { fg }));
            out.push((left, right, on));
            if let Some(f) = self.field.as_ref().filter(|f| f.row == r) {
                let dots = "\u{2022}".repeat(f.text.chars().count().min(24));
                let (state, color) = match &f.check {
                    Check::Edit => (String::new(), t.dim),
                    Check::Busy => ("checking\u{2026}".into(), t.dim),
                    Check::Ok(_) => ("\u{2713}".into(), t.success),
                    Check::Bad(e) => (format!("\u{2717} {e}"), t.error),
                };
                out.push((
                    vec![
                        ("      key ".into(), t.dim),
                        (format!("{dots}\u{258f}"), t.bright),
                    ],
                    vec![(state, color)],
                    false,
                ));
            }
            if r == Row::Test {
                if let TestState::Done(Ok(Some(text))) = &test {
                    let shown: String = text.chars().take(inner.saturating_sub(8)).collect();
                    out.push((
                        vec![(format!("      \u{201c}{shown}\u{201d}"), t.muted)],
                        vec![],
                        false,
                    ));
                }
            }
        }
        while out.len() < 17 {
            out.push(blank());
        }
        out.push(blank());
        let hint = if let Some(n) = &self.note {
            let c = if n.starts_with('\u{2717}') {
                t.error
            } else {
                t.muted
            };
            (n.clone(), c)
        } else if let Some(f) = &self.field {
            let h = if f.had {
                "enter save \u{b7} empty removes \u{b7} esc"
            } else {
                "enter save \u{b7} esc"
            };
            (h.to_string(), t.dim)
        } else if self.capturing {
            ("press the new key \u{b7} esc".to_string(), t.dim)
        } else if key_id(sel).is_some() {
            (
                "\u{2191}\u{2193} \u{b7} enter \u{b7} k key \u{b7} esc".to_string(),
                t.dim,
            )
        } else {
            (
                "\u{2191}\u{2193} \u{b7} enter \u{b7} esc".to_string(),
                t.dim,
            )
        };
        out.push((vec![(format!("  {}", hint.0), hint.1)], vec![], false));

        let title = " Voice ";
        let top = format!(
            "\u{256d}\u{2500}{title}{}\u{256e}",
            "\u{2500}".repeat(inner.saturating_sub(title.len() + 1))
        );
        let mut lines = vec![paint(&top, t.dim, t.surface)];
        for (left, right, on) in &out {
            let bg = if *on { t.raised } else { t.surface };
            let body = compose(left, right, inner, bg);
            lines.push(format!(
                "{}\u{2502}{}{}\u{2502}\x1b[0m",
                fgbg(t.dim, t.surface),
                body,
                fgbg(t.dim, t.surface)
            ));
        }
        let bottom = format!("\u{2570}{}\u{256f}", "\u{2500}".repeat(inner));
        lines.push(paint(&bottom, t.dim, t.surface));
        lines
    }
}

fn short_problem(p: &str) -> String {
    p.split(" \u{b7} ")
        .next()
        .unwrap_or(p)
        .replace("No ", "no ")
}

fn onoff(v: bool, t: &Theme) -> Vec<(String, Rgb)> {
    vec![(
        if v { "on" } else { "off" }.to_string(),
        if v { t.success } else { t.dim },
    )]
}

fn bars(levels: &VecDeque<usize>) -> String {
    let b = [
        '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}',
        '\u{2588}',
    ];
    let mut s: String = levels.iter().map(|l| b[(*l).min(7)]).collect();
    while s.chars().count() < 7 {
        s.push(b[0]);
    }
    s
}

fn test_right(st: &TestState, t: &Theme) -> Vec<(String, Rgb)> {
    match st {
        TestState::Idle => vec![],
        TestState::Rec(lv, ms) => vec![(
            format!("\u{25cf} {} {}s", bars(lv), 3u64.saturating_sub(ms / 1000)),
            t.accent,
        )],
        TestState::Transcribing => vec![("transcribing\u{2026}".into(), t.dim)],
        TestState::Download(p) => vec![(format!("downloading {p}%"), t.dim)],
        TestState::Done(Ok(_)) => vec![("\u{2713} Ready".into(), t.success)],
        TestState::Done(Err(e)) => vec![(format!("\u{2717} {e}"), t.error)],
    }
}

fn fgbg(fg: Rgb, bg: Rgb) -> String {
    format!(
        "\x1b[38;2;{};{};{}m\x1b[48;2;{};{};{}m",
        fg.0, fg.1, fg.2, bg.0, bg.1, bg.2
    )
}

fn paint(s: &str, fg: Rgb, bg: Rgb) -> String {
    format!("{}{s}\x1b[0m", fgbg(fg, bg))
}

fn compose(left: &[(String, Rgb)], right: &[(String, Rgb)], w: usize, bg: Rgb) -> String {
    let rlen: usize = right.iter().map(|(s, _)| s.chars().count()).sum();
    let right_pad = if rlen > 0 { rlen + 2 } else { 0 };
    let mut s = String::new();
    let mut used = 0;
    for (text, c) in left {
        let room = w.saturating_sub(right_pad).saturating_sub(used);
        let cut: String = text.chars().take(room).collect();
        used += cut.chars().count();
        s.push_str(&paint(&cut, *c, bg));
    }
    let gap = w.saturating_sub(used + rlen + if rlen > 0 { 2 } else { 0 });
    s.push_str(&paint(&" ".repeat(gap), bg, bg));
    for (text, c) in right {
        let cut: String = text.chars().take(w.saturating_sub(used)).collect();
        s.push_str(&paint(&cut, *c, bg));
    }
    if rlen > 0 {
        s.push_str(&paint("  ", bg, bg));
    }
    s
}

pub fn run() -> Result<(), String> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        return Err("The page needs a terminal".into());
    }
    let hosted = std::env::var_os("GRAY_MODAL_THEME").is_some();
    terminal::enable_raw_mode().map_err(|e| e.to_string())?;
    let mut out = std::io::stdout();
    let _ = if hosted {
        execute!(out, cursor::Hide)
    } else {
        execute!(out, terminal::EnterAlternateScreen, cursor::Hide)
    };
    let r = event_loop(&mut out, hosted);
    let _ = if hosted {
        execute!(out, cursor::Show)
    } else {
        execute!(out, cursor::Show, terminal::LeaveAlternateScreen)
    };
    let _ = terminal::disable_raw_mode();
    r
}

fn event_loop(out: &mut std::io::Stdout, hosted: bool) -> Result<(), String> {
    let t = theme();
    let mut page = Page::new();
    let mut last = String::new();
    let count = rows().len();
    loop {
        page.poll_check();
        if let Some(Field {
            check: Check::Ok(at),
            ..
        }) = &page.field
        {
            if at.elapsed() > Duration::from_millis(700) {
                page.field = None;
            }
        }
        let (cols, rows_h) = terminal::size().map_err(|e| e.to_string())?;
        let width = (cols as usize).saturating_sub(2).clamp(20, 50);
        let lines = page.render(&t, width);
        let x = (cols as usize).saturating_sub(width) / 2 + 1;
        let y = (rows_h as usize).saturating_sub(lines.len()) / 2 + 1;
        let mut frame = String::new();
        if !hosted && last.is_empty() {
            frame.push_str("\x1b[2J");
        }
        for (i, l) in lines.iter().enumerate() {
            frame.push_str(&format!("\x1b[{};{}H{l}", y + i, x));
        }
        if frame != last {
            let _ = out.write_all(frame.as_bytes());
            let _ = out.flush();
            last = frame;
        }
        if !event::poll(Duration::from_millis(80)).map_err(|e| e.to_string())? {
            continue;
        }
        let Event::Key(k) = event::read().map_err(|e| e.to_string())? else {
            continue;
        };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(());
        }
        if page.capturing {
            page.capture_key(k);
            continue;
        }
        if page.field.is_some() {
            page.field_key(k);
            continue;
        }
        let row = rows()[page.sel];
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
            KeyCode::Up => page.sel = (page.sel + count - 1) % count,
            KeyCode::Down | KeyCode::Tab => page.sel = (page.sel + 1) % count,
            KeyCode::Enter | KeyCode::Char(' ') => page.press(row),
            KeyCode::Char('k') => page.open_field(row),
            _ => {}
        }
    }
}
