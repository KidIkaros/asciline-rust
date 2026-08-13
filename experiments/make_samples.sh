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
SOURCE_FPS=$(ffprobe -v error -select_streams v:0 -show_entries stream=r_frame_rate \
    -of csv=p=0 "$SOURCE30" | awk -F/ '{printf "%.0f", $1 / $2}')

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
mkdir -p samples/images samples/evidence

# ── compile the actual cartoon through all three ASCILINE formats ──
"$BIN" "$SOURCE30" --cols 240 --mode 4 --out "$TMP/ascii" >/dev/null
"$BIN" "$SOURCE30" --cols 240 --pixel --out "$TMP/pixel" >/dev/null
for QF in 40 70 90; do
    "$BIN" "$SOURCE30" --cols 240 --profile --qf "$QF" --out "$TMP/profile_qf$QF" \
        > "$TMP/profile_qf${QF}_report.txt" 2>&1
done
PROFILE="$TMP/profile_qf70.ascf"
PROFILE_REPORT="$TMP/profile_qf70_report.txt"

# ── source and decoded stills: the exact same frame, not a synthetic pattern ──
FRAME=90
ffmpeg -y -v error -ss 3 -i "$SOURCE30" -frames:v 1 samples/images/cartoon_source.png
"$RENDER" "$TMP/ascii.ascf" --out "$TMP/r_ascii" --frame "$FRAME" >/dev/null
"$RENDER" "$TMP/pixel.ascf" --out "$TMP/r_pixel" --scale 2 --frame "$FRAME" >/dev/null
"$RENDER" "$PROFILE" --out "$TMP/r_profile" --scale 2 --frame "$FRAME" >/dev/null
ffmpeg -y -v error -i "$TMP/r_ascii/frame_000090.ppm" samples/images/cartoon_ascii.png
ffmpeg -y -v error -i "$TMP/r_pixel/frame_000090.ppm" samples/images/cartoon_pixel.png
ffmpeg -y -v error -i "$TMP/r_profile/frame_000090.ppm" samples/images/cartoon_profile.png
grep -A8 '\[Quality\]' "$PROFILE_REPORT" > samples/big_buck_bunny_profile_quality.txt || true
cp "$TMP/ascii.ascf" samples/big_buck_bunny_ascii.ascf
cp "$TMP/pixel.ascf" samples/big_buck_bunny_pixel.ascf
cp "$PROFILE" samples/big_buck_bunny_profile.ascf
PIXEL_SIZE=$(stat -c %s "$TMP/pixel.ascf")
{
    echo '# Big Buck Bunny profile quality matrix'
    echo
    echo '| QF | Profile size | Pixel/profile ratio | PSNR-Y | SSIM-Y | PSNR-RGB |'
    echo '|---:|---:|---:|---:|---:|---:|'
    for QF in 40 70 90; do
        SIZE=$(stat -c %s "$TMP/profile_qf$QF.ascf")
        REPORT="$TMP/profile_qf${QF}_report.txt"
        PSNR=$(grep 'PSNR-Y' "$REPORT" | head -1 | awk '{print $3}')
        SSIM=$(grep 'SSIM-Y' "$REPORT" | head -1 | awk '{print $3}')
        RGB=$(grep 'PSNR-RGB' "$REPORT" | head -1 | awk '{print $3}')
        RATIO=$(python3 - "$PIXEL_SIZE" "$SIZE" <<'PY'
import sys
print(f"{int(sys.argv[1]) / int(sys.argv[2]):.1f}x")
PY
)
        printf '| %s | %s B | %s | %s dB | %s | %s dB |\n' "$QF" "$SIZE" "$RATIO" "$PSNR" "$SSIM" "$RGB"
    done
} > samples/big_buck_bunny_quality_matrix.md

# ── render all decoded frames once for the GIFs and comparison MP4 ──
"$RENDER" "$TMP/pixel.ascf" --out "$TMP/r_pixel_all" --scale 2 >/dev/null
"$RENDER" "$PROFILE" --out "$TMP/r_profile_all" --scale 2 >/dev/null
"$RENDER" "$PROFILE" --out "$TMP/r_profile_large" --scale 3 >/dev/null
ffmpeg -y -v error -i "$SOURCE30" -vf scale=480:270 -start_number 0 "$TMP/source_%06d.ppm"

