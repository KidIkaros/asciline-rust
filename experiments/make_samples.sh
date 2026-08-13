#!/usr/bin/env bash
# Regenerate the committed samples/ format evidence: a source clip compiled in
# all three formats (ASCII mode, pixel mode, --profile lossy DCT), rendered by
# OUR decoders via asciline-render into the README comparison stills + an
# animated GIF, plus the .ascf files themselves and the profile quality report.
#
# Run from the repo root:  experiments/make_samples.sh
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release --bin asciline-compile --bin asciline-render

BIN=target/release/asciline-compile
RENDER=target/release/asciline-render

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

mkdir -p samples/images

# Deterministic 6 s mandelbrot zoom source, lossless FFV1 (same reasoning as
# the fuzz corpus: no x264 rate-control drift between ffmpeg builds).
ffmpeg -y -v error -f lavfi -i 'mandelbrot=size=640x360:rate=30' -t 6 \
    -c:v ffv1 -f matroska "$TMP/clip.mkv"

# source still for the comparison table (~2 s in)
ffmpeg -y -v error -ss 2 -i "$TMP/clip.mkv" -frames:v 1 samples/images/source.png

# ── the three formats ──
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

echo '=== samples/ ==='
ls -la samples samples/images | grep -vE '^total|^d'
