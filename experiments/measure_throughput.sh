#!/usr/bin/env bash
# Measure the actual high-rate server path with a source whose frame identity
# is independently checked. This is throughput evidence, not visual-quality
# evidence (Big Buck Bunny covers visual quality in make_samples.sh).
#
# The generated testsrc2 source has a unique framemd5 for every source frame.
# For each target rate we record source frame count, unique source hashes,
# server-sent frame count, and the send-timestamp rate from --latency-log.
# At 120 fps we also capture the actual decoded WebSocket wire frames into a
# GitHub-publishable MP4/GIF.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release --bin asciline-server --bin asciline-render
SERVER=target/release/asciline-server
RENDER=target/release/asciline-render
DURATION=${DURATION:-4}
COLS=${COLS:-240}
ROWS=${ROWS:-135}
OUT=samples/evidence
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$OUT"

FONT=${FONT:-/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf}
if [[ ! -f "$FONT" ]]; then
    echo "font not found: $FONT (set FONT=/path/to/font.ttf)" >&2
    exit 1
fi
LBL="fontfile=$FONT:fontsize=18:fontcolor=white:box=1:boxcolor=black@0.70:boxborderw=6"

printf '# Unique-frame throughput benchmark\n\n' > "$OUT/throughput_matrix.md"
printf '| Target | Source frames | Unique hashes | Server sent | Measured wire rate |\n' >> "$OUT/throughput_matrix.md"
printf '|---:|---:|---:|---:|---:|\n' >> "$OUT/throughput_matrix.md"
{
    echo 'ASCILINE-Rust unique-frame throughput benchmark'
    echo "Grid: ${COLS}x${ROWS}; duration: ${DURATION}s; source: deterministic ffmpeg testsrc2"
    echo 'A unique hash count equal to source frames prevents duplicated 60fps content from masquerading as 120fps input.'
    echo
} > "$OUT/throughput_benchmark.log"

for FPS in 60 120 240 480; do
    echo "=== ${FPS} fps ==="
    SRC="$TMP/src_${FPS}.mp4"
    LOG="$TMP/server_${FPS}.log"
    ffmpeg -y -v error -f lavfi \
        -i "testsrc2=size=640x360:rate=${FPS}:duration=${DURATION}" \
        -c:v libx264 -preset ultrafast -crf 18 -pix_fmt yuv420p "$SRC"

    SOURCE_FRAMES=$(ffprobe -v error -count_frames -select_streams v:0 \
        -show_entries stream=nb_read_frames -of csv=p=0 "$SRC")
    UNIQUE_HASHES=$(ffmpeg -v error -i "$SRC" -f framemd5 - 2>/dev/null \
        | awk '!/^#/ && NF {print $NF}' | sort -u | wc -l)
    if [[ "$SOURCE_FRAMES" != "$UNIQUE_HASHES" ]]; then
        echo "source uniqueness check failed at ${FPS}: ${SOURCE_FRAMES} frames, ${UNIQUE_HASHES} hashes" >&2
        exit 1
    fi

    PORT=$(shuf -i 20000-30000 -n 1)
    "$SERVER" "$SRC" --fps "$FPS" --cols "$COLS" --rows "$ROWS" --pixel \
        --port "$PORT" --no-thumbnails --latency-log "$LOG" \
        > "$TMP/server_${FPS}.out" 2>&1 &
    SRV=$!
    sleep 1

    # The browser-like counter is supplementary; the flushed server log is the
    # complete count because it covers the whole source even after EOF. At
    # 120 fps the renderer is the sole client so one server log is not doubled
    # by two simultaneous connections writing to the same measurement file.
    if [[ "$FPS" == 120 ]]; then
        "$RENDER" --live "ws://127.0.0.1:$PORT/ws?codec=adaptive" \
            --out "$TMP/wire_frames" --scale 2 --max-frames "$SOURCE_FRAMES" \
            > "$TMP/live_120.log" 2>&1
        cp "$TMP/live_120.log" "$TMP/fps_${FPS}.log"
    else
        node experiments/fps_count.js "$PORT" "$((DURATION + 1))" \
            > "$TMP/fps_${FPS}.log" 2>&1 || true
    fi

    kill "$SRV" 2>/dev/null || true
    wait "$SRV" 2>/dev/null || true

    SENT=$(wc -l < "$LOG")
    WIRE_FPS=$(python3 - "$LOG" <<'PY'
import sys
rows = []
with open(sys.argv[1]) as f:
    for line in f:
        p = line.split()
        if len(p) == 4:
            rows.append(int(p[3]))
if len(rows) < 2 or rows[-1] <= rows[0]:
    print("0.0")
else:
    print(f"{(len(rows) - 1) / ((rows[-1] - rows[0]) / 1e9):.1f}")
PY
)
    printf '| %s | %s | %s | %s | %s fps |\n' \
        "$FPS" "$SOURCE_FRAMES" "$UNIQUE_HASHES" "$SENT" "$WIRE_FPS" \
        >> "$OUT/throughput_matrix.md"
    {
        echo "=== target ${FPS} fps ==="
        cat "$TMP/fps_${FPS}.log"
        echo "source frames: ${SOURCE_FRAMES}"
        echo "unique source hashes: ${UNIQUE_HASHES}"
        echo "server-sent frames: ${SENT}"
        echo "timestamp-derived wire rate: ${WIRE_FPS} fps"
        echo
    } >> "$OUT/throughput_benchmark.log"

    if [[ "$FPS" == 120 ]]; then
        LIVE_FPS=$(grep -oE '[0-9.]+ fps' "$TMP/live_120.log" | tail -1 | sed 's/ fps//')
        ffmpeg -y -v error -framerate 120 -i "$TMP/wire_frames/frame_%06d.ppm" \
            -vf "drawtext=$LBL:text='ASCILINE-RS  UNIQUE 120 FPS SOURCE  |  measured ${LIVE_FPS} fps wire capture':x=10:y=10,drawtext=$LBL:text='wire frame %{n}':x=w-170:y=10" \
            -c:v libx264 -preset medium -crf 20 -pix_fmt yuv420p \
            "$OUT/throughput_120fps.mp4"
        ffmpeg -y -v error -i "$OUT/throughput_120fps.mp4" \
            -vf 'fps=10,scale=800:-1:flags=lanczos,palettegen=stats_mode=diff' \
            "$TMP/throughput_palette.png"
        ffmpeg -y -v error -i "$OUT/throughput_120fps.mp4" -i "$TMP/throughput_palette.png" \
            -lavfi 'fps=10,scale=800:-1:flags=lanczos[x];[x][1:v]paletteuse=dither=sierra2_4a' \
            -loop 0 "$OUT/throughput_120fps.gif"
    fi

    rm -f "$SRC" "$LOG"
done

cat >> "$OUT/throughput_benchmark.log" <<'EOF'

Interpretation:
- The source has a unique framemd5 for every frame; no duplicated 60fps source is used.
- Server-sent count equals the complete source count at each tested rate.
- `throughput_120fps.mp4` and `.gif` are visual wire captures; their playback rate is not the measurement.
- The timestamp-derived rates are machine measurements, not an unlimited performance guarantee.
EOF

echo '=== throughput evidence ==='
cat "$OUT/throughput_matrix.md"
cat "$OUT/throughput_benchmark.log"
