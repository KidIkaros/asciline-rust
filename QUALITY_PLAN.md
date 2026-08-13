# ASCILINE-Rust quality plan

A source-backed roadmap for improving the lossy `--profile` codec's
reconstruction quality and rate-distortion efficiency. Every item is mapped to
whether it is **encoder-only** (wire-compatible: the unchanged `web/codec.js`
decoder keeps working) or needs a **new tag / profile** (decoder changes on
both sides, plus differential-vector updates).

Current profile encoder: YUV 4:2:0 → 8×8 integer DCT → JPEG-style quant tables
scaled by a **global** QF → zigzag RLE + DC DPCM → zlib. Motion: luma-only,
±7 integer search (default) with optional half-pel refinement (tag 6), single
reference, skip = zero motion + small SSE. Baseline (cols=240, tag-4): Big
Buck Bunny QF=70 → 337,023 B at PSNR-Y 36.57 / SSIM-Y 0.9686; drone QF=70 →
240,238 B at 37.89 / 0.9470.

## Ranked levers

### 1. Adaptive quantization (AQ) — per-block quantizer — *tag-5, done*

- *"Adaptive Quant is claimed as the biggest performance improvement of x264
  optimization history"* — [x264 AQ analysis](https://huyunf.github.io/blogs/2017/12/06/x264_adaptive_quant/).
  x265's AQ mode *"dramatically reduces bitrate while having a minimal effect
  on perceived quality"* — [r/AV1](https://www.reddit.com/r/AV1/comments/dfelo5/bitrate_comparison_with_x265_aqmode/).
  AWS describes AQ as allowing the encoder to *"vary compression within a
  frame to improve subjective visual quality"* and reduce smear in high-detail
  areas — [AWS Elemental](https://docs.aws.amazon.com/elemental-server/latest/ug/vq-quantization.html).
- Our QF is global: flat sky is quantized as hard as the tree line. x264's AQ
  mode 1 (variance-based) and mode 2 (edge-aware) shift bits toward textured
  blocks; `aq-strength` sets how much.
- **Signal cost:** each block needs a small per-block QP delta (a few bits).
  Decoder must apply it → new tag.
- **Why first:** the highest measured lever in the industry; our content mix
  (drone foliage, cartoon fur) is exactly the "complex vs flat" case AQ fixes.

**Implemented (tag 5, `--aq 2|4`, opt-in; `--aq 0` = tag 4 bit-exact):** the
keyframe header signals `aq_levels` and each luma plane leads with a packed
per-block map (`log2(levels)` bits/block, MSB-first) selecting a quant-step
multiplier: 2 levels `[1.0, 0.5]`, 4 levels `[1.5, 1.0, 0.75, 0.5]`. Blocks
are classified from the luma variance vs the frame's median block variance
(deterministic, encoder-only): flat blocks stay at (or above) the base table,
detail blocks quantize finer. The decoder scales its table with identical
integer math `floor((m·num+2)/4)`, so no floats cross the wire; chroma keeps
the base tables. Both the Rust decoder and `web/codec.js` handle tag 5, and
`node experiments/check_profile_aq_vectors.js` proves bit-exact cross-
implementation decoding.

**Measured (QF=70, cols=240, `--fps` kept):**

| source | tag-4 size/PSNR-Y/SSIM-Y | `--aq 2` | Δ |
|---|---:|---:|---:|
| Big Buck Bunny | 337,023 B / 36.57 / 0.9686 | 466,221 B / 38.82 / 0.9766 | +2.25 dB, +0.008 SSIM at +38% size |
| Drone 60fps | 240,238 B / 37.89 / 0.9470 | 402,566 B / 39.72 / 0.9575 | +1.83 dB, +0.011 SSIM at +68% size |

At **equal quality** the story matches x264's claim: tag-4 needs QF≈84-85
(~500-516 KB on BBB) to reach AQ's ~38.8 dB, so AQ is ~10% smaller at the
same PSNR-Y. Without rate control, fixed-QF AQ is a quality↔size knob (the
fine half spends more; detail-dense footage like the drone spends more still);
a future tag-6 could add real rate control to make it rate-neutral.
`aq=2` beat `aq=4` on both size and quality in our measurements, so it is the
recommended setting.

### 2. Subpixel motion estimation (half-pel, then quarter-pel) — *tag-6, done (half-pel)*

- *"PSNR improvements … up to 1.5 dB"* from half- and quarter-pixel estimation
  vs integer — [ScienceDirect](https://www.sciencedirect.com/topics/engineering/subpixels);
  each finer level costs ~1.5× encode time — [Fora Soft](https://www.forasoft.com/learn/video-encoding/articles/inter-frame-coding-motion-estimation).
- Our search was integer-pel only. Real motion rarely lands on an integer
  grid, so the residual (and therefore the coefficient count) is larger than
  necessary.
- **Signal cost:** fractional MV components + interpolation on the decode side
  → new tag (6).

**Implemented (tag 6, now the compiler default; `--no-hpel` restores tag 5,
`--no-hpel --aq 0` the tag-4 bit-exact stream):** the motion search runs
its integer pass (±7) and then refines the best vector(s) to half-pel
precision with the interpolated SAD (the standard two-stage H.264 approach,
~9 extra candidates per block). The wire MVs are half-pel units (`i8`,
2× displacement + fractional bit) and the decoder interpolates the luma
reference bilinearly — `(A+B+1)>>1` / `(A+B+C+D+2)>>2`, integer, edge-clamped
— with identical math in Rust and `web/codec.js` (proven by `node
experiments/check_profile_hpel_vectors.js`, wired into CI). Because even
half-pel displacements are plain integer motion, tag 6 is a strict superset
of tag 5: the encoder can always fall back to an integer vector, so it is
never worse. AQ composes with half-pel (tag-6 keyframes always carry the
`aq_levels` byte, 0 = off). Note this is plain bilinear interpolation, not
H.264's 6-tap half-pel filter — a cheap and sufficient first step; the fuzz
corpus generator pins `--no-hpel --aq 0` so the committed seeds stay
byte-stable.

**Measured (QF=70, cols=240, native fps):**

| source | tag-5 (`--aq 2`) | tag-6 (default) | Δ |
|---|---:|---:|---:|
| Big Buck Bunny 30fps | 466,221 B / 38.82 / 0.9766 | 451,574 B / 39.54 / 0.9827 | **−3.1% size** AND +0.72 dB, +0.006 SSIM |
| Drone 60fps | 402,566 B / 39.72 / 0.9575 | 416,711 B / 40.29 / 0.9632 | +3.5% size, **+0.57 dB**, +0.006 SSIM |

The honest read: on 30 fps content (large per-frame motion) half-pel is a
strict win — smaller *and* sharper. On 60 fps content (small motion already
near an integer grid) it is ~quality for free: the file barely moves while
PSNR-Y climbs ~0.6 dB. This matches the research's +0.3–1.5 dB claim. Next
step, if pursued: quarter-pel and/or the H.264 6-tap filter.

### 3. In-loop deblocking filter — *tag-5*

- H.264's in-loop deblocker *"significantly reduce[s] the blocking artifacts
  and improve[s] visual quality and prediction"* — [IJECCE](https://ijecce.org/administrator/components/com_jresearch/files/publications/IJECCE_2902_Final.pdf);
  AV1's filter stack (deblock + CDEF) is why it *"gets soft, not blocky"* when
  squeezed — [Mozilla](https://hacks.mozilla.org/2018/06/av1-next-generation-video-the-constrained-directional-enhancement-filter/).
- We see 8×8 blocking exactly in the low-QF regime (QF≤40). A cheap
  boundary-strength deblock on the reconstructed reference costs no bits and
  improves both the displayed frame and every subsequent prediction.
- **Signal cost:** none (it is decoder-defined), but it changes the decoded
  output → new tag and updated differential vectors.

### 4. Better RDO (SATD) — *encoder-only, done*

- x264's motion/mode-decision ladder is SAD → **SATD** → RD, where SATD (sum
  of absolute Hadamard-transformed differences) is the cheap distortion proxy
  that predicts coefficient survival better than SAD/SSE — [x264 manpage](https://manpages.debian.org/testing/x264/x264.1).
- Implemented: the opt-in `--rdo-lambda` refinement now costs
  `SATD + λ·(3·nnz + 1)` (Hadamard distortion + pair-count rate). Measured on
  Big Buck Bunny QF=70: ~2.4% smaller at −0.06 dB PSNR-Y vs pure SAD — a
  size↔quality knob, not a pure quality win, so it stays off by default
  (reproduce with `experiments/measure_profile_ab.sh`).
- Per-QF skip-threshold scaling was prototyped and **rejected** on
  measurement: QF=90 grew the file +9% for +0.04 dB, QF=40 was neutral — the
  fixed SSE threshold was already reasonable.
- **Signal cost:** none. This only changes which bits the encoder emits; the
  stream format is untouched (verified by the differential harnesses).

### 5. Psychovisual tuning — *encoder-only, metric caveat*

- x264's `psy-rd` deliberately trades PSNR for perceived quality (keeps
  texture/grain); x265 *"always tunes for highest perceived visual quality"* by
  default — [x265 docs](https://x265.readthedocs.io/en/latest/presets.html).
- Our quality gate is PSNR/SSIM based, so perceptual tweaks (banding
  prevention, texture retention, sharper edges at low QF) will not show on the
  current metrics. Plan a visual A/B (or a perceptual metric like VMAF/SSIM
  window) before tuning these.

### 6. Entropy backend (zstd) — *new container*

- zlib→miniz_oxide already saved ~23% on adaptive. zstd (pure-Rust `ruzstd`)
  would likely add more on top, but it changes the wire format, so it belongs
  to a container v2 decision rather than a codec tag.

## Roadmap

| Phase | Work | Format impact | Expected |
|---|---|---|---|
| 0 (done) | Wider integer motion search (default ±7), opt-in RDO | none | BBB −2.7% size, +0.10 dB |
| 1 (done) | SATD-based RDO (skip-threshold scaling measured, rejected) | none | better RDO Pareto |
| 2 (done) | AQ: per-block QP, luma variance map (tag 5, `--aq 2` default, `--aq 4` / `--aq 0` opt) | tag-5 | +2.25 dB / +0.008 SSIM on BBB at +38% size; ~10% smaller at equal quality |
| 3 (done) | Half-pel motion: integer search + bilinear subpel refine (tag 6, now the default; `--no-hpel` opt-out) | tag-6 | BBB −3.1% size AND +0.72 dB; drone +0.57 dB at +3.5% size |
| 4 | In-loop deblock | tag-7 | QF≤40 perceptual win |
| 5 | Quarter-pel + 6-tap half-pel filter | tag-7 | another ~0.3–0.7 dB on motion |

## Measuring

- `experiments/measure_profile_ab.sh` — sweeps `--r-search`/`--rdo-lambda`,
  `--aq` and `--hpel` and reports size + PSNR-Y/SSIM-Y + wall time.
- `experiments/make_samples.sh` — regenerates the committed quality matrix
  (QF 40/70/90) and the cartoon/drone evidence; source hashes are pinned.
- Every tag change must update `web/codec.js`, the differential checkers
  (`check_profile_vectors.js` / `check_profile_aq_vectors.js` /
  `check_profile_hpel_vectors.js`), and the fuzz corpus (pinned to tag 4).
