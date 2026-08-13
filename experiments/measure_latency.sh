#!/usr/bin/env bash
# Measure end-to-end frame latency of the live server (frame in -> wire out ->
# decode -> display) and report p50/p95/p99/max per stage.
#
# Pipeline: a 120 fps h264 source is streamed by asciline-server (--fps 120,
# 240-column pixel mode = the highest-detail live config). The server records
# t_read/t_encode/t_send per frame (--latency-log), asciline-render --live
# records t_recv/t_decode/t_render, and experiments/analyze_latency.py joins
# the two logs by frame index. Both processes run on the same host, so their
# monotonic timestamps are directly comparable.
#
# Run from the repo root:  experiments/measure_latency.sh [fps] [seconds]
# Defaults: 120 fps, 6 seconds. First argument can be any target fps.
set -euo pipefail
cd "$(dirname "$0")/.."

FPS="${1:-120}"
SECS="${2:-6}"

cargo build --release --bin asciline-server --bin asciline-render

SERVER=target/release/asciline-server
RENDER=target/release/asciline-render

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# Deterministic source at the target fps. h264 (not FFV1): FFV1's decode cost
# caps the server near 47 fps regardless of the pipeline — the h264 source is
# what lets the wire stream actually run at 120 fps (measured 111-116 fps).
ffmpeg -y -v error -f lavfi -i "mandelbrot=size=640x360:rate=${FPS}" -t "$SECS" \
    -c:v libx264 -preset veryfast -crf 18 -pix_fmt yuv420p "$TMP/src.mp4"

PORT=$(shuf -i 20000-30000 -n 1)
"$SERVER" "$TMP/src.mp4" --fps "$FPS" --cols 240 --pixel --port "$PORT" --no-thumbnails \
    --latency-log "$TMP/server.log" >/dev/null 2>&1 &
SRV=$!
sleep 2

NFRAMES=$((FPS * SECS))
"$RENDER" --live "ws://127.0.0.1:$PORT/ws?codec=adaptive" --out "$TMP/frames" \
    --scale 2 --max-frames "$NFRAMES" --latency-log "$TMP/client.log" \
    >"$TMP/client.out" 2>&1 || true
kill "$SRV" 2>/dev/null || true
wait "$SRV" 2>/dev/null || true

echo "=== capture (stderr of asciline-render --live) ==="
grep -E 'live INIT|live capture' "$TMP/client.out" || tail -3 "$TMP/client.out"

echo
echo "=== server log: $(wc -l < "$TMP/server.log") frames, client log: $(wc -l < "$TMP/client.log") frames ==="
echo
python3 experiments/analyze_latency.py "$TMP/server.log" "$TMP/client.log"
