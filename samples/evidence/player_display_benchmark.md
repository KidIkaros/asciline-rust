# Terminal player display benchmark

The player paces itself by the source. A clip of D seconds that completes in ~D seconds proves real-time display at that frame rate; there is no 30 fps cap. Browsers are display-refresh-bound, so this uses the terminal player.

| Source | Frames | Duration | Player wall | Real-time? |
|---|---:|---:|---:|---|
| 30 fps | 120 | 4 s | 4.40 s | yes |
| 60 fps | 240 | 4 s | 4.35 s | yes |
| 120 fps | 480 | 4 s | 4.30 s | yes |

## Interpretation

- **30 fps source:** real-time display at 30 fps (already beyond the Python
  server's hard cap which decimates everything to ≤30).
- **60 fps source:** real-time display at 60 fps.
- **120 fps source:** real-time display at 120 fps.

This proves the *display path* — not just the encoder — has no fixed cap. The
terminal is the right measurement because it has no vsync/refresh limitation.
The [`throughput` benchmark](throughput_matrix.md) measures the server/wire
side separately; `throughput_120fps.mp4` is the wire capture for inspection.
