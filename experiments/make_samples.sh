#!/usr/bin/env bash
# Regenerate the committed samples/ format evidence:
#   - comparison stills + animated GIF (rendered by OUR decoders via
#     asciline-render, from compiled .ascf)
#   - playable .ascf files in all three formats + the profile quality report
#   - REAL-VIDEO evidence (mp4, not the GIF): a 240-column side-by-side
#     SOURCE|PIXEL|PROFILE comparison, and a true 120 fps capture of the
#     live server's actual WebSocket frames (asciline-render --live).
#
# Run from the repo root:  experiments/make_samples.sh
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release --bin asciline-compile --bin asciline-render --bin asciline-server

BIN=target/release/asciline-compile
RENDER=target/release/asciline-render
SERVER=target/release/asciline-server
FONT=/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf
LBL="fontfile=$FONT:fontsize=20:fontcolor=white:box=1:boxcolor=black@0.55:boxborderw=8"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

mkdir -p samples/images samples/evidence

# Deterministic 6 s mandelbrot zoom source, lossless FFV1 (same reasoning as
# the fuzz corpus: no x264 rate-control drift between ffmpeg builds).
ffmpeg -y -v error -f lavfi -i 'mandelbrot=size=640x360:rate=30' -t 6 \
    -c:v ffv1 -f matroska "$TMP/clip.mkv"

# source still for the comparison table (~2 s in)
ffmpeg -y -v error -ss 2 -i "$TMP/clip.mkv" -frames:v 1 samples/images/source.png

# ── the three formats (120 cols for the committed samples) ──
"$BIN" "$TMP/clip.mkv" --cols 120 --mode 4 --out "$TMP/ascii" >/dev/null
"$BIN" "$TMP/clip.mkv" --cols 120 --pixel --out "$TMP/pixel" >/dev/null
"$BIN" "$TMP/clip.mkv" --cols 120 --profile --qf 70 --out "$TMP/prof" > "$TMP/prof_report.txt" 2>&1

# ── render the comparison stills (frame 60 = 2 s) ──
for f in ascii pixel prof; do
    "$RENDER" "$TMP/$f.ascf" --out "$TMP/r_$f" --frame 60 >/dev/null
    ffmpeg -y -v error -i "$TMP/r_$f/frame_000060.ppm" "samples/images/$f.png"
done

# ── animated GIF teaser (pixel mode, downscaled + palette-limited; every 4th
#    frame keeps it ~0.6 MB for the repo) ──
"$RENDER" "$TMP/pixel.ascf" --out "$TMP/r_gif" >/dev/null
ffmpeg -y -v error -framerate 30 -i "$TMP/r_gif/frame_%06d.ppm" \
    -vf 'select=not(mod(n\,4)),scale=200:-2,palettegen=stats_mode=diff' /tmp/asciline_pal.png
ffmpeg -y -v error -framerate 30 -i "$TMP/r_gif/frame_%06d.ppm" \
    -i /tmp/asciline_pal.png -lavfi 'select=not(mod(n\,4)),scale=200:-2 [x];[x][1:v] paletteuse' \
    samples/images/pixel.gif
rm -f /tmp/asciline_pal.png

# ── the playable .ascf samples + the profile quality report ──
cp "$TMP/ascii.ascf" samples/mandelbrot_ascii.ascf
cp "$TMP/pixel.ascf" samples/mandelbrot_pixel.ascf
cp "$TMP/prof.ascf"  samples/mandelbrot_profile.ascf
grep -A8 '\[Quality\]' "$TMP/prof_report.txt" > samples/mandelbrot_profile_quality.txt || true

# ════════════════════════════════════════════════════════════════════════════
# REAL-VIDEO EVIDENCE
# ════════════════════════════════════════════════════════════════════════════

