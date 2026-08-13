#!/usr/bin/env bash
# Compare .ascf output size + encode wall-time between flate2's two pure-Rust
# zlib backends: miniz_oxide (the crate default) and zlib-rs. Both emit valid
# zlib streams; this measures how much the choice costs in bytes and seconds
# on real clips. Measured 2026-08-12 (8 s 640x360 clip, 240 cols):
#   adaptive pixel tol=8: miniz 2,875,260 B vs zlib-rs 3,536,211 B (~23% smaller)
#   profile QF=70:         miniz   494,506 B vs zlib-rs   493,160 B (a wash)
#
# Usage:  experiments/compare_zlib_backends.sh <video> [compiler flags…]
# Example: experiments/compare_zlib_backends.sh clip.mp4 --profile --qf 70
#
# The script temporarily flips the flate2 feature in Cargo.toml and restores
# it on exit (trap), so the committed tree is never left modified.
set -euo pipefail
cd "$(dirname "$0")/.."

VIDEO="${1:?usage: $0 <video> [compiler flags…]}"; shift || true
# Capture the remaining script args BEFORE any function call: inside a bash
# function, "$@" refers to the *function's* arguments, not the script's.
ARGS=("$@")
COLS="${COLS:-240}"

BACKUP=$(mktemp)
cp Cargo.toml "$BACKUP"
restore() { cp "$BACKUP" Cargo.toml; rm -f "$BACKUP"; }
trap restore EXIT

set_backend() {
    # $1 = flate2 feature name (zlib-rs | rust_backend)
    python3 - "$1" <<'PYEOF'
import re, sys
feat = sys.argv[1]
src = open("Cargo.toml").read()
pat = r'flate2 = \{ version = "1", default-features = false, features = \[[^\]]*\] \}'
src = re.sub(pat, f'flate2 = {{ version = "1", default-features = false, features = ["{feat}"] }}', src)
open("Cargo.toml", "w").write(src)
PYEOF
}

run_backend() {
    local name="$1" feature="$2"
    set_backend "$feature"
    echo "== $name =="
    if cargo build --release --quiet 2>&1 | grep -E 'error|warning' | head -3; then :; fi
    local out="/tmp/asciline_backend_${name}"
    local start end size dt
    start=$(date +%s.%N)
    ./target/release/asciline-compile "$VIDEO" --cols "$COLS" "${ARGS[@]}" --no-quality \
        --out "$out" > /dev/null 2>&1
    end=$(date +%s.%N)
    size=$(stat -c%s "$out.ascf")
    dt=$(awk "BEGIN { printf \"%.2f\", $end - $start }")
    human=$(numfmt --to=iec "$size" 2>/dev/null || echo "${size} B")
    echo "  .ascf size  : ${human}  ($size bytes)"
    echo "  encode time : ${dt}s"
    rm -f "$out.ascf" "$out.mp3"
}

# miniz_oxide is the crate default; zlib-rs is the comparison.
run_backend "miniz_oxide" "rust_backend"
run_backend "zlib-rs"     "zlib-rs"

echo
echo "Restored Cargo.toml (flate2 backend = zlib-rs again)."
