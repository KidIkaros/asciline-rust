#!/usr/bin/env bash
# Regenerate the committed format evidence from the pinned Big Buck Bunny
# excerpts in samples/source/. The source is the Blender Foundation cartoon;
# see samples/SOURCE.md for license, URLs, and checksums.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release --bin asciline-compile --bin asciline-render --bin asciline-server

BIN=target/release/asciline-compile
RENDER=target/release/asciline-render
SERVER=target/release/asciline-server
FONT=${FONT:-/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf}
if [[ ! -f "$FONT" ]]; then
    echo "font not found: $FONT (set FONT=/path/to/font.ttf)" >&2
    exit 1
fi
LBL="fontfile=$FONT:fontsize=18:fontcolor=white:box=1:boxcolor=black@0.70:boxborderw=6"

SOURCE30=samples/source/big_buck_bunny_excerpt_30fps.mp4
SOURCE60=samples/source/big_buck_bunny_excerpt_60fps.mp4
SHA30=8f113ef593688f47ec8d8b0d093fb955cb04bc350826c775d2e9ca451870856e
SHA60=1cf8e47cdef1c3acb4cab994a463a0ca6dabe1532bc89f09f90873dae45e98e8
[[ "$(sha256sum "$SOURCE30" | awk '{print $1}')" == "$SHA30" ]]
[[ "$(sha256sum "$SOURCE60" | awk '{print $1}')" == "$SHA60" ]]

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
mkdir -p samples/images samples/evidence

# ── compile the actual cartoon through all three ASCILINE formats ──
"$BIN" "$SOURCE30" --cols 240 --mode 4 --out "$TMP/ascii" >/dev/null
"$BIN" "$SOURCE30" --cols 240 --pixel --out "$TMP/pixel" >/dev/null
"$BIN" "$SOURCE30" --cols 240 --profile --qf 70 --out "$TMP/profile" > "$TMP/profile_report.txt" 2>&1

# ── source and decoded stills: the exact same frame, not a synthetic pattern ──
FRAME=90
ffmpeg -y -v error -ss 3 -i "$SOURCE30" -frames:v 1 samples/images/cartoon_source.png
"$RENDER" "$TMP/ascii.ascf" --out "$TMP/r_ascii" --frame "$FRAME" >/dev/null
"$RENDER" "$TMP/pixel.ascf" --out "$TMP/r_pixel" --scale 2 --frame "$FRAME" >/dev/null
"$RENDER" "$TMP/profile.ascf" --out "$TMP/r_profile" --scale 2 --frame "$FRAME" >/dev/null
ffmpeg -y -v error -i "$TMP/r_ascii/frame_000090.ppm" samples/images/cartoon_ascii.png
ffmpeg -y -v error -i "$TMP/r_pixel/frame_000090.ppm" samples/images/cartoon_pixel.png
ffmpeg -y -v error -i "$TMP/r_profile/frame_000090.ppm" samples/images/cartoon_profile.png
grep -A8 '\[Quality\]' "$TMP/profile_report.txt" > samples/big_buck_bunny_profile_quality.txt || true
cp "$TMP/ascii.ascf" samples/big_buck_bunny_ascii.ascf
cp "$TMP/pixel.ascf" samples/big_buck_bunny_pixel.ascf
cp "$TMP/profile.ascf" samples/big_buck_bunny_profile.ascf

# ── render all decoded frames once for the GIFs and comparison MP4 ──
"$RENDER" "$TMP/pixel.ascf" --out "$TMP/r_pixel_all" --scale 2 >/dev/null
"$RENDER" "$TMP/profile.ascf" --out "$TMP/r_profile_all" --scale 2 >/dev/null
ffmpeg -y -v error -i "$SOURCE30" -vf scale=480:270 -start_number 0 "$TMP/source_%06d.ppm"

