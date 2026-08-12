# Can ASCILINE run faster than 30 fps? Yes — here's how.

This document explains why the original Python implementation is capped at ~30 fps,
what the Rust port changes, and how the two compare with real measurements.

## 1. Why the original caps at 30 fps

Two separate reasons stack up:

### 1a. It's a hard software cap, not a hardware limit

`stream_server.py` contains:

```python
MAX_FPS = 30
if source_fps > MAX_FPS:
    skip_n = round(source_fps / MAX_FPS)   # e.g. 60/30 = 2
    effective_fps = source_fps / skip_n
```

Any source above 30 fps is **decimated to ≤30 fps before streaming**, and every
frame is then paced with `sleep(frame_t)`. The client is told the decimated rate
via the `INIT:` handshake, so playback is smooth — but deliberately 30 fps.
That's a product decision made because the Python pipeline can't reliably do more.

### 1b. The Python pipeline is serial and interpreter-bound

Even *without* the cap, the per-frame path is:

```
decode (OpenCV, C) → numpy resize → numpy gray → numpy char mapping
→ python loop building the framebuffer → zlib encode → uvicorn async send
```

- Everything runs in **one thread** (the GIL serializes decode, map, encode, and
  the asyncio loop hop for each frame).
- The map+encode stage is numpy + Python per-frame overhead — measured at
  **742 µs/frame** (240×67 grid, mode 6, lossless adaptive) → a ~1,350 fps
  *ceiling* for that stage alone, before decode, GC, socket latency, and the
  event loop are added back in.
- Frame pacing is `time.sleep`, and high column counts make the framebuffer
  copies and zlib passes bigger, which is why the README warns "pushing `--cols`
  beyond what your machine can encode/send in time causes desync".

The result: on a typical machine the whole loop lands around 30 fps — hence the cap.

## 2. What the Rust port changes

| Bottleneck (Python) | Rust fix |
|---|---|
| Hard `MAX_FPS = 30` cap | **No cap.** Sources play at native rate; `--fps N` sets any target. |
| Serial decode→map→encode→send | **Pipelined**: a dedicated OS thread decodes ffmpeg frames into a bounded channel while map+encode (tokio blocking pool) and the WebSocket send overlap it. |
| Decode at full res, then resize | ffmpeg's `scale=` filter resizes **inside** ffmpeg (SIMD) — only the tiny `cols×rows×3` frame crosses the pipe. Decimation is ffmpeg's `fps=` filter, not manual `grab()` skipping. |
| Python/Numpy per-frame overhead + GIL | Zero-copy-ish Rust loops, no interpreter; row-parallel mapping with rayon; autovectorized LUT lookups. |
| zlib via CPython + re-allocated numpy buffers per frame | `flate2` (pure-Rust miniz) + reused scratch buffers; keyframe every 48 frames, delta-first adaptive codec by default. |
| `sleep()` pacing in an asyncio loop | `tokio::time::sleep` on the async task only — the decode and encode threads never wait on it. |

## 3. Measured results (this machine: 12-core Ryzen, Linux, ffmpeg 6.1)

### 3a. Map + adaptive-encode stage (the CPU core of the pipeline)

```
cargo test --release --test bench_encode -- --ignored --nocapture
python3 experiments/bench_python.py     # same work, numpy + original codec.py
```

| grid | cells | Rust µs/frame | Python µs/frame | speed-up | Rust fps ceiling |
|---|---|---|---|---|---|
| 80×23 | 1,840 | 159 | 158 | 1.0× | ~6,300 |
| 200×56 | 11,200 | 247 | 544 | 2.2× | ~4,000 |
| 240×67 | 16,080 | 276 | 742 | **2.7×** | ~3,600 |
| 480×135 | 64,800 | 684 | 2,920 | **4.3×** | ~1,460 |
| 560×315 | 176,400 | 1,818 | — | — | ~550 |

Even at 560×315 (roughly the heavy end of pixel mode), the encode stage alone
could feed **550 fps**. 60 fps is nowhere near the ceiling.

### 3b. End-to-end streaming (live server, real 60 fps source)

```
asciline-server test60.mp4 --mode 6 --cols 240 --fps 60
node experiments/fps_count.js 8321 3
```

```
INIT: fps=60.0 mode=6 grid=240x68
frames in 3.0s: 178 → 59.3 fps
```

The Rust server streams **59.3 fps** on the exact 60 fps source that the Python
server would decimate to 30. `--fps 120` and beyond are equally supported.

### 3c. Terminal player (real-time check)

| run | wall time | video | verdict |
|---|---|---|---|
| 240 cols @60 fps | 10.21 s | 10 s / 600 frames | real-time at 60 fps |
| 480 cols @60 fps | 10.44 s | 10 s / 600 frames | real-time at 60 fps |
| 720p source, 560 cols @60 fps | 5.31 s | 5 s / 300 frames | real-time at 60 fps |

## 4. What you can push further

- **`--fps 120`+** on the server: the client render loop (`app.js`) paces from
  `INIT`'s fps field and the canvas `fillText` path handles it; browsers
  cap `requestAnimationFrame` at the display rate, so >120 is only useful for
  offline processing, where the compiler's `--fps` already decimates instead.
- **Larger grids**: at 480×135 the encode stage runs ~4× faster than Python, so
  the practical ceiling is the client's canvas draw, not the server.
- **`--quality high|balanced|low`** cuts wire bytes further via lossy temporal
  deltas (chars stay exact); bandwidth stops mattering before CPU does.
- **SIMD / multithreaded zlib**: `flate2` can switch to `zlib-ng` or `zstd`
  (`--features`), and the mapper is already row-parallel via rayon.
- **Hardware decode**: `ffmpeg -hwaccel auto` can be added to the decode args
  for machines with GPU decoders; the Rust side is not the bottleneck.

## 5. Caveats

- The browser client is the *original* JS player. Its jitter buffer and
  `requestAnimationFrame` loop were tuned for 24–30 fps; at 60 fps it still
  keeps up (verified by the frame counter above), but very high fps benefits
  from the "buffer" depth reports the server already uses for backpressure.
- `/audio` remains the master clock for A/V sync; frame pacing follows
  `frame_index / fps`, so a 60 fps stream stays in sync with audio the same way
  the 30 fps stream did.
