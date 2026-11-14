<div align="center">
  <img alt="gray-voice" src="assets/icon.svg" width="120" height="120" />
  <h1>gray-voice</h1>
  <p><strong>Local TTS and STT — the machine speaks and listens without a cloud.</strong></p>
  <p>
    <a href="https://gray.alignment.id">Website</a> ·
    <a href="https://github.com/vstaln/gray-voice">Source</a> ·
    <a href="https://github.com/vstaln/gray">gray</a>
  </p>
  <p>
    <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-1c1c20?style=flat-square&labelColor=0a0a0b" /></a>
    <a href="https://www.rust-lang.org"><img alt="Built with Rust" src="https://img.shields.io/badge/built%20with-rust-1c1c20?style=flat-square&labelColor=0a0a0b&logo=rust&logoColor=d4a373" /></a>
    <img alt="gray plugin" src="https://img.shields.io/badge/gray-plugin-1c1c20?style=flat-square&labelColor=0a0a0b&color=7aa2f7" />
  </p>
</div>

<br/>
```bash
gray plugin install voice
```

`gray-voice` wires local speech tools into the agent. Everything is
binary-detected at call time: absent pieces fail with install hints, not
crashes.

## Tools

| Tool | What it does |
|---|---|
| `voice_say` | Speak text through the first available of `say` (macOS), `spd-say`, `espeak-ng`, `espeak`, `piper`. |
| `voice_listen` | Transcribe a mic recording via `whisper.cpp` (`whisper-cli`) over `arecord`/`sox` capture. |

## The `/voice` page

Bare `/voice` opens the page: listen backend, speak voice, talk key, and
whether answers are read back as they land.

| Command | What it does |
|---|---|
| `/voice` | open the page |
| `/voice on` · `/voice off` | ambient listening |
| `/voice say TEXT` | speak text |
| `/voice test` | speak a line, then transcribe it back |

Dictation sits on your talk key (`ctrl+space` unless `keybindings.json` says
otherwise for `plugin.voice.dictate`) and types the transcript into the
composer. Holding the key talks only while held. Read-answers runs on the
turn-end action, so the answer is spoken once the turn finishes, not
sentence by sentence.

## Cloud providers

Listen: `whisper.cpp` locally, or Deepgram / Groq. Speak: `say` /
`spd-say` / `espeak-ng` / `espeak` / `piper` locally, or Deepgram Aura /
Fish Audio. Keys go in the page, or come from `DEEPGRAM_API_KEY`,
`GROQ_API_KEY`, `FISH_API_KEY`; they are stored in
`~/.gray/voice/config.json`. Keys from an older `gray-voice` / `voicepro`
install are migrated on first run.

## Requirements (per capability)

| Capability | Needs |
|---|---|
| TTS | any of `say` / `spd-say` / `espeak-ng` / `espeak` / `piper` |
| STT | `whisper-cli` + `arecord` or `sox` + a whisper model |
| Cloud STT | a Deepgram or Groq key |
| Cloud TTS | a Deepgram or Fish Audio key |

## Build from source

```sh
cargo build --release
gray plugin check ./target/release/voice
```

## License

MIT
