#!/usr/bin/env bash
# Compare compile speed, output size, and quality on the exact same pinned
# Big Buck Bunny source. This measures encoding throughput (frames processed
# per wall-second), not playback FPS; all output clips retain the source clip's
# own 30fps display rate.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release --bin asciline-compile
BIN=target/release/asciline-compile
SOURCE=samples/source/big_buck_bunny_excerpt_30fps.mp4
COLS=${COLS:-240}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
FRAMES=$(ffprobe -v error -count_frames -select_streams v:0 \
    -show_entries stream=nb_read_frames -of csv=p=0 "$SOURCE")
SOURCE_FPS=$(ffprobe -v error -select_streams v:0 -show_entries stream=r_frame_rate \
    -of csv=p=0 "$SOURCE" | awk -F/ '{printf "%.0f", $1 / $2}')
PIXEL_BYTES=$(
    "$BIN" "$SOURCE" --cols "$COLS" --pixel --out "$TMP/pixel" >/dev/null 2>&1
    stat -c %s "$TMP/pixel.ascf"
)
rm -f "$TMP/pixel.ascf" "$TMP/pixel.mp3"

printf '# Big Buck Bunny compile speed analysis\n\n' > samples/big_buck_bunny_speed_analysis.md
printf 'Same source: %s frames at %s fps, %s columns. Compile FPS is frames processed per wall-second; display FPS remains the source FPS.\n\n' \
    "$FRAMES" "$SOURCE_FPS" "$COLS" >> samples/big_buck_bunny_speed_analysis.md
printf '| Format | Display FPS | Frames | Wall time | Compile FPS | Output bytes | PSNR-Y | SSIM-Y |\n' \
    >> samples/big_buck_bunny_speed_analysis.md
printf '|---|---:|---:|---:|---:|---:|---:|---:|\n' \
    >> samples/big_buck_bunny_speed_analysis.md

measure() {
    local name=$1
    local label=$2
    shift 2
    local report="$TMP/${name}.report"
    local timefile="$TMP/${name}.time"
    /usr/bin/time -f '%e' -o "$timefile" \
        "$BIN" "$SOURCE" --cols "$COLS" --out "$TMP/$name" "$@" \
        > "$report" 2>&1
    local wall
    wall=$(cat "$timefile")
    local bytes
    bytes=$(stat -c %s "$TMP/$name.ascf")
    local compile_fps
    compile_fps=$(python3 - "$FRAMES" "$wall" <<'PY'
import sys
print(f"{int(sys.argv[1]) / float(sys.argv[2]):.1f}")
PY
)
    local psnr='—'
    local ssim='—'
    if grep -q 'PSNR-Y' "$report"; then
        psnr=$(grep 'PSNR-Y' "$report" | head -1 | awk '{print $3}')
        ssim=$(grep 'SSIM-Y' "$report" | head -1 | awk '{print $3}')
    fi
    printf '| %s | %s | %s | %s s | %s | %s | %s | %s |\n' \
        "$label" "$SOURCE_FPS" "$FRAMES" "$wall" "$compile_fps" \
        "$bytes" "$psnr" "$ssim" >> samples/big_buck_bunny_speed_analysis.md
    printf '%-28s display=%sf frames=%s wall=%ss compile=%sf output=%sB PSNR-Y=%s SSIM-Y=%s\n' \
        "$label" "$SOURCE_FPS" "$FRAMES" "$wall" "$compile_fps" \
        "$bytes" "$psnr" "$ssim"
}

measure ascii 'ASCII mode' --mode 4
measure pixel 'PIXEL lossless' --pixel
measure profile 'PROFILE QF=70' --profile --qf 70
measure profile_fast 'PROFILE QF=70 (no quality report)' --profile --qf 70 --no-quality

cat >> samples/big_buck_bunny_speed_analysis.md <<'EOF'

## Interpretation

- **Display FPS** is the source/container playback rate. It is 30 fps for all
  four outputs; the comparison video should not make one panel appear slower.
- **Compile FPS** is offline encoding throughput and is the relevant speed
  comparison for `.ascf` production.
- The no-quality profile row isolates the SSIM/quality-report cost from DCT
  encoding cost.
- GIFs are previews and may play at a browser-controlled rate. Use the MP4
  comparison and this table for timing claims.
EOF
