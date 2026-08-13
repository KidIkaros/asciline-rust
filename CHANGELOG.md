# Changelog

All notable changes to the ASCILINE Rust port are documented here. The format
follows [Keep a Changelog](https://keepachangelog.com/); versioning follows
[SemVer](https://semver.org/).

## [Unreleased]

### Added

- Per-frame latency measurement: `asciline-server --latency-log` records
  `t_read/t_encode/t_send` per sent frame, `asciline-render --live
  --latency-log` records `t_recv/t_decode/t_render`, and
  `experiments/analyze_latency.py` joins the two logs and reports a stage
  breakdown with p50/p95/p99/max (`experiments/measure_latency.sh` runs the
  whole measurement). Measured at 120 fps (240-col pixel, 720 frames, 720/720
  joined): **p95 end-to-end latency 5.74 ms** — under the 8.33 ms per-frame
  budget at 120 fps. Both loggers flush per record so a killed process can't
  silently drop its tail (fixed a 640-vs-720 log mismatch this caught).
- `asciline-render --live`: a WebSocket capture mode that records the real
  wire frames `asciline-server` sends (INIT + every frame) and rasterizes
  them, turning a live stream into video — the basis for the 120 fps proof.
- Replaced the synthetic Mandelbrot evidence with a recognizable, pinned
  **Big Buck Bunny** cartoon excerpt (official Blender Foundation source,
  CC BY 3.0, attribution/checksums in `samples/SOURCE.md`). New evidence in
  `samples/evidence/` includes GitHub-inline GIFs for SOURCE | PIXEL |
  PROFILE, a profile-only playback, an explicitly amplified difference view,
  full-resolution MP4 versions, and a live cartoon wire capture. The 120 fps
  capture contains 480 frames at a measured 115.6 fps with a 120 fps target;
  its log is the authoritative transport evidence. `make_samples.sh` verifies
  the committed source hashes before regenerating all artifacts.
- `tests/load_server.rs` — a real-binary concurrent load test proving
  `--max-clients` under contention (both in-cap clients stream, `/healthz`
  reports the exact in-use count, the overflow connection is rejected, and a
  freed slot is reusable). Shared server-test helpers now live in
  `tests/common/mod.rs`.
- A `fuzz/` crate with four libFuzzer targets (`parse_ascf`, `adaptive_decode`,
  `profile_decode`, `ascf_stream`) mirroring the proptest harness's
  no-panic-on-any-input guarantee with a mutation engine, plus a committed
  seed corpus (`fuzz/corpus`, regenerable via `experiments/make_fuzz_corpus.sh`)
  carrying real tag-4 / adaptive wire records so the deep decoder paths are
  reachable. A nightly `cargo-fuzz` smoke job (build + 45 s per target) runs
  in CI, and a `corpus-check` job regenerates `fuzz/corpus` and fails on any
  drift — the generator is deterministic (the source clip is encoded
  losslessly with FFV1 so the seeds don't depend on the ffmpeg build's x264
  rate control), keeping CI fuzzing on the exact committed seeds.
- A `vectors-check` job does the same for the committed Python differential
  vectors (`vectors_python.bin`, `vectors_profile_py.bin`): the original
  `codec.py` is now vendored unmodified at `experiments/vendor/` (pinned
  commit, see NOTICE), so the generators run hermetically with only numpy and
  CI regenerates + compares the vectors byte-for-byte, failing on drift.
- `asciline-render` — a headless `.ascf` → PPM frame renderer (pixel mode =
  coloured blocks, ASCII mode = palette characters in a public-domain 8×8
  bitmap font) that turns compiled clips into images/video via ffmpeg.
- `samples/` — committed format evidence using an 8 s Big Buck Bunny cartoon
  excerpt compiled in ASCII, lossless pixel, and `--profile` modes, with
  decoded comparison images, GitHub-native GIFs, full MP4/difference views,
  pinned source excerpts, playable `.ascf` files, and the profile quality
  report. Regenerate with `experiments/make_samples.sh`.
- `DEPLOYMENT.md` and `deploy/` — production deployment kit: a systemd unit
  (`asciline.service`), Docker Compose (`.env.example`, healthcheck, looped
  folder mode), and a Caddy TLS reverse-proxy config. The `deploy/` tree and
  `DEPLOYMENT.md` are shipped inside every release tarball.
- `--token` now also reads `ASCILINE_TOKEN` from the environment (clap `env`),
  so systemd `EnvironmentFile=` and compose `environment:` can inject the
  secret without shell expansion.
- `experiments/compare_zlib_backends.sh` — reproducible zlib-backend
  measurement.

### Changed

- Default zlib backend is now flate2's `miniz_oxide` instead of `zlib-rs`.
  Measured on real clips at the same compression level: miniz_oxide produces
  ~23% smaller `.ascf` files on the adaptive pixel path (2,875,260 B vs
  3,536,211 B on an 8 s 640x360 clip at 240 cols), the profile path is a
  wash, and zlib-rs's speed edge was marginal/reversed — size wins for a
  codec container. Both remain pure Rust; `experiments/compare_zlib_backends.sh`
  can re-measure either backend.

### Added

- GitHub Actions CI (`fmt`/`clippy -D warnings`/`cargo test`, the committed
  bit-exact differential harnesses, the e2e server test, the
  `--quality-threshold` gate smoke test, `cargo-audit`) and a tag-driven
  release workflow that packages the three binaries + `web/` + checksums into
  a GitHub release.
- Dockerfile (multi-stage, non-root runtime user, `/healthz` health check) and
  `install.sh` (cargo build + install to `~/.local/bin`).
- `RELEASE-NOTES.md` for the v0.1.0 release.

### Security

- Server hardening: `--max-clients N` bounds concurrent WebSocket streams
  (each spawns an ffmpeg child + decode thread), a global ffmpeg-process
  semaphore bounds `/audio` transcodes and scrub-sprite builds, optional
  `--token` auth guard on `/ws`, `/audio` and `/scrub*`, a `/healthz`
  endpoint, and graceful shutdown on SIGINT.
- Decoder hardening: zlib decompression is capped (64 MiB) against
  decompression bombs; the RLE_FULL path bounds-checks every run (a truncated
  run used to panic with an out-of-bounds slice); the tag-4 profile decoder
  rejects keyframes declaring grids over 4 M pixels (a crafted header
  previously requested a multi-GB allocation); `asciline-player` caps
  per-record lengths before allocating.
- Grid clamps: `--cols`/`--rows` are clamped (playlist overrides included) so
  a malformed entry can't ask ffmpeg for a gigantic grid.

### Fixed

- `TAG_RLE_FULL` decode could panic on a truncated run (`body[off+2..off+2+cell]`
  without a bounds check).
- Profile decoder panicked on crafted keyframe grids (found by the new
  libFuzzer harness, `fuzz/artifacts` in the nightly CI job): a 1×N grid has
  empty chroma planes and an odd width/height leaves the last luma row/column
  without chroma — both made `yuv_to_bgr` index out of bounds. The keyframe
  validation now requires even w,h >= 2 (real grids are multiples of 16), and
  a regression test covers every rejected shape plus a legal 2×2 grid.

## [0.1.0] — 2026-08-12

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
  - `--no-quality` disables them (and now skips the SSIM *computation*
    entirely, not just the printed lines — the blur was ~60% of compile
    instructions when enabled); lossless frames render as `∞`.
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
- Parallel profile encoder (rayon): block-independent motion search, DCT,
  quantization and reconstruction run in a parallel phase; the serial phase
  (skip mask, motion vectors, DC-DPCM'd zigzag stream) assembles the exact
  same bytes as before — verified bit-identical across thread counts (1 vs 8
  in a test, 1 vs 12 empirically) and against the original vectors. ~31 s →
  ~16 s on a 450-frame 720p clip at 480 cols.
- Rayon-parallel SSIM blur (the report's dominant cost): each output pixel is
  an independent dot product, so the blur parallelizes over rows with
  bit-identical results; mirror-index LUTs replace the per-pixel `rem_euclid`
  in the hot loop (~4x faster single-threaded too). All five windowed moments
  are batched into a single parallel pass (LUTs built once per SSIM).
- Parallel RGB↔YUV conversions (per-pixel independent) and a `zlib-rs`
  backend for `flate2` (Cloudflare's memory-safe pure-Rust zlib, no C deps) —
  the remaining serial tail. `--no-quality` compiles dropped 7.1 s → 4.4 s on
  a 450-frame 720p clip at 480 cols; report-on compiles ~31 s → ~17 s.
- `--quality-threshold N`: CI quality gate — `asciline-compile` exits non-zero
  when the mean PSNR-Y of the lossy reconstruction falls below N dB (requires
  the quality report and a lossy compile).

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
