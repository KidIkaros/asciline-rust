#!/usr/bin/env sh
#
# asciline-rust install script
#
#   Builds the four release binaries (asciline-server, asciline-player,
#   asciline-compile, asciline-render) with cargo and installs them to
#   ~/.local/bin (override with PREFIX=/usr PREFIX=~/.cargo ...).
#
# Requirements: cargo (Rust 1.87+) and ffmpeg + ffprobe on PATH.
#
set -e

PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"

echo "==> asciline-rust installer"

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo not found — install Rust first (https://rustup.rs)" >&2
    exit 1
fi
if ! command -v ffmpeg >/dev/null 2>&1 || ! command -v ffprobe >/dev/null 2>&1; then
    echo "warning: ffmpeg/ffprobe not on PATH — the server and player need them at runtime" >&2
fi

echo "==> Building release binaries (this can take a few minutes)..."
cargo build --release

mkdir -p "$BIN_DIR"
for bin in asciline-server asciline-player asciline-compile asciline-render; do
    install -m 0755 "target/release/$bin" "$BIN_DIR/$bin"
    echo "    installed $BIN_DIR/$bin"
done

echo
echo "==> Done."
echo "    Add $BIN_DIR to your PATH if it isn't already:"
echo "        export PATH=\"$BIN_DIR:\$PATH\""
echo "    Then run: asciline-server video.mp4 --cols 240"