# Source/pixel/profile are padded to the same 480x288 panel. The difference
# panel is deliberately amplified and explicitly labeled; it is not presented
# as ordinary displayed output.
PSNR_Y=$(grep 'PSNR-Y' "$TMP/profile_report.txt" | head -1 | awk '{print $3}')
PROFILE_SIZE=$(stat -c %s "$TMP/profile.ascf")
PIXEL_SIZE=$(stat -c %s "$TMP/pixel.ascf")
RATIO=$(python3 - "$PIXEL_SIZE" "$PROFILE_SIZE" <<'PY'
import sys
print(f"{int(sys.argv[1]) / int(sys.argv[2]):.1f}")
PY
)
FILTER="\
[0:v]pad=480:288:0:9:black,drawtext=$LBL:text='SOURCE  Big Buck Bunny':x=10:y=10[a];\
[1:v]pad=480:288:0:9:black,drawtext=$LBL:text='PIXEL  lossless adaptive':x=10:y=10[b];\
[2:v]drawtext=$LBL:text='PROFILE  QF=70  PSNR-Y ${PSNR_Y} dB  ${RATIO}x smaller':x=10:y=10[c];\
[a][b][c]hstack=3,drawtext=$LBL:text='Big Buck Bunny  |  frame %{n}  |  30 fps':x=12:y=h-38[v]"
ffmpeg -y -v error \
    -framerate 30 -i "$TMP/source_%06d.ppm" \
    -framerate 30 -i "$TMP/r_pixel_all/frame_%06d.ppm" \
    -framerate 30 -i "$TMP/r_profile_all/frame_%06d.ppm" \
    -filter_complex "$FILTER" -map '[v]' -c:v libx264 -preset medium -crf 18 -pix_fmt yuv420p \
    samples/evidence/cartoon_compare.mp4

# GitHub embeds GIFs reliably; MP4 remains available as the full-resolution
# download. GIFs are 15 fps for browser compatibility, but retain every second
# source frame and are large enough to judge faces, fur, foliage, and edges.
ffmpeg -y -v error -i samples/evidence/cartoon_compare.mp4 \
    -vf 'fps=10,scale=800:-1:flags=lanczos,palettegen=stats_mode=diff' "$TMP/compare_palette.png"
ffmpeg -y -v error -i samples/evidence/cartoon_compare.mp4 -i "$TMP/compare_palette.png" \
    -lavfi 'fps=10,scale=800:-1:flags=lanczos[x];[x][1:v]paletteuse=dither=sierra2_4a' -loop 0 \
    samples/evidence/cartoon_compare.gif

# Profile-only view: no panel shrinking, so viewers can judge the actual
# reconstruction at a readable size.
ffmpeg -y -v error -framerate 30 -i "$TMP/r_profile_all/frame_%06d.ppm" \
    -vf "drawtext=$LBL:text='PROFILE  QF=70  Big Buck Bunny  |  ${RATIO}x smaller  |  PSNR-Y ${PSNR_Y} dB':x=10:y=10" \
    -c:v libx264 -preset medium -crf 18 -pix_fmt yuv420p samples/evidence/cartoon_profile.mp4
ffmpeg -y -v error -i samples/evidence/cartoon_profile.mp4 \
    -vf 'fps=10,scale=560:-1:flags=lanczos,palettegen=stats_mode=diff' "$TMP/profile_palette.png"
ffmpeg -y -v error -i samples/evidence/cartoon_profile.mp4 -i "$TMP/profile_palette.png" \
    -lavfi 'fps=10,scale=560:-1:flags=lanczos[x];[x][1:v]paletteuse=dither=sierra2_4a' -loop 0 \
    samples/evidence/cartoon_profile.gif

# Difference view: amplified only to make subtle DCT/chroma errors visible.
ffmpeg -y -v error \
    -framerate 30 -i "$TMP/source_%06d.ppm" \
    -framerate 30 -i "$TMP/r_profile_all/frame_%06d.ppm" \
    -filter_complex "\
      [0:v]pad=480:288:0:9:black,split=2[a_blend][a_label];\
      [1:v]crop=480:288:0:0,split=2[b_blend][b_label];\
      [a_blend][b_blend]blend=all_mode=difference,eq=contrast=4:brightness=0.08,drawtext=$LBL:text='AMPLIFIED DIFFERENCE  4x  (not normal output)':x=10:y=10[d];\
      [a_label]drawtext=$LBL:text='SOURCE':x=10:y=10[s];\
      [b_label]drawtext=$LBL:text='PROFILE QF=70':x=10:y=10[p];\
      [s][p][d]hstack=3,drawtext=$LBL:text='Big Buck Bunny  |  differences amplified for inspection':x=12:y=h-38[v]" \
    -map '[v]' -c:v libx264 -preset medium -crf 18 -pix_fmt yuv420p samples/evidence/cartoon_difference.mp4
