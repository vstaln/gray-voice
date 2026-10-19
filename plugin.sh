#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/voice.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/voice" ;;
    *) rel="x86_64-apple-darwin/voice" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/voice" ;;
    *) rel="x86_64-unknown-linux-musl/voice" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/voice.exe" "$here/target/release/voice" "$here/target/debug/voice.exe" "$here/target/debug/voice"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "voice binary not found in $here" >&2
exit 1
