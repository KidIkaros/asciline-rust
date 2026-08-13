# ASCILINE-Rust — internal roadmap

Private iteration plan. **Not linked from the README and not intended for
publishing** — it is the working list of what to tackle next, what we have
already measured (so we do not re-litigate it), and the traps that cost time.

For the deep-dive on *codec* quality levers (with the literature trail), see
`QUALITY_PLAN.md` — this file is the project-level view: what to build, in
what order, and how to keep the published evidence honest.

---

## Where we are (2026-08)

`--profile` codec tags in the wire format, oldest → newest:

| tag | feature | default? | measured (QF=70, cols=240) |
|---|---|---|---|
| 4 | original integer-DCT profile (codec.py bit-exact) | `--no-hpel --aq 0` | BBB 337 KB @ 36.57 dB |
| 5 | AQ: per-block quant-scale map (luma variance) | opt-in `--aq 2|4` | BBB 466 KB @ 38.82 dB (+2.25 dB) |
| 6 | half-pel motion (bilinear interpolation) | `--no-qpel` | BBB 451 KB @ 39.54 dB (−3.1% size AND +0.72 dB) |
| 7 | quarter-pel motion (H.264 6-tap + bilinear) | **default** | BBB 472 KB @ 39.52 dB (≈neutral); drone 467 KB @ 40.37 dB (+0.08 dB) |

Quality gate: PSNR-Y ≥ threshold via `--quality-threshold`; the compile tool
always prints the mean/min/max PSNR-Y + SSIM-Y + PSNR-RGB report.

---

## Top priorities (short horizon)

1. **Rate control (`--target-size`)** — the single most valuable feature left.
   Problem today: fixed-QF is a quality↔size roulette (AQ +0.38×, qpel +0.12×
   on the drone at QF=70); the README has to caveat every headline number.
   Design (wire-compatible — QF is already signaled per keyframe):
   - Measure bytes produced since the previous keyframe (or a preview pass).
   - Per-keyframe feedback: `qf_next = qf_cur * sqrt(target / achieved)`,
     clamped, plus an overall scale computed from the first keyframe.
   - Interaction with AQ: rate control adjusts the base QF; AQ continues to
     shift bits within the frame (that is its whole point).
   - Scene-cut keyframes inherit the current QF (no re-tuning mid-cut).
   - Acceptance: hit a target size within ±5% on BBB and the drone; document
     the quality at that size instead of a size at that quality.
   - Trap: the fuzz corpus and sample evidence must be pinned to
     `--no-hpel --aq 0` (already done for the corpus) so the new default
     doesn't churn the committed bytes.

2. **Visual A/B evidence for quarter-pel.** The metric says qpel is a wash on
   BBB (+0.08 dB drone, +12% size). Before we keep claiming it as the default,
   produce a side-by-side GIF (tag 6 vs tag 7 at QF 70, drone pan section) so
   *we* can see whether the smoother pans are perceptible. If not, consider
   flipping the default back to tag 6 (smaller at equal quality on BBB).
   This is cheap and decides the most defensible default.

3. **Decode-side performance check for tag 7.** Sub-pel interpolation runs on
   the decode path (player + `web/codec.js`). Confirm the 6-tap filter keeps
   display ≥60 fps on the drone sample — if it bites, add a small
   half-pel-only fast path or precompute the 6-tap plane once per frame.

## Medium horizon

4. **AQ model upgrades** (tag 5 already extensible — the map signals the
   level count, so a 3-level mode is wire-compatible if we add `aq_levels=3`):
   - Edge-aware strength (x264 AQ mode 2) instead of variance-only; or
     variance thresholds tuned per content class (cartoon vs photo).
   - Chroma AQ: today chroma always uses the base tables — the PSNR-RGB gap
     vs PSNR-Y (e.g. 32.96 vs 39.52 dB on BBB QF=70) says color is the
     binding constraint. A cheap chroma AQ map (2 levels) could close more
     of that gap than another luma refinement.

5. **RDO ladder tuning** — measured earlier: SATD-RDO was ~2.4% smaller at
   −0.06 dB (a size↔quality knob, stays off by default). Revisit only if a
   quality-focused use case (not size) demands it.

6. **Perceptual metrics.** PSNR/SSIM gate is blind to banding/smearing at low
   QF. If we start tuning psychovisually (texture retention at QF 40, banding
   prevention), plan a VMAF/SSIM-window side-channel in the quality report —
   without it, perceptual changes are unmeasurable in CI.

7. **Container v2 (`.ascf`)**: zstd (ruzstd) instead of zlib behind a
   container version bump (adaptive codec already saved ~23% moving to
   miniz_oxide); optional seek index written into the file at compile time
   (the scan-on-open index works, but an on-disk index makes seeking O(1) and
   shrinks the open cost for long clips).

## Deliberately deferred / rejected (with reasons)

- **In-loop deblocking (tag 8)** — prototyped, measured, rejected: −2 to −6 dB
  PSNR-Y and 2-3× size at every filter strength. This codec's DC-DPCM +
  dead-zone quantization does not produce H.264-style boundary steps, so the
  loop filter only adds smoothing error the encoder fights. If revisited:
  require an edge-detection guard that only filters true quantizer steps, and
  an H.264-style strength ladder. Low priority.
- **Skip-threshold scaling per QF** — prototyped, rejected: QF=90 grew +9%
  for +0.04 dB; QF=40 neutral. The fixed SSE threshold is fine.
- **`--qpel-bilinear`** (encoder-side bilinear instead of 6-tap) — measured
  ≈+0.05 dB at ≈1% size, kept as a flag but not worth a wire change.
- **More entropy backends / image formats** (WebP/AVIF per frame) — out of
  scope; the wire format is the product.

## Cross-cutting: keep the published evidence honest

Every time a **default changes** (AQ, half-pel, qpel, next: rate control), the
following must move in the same commit or the README and the repo disagree
(this has cost us twice):

1. Regenerate: `experiments/make_samples.sh` (`.ascf`, quality matrix,
   GIFs/MP4s with burned-in labels) and
   `experiments/measure_sample_speed.sh` (compile FPS table).
2. Update the hardcoded numbers in `README.md` (headline, matrix, drone
   section), `CHANGELOG.md` [Unreleased], and `QUALITY_PLAN.md` tables.
3. Re-run: differential checkers (`check_profile_vectors.js` /
   `check_profile_aq_vectors.js` / `check_profile_hpel_vectors.js` /
   `check_profile_qpel_vectors.js`), fuzz corpus drift (`make_fuzz_corpus.sh`
   — pinned to tag 4), samples-check job, quality gate.
4. Verify the pushed bytes match GitHub (content-length on raw URLs) — CI
   `samples-check` catches local drift, not stale remote files.

## How to work items

- One feature per commit; measure before and after on the two pinned clips
  (BBB 30fps, drone 60fps — hashes pinned in `make_samples.sh`).
- Any wire-format change = new tag + Rust decoder + `web/codec.js` + a new
  differential checker in CI + fuzz target coverage for the new parse path
  (the tag-6 divide-by-zero was caught by the nightly fuzz smoke — keep that
  loop).
- The quality report is the arbiter: mean/min/max PSNR-Y + SSIM-Y + PSNR-RGB
  and the worst frame (scene cuts show up there).
