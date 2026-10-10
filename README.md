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

## Requirements (per capability)

| Capability | Needs |
|---|---|
| TTS | any of `say` / `spd-say` / `espeak-ng` / `espeak` / `piper` |
| STT | `whisper-cli` + `arecord` or `sox` + a whisper model |

## Build from source

```sh
cargo build --release
gray plugin check ./target/release/voice
```

## License

MIT
