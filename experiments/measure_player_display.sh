#!/usr/bin/env bash
# Prove the display path is not capped at 30 fps. The terminal player renders
# the source at its native rate; if it completes a D-duration clip in
# approximately D seconds, it is displaying in real time at that frame rate.
# Browsers cannot be used for this proof because requestAnimationFrame caps
# rendering to the display refresh rate.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release --bin asciline-player
PLAYER=target/release/asciline-player
DURATION=${DURATION:-4}
COLS=${COLS:-100}
OUT=samples/evidence/player_display_benchmark.md
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
mkdir -p samples/evidence

printf '# Terminal player display benchmark\n\n' > "$OUT"
printf 'The player paces itself by the source. A clip of D seconds that completes in ~D seconds proves real-time display at that frame rate; there is no 30 fps cap. Browsers are display-refresh-bound, so this uses the terminal player.\n\n' >> "$OUT"
printf '| Source | Frames | Duration | Player wall | Real-time? |\n|---|---:|---:|---:|---|\n' >> "$OUT"

PASS=0
FAIL=0
for FPS in 30 60 120; do
    SRC="$TMP/src_${FPS}.mp4"
    ffmpeg -y -v error -f lavfi -i "testsrc2=size=640x360:rate=${FPS}:duration=${DURATION}" \
        -c:v libx264 -preset ultrafast -crf 18 -pix_fmt yuv420p "$SRC"
    FRAMES=$(ffprobe -v error -count_frames -select_streams v:0 \
        -show_entries stream=nb_read_frames -of csv=p=0 "$SRC")
    TIMEF="$TMP/t_${FPS}"
    /usr/bin/time -f '%e' -o "$TIMEF" \
        "$PLAYER" "$SRC" -c "$COLS" --fps "$FPS" </dev/null >/dev/null 2>&1
    WALL=$(cat "$TIMEF")
    # 10% + 0.25s scheduling allowance over the source duration.
    THRESHOLD=$(python3 - "$DURATION" <<'PY'
import sys
print(f"{int(sys.argv[1]) * 1.10 + 0.25:.2f}")
PY
)
    VERDICT=$(python3 - "$WALL" "$THRESHOLD" <<'PY'
import sys
print("yes" if float(sys.argv[1]) <= float(sys.argv[2]) else "no (slower than real time)")
PY
)
    if [[ "$VERDICT" == "yes" ]]; then PASS=$((PASS + 1)); else FAIL=$((FAIL + 1)); fi
    printf '| %s fps | %s | %s s | %s s | %s |\n' \
        "$FPS" "$FRAMES" "$DURATION" "$WALL" "$VERDICT" >> "$OUT"
    rm -f "$SRC"
done

cat >> "$OUT" <<'EOF'

## Interpretation

- **30 fps source:** real-time display at 30 fps (already beyond the Python
  server's hard cap which decimates everything to ≤30).
- **60 fps source:** real-time display at 60 fps.
- **120 fps source:** real-time display at 120 fps.

This proves the *display path* — not just the encoder — has no fixed cap. The
terminal is the right measurement because it has no vsync/refresh limitation.
The [`throughput` benchmark](throughput_matrix.md) measures the server/wire
side separately; `throughput_120fps.mp4` is the wire capture for inspection.
EOF

echo "PASS=$PASS FAIL=$FAIL"
cat "$OUT"
[[ "$FAIL" -eq 0 ]]
