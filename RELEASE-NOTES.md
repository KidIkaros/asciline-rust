# ASCILINE Rust — v0.1.0

A from-scratch Rust re-implementation of [ASCILINE](https://github.com/YusufB5/ASCILINE):
real-time ASCII video rendering with no 30 fps cap. The wire protocol is
byte-compatible with the original, so the unchanged browser client works as-is.

## Binaries

- **asciline-server** — drop-in replacement for `stream_server.py` (axum +
  WebSocket): INIT handshake, adaptive codec frames, `/audio` MP3, `/scrub`
  thumbnails, pause/seek/filter/reinit, backpressure, webcam/folders/playlists.
- **asciline-player** — true-color terminal player (also plays `.ascf` clips).
- **asciline-compile** — `.ascf` compiler with the tag-4 lossy DCT `--profile`
  (up to ~8× smaller files), PSNR/SSIM quality reports, and a
  `--quality-threshold` CI gate.

## Highlights

- **No 30 fps cap**: decode/map/encode pipeline streams 60 fps sources at
  native rate (~59.3 fps measured); map+encode alone runs at a ~3,600 fps
  ceiling at 240×67. See `PERF_ANALYSIS.md`.
- **Bit-exact** against both originals — verified by committed differential
  vector harnesses against `codec.py` (Python) and the shipped `codec.js`
  (browser) in both directions, plus an end-to-end server test.
- **Parallel**: rayon-parallel profile encoder, SSIM blur, and RGB↔YUV; the
  zlib-rs (Cloudflare) pure-Rust deflate backend.

## Requirements

- Linux x86_64
- `ffmpeg` + `ffprobe` on PATH (decode, audio, thumbnails)

## Install

```sh
./install.sh            # builds with cargo, installs to ~/.local/bin
```

## Quick start

```sh
asciline-server video.mp4 --cols 240     # open http://localhost:8000
asciline-compile video.mp4 --profile --qf 70 --out clip
asciline-player clip.ascf
```

Docker: `docker build -t asciline-server . && docker run -p 8000:8000 -v $PWD/videos:/srv/asciline/videos asciline-server`

See `README.md` for the full flag reference and validation instructions.