# Source/pixel/profile are padded to the same 480x288 panel. The difference
# panel is deliberately amplified and explicitly labeled; it is not presented
# as ordinary displayed output.
PSNR_Y=$(grep 'PSNR-Y' "$PROFILE_REPORT" | head -1 | awk '{print $3}')
PROFILE_SIZE=$(stat -c %s "$PROFILE")
PIXEL_SIZE=$(stat -c %s "$TMP/pixel.ascf")
RATIO=$(python3 - "$PIXEL_SIZE" "$PROFILE_SIZE" <<'PY'
import sys
print(f"{int(sys.argv[1]) / int(sys.argv[2]):.1f}")
PY
)
FILTER="\
[0:v]pad=480:288:0:9:black,drawtext=$LBL:text='SOURCE  Big Buck Bunny  |  clip ${SOURCE_FPS} fps':x=10:y=10[a];\
[1:v]pad=480:288:0:9:black,drawtext=$LBL:text='PIXEL  lossless adaptive  |  clip ${SOURCE_FPS} fps':x=10:y=10[b];\
[2:v]drawtext=$LBL:text='PROFILE  QF=70  |  clip ${SOURCE_FPS} fps  |  PSNR-Y ${PSNR_Y} dB  |  ${RATIO}x smaller':x=10:y=10[c];\
[a][b][c]hstack=3,drawtext=$LBL:text='Big Buck Bunny  |  synchronized clip ${SOURCE_FPS} fps  |  GIF preview 10 fps':x=12:y=h-38[v]"
ffmpeg -y -v error \
    -framerate "$SOURCE_FPS" -i "$TMP/source_%06d.ppm" \
    -framerate "$SOURCE_FPS" -i "$TMP/r_pixel_all/frame_%06d.ppm" \
    -framerate "$SOURCE_FPS" -i "$TMP/r_profile_all/frame_%06d.ppm" \
    -filter_complex "$FILTER" -map '[v]' -c:v libx264 -preset medium -crf 18 -pix_fmt yuv420p \
    samples/evidence/cartoon_compare.mp4

# ── large source/profile inspection ──
# The overview GIF has three panels. This two-panel artifact keeps each image
# at a readable 720×405-ish size for judging faces, fur, foliage, and edges.
ffmpeg -y -v error -i "$SOURCE30" -vf scale=720:405 -start_number 0 "$TMP/source_large_%06d.ppm"
ffmpeg -y -v error \
    -framerate "$SOURCE_FPS" -i "$TMP/source_large_%06d.ppm" \
    -framerate "$SOURCE_FPS" -i "$TMP/r_profile_large/frame_%06d.ppm" \
    -filter_complex "\
      [0:v]pad=720:432:0:13:color=black,drawtext=$LBL:text='SOURCE  Big Buck Bunny  |  clip ${SOURCE_FPS} fps':x=12:y=12[a];\
      [1:v]drawtext=$LBL:text='PROFILE  QF=70  |  clip ${SOURCE_FPS} fps  |  PSNR-Y ${PSNR_Y} dB':x=12:y=12[b];\
      [a][b]hstack,drawtext=$LBL:text='source vs displayed profile  |  synchronized clip ${SOURCE_FPS} fps  |  GIF preview 5 fps':x=12:y=h-38[v]" \
    -map '[v]' -c:v libx264 -preset medium -crf 18 -pix_fmt yuv420p \
    samples/evidence/cartoon_source_profile_large.mp4
ffmpeg -y -v error -i samples/evidence/cartoon_source_profile_large.mp4 \
    -vf 'fps=5,scale=960:-1:flags=lanczos,palettegen=stats_mode=diff' "$TMP/large_palette.png"
ffmpeg -y -v error -i samples/evidence/cartoon_source_profile_large.mp4 -i "$TMP/large_palette.png" \
    -lavfi 'fps=5,scale=960:-1:flags=lanczos[x];[x][1:v]paletteuse=dither=sierra2_4a' -loop 0 \
    samples/evidence/cartoon_source_profile_large.gif

# Center-detail crop: a second view for spotting ringing, chroma edges, and
# texture changes without shrinking the whole frame into three columns.
ffmpeg -y -v error \
    -framerate "$SOURCE_FPS" -i "$TMP/source_large_%06d.ppm" \
    -framerate "$SOURCE_FPS" -i "$TMP/r_profile_large/frame_%06d.ppm" \
    -filter_complex "\
      [0:v]crop=360:202:180:13,scale=640:360:flags=lanczos,drawtext=$LBL:text='SOURCE  center detail  |  clip ${SOURCE_FPS} fps':x=12:y=12[a];\
      [1:v]crop=360:202:180:0,scale=640:360:flags=lanczos,drawtext=$LBL:text='PROFILE  QF=70  center detail  |  clip ${SOURCE_FPS} fps':x=12:y=12[b];\
      [a][b]hstack,drawtext=$LBL:text='detail inspection  |  synchronized clip ${SOURCE_FPS} fps  |  GIF preview 5 fps':x=12:y=h-38[v]" \
    -map '[v]' -c:v libx264 -preset medium -crf 18 -pix_fmt yuv420p \
    samples/evidence/cartoon_detail_compare.mp4
ffmpeg -y -v error -i samples/evidence/cartoon_detail_compare.mp4 \
    -vf 'fps=5,scale=960:-1:flags=lanczos,palettegen=stats_mode=diff' "$TMP/detail_palette.png"
