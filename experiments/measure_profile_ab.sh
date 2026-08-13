#!/usr/bin/env bash
# A/B the profile encoder's motion-search knobs (--r-search / --rdo-lambda)
# against a pinned source, reporting output size + PSNR-Y/SSIM-Y + wall time
# so the rate-distortion trade-off is measurable.
#
# Usage:
#   experiments/measure_profile_ab.sh                      # drone, 240 cols, QF=70
#   SOURCE=samples/source/big_buck_bunny_excerpt_30fps.mp4 FPS=30 \
#     experiments/measure_profile_ab.sh                    # cartoon
#
# The quality report is printed by `asciline-compile` (PSNR-Y / SSIM-Y against
# the source frames), so every case is comparable at a glance.
set -euo pipefail
cd "$(dirname "$0")/.."

SOURCE=${SOURCE:-samples/source/drone_excerpt_720p60.mp4}
COLS=${COLS:-240}
QF=${QF:-70}
FPS=${FPS:-60}
BIN=target/release/asciline-compile
cargo build --release --bin asciline-compile --quiet

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

run_case() {
    local label="$1" r="$2" lam="$3"
    local out="$TMP/ab" t0 t1 size psnr ssim wall
    t0=$(date +%s.%N)
    "$BIN" "$SOURCE" --cols "$COLS" --profile --qf "$QF" --fps "$FPS" \
        --r-search "$r" --rdo-lambda "$lam" --out "$out" > "$TMP/report.txt" 2>&1
    t1=$(date +%s.%N)
    size=$(stat -c %s "$out.ascf")
    psnr=$(grep 'PSNR-Y' "$TMP/report.txt" | head -1 | awk '{print $3}')
    ssim=$(grep 'SSIM-Y' "$TMP/report.txt" | head -1 | awk '{print $3}')
    wall=$(python3 - "$t0" "$t1" <<'PY'
import sys
print(f"{float(sys.argv[2]) - float(sys.argv[1]):.2f}")
PY
)
    printf '%-16s %12s B  PSNR-Y %8s  SSIM-Y %7s  %ss\n' "$label" "$size" "$psnr" "$ssim" "$wall"
}

echo "# Profile motion-search A/B"
echo "# source: $SOURCE (cols=$COLS qf=$QF fps=$FPS)"
printf '%-16s %12s  %8s  %7s  %s\n' "config" "size" "PSNR-Y" "SSIM-Y" "wall"
run_case "base r3"         3   0
run_case "r7"              7   0
run_case "r15"             15  0
run_case "r7 l25"          7   25
run_case "r7 l100"         7   100
run_case "r7 l400"         7   400
run_case "r7 l1600"        7   1600
run_case "r15 l400"        15  400
