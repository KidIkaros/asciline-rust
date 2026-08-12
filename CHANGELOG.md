# Changelog

All notable changes to the ASCILINE Rust port are documented here. The format
follows [Keep a Changelog](https://keepachangelog.com/); versioning follows
[SemVer](https://semver.org/).

## [0.1.0] — Unreleased

Initial release: a from-scratch Rust re-implementation of the ASCILINE backend
(<https://github.com/YusufB5/ASCILINE>). The wire protocol is byte-compatible
with the original, so the unchanged browser client in `web/` works as-is. See
[README.md](README.md) and [NOTICE](NOTICE) for details and attribution.

### Added

- `asciline-server` — drop-in replacement for `stream_server.py` (axum +
  WebSocket) speaking the exact original wire protocol: INIT handshake,
  adaptive codec frames (RAW/ZLIB/DELTA/RLE_FULL), `/audio` MP3 streaming,
  `/scrub` hover thumbnails, pause/seek/filter/reinit commands, backpressure
  frame-dropping, webcam mode, folders/playlists/looping.
- `asciline-player` — true-color terminal player (run-length-compressed
  `38;2;r;g;b` escapes, aspect-correct auto-fit, `.ascf` playback, webcam).
- `asciline-compile` — `.ascf` compiler (v2 `ASC2` container, audio
  extraction, fps decimation), matching `compiler.py`.
- Tag-4 lossy DCT compression profile (`--profile`, `--qf 1-100`): a faithful,
  bit-exact port of `codec.py`'s `ProfileEncoder` / `codec.js`
  `makeProfileDecoder` — YUV 4:2:0, luma motion search (±3 px), integer DCT,
  JPEG-style quant tables scaled by quality factor, zigzag RLE + DC DPCM,
  keyframes every 48 frames. Measured ~8× smaller than the lossless adaptive
  pixel profile.
- PSNR/SSIM quality reports at the end of every lossy compile (mean/min/max,
  plus the weakest frame index — typically a scene cut or motion burst):
  - `--profile`: luma + full-colour deviation of the DCT reconstruction.
  - adaptive `--pixel` with `--tolerance`/`--quantize`: deviation of the shown
    framebuffer vs the source video frame.
  - `--no-quality` disables them; lossless frames render as `∞`.
- Scene-cut keyframes for `--profile` (on by default in the compiler): when a
  frame's luma stops resembling the previous reconstruction (mean absolute
  difference > `SCENE_CUT_MAD`, a hard-cut threshold), the encoder re-encodes
  it as a fresh keyframe instead of a stale motion-predicted inter frame.
  Fixes the worst-frame quality collapse at cuts and usually shrinks the file
  (measured: 172 KB → 164 KB on a 4 s clip with a mid-stream cut, worst
  frame 35.4 → 36.7 dB). `--no-scene-cut` restores the original fixed cadence;
  the detector defaults off in the library so the encoder stays bit-exact
  with `codec.py` unless enabled.
- Bit-exact differential harnesses against both originals — the Python
  `codec.py` encoder and the shipped browser `codec.js` decoder — for the
  adaptive codec and the tag-4 profile, plus an end-to-end server test and
  map+encode benchmarks.

### Performance

- No 30 fps cap (the Python server hard-caps sources at 30 fps): `--fps 60`
  streams at 59.3 fps on a 60 fps source, with decode on a dedicated thread,
  map/encode in a tokio blocking pool, and resize/decimation inside ffmpeg's
  SIMD filters.
- Map+encode is 2.7–4.3× faster than the equivalent numpy+codec.py stage
  (~3,600 fps encode ceiling at 240×67). Details in [PERF_ANALYSIS.md](PERF_ANALYSIS.md).

### Fixed

- DELTA wire format: the encoder wrote interleaved `(index, cell)` entries
  while `codec.py`/`codec.js` decode a block layout (all indices, then all
  values) — every delta frame decoded as garbage and could panic the
  shown-frame tracking. Now emits the canonical block layout; the differential
  vectors include mostly-static cases so the delta path is exercised in both
  directions.
- Pixel-mode `--tolerance` semantics: tolerance now applies to every colour
  channel, matching `codec.py`'s C==3 branch (channel 0 was previously treated
  as exact, diverging from the original's lossy behavior).
- The ffmpeg stderr pipe is drained on every decode run (stall risk on long
  runs), and the decode-thread shutdown no longer has a join-hang window.
