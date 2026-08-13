# ASCILINE-Rust quality plan

A source-backed roadmap for improving the lossy `--profile` (tag-4) codec's
reconstruction quality and rate-distortion efficiency. Every item is mapped to
whether it is **encoder-only** (wire-compatible: the unchanged `web/codec.js`
decoder keeps working) or needs a **new tag / profile** (decoder changes on
both sides, plus differential-vector updates).

Current profile encoder: YUV 4:2:0 → 8×8 integer DCT → JPEG-style quant tables
scaled by a **global** QF → zigzag RLE + DC DPCM → zlib. Motion: luma-only,
integer-pel, ±7 search, single reference, skip = zero motion + small SSE.
Baselines (cols=240): Big Buck Bunny QF=70 → 337,023 B at PSNR-Y 36.57 /
SSIM-Y 0.9686; drone QF=70 → 240,238 B at 37.89 / 0.9470.

## Ranked levers

### 1. Adaptive quantization (AQ) — per-block quantizer — *tag-5*

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

### 2. Subpixel motion estimation (half-pel, then quarter-pel) — *tag-5*

- *"PSNR improvements … up to 1.5 dB"* from half- and quarter-pixel estimation
  vs integer — [ScienceDirect](https://www.sciencedirect.com/topics/engineering/subpixels);
  each finer level costs ~1.5× encode time — [Fora Soft](https://www.forasoft.com/learn/video-encoding/articles/inter-frame-coding-motion-estimation).
- Our search is integer-pel only. Real motion rarely lands on an integer grid,
  so the residual (and therefore the coefficient count) is larger than
  necessary. Half-pixel with a bilinear or H.264-style 6-tap interpolation is
  the standard first step.
- **Signal cost:** fractional MV components + interpolation on the decode side
  → new tag. Encoder-only subpel (search at subpel, round to integer for the
  wire) is a cheap partial win that stays wire-compatible.

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
| 2 | AQ: per-block QP (mode 1 variance, then edge-aware) | tag-5 | biggest quality/bitrate lever |
| 3 | Half-pel motion (encoder-side rounding first) | tag-5 | up to ~0.5–1.5 dB on motion |
| 4 | In-loop deblock | tag-5 | QF≤40 perceptual win |

## Measuring

- `experiments/measure_profile_ab.sh` — sweeps `--r-search`/`--rdo-lambda`
  and reports size + PSNR-Y/SSIM-Y + wall time.
- `experiments/make_samples.sh` — regenerates the committed quality matrix
  (QF 40/70/90) and the cartoon/drone evidence; source hashes are pinned.
- Every tag-5 change must update `web/codec.js`, the differential generators
  (`gen_profile_vectors.py` / `gen_python_vectors.py`), and the fuzz corpus.