# ── 1. quality comparison: SOURCE | PIXEL | PROFILE, 240 columns, 30 fps ──
"$BIN" "$TMP/clip.mkv" --cols 240 --pixel --out "$TMP/v_pixel" >/dev/null
"$BIN" "$TMP/clip.mkv" --cols 240 --profile --qf 70 --out "$TMP/v_prof" > "$TMP/v_prof.txt" 2>&1
"$RENDER" "$TMP/v_pixel.ascf" --out "$TMP/r_vp" --scale 2 >/dev/null
"$RENDER" "$TMP/v_prof.ascf"  --out "$TMP/r_vf" --scale 2 >/dev/null
ffmpeg -y -v error -i "$TMP/clip.mkv" -vf 'scale=480:270' "$TMP/src_%06d.ppm"

PSNR_Y=$(grep 'PSNR-Y' "$TMP/v_prof.txt" | head -1 | awk '{print $3}')
# the profile encoder pads rows to a multiple of 16 (240x144 vs 240x135),
# so pad the other two panels to match before hstacking
ffmpeg -y -v error \
    -framerate 30 -i "$TMP/src_%06d.ppm" \
    -framerate 30 -i "$TMP/r_vp/frame_%06d.ppm" \
    -framerate 30 -i "$TMP/r_vf/frame_%06d.ppm" \
    -filter_complex "\
        [0:v]scale=480:270,pad=480:288:0:9:black,drawtext=$LBL:text='SOURCE':x=12:y=12[a];\
        [1:v]pad=480:288:0:9:black,drawtext=$LBL:text='PIXEL  lossless adaptive':x=12:y=12[b];\
        [2:v]drawtext=$LBL:text='PROFILE QF=70  PSNR-Y ${PSNR_Y} dB':x=12:y=12[c];\
        [a][b][c]hstack=3,drawtext=$LBL:text='frame %{n}  30 fps':x=w-240:y=12[v]" \
    -map '[v]' -c:v libx264 -preset medium -crf 20 -pix_fmt yuv420p \
    samples/evidence/quality_compare.mp4

# ── 2. true 120 fps live capture: the server streams a 120 fps source and we
#       record the actual WebSocket frames at 120 fps (--pixel bypasses the
#       codec, so every wire frame IS a displayed frame; an x264 source is
#       used because FFV1 decode caps the server around 47 fps, not the
#       pipeline itself — measured 117+ fps with h264) ──
ffmpeg -y -v error -f lavfi -i 'mandelbrot=size=640x360:rate=120' -t 6 \
    -c:v libx264 -preset veryfast -crf 18 -pix_fmt yuv420p "$TMP/mb120.mp4"
PORT=$(shuf -i 20000-30000 -n 1)
"$SERVER" "$TMP/mb120.mp4" --fps 120 --cols 240 --pixel --port "$PORT" --no-thumbnails \
    >/dev/null 2>&1 &
SRV=$!
sleep 2
node experiments/fps_count.js "$PORT" 3 > "$TMP/fps.log" 2>&1
"$RENDER" --live "ws://127.0.0.1:$PORT/ws?codec=adaptive" --out "$TMP/r_live" \
    --scale 2 --max-frames 720 > "$TMP/live.log" 2>&1
kill "$SRV" 2>/dev/null || true
wait "$SRV" 2>/dev/null || true

LIVE_FPS=$(grep -oE '[0-9.]+ fps' "$TMP/live.log" | tail -1 | sed 's/ fps//')
{
    echo "== fps_count.js — browser-like WS frame counter (3 s window) =="
    cat "$TMP/fps.log"
    echo
    echo "== asciline-render --live — full 720-frame capture of the wire stream =="
    cat "$TMP/live.log"
} > samples/evidence/stream_120fps.log

ffmpeg -y -v error -framerate 120 -i "$TMP/r_live/frame_%06d.ppm" \
    -vf "drawtext=$LBL:text='ASCILINE-RS LIVE  240x135 pixel  120 fps source  measured ${LIVE_FPS} fps':x=12:y=12,drawtext=$LBL:text='frame %{n}':x=w-180:y=12" \
    -c:v libx264 -preset medium -crf 23 -pix_fmt yuv420p \
    samples/evidence/stream_120fps.mp4

echo '=== samples/ ==='
ls -la samples samples/images samples/evidence | grep -vE '^total|^d'
