# ASCILINE · Rust

A Rust port of [ASCILINE](https://github.com/YusufB5/ASCILINE) — a real-time ASCII
video rendering engine. It decodes video, maps pixels to ASCII characters (or
colored blocks), compresses every frame with an adaptive codec
(RAW/ZLIB/DELTA/RLE_FULL), and streams it over WebSocket — or renders it
directly in a true-color terminal.

The wire protocol is **byte-compatible with the original**, so the unchanged
browser client (`web/`) works as-is against the Rust server. The codec is
verified bit-exact against both the original Python encoder (`codec.py`) and the
original JS decoder (`codec.js`) — see [Validation](#validation).

## Why Rust?

Beyond the port itself, this answers the question *"can ASCILINE run faster than
30 fps?"* — **yes**:

- the Python server hard-caps every source to ≤30 fps (`MAX_FPS = 30` + frame
  skipping) and runs decode → map → encode → send serially under the GIL;
- the Rust server has **no fps cap** (sources play at native rate, `--fps N` for
  any target), pipelines decode (dedicated thread) against map/encode (tokio
  blocking pool) and the WebSocket send, does the resize/decimation inside
  ffmpeg's SIMD filters, and parallelizes mapping over rows with rayon.

Measured on a 60 fps source, `asciline-server --fps 60` streams **59.3 fps**,
and the map+encode stage alone runs at a **~3,600 fps ceiling** at 240×67
(2.7× faster than the equivalent numpy+codec.py work). Details and tables:
**[PERF_ANALYSIS.md](PERF_ANALYSIS.md)**.

## Requirements

- Rust 1.75+ (`cargo build --release`)
- `ffmpeg` + `ffprobe` on `PATH` (used for decode, audio, and thumbnails — no C
  libraries, no OpenCV)

## Build

```bash
cargo build --release
# binaries: target/release/asciline-server  asciline-player  asciline-compile
```

## 1. Streaming server (drop-in for `stream_server.py`)

```bash
asciline-server video.mp4 --cols 240
asciline-server --folder videos --cols 200 --loop
asciline-server --playlist playlist.json --mode 4
asciline-server --webcam --cols 240
```

Open http://localhost:8000 — the original player UI, served from `web/`, works
unchanged (`INIT:` handshake, binary frames, `/audio` MP3, `/scrub` hover
thumbnails, pause/seek/filter/reinit commands, backpressure frame-dropping).

```
--mode 1-6     color quality (1=B&W … 6=16M colors)      default 1
--pixel        colored-block mode
--cols N       grid columns (default 200 text / 450 pixel)
--rows N       grid rows (0 = auto from aspect ratio)
--fps N        target streaming FPS (default: source rate — no 30 fps cap)
--quality {lossless,high,balanced,low}   lossy temporal delta for the codec
--vol 0-5      audio volume (0 = no ffmpeg audio run at all)
--loop         loop the queue
--playlist/--folder/--webcam   sources
--host/--port  bind address
--debug        RAW vs WIRE bandwidth logging
```

## 2. Terminal player (`ascii_video_player2.py`)

```bash
asciline-player video.mp4 --cols 100          # true-color ANSI, source FPS
asciline-player --webcam --cols 100
asciline-player movie.ascf                    # plays compiled .ascf clips too
asciline-player video.mp4 -c 240 --fps 60     # 60 fps in the terminal
```

Zero-flicker rendering (hide cursor, disable wrap, `\x1b[H` + full-frame
rewrite), run-length-compressed `38;2;r;g;b` escapes, aspect-correct auto-fit
(`CHAR_RATIO 0.45`), palette + quantize options.

## 3. Compiler (`compiler.py`)

```bash
asciline-compile your_video.mp4 --cols 250 --pixel --quantize 2
asciline-compile your_video.mp4 --mode 6 --hard
asciline-compile your_video.mp4 --profile --qf 70   # smallest files (lossy DCT)
```

Writes the v2 `ASC2` container: 18-byte header + length-prefixed codec frames,
plus the extracted `*.mp3` audio track. Playable by `asciline-player`, the
original `static_player/`, and the Studio IDE. Options mirror the original:
`--cols/--rows/--mode/--pixel/--tolerance/--quantize/--hard/--out/--out-dir/--fps`.

### `--profile` — lossy DCT compression (tag 4)

The opt-in maximum-compression profile (a faithful port of `codec.py`'s
`ProfileEncoder`): frames become YUV 4:2:0, every 8×8 block is motion-compensated
(luma, ±3 integer search) and transformed with a deterministic **integer** DCT,
quantized with JPEG-style tables, and the non-zero zigzag coefficients are
run-length coded with DC DPCM — skipped blocks cost a single bit. It implies
`--pixel` and pads the grid up to multiples of 16.

```bash
asciline-compile clip.mp4 --profile --qf 40   # aggressive (smallest)
asciline-compile clip.mp4 --profile --qf 90   # near-lossless (largest)
```

`--qf` is the JPEG-style quality factor 1-100 (default 70). The decoded frames
are reconstructions — lossy, but decoded **bit-exactly by the original browser
decoder** (`codec.js` `makeProfileDecoder`) and by `asciline-player`. Measured
on a 720p clip at 320 cols: **0.34 MB vs 2.64 MB** for the lossless adaptive
pixel profile (~8× smaller).

Every `--profile` compile ends with a **quality report** — how far the lossy
reconstruction drifts from the source, averaged over all frames (mean, min,
max): **PSNR-Y** and **SSIM-Y** on the luma planes the DCT actually transforms
(standard codec metrics), plus **PSNR-RGB** on the full displayed pixels (which
also captures the 4:2:0 chroma subsampling and quant-table error):

```
[Quality] Lossy DCT reconstruction vs source (300 frames, QF=70):
[Quality]   PSNR-Y   39.47 dB   (min  37.57 / max  40.36)
[Quality]   SSIM-Y   0.9827      (min 0.9752 / max 0.9858)
[Quality]   PSNR-RGB 26.58 dB   (min  25.50 / max  28.82)
[Quality]   worst frame #60    PSNR-Y  35.37 dB  (SSIM 0.9105 / RGB 31.05 dB)
```

SSIM is the standard Wang et al. metric (11×11 Gaussian window, σ=1.5) in
`src/quality.rs`. Unsurprisingly, PSNR-RGB runs well below PSNR-Y — the chroma
planes are subsampled 2:2 and quantized with the much coarser JPEG chroma
tables, so color fidelity is the binding constraint, exactly as in the original.
Lossless frames (e.g. a fully static segment) show as `∞`. The `worst frame`
line names the weakest frame (lowest PSNR-Y, with its own SSIM/RGB) — in
practice this is a scene cut or a motion burst that lands between keyframes
(cuts exactly on a keyframe boundary re-encode cleanly and won't show up). The
report costs ~1-4% of compile time; `--no-quality` skips it for scripted batch
compiles.
Note: combining `--profile` with `--quantize` measures against the
pre-quantized source, so the color numbers then look better than they would
against the original video.

The **adaptive pixel** path prints the same report whenever `--tolerance`
(temporal colour drift) or `--quantize` (colour depth) makes it lossy —
compared against the **original video frame** (not the quantized input), so
both loss sources show up (the comparison is the codec's *shown* framebuffer,
what players actually display after delta skipping). In pixel mode `--tolerance`
applies to every colour channel, exactly like `codec.py`; the ASCII mode's
character plane stays exact but that path doesn't print a report:

```
[Quality] Adaptive pixel reconstruction vs source (120 frames, tolerance=8, quantize=2):
[Quality]   PSNR-Y   41.24 dB   (min  39.15 / max  43.36)
[Quality]   SSIM-Y   0.9821      (min 0.9704 / max 0.9921)
[Quality]   PSNR-RGB 40.43 dB   (min  38.32 / max  42.66)
[Quality]   worst frame #63    PSNR-Y  39.15 dB  (SSIM 0.9704 / RGB 38.32 dB)
```

A `∞` result is honest, not a bug: when every cell exceeds the tolerance (fast
motion) or full-frame encoding wins the race, the codec genuinely loses
nothing. Lossless compiles (`--pixel` with tolerance 0 / quantize 0) skip the
report entirely.

One nuance: the encoder's forward DCT uses plain f64 loops where `codec.py`
runs numpy/BLAS, so on rare rounding or tie boundaries it may select slightly
different coefficients or motion vectors than the Python encoder. Both streams
are valid and decode identically on every ASCILINE decoder — compatibility is
in the bit-exact decode direction, which is what the tests verify.

## Architecture

```
┌─────────────┐  pipe: raw RGB24   ┌──────────────┐  bounded    ┌──────────────────────┐
│ ffmpeg child│  (cols×rows×3/frame)│ decode thread│  channel   │ async task:          │
│ scale+fps   │──────────────────▶│ (FrameReader)│───────────▶│ map (rayon) + adaptive│
│ filters     │                    │              │            │ encode (blocking pool)│
└─────────────┘                    └──────────────┘            │ + pacing + WS send    │
                                                               └──────────────────────┘
```

- `src/video.rs` — ffprobe probing + ffmpeg `rawvideo` pipe decoding
- `src/mapper.rs` — pixel → `[char,R,G,B]` / `[B,G,R]` framebuffers (row-parallel)
- `src/codec.rs` — adaptive encoder + decoder (RAW/ZLIB/DELTA/RLE_FULL, keyframes)
- `src/profile.rs` — tag-4 lossy DCT profile encoder + decoder (motion search,
  integer DCT, zigzag RLE, DC DPCM) — used by `--profile` and `.ascf` playback
- `src/quality.rs` — PSNR / SSIM (Gaussian-window) metrics for the `--profile`
  quality report
- `src/filters.rs` — gray LUT (brightness/contrast/gamma/invert) + unsharp mask
- `src/protocol.rs` — INIT handshake, `.ascf` container
- `src/server.rs` — axum HTTP + WebSocket streaming pipeline
- `web/` — the original frontend, served as-is

## Validation

```bash
cargo test                    # 36 unit tests: codec + profile round-trips (incl. DELTA wire format + tolerance semantics), mapper, filters, protocol, quality

# Cross-implementation, bit-exact codec checks (adaptive):
python3 experiments/gen_python_vectors.py > experiments/vectors_python.bin
cargo test --test decode_python_vectors -- --ignored    # Rust decoder ↔ Python codec.py
node experiments/check_rust_vectors.js                  # Rust encoder ↔ shipped codec.js
# (the vector generator includes mostly-static cases so the DELTA wire path is
#  exercised in both directions — the original harness never emitted deltas)

# Cross-implementation, bit-exact codec checks (tag-4 lossy DCT profile):
python3 experiments/gen_profile_vectors.py > experiments/vectors_profile_py.bin
cargo test --test decode_profile_vectors -- --ignored  # Rust ProfileDecoder ↔ Python ProfileEncoder
cargo test --test roundtrip_profile -- --ignored       # Rust ProfileEncoder round-trip + vectors
node experiments/check_profile_vectors.js              # Rust ProfileEncoder ↔ shipped codec.js

cargo test --test e2e_server -- --nocapture             # boots the real server, WS INIT + frames
cargo test --release --test bench_encode -- --ignored --nocapture   # map+encode benchmark
python3 experiments/bench_python.py                     # same benchmark for the Python stage
node experiments/fps_count.js <port> 3                  # measure live streamed fps
```

## Not ported / differences

- `yt-dlp` / YouTube URL playback (local files, folders, playlists, webcams only)
- the Python server's interactive `/help` command loop

See [NOTICE](NOTICE) for frontend attribution and the license.