ffmpeg -y -v error -i samples/evidence/cartoon_detail_compare.mp4 -i "$TMP/detail_palette.png" \
    -lavfi 'fps=5,scale=960:-1:flags=lanczos[x];[x][1:v]paletteuse=dither=sierra2_4a' -loop 0 \
    samples/evidence/cartoon_detail_compare.gif

# GitHub embeds GIFs reliably; MP4 remains available as the full-resolution
# download. GIFs use 10 fps for browser compatibility and file size, while the
# MP4s retain the original 30 fps.
ffmpeg -y -v error -i samples/evidence/cartoon_compare.mp4 \
    -vf 'fps=10,scale=800:-1:flags=lanczos,palettegen=stats_mode=diff' "$TMP/compare_palette.png"
ffmpeg -y -v error -i samples/evidence/cartoon_compare.mp4 -i "$TMP/compare_palette.png" \
    -lavfi 'fps=10,scale=800:-1:flags=lanczos[x];[x][1:v]paletteuse=dither=sierra2_4a' -loop 0 \
    samples/evidence/cartoon_compare.gif

# Profile-only view: no panel shrinking, so viewers can judge the actual
# reconstruction at a readable size.
ffmpeg -y -v error -framerate "$SOURCE_FPS" -i "$TMP/r_profile_all/frame_%06d.ppm" \
    -vf "drawtext=$LBL:text='PROFILE  QF=70  |  clip ${SOURCE_FPS} fps  |  ${RATIO}x smaller  |  PSNR-Y ${PSNR_Y} dB':x=10:y=10" \
    -c:v libx264 -preset medium -crf 18 -pix_fmt yuv420p samples/evidence/cartoon_profile.mp4
ffmpeg -y -v error -i samples/evidence/cartoon_profile.mp4 \
    -vf 'fps=10,scale=560:-1:flags=lanczos,palettegen=stats_mode=diff' "$TMP/profile_palette.png"
ffmpeg -y -v error -i samples/evidence/cartoon_profile.mp4 -i "$TMP/profile_palette.png" \
    -lavfi 'fps=10,scale=560:-1:flags=lanczos[x];[x][1:v]paletteuse=dither=sierra2_4a' -loop 0 \
    samples/evidence/cartoon_profile.gif

# Difference view: amplified only to make subtle DCT/chroma errors visible.
ffmpeg -y -v error \
    -framerate "$SOURCE_FPS" -i "$TMP/source_%06d.ppm" \
    -framerate "$SOURCE_FPS" -i "$TMP/r_profile_all/frame_%06d.ppm" \
    -filter_complex "\
      [0:v]pad=480:288:0:9:black,split=2[a_blend][a_label];\
      [1:v]crop=480:288:0:0,split=2[b_blend][b_label];\
      [a_blend][b_blend]blend=all_mode=difference,eq=contrast=4:brightness=0.08,drawtext=$LBL:text='AMPLIFIED DIFFERENCE  4x  (not normal output)  |  clip ${SOURCE_FPS} fps':x=10:y=10[d];\
      [a_label]drawtext=$LBL:text='SOURCE  |  clip ${SOURCE_FPS} fps':x=10:y=10[s];\
      [b_label]drawtext=$LBL:text='PROFILE QF=70  |  clip ${SOURCE_FPS} fps':x=10:y=10[p];\
      [s][p][d]hstack=3,drawtext=$LBL:text='Big Buck Bunny  |  synchronized clip ${SOURCE_FPS} fps  |  GIF preview 10 fps':x=12:y=h-38[v]" \
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
SOURCE60_FRAMES=$(ffprobe -v error -count_frames -select_streams v:0 \
    -show_entries stream=nb_read_frames -of csv=p=0 "$SOURCE60")
{
    echo '== fps_count.js — browser-like WS frame counter =='
    cat "$TMP/fps.log"
    echo
    echo '== asciline-render --live — wire capture =='
    cat "$TMP/live.log"
    echo
    echo "Source content frames: ${SOURCE60_FRAMES} at 60 fps"
    echo 'Server target: 120 fps; wire capture is transport throughput, not 120 unique source frames.'
    echo 'The GIF is illustrative. The frame count/rate log is the authoritative wire evidence.'
} > samples/evidence/cartoon_wire_120fps.log
ffmpeg -y -v error -framerate 120 -i "$TMP/r_live/frame_%06d.ppm" \
    -vf "drawtext=$LBL:text='ASCILINE-RS LIVE  Big Buck Bunny  60 fps source  120 fps target  measured ${LIVE_FPS} fps':x=10:y=10,drawtext=$LBL:text='wire frame %{n}':x=w-160:y=10" \
    -c:v libx264 -preset medium -crf 20 -pix_fmt yuv420p samples/evidence/cartoon_wire_120fps.mp4

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
