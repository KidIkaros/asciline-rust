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
| zlib via CPython + re-allocated numpy buffers per frame | `flate2` (pure-Rust miniz_oxide backend, no C deps) + reused scratch buffers; keyframe every 48 frames, delta-first adaptive codec by default. |
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

### 3d. End-to-end latency at 120 fps (frame in → wire out → decode → display)

How *usable* is the frame rate? Measured with the built-in latency logging:
`asciline-server --latency-log` records `t_read/t_encode/t_send` per frame,
`asciline-render --live --latency-log` records `t_recv/t_decode/t_render`, and
`experiments/analyze_latency.py` joins the two logs by frame index
(`experiments/measure_latency.sh` runs the whole thing: 120 fps h264 source,
240-column pixel mode = the highest-detail live config).

```
live capture: 720 frames in 6.13s -> 117.4 fps (480x270 per frame)
joined frames: 720, server-only: 0, client-only: 0

per-frame latency by stage (ms):
  encode (map+codec)             n= 720  p50=   0.57  p95=   2.94  p99=   4.93  max=  16.00
  wire (send->recv)              n= 720  p50=   0.11  p95=   0.68  p99=   5.59  max=   7.83
  decode                         n= 720  p50=   0.01  p95=   0.05  p99=   0.07  max=   0.13
  render (raster+write)          n= 720  p50=   1.36  p95=   2.06  p99=   2.92  max=   6.27
  total (frame-in->display)      n= 720  p50=   2.44  p95=   5.74  p99=   9.10  max=  17.87
```

**p95 end-to-end is 5.74 ms — comfortably under the 8.33 ms per-frame budget
at 120 fps**, and every stage is sub-millisecond at p50. The pipeline stays
paced by the source, not by its own latency; the p99/max spikes are encode-pool
and scheduler jitter, not systematic cost. The two logs must match frame-for-
frame for the join to be meaningful — both loggers flush per record so a
killed process can't silently drop its tail (a 640-vs-720 mismatch this
measurement originally caught and fixed).

## 4. The `--profile` compiler is parallel too

The lossy DCT profile compiler (`asciline-compile --profile`) was also
single-threaded at first and is now parallel end-to-end, with the same
bit-exactness guarantees as everything else:

- **Blocks** (rayon): every 8×8 block's motion search, prediction, DCT,
  quantization, skip decision and reconstruction are independent — computed in
  a parallel phase. The serial phase then assembles the skip mask, motion
  vectors and the DC-DPCM'd zigzag stream in raster order (the DPCM predictor
  chain is inherently serial).
- **YUV conversions** (rayon): the RGB→YUV luma/chroma and the YUV→BGR
  reconstruction loops are per-pixel independent, so they parallelize too.
- **SSIM blur** (rayon + LUTs): the quality report's Gaussian blur is the
  single biggest per-frame cost when the report is enabled (~60% of compile
  instructions), so it parallelizes over pixels; mirror-index LUTs replace the
  per-pixel `rem_euclid` (~4× faster even single-threaded), and all five
  windowed moments (E[x], E[y], E[x²], E[y²], E[xy]) are batched into one
  parallel pass.
- **zlib**: `flate2` runs on the default pure-Rust `miniz_oxide` backend.
  Both miniz_oxide and zlib-rs (Cloudflare's memory-safe pure-Rust zlib) were
  measured on real clips (`experiments/compare_zlib_backends.sh`), and
  miniz_oxide won on the metric that matters for a codec container — size:

  | mode (8 s 640×360 clip, 240 cols) | miniz_oxide | zlib-rs |
  |---|---|---|
  | adaptive pixel, tol=8 | **2,875,260 B** | 3,536,211 B (+23%) |
  | profile QF=70 | 494,506 B | 493,160 B (≈equal) |

  Speed was a wash (zlib isn't the dominant cost of either encode path), so
  the default stays miniz_oxide. The wire bytes differ between backends but
  are equally valid zlib streams — decode compatibility, which is what the
  differential harnesses verify, is unaffected.

Every one of these is verified **bit-identical to the serial build**: a
thread-count determinism test (1 vs 8 threads), the cross-implementation
vectors against `codec.py`/`codec.js` (decode side), and end-to-end `cmp`
identity of the `.ascf` output across thread counts and with/without the
quality report.

Measured on a 15 s 720p clip (450 frames, 480 cols, 12 cores):

| build | time |
|---|---|
| original single-threaded compiler | ~31 s |
| parallel, quality report on | ~17 s (2.0×) |
| parallel, `--no-quality` (SSIM skipped) | ~4.4 s (7×) |

`--no-quality` used to skip only the *printed* report while still computing the
SSIM — the dominant per-frame cost. It now skips the computation entirely
(`ProfileEncoder::collect_stats`), which is where the extra 4× comes from.

## 5. What you can push further

- **`--fps 120`+** on the server: the client render loop (`app.js`) paces from
  `INIT`'s fps field and the canvas `fillText` path handles it; browsers
  cap `requestAnimationFrame` at the display rate, so >120 is only useful for
  offline processing, where the compiler's `--fps` already decimates instead.
- **Larger grids**: at 480×135 the encode stage runs ~4× faster than Python, so
  the practical ceiling is the client's canvas draw, not the server.
- **`--quality high|balanced|low`** cuts wire bytes further via lossy temporal
  deltas (chars stay exact); bandwidth stops mattering before CPU does.
- **SIMD / multithreaded zlib**: the serial zlib pass is a real share of
  `--profile` compiles; a parallel deflate backend (zlib-rs or zlib-ng) would
  shrink it, but the measured zlib-rs ratio cost (~23% larger adaptive files)
  is why the default stays miniz_oxide — re-run
  `experiments/compare_zlib_backends.sh` on your content before switching. The
  mapper is already row-parallel via rayon.
- **Hardware decode**: `ffmpeg -hwaccel auto` can be added to the decode args
  for machines with GPU decoders; the Rust side is not the bottleneck.

## 6. Caveats

- The browser client is the *original* JS player. Its jitter buffer and
  `requestAnimationFrame` loop were tuned for 24–30 fps; at 60 fps it still
  keeps up (verified by the frame counter above), but very high fps benefits
  from the "buffer" depth reports the server already uses for backpressure.
- `/audio` remains the master clock for A/V sync; frame pacing follows
  `frame_index / fps`, so a 60 fps stream stays in sync with audio the same way
  the 30 fps stream did.
