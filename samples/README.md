# ASCILINE-Rust visual evidence

This directory contains visual evidence generated from the pinned Big Buck Bunny
excerpts in `source/`. The source is the Blender Foundation short film, licensed
under **CC BY 3.0**. See [`SOURCE.md`](SOURCE.md) for official links,
attribution, timestamps, and SHA-256 checksums.

## What to inspect first

1. **`evidence/cartoon_compare.gif`** — quick overview: SOURCE | lossless
   PIXEL | lossy PROFILE QF=70.
2. **`evidence/cartoon_source_profile_large.gif`** — larger source/profile
   comparison where each panel is readable.
3. **`evidence/cartoon_detail_compare.gif`** — center-detail crop for spotting
   ringing, chroma-edge changes, and texture differences.
4. **`evidence/cartoon_difference.gif`** — the third panel is a 4× amplified
   difference image. It is deliberately enhanced for inspection and is not
   representative of normal displayed output.
5. **`evidence/cartoon_profile.gif`** — profile-only playback at a readable
   size.

The source, PIXEL, and PROFILE panels use the same source frame timing. PIXEL
and PROFILE frames are decoded by `asciline-render`, not taken from an encoder
intermediate.

## Quality/compression trade-off

`big_buck_bunny_quality_matrix.md` contains the measured QF 40/70/90 matrix.
The quality factor is JPEG-style: higher QF generally produces larger files and
higher reconstruction quality. The metrics are computed by `asciline-compile`
against the original source frames:

- **PSNR-Y**: luma fidelity;
- **SSIM-Y**: structural luma similarity;
- **PSNR-RGB**: displayed-color fidelity, including 4:2:0 chroma effects.

The QF=70 profile is the headline comparison because it balances compression
and visible quality. Use the matrix rather than a single quality number when
comparing settings.

## Real 60 fps footage (drone flight)

The cartoon is only 30/60 fps, so a separate pinned **drone flight** clip
(720p60, CC BY 3.0) demonstrates that real-world content above 30 fps is
compiled and displayed at its native rate:

- `evidence/drone_compare.gif` / `.mp4` — SOURCE | lossless PIXEL | lossy
  PROFILE QF=70, all synchronized at **60 fps**;
- `evidence/drone_profile.gif` / `.mp4` — profile-only playback at 60 fps;
- `images/drone_source.png`, `images/drone_pixel.png`, `images/drone_profile.png`
  — the same frame across the three formats;
- `drone_profile.ascf`, `drone_pixel.ascf` — the playable compiled samples
  (compile uses `--fps 60`, since the offline compiler otherwise keeps the
  original's >30 fps decimation default);
- `drone_profile_quality.txt` — the PSNR/SSIM report for the 60 fps profile.

Every comparison panel carries its own `clip 60 fps` label so the drone output
cannot be mistaken for a downsampled-to-30 preview. The source is genuine
motion: `framemd5` reports ~482 unique hashes across 480 decoded frames.

## Throughput evidence

`evidence/throughput_120fps.mp4` and `.gif` are the visual proof of a genuinely
unique 120 fps source reaching the server wire. The deterministic `testsrc2`
source is checked with per-frame `framemd5`: every source frame has a distinct
hash. The companion files are:

- `evidence/throughput_matrix.md` — 60/120/240/480 fps results;
- `evidence/throughput_benchmark.log` — source count, unique-hash count,
  server-sent count, and timestamp-derived wire rate;
- `experiments/measure_throughput.sh` — the reproducible benchmark.

The measured run delivered every unique source frame at these rates:

| Target | Unique source frames | Server sent | Wire rate |
|---:|---:|---:|---:|
| 60 | 240 | 240 | 61.9 fps |
| 120 | 480 | 480 | 124.9 fps |
| 240 | 960 | 960 | 249.3 fps |
| 480 | 1,920 | 1,920 | 494.7 fps |

GIF/MP4 playback rate is not used as proof of throughput because browsers and
video players can throttle presentation. The benchmark proves measured rates
on this machine, not an unlimited performance guarantee on every machine.

## Display rate evidence

The [`terminal player display benchmark`](evidence/player_display_benchmark.md)
answers the question users actually care about: does the *display* run faster
than 30 fps? The terminal player (not display-refresh-bound like a browser)
renders a deterministic 4 s source in real time at 30, 60, and 120 fps:

| Source | Frames | Duration | Player wall | Real-time? |
|---|---:|---:|---:|---|
| 30 fps | 120 | 4 s | 4.40 s | yes |
| 60 fps | 240 | 4 s | 4.35 s | yes |
| 120 fps | 480 | 4 s | 4.30 s | yes |

The display rate equals the source rate; the software imposes no cap. The
compiler, by contrast, defaults to the original's >30 fps decimation for
offline `.ascf` production unless `--fps N` is given.

For end-to-end latency, use:

```sh
experiments/measure_latency.sh 120 6
```

That measurement uses per-frame server/client logs and reports the p95 frame-in
to display latency separately from wire throughput.

## Reproducing the artifacts

From the repository root:

```sh
experiments/make_samples.sh
```

The script verifies both pinned source hashes, compiles ASCII/PIXEL/PROFILE
outputs, generates the quality matrix and comparisons, and captures the live
wire artifact. The source-derived `.ascf`, PNG, GIF, and MP4 outputs are
reproducible; the live FPS log naturally varies with machine scheduling.
