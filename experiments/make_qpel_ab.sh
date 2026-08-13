#!/usr/bin/env bash
# Side-by-side visual A/B of tag 6 (half-pel) vs tag 7 (quarter-pel) on a
# drone pan section, so the perceptibility of the 6-tap interpolation can be
# judged with the eye rather than the PSNR delta alone (which measures the
# two within +0.08 dB on the drone at QF=70).
#
# Usage:
#   experiments/make_qpel_ab.sh
#
# Writes samples/evidence/drone_qpel_ab.gif (plus the source MP4).
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release --bin asciline-compile --quiet
cargo build --release --bin asciline-render --quiet
BIN=target/release/asciline-compile
REND=target/release/asciline-render
SRC=samples/source/drone_excerpt_720p60.mp4
COLS=${COLS:-240}
FPS=60
# 2-second panning window (0-based start frame, frame count at 60 fps).
START=${START:-180}
N=${N:-120}
OUT=samples/evidence

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# Compile the identical source as tag 6 (half-pel) and tag 7 (quarter-pel,
# the default), then render both to PPM sequences.
"$BIN" "$SRC" --cols "$COLS" --profile --fps "$FPS" --no-quality --no-qpel --out "$TMP/t6" >/dev/null
"$BIN" "$SRC" --cols "$COLS" --profile --fps "$FPS" --no-quality --out "$TMP/t7" >/dev/null
"$REND" "$TMP/t6.ascf" --out "$TMP/r6" >/dev/null
"$REND" "$TMP/t7.ascf" --out "$TMP/r7" >/dev/null

ffmpeg -y -v error \
    -framerate "$FPS" -start_number "$START" -i "$TMP/r6/frame_%06d.ppm" \
    -framerate "$FPS" -start_number "$START" -i "$TMP/r7/frame_%06d.ppm" \
    -filter_complex "
        [0:v]pad=iw:ih+18:0:18:black,drawtext=fontsize=16:text='tag 6  half-pel':x=8:y=4[a];
        [1:v]pad=iw:ih+18:0:18:black,drawtext=fontsize=16:text='tag 7  quarter-pel (default)':x=8:y=4[b];
        [a][b]hstack,drawtext=fontsize=16:text='drone 60 fps  |  QF=70  |  side-by-side':x=8:h-24[v]" \
    -map '[v]' -frames:v "$N" -pix_fmt yuv420p "$TMP/ab.mp4"

ffmpeg -y -v error -i "$TMP/ab.mp4" \
    -vf 'fps=10,scale=480:-1:flags=lanczos,palettegen=stats_mode=diff' "$TMP/pal.png"
ffmpeg -y -v error -i "$TMP/ab.mp4" -i "$TMP/pal.png" \
    -lavfi 'fps=10,scale=480:-1:flags=lanczos[x];[x][1:v]paletteuse=dither=sierra2_4a' -loop 0 \
    "$OUT/drone_qpel_ab.gif"
cp "$TMP/ab.mp4" "$OUT/drone_qpel_ab.mp4"
echo "wrote $OUT/drone_qpel_ab.gif ($(stat -c %s "$OUT/drone_qpel_ab.gif") bytes) + .mp4"