ffmpeg -y -v error -i samples/evidence/cartoon_difference.mp4 \
    -vf 'fps=10,scale=800:-1:flags=lanczos,palettegen=stats_mode=diff' "$TMP/diff_palette.png"
ffmpeg -y -v error -i samples/evidence/cartoon_difference.mp4 -i "$TMP/diff_palette.png" \
    -lavfi 'fps=10,scale=800:-1:flags=lanczos[x];[x][1:v]paletteuse=dither=sierra2_4a' -loop 0 \
    samples/evidence/cartoon_difference.gif

# ── real 60fps cartoon source over the 120fps server wire path ──
PORT=$(shuf -i 20000-30000 -n 1)
"$SERVER" "$SOURCE60" --fps 120 --cols 240 --pixel --port "$PORT" --no-thumbnails >/dev/null 2>&1 &
SRV=$!
cleanup_server() { kill "$SRV" 2>/dev/null || true; wait "$SRV" 2>/dev/null || true; }
trap 'cleanup_server; rm -rf "$TMP"' EXIT
sleep 2
node experiments/fps_count.js "$PORT" 3 > "$TMP/fps.log" 2>&1
"$RENDER" --live "ws://127.0.0.1:$PORT/ws?codec=adaptive" --out "$TMP/r_live" \
    --scale 2 --max-frames 480 > "$TMP/live.log" 2>&1
cleanup_server
LIVE_FPS=$(grep -oE '[0-9.]+ fps' "$TMP/live.log" | tail -1 | sed 's/ fps//')
{
    echo '== fps_count.js — browser-like WS frame counter =='
    cat "$TMP/fps.log"
    echo
    echo '== asciline-render --live — wire capture =='
    cat "$TMP/live.log"
    echo
    echo 'Note: the pinned cartoon source is 60 fps; the server target is 120 fps.'
    echo 'The GIF is illustrative. The frame count/rate log is the authoritative wire evidence.'
} > samples/evidence/cartoon_120fps.log
ffmpeg -y -v error -framerate 120 -i "$TMP/r_live/frame_%06d.ppm" \
    -vf "drawtext=$LBL:text='ASCILINE-RS LIVE  Big Buck Bunny  60 fps source  120 fps target  measured ${LIVE_FPS} fps':x=10:y=10,drawtext=$LBL:text='wire frame %{n}':x=w-160:y=10" \
    -c:v libx264 -preset medium -crf 20 -pix_fmt yuv420p samples/evidence/cartoon_120fps.mp4

cat > samples/SOURCE.md <<'EOF'
# Evidence source and attribution

The visual evidence uses an 8-second excerpt of **Big Buck Bunny**, the Blender
Foundation animated short, rather than a synthetic test pattern.

- Official project: <https://peach.blender.org/>
- Official downloads: <https://download.blender.org/peach/bigbuckbunny_movies/>
- License: Creative Commons Attribution 3.0 Unported (CC BY 3.0)
- Attribution: Big Buck Bunny © Blender Foundation / peach.blender.org
- Excerpt: approximately 00:01:00–00:01:08 from the official 640×360 release;
  the 60-fps excerpt is approximately 00:01:00–00:01:04 from the official
  1080p 60-fps release, resized to 640×360.

The committed source excerpts are lossless H.264 intermediates created from the
official downloads. Their hashes are checked by `experiments/make_samples.sh`:

```text
8f113ef593688f47ec8d8b0d093fb955cb04bc350826c775d2e9ca451870856e  big_buck_bunny_excerpt_30fps.mp4
1cf8e47cdef1c3acb4cab994a463a0ca6dabe1532bc89f09f90873dae45e98e8  big_buck_bunny_excerpt_60fps.mp4
```
EOF

echo '=== samples/evidence ==='
ls -lh samples/evidence
