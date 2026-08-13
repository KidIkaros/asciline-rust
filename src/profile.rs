//! Opt-in lossy DCT profile (tag 4, pixel mode) — a faithful port of the
//! `ProfileEncoder` / `makeProfileDecoder` pair from `codec.py` / `codec.js`.
//!
//! This is the maximum-compression `.ascf` profile: frames are converted to
//! YUV 4:2:0, every 8×8 block is motion-compensated (luma, integer search,
//! optionally rate-distortion optimized),
//! transformed with a deterministic **integer** DCT basis (`round(F*64)`),
//! quantized with JPEG-style tables scaled by a quality factor, and the
//! non-zero zigzag coefficients are run-length coded with DC DPCM. Skipped
//! blocks (zero motion + low energy) cost a single bit.
//!
//! The whole chain is integer and deterministic, so a stream encoded here
//! decodes bit-identically in the shipped browser decoder (`web/codec.js`) and
//! vice-versa. Decoded frames are reconstructed lossily — this profile trades
//! exact pixels for the smallest `.ascf` files.
//!
//! Index loops over the 8×8 blocks/coefficients are the clearest expression of
//! this linear algebra; silence the iterator rewrites they'd otherwise trigger.
#![allow(clippy::needless_range_loop, clippy::explicit_counter_loop)]

//! Wire format (per frame message, after the shared `[u32 BE index][u8 tag]`):
//! ```text
//! payload = zlib( body )
//! body:
//!   [u8 ftype]                       0 = keyframe, 1 = inter
//!   keyframe only: [u8 QF][u16 BE cols][u16 BE rows]
//!     tags 5/6 only: [+ u8 aq_levels]  2 or 4 quant-scale levels
//!   then 3 planes (Y full, Cb/Cr half), each:
//!     tags 5/6 luma only: [ceil(nb*bits/8) bytes AQ map, MSB-first bit-packed,
//!                          bits = log2(aq_levels) per block]
//!     inter only: [ceil(nb/8) bytes skip mask, MSB-first]
//!     per coded block, raster order:
//!       luma inter: [i8 dx][i8 dy]     tag 6: half-pel units (2× pel + frac)
//!       [u8 n_pairs][ (u8 run)(i16 LE value) × n_pairs ]
//! ```
//!
//! Tags 5/6 differ from tag 4 only in the marked places. Tag 5 (adaptive
//! quantization): the keyframe signals how many per-block quant-scale levels
//! the stream uses, and each luma plane carries the packed map selecting one
//! of them per block. Tag 6 (half-pixel motion, an opt-in prototype): the tag
//! itself signals that inter motion vectors are half-pel units and both the
//! encoder and decoder interpolate the luma reference bilinearly
//! (`(A+B+1)>>1` / `(A+B+C+D+2)>>2`, integer math, edge-clamped — identical
//! on both sides, so no floats cross the wire). Even half-pel displacements
//! are plain integer motion, so tag 6 is a strict superset of tag 5.

use anyhow::{bail, Result};
use rayon::prelude::*;

use crate::codec::{zlib_compress, zlib_decompress, TAG_PROFILE, TAG_PROFILE_AQ, TAG_PROFILE_HPEL};
use crate::quality::{psnr, ssim, QualityStats};

/// Forced keyframe interval (same as the adaptive codec's).
const KEY: u32 = 48;
/// Default motion search radius (integer pixels) — codec.py uses ±3.
pub const DEFAULT_R_SEARCH: i32 = 3;
/// Rate-distortion motion refinement: how many lowest-SAD candidates receive a
/// full DCT + quantize + rate evaluation before the vector is chosen.
const RDO_K: usize = 8;
/// Default dead-zone: coefficients with |t| below this round to zero.
const DEFAULT_DZ: f64 = 0.75;
/// Default skip threshold: inter blocks with SSE below this and zero motion
/// are skipped even if they have non-zero coefficients.
const DEFAULT_SKIP_T: f64 = 256.0;
/// Half-pixel displacement when the best integer motion vector is (mx, my):
/// the 9 candidates (2mx+fx, 2my+fy), fx,fy ∈ {-1,0,1}, are scored by the
/// interpolated SAD and the best is returned in half-pel units.
const HPEL_REFINE: i32 = 1;

/// Adaptive-quantization (tag 5) quant-step multipliers over a denominator of
/// 4, indexed by map value. Index 0 is the COARSEST (flat regions), the last
/// is the FINEST (detail) — mirroring x264 AQ's "spend bits where the eye
/// looks". num = 4 is the identity scale (the tag-4 table).
// Measured on Big Buck Bunny + drone at QF=70: these tables gave the best
// quality-per-byte (2 levels: flat stays at the base table, detail halves its
// step; 4 levels: a log-spaced ladder about the identity). Coarser-than-base
// tables (e.g. [8,2]) shrank the file but cost more PSNR/SSIM than they
// saved — without rate control, fixed-QF AQ is a quality↔size knob.
const AQ_NUMS_2: [i64; 2] = [4, 2];
const AQ_NUMS_4: [i64; 4] = [6, 4, 3, 2];

/// The quant-step multiplier (over 4) for an AQ map value.
#[inline]
fn aq_num(levels: u8, idx: u8) -> i64 {
    match levels {
        2 => AQ_NUMS_2[idx as usize],
        4 => AQ_NUMS_4[idx as usize],
        _ => 4, // identity
    }
}

/// Scale a quant table by `num/4`, rounding to the nearest integer. Both sides
/// of the wire compute this with identical integer math: `floor((m*num+2)/4)`.
/// `m >= 1` and `num >= 2` keep the numerator positive, so floor division is
/// unambiguous across Rust (`div_euclid`) and JS (`Math.floor`).
#[inline]
fn scale_qm(qm: &[i64; 64], num: i64) -> [i64; 64] {
    let mut out = [0i64; 64];
    for i in 0..64 {
        out[i] = ((qm[i] * num + 2) / 4).max(1);
    }
    out
}

/// Per-block luma variance (E[x²] − E[x]² over the 8×8 block) — the AQ
/// activity signal. Pure function of the plane, so it is deterministic.
#[inline]
fn block_var(y: &[u8], w: usize, by: usize, bx: usize) -> f64 {
    let mut s: i64 = 0;
    let mut s2: i64 = 0;
    for r in 0..8usize {
        let row = (by * 8 + r) * w + bx * 8;
        for x in 0..8usize {
            let v = y[row + x] as i64;
            s += v;
            s2 += v * v;
        }
    }
    let n = 64.0f64;
    let mean = s as f64 / n;
    let mean_sq = s2 as f64 / n;
    (mean_sq - mean * mean).max(0.0)
}

/// Map each luma block to an AQ quant-scale index from its variance, relative
/// to the frame's median block variance `m` (adaptive, deterministic). High
/// variance = detail → finer quant; low variance = flat → coarser quant.
fn aq_indices(y: &[u8], w: usize, h: usize, levels: u8) -> Vec<u8> {
    let nbx = w / 8;
    let nby = h / 8;
    let nb = nbx * nby;
    let vars: Vec<f64> = (0..nb)
        .map(|bi| block_var(y, w, bi / nbx, bi % nbx))
        .collect();
    let mut sorted = vars.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let m = sorted[nb / 2].max(1.0);
    vars.into_iter()
        .map(|v| match levels {
            2 => {
                if v < m {
                    0
                } else {
                    1
                }
            }
            _ => {
                // 4 levels: quartile-ish log-spaced thresholds about the median.
                if v < m / 8.0 {
                    0
                } else if v < m / 2.0 {
                    1
                } else if v < m * 2.0 {
                    2
                } else {
                    3
                }
            }
        })
        .collect()
}

/// Pack per-block AQ indices into `bits`-per-block bytes, MSB-first — the
/// same bit order as the skip mask (1 bit/block), extended to 2 bits for
/// 4-level maps. Identical packing on the JS side (`web/codec.js`).
fn pack_aq_map(indices: &[u8], bits: u8) -> Vec<u8> {
    let nbytes = indices.len().div_ceil(8 / bits as usize);
    let mut out = vec![0u8; nbytes];
    for (bi, &idx) in indices.iter().enumerate() {
        let bit = bi * bits as usize;
        let byte = bit >> 3;
        // MSB-first within the byte: a `bits`-wide field ends at bit 7, so
        // shift = 8 - bits - (bit & 7) (bits=1 → 7-(bit&7), the skip-mask order).
        let shift = 8 - bits as usize - (bit & 7);
        out[byte] |= (idx & ((1 << bits) - 1)) << shift;
    }
    out
}

/// Unpack the AQ map byte-wise and return the per-block quant-scale numerator
/// (`num`, denominator 4). Mirrors `pack_aq_map` exactly.
fn unpack_aq_map(map: &[u8], nb: usize, levels: u8) -> Vec<i64> {
    let bits = (levels as u32).ilog2() as usize;
    let mask = (1 << bits) - 1;
    (0..nb)
        .map(|bi| {
            let bit = bi * bits;
            let shift = 8 - bits - (bit & 7);
            let idx = (map[bit >> 3] >> shift) & mask as u8;
            aq_num(levels, idx)
        })
        .collect()
}
/// Scene-cut keyframe threshold: when the mean absolute luma deviation between
/// the source frame and the previous reconstruction exceeds this (0-255 scale),
/// motion prediction is useless and the frame is encoded as a fresh keyframe.
/// The compiler enables this; `ProfileEncoder::new` keeps the original
/// fixed-schedule behavior (0.0 disables detection) for bit-exact parity.
pub const SCENE_CUT_MAD: f64 = 40.0;
/// Cap on a decoded profile grid in pixels. The wire keyframe header declares
/// u16 dims, so a crafted frame could claim 65535² (a multi-GB allocation);
/// any real `.ascf` grid is far below 2048×2048 (the compiler pads to 16).
const MAX_GRID_PIXELS: u64 = 1 << 22; // 4,194,304 px

// ────────────────────────────────────────────────────────────────────────────
// Deterministic constants (bit-exact with codec.py / codec.js)
// ────────────────────────────────────────────────────────────────────────────

/// Integer DCT basis `round(F*64)`, row-major 8×8.
const MI: [[i64; 8]; 8] = [
    [23, 23, 23, 23, 23, 23, 23, 23],
    [31, 27, 18, 6, -6, -18, -27, -31],
    [30, 12, -12, -30, -30, -12, 12, 30],
    [27, -6, -31, -18, 18, 31, 6, -27],
    [23, -23, -23, 23, 23, -23, -23, 23],
    [18, -31, 6, 27, -27, -6, 31, -18],
    [12, -30, 30, -12, -12, 30, -30, 12],
    [6, -18, 27, -31, 31, -27, 18, -6],
];

/// Standard JPEG zigzag order (spatial index of zigzag position k).
const ZZ: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Luma quantization table (JPEG style).
const QL_BASE: [i64; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];

/// Chroma quantization table (JPEG style).
const QC_BASE: [i64; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99,
    47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];

/// The orthonormal float DCT-II basis `F` (matches `codec.py` `_F` exactly).
static DCT_BASIS: std::sync::OnceLock<[[f64; 8]; 8]> = std::sync::OnceLock::new();

fn dct_basis() -> &'static [[f64; 8]; 8] {
    DCT_BASIS.get_or_init(|| {
        let mut f = [[0.0f64; 8]; 8];
        for k in 0..8usize {
            let s: f64 = if k == 0 {
                (1.0f64 / 8.0).sqrt()
            } else {
                (2.0f64 / 8.0).sqrt()
            };
            for n in 0..8usize {
                f[k][n] = s * ((2 * n + 1) as f64 * k as f64 * std::f64::consts::PI / 16.0).cos();
            }
        }
        f
    })
}

/// Quality-factor scaled quant tables: `clip(floor((m*S+50)/100), 1, 255)`.
fn qtables(qf: i64) -> ([i64; 64], [i64; 64]) {
    let s = if qf < 50 {
        5000.0 / qf as f64
    } else {
        200.0 - 2.0 * qf as f64
    };
    let scl = |m: i64| -> i64 {
        let v = ((m as f64 * s + 50.0) / 100.0).floor() as i64;
        v.clamp(1, 255)
    };
    let mut ql = [0i64; 64];
    let mut qc = [0i64; 64];
    for i in 0..64 {
        ql[i] = scl(QL_BASE[i]);
        qc[i] = scl(QC_BASE[i]);
    }
    (ql, qc)
}

/// `numpy.round` semantics: round-half-to-even (banker's rounding).
#[inline]
fn np_round(x: f64) -> f64 {
    if (x - x.trunc()).abs() == 0.5 {
        let f = x.floor();
        if f % 2.0 == 0.0 {
            f
        } else {
            f + 1.0
        }
    } else {
        x.round()
    }
}

/// Integer IDCT: `floor_div(MIT @ C @ MI + 2048, 4096)`.
fn idct_int(c: &[[i64; 8]; 8]) -> [[i64; 8]; 8] {
    // tmp[u][x] = sum_v c[u][v] * MI[v][x]
    let mut tmp = [[0i64; 8]; 8];
    for u in 0..8 {
        for x in 0..8 {
            let mut s = 0i64;
            for v in 0..8 {
                s += c[u][v] * MI[v][x];
            }
            tmp[u][x] = s;
        }
    }
    // o[y][x] = sum_u MI[u][y] * tmp[u][x]
    let mut o = [[0i64; 8]; 8];
    for y in 0..8 {
        for x in 0..8 {
            let mut s = 0i64;
            for u in 0..8 {
                s += MI[u][y] * tmp[u][x];
            }
            o[y][x] = (s + 2048).div_euclid(4096);
        }
    }
    o
}

// ────────────────────────────────────────────────────────────────────────────
// Encoder
// ────────────────────────────────────────────────────────────────────────────

/// One planar buffer (w×h bytes).
struct Plane {
    w: usize,
    h: usize,
    buf: Vec<u8>,
}

impl Plane {
    fn new(w: usize, h: usize) -> Plane {
        Plane {
            w,
            h,
            buf: vec![0u8; w * h],
        }
    }
}

/// Stateful lossy DCT profile encoder. Frames are `w*h*3` BGR bytes.
pub struct ProfileEncoder {
    pub w: usize,
    pub h: usize,
    pub qf: u8,
    pub dz: f64,
    pub skip_t: f64,
    /// Adaptive quantization (tag 5): per-block quant-scale levels for the
    /// luma plane, 0 = off (tag 4, bit-exact original). `2` and `4` select
    /// two- and four-level maps derived from each block's luma variance —
    /// flat blocks quantize coarser, detail blocks finer (x264-style AQ).
    pub aq_levels: u8,
    /// Motion search radius (integer pixels, ±N). Larger radii help smooth
    /// pans/zooms (e.g. drone footage); ±3 matches codec.py.
    pub r_search: i32,
    /// Rate-distortion λ for motion-vector selection. `0.0` = pure SAD (the
    /// original behavior); `> 0` enables SAD-prefilter + RDO refinement
    /// (SATD distortion + coefficient-count rate), which trades a little
    /// encode time for fewer bits at the same distortion.
    pub rdo_lambda: f64,
    /// Half-pixel motion compensation (tag 6, opt-in prototype): the motion
    /// search refines the best integer vector to half-pel precision and the
    /// decoder interpolates the luma reference bilinearly. A strict superset
    /// of integer motion (even half-pel displacements are plain integer
    /// shifts), so it never hurts — but requires a decoder that knows tag 6.
    pub hpel: bool,
    pub level: u32,
    /// Scene-cut detection: mean absolute luma deviation vs the previous
    /// reconstruction above which an inter frame is re-encoded as a keyframe.
    /// 0.0 = disabled (the original fixed every-48-frames schedule).
    pub scene_cut_mad: f64,
    /// Collect per-frame PSNR/SSIM statistics (the quality report). This is
    /// the dominant cost of an enabled report (~60% of compile time), so
    /// `asciline-compile --no-quality` turns it off to truly skip it — the
    /// wire output is unaffected either way.
    pub collect_stats: bool,
    prev: Option<[Plane; 3]>,
    n: u32,
    /// Per-frame PSNR/SSIM of the lossy reconstruction vs the source.
    stats: QualityStats,
}

impl ProfileEncoder {
    pub fn new(w: usize, h: usize, qf: u8) -> ProfileEncoder {
        ProfileEncoder::new_with(w, h, qf, DEFAULT_DZ, DEFAULT_SKIP_T, 6)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with(
        w: usize,
        h: usize,
        qf: u8,
        dz: f64,
        skip_t: f64,
        level: u32,
    ) -> ProfileEncoder {
        assert!(
            w.is_multiple_of(16) && h.is_multiple_of(16),
            "profile requires cols and rows multiples of 16 (compiler pads)"
        );
        ProfileEncoder {
            w,
            h,
            qf,
            dz,
            skip_t,
            aq_levels: 0, // off by default: tag 4, bit-exact original behavior
            r_search: DEFAULT_R_SEARCH,
            rdo_lambda: 0.0,
            hpel: false, // off by default: tag 4/5, bit-exact original behavior
            level,
            scene_cut_mad: 0.0, // disabled by default: bit-exact original behavior
            collect_stats: true,
            prev: None,
            n: 0,
            stats: QualityStats::new(),
        }
    }

    pub fn reset(&mut self) {
        self.prev = None;
        self.n = 0;
        self.stats = QualityStats::new();
    }

    /// Running reconstruction-quality statistics (PSNR/SSIM vs the source).
    pub fn stats(&self) -> &QualityStats {
        &self.stats
    }

    /// Encode one BGR frame → `(wire message, shown BGR bytes)`.
    pub fn encode(&mut self, frame_bgr: &[u8]) -> (Vec<u8>, Vec<u8>) {
        assert_eq!(
            frame_bgr.len(),
            self.w * self.h * 3,
            "profile frame size mismatch"
        );

        // ── RGB → YUV 4:2:0 (float32 math, bit-exact with codec.py) ──
        let w = self.w;
        let h = self.h;
        let cw = w / 2;
        let ch = h / 2;

        let mut y = vec![0u8; w * h];
        let mut cb = vec![0u8; cw * ch];
        let mut cr = vec![0u8; cw * ch];

        // Luma: one f32 left-fold per pixel — parallel over pixels (each output
        // depends only on its own input, so results are bit-identical to serial).
        y.par_iter_mut().enumerate().for_each(|(i, out)| {
            let b = frame_bgr[i * 3] as f32;
            let g = frame_bgr[i * 3 + 1] as f32;
            let r = frame_bgr[i * 3 + 2] as f32;
            let v = (0.299 * r + 0.587 * g) + 0.114 * b; // left fold in f32
            *out = (v.clamp(0.0, 255.0)) as u8;
        });

        // Chroma: each 2×2 block is independent — parallel over blocks. Each
        // pixel computes its own (cb, cr) pair, collected in order (bit-exact
        // vs the serial loop).
        let chroma: Vec<(u8, u8)> = (0..ch * cw)
            .into_par_iter()
            .map(|k| {
                let cy = k / cw;
                let cx = k % cw;
                // 2×2 block (C-order: y0x0, y0x1, y1x0, y1x1). numpy's mean over
                // the 2×2 uses pairwise summation: ((a+b)+(c+d))/4 — match it
                // bit-for-bit with the same association in f32.
                let mut acc_cb = [0.0f32; 4];
                let mut acc_cr = [0.0f32; 4];
                for (i, (dy, dx)) in [(0usize, 0usize), (0, 1), (1, 0), (1, 1)]
                    .iter()
                    .enumerate()
                {
                    let idx = ((cy * 2 + dy) * w + (cx * 2 + dx)) * 3;
                    let b = frame_bgr[idx] as f32;
                    let g = frame_bgr[idx + 1] as f32;
                    let r = frame_bgr[idx + 2] as f32;
                    acc_cb[i] = (128.0 - 0.168736 * r) - 0.331264 * g + 0.5 * b;
                    acc_cr[i] = (128.0 + 0.5 * r) - 0.418688 * g - 0.081312 * b;
                }
                let sum_cb = (acc_cb[0] + acc_cb[1]) + (acc_cb[2] + acc_cb[3]);
                let sum_cr = (acc_cr[0] + acc_cr[1]) + (acc_cr[2] + acc_cr[3]);
                (
                    ((sum_cb / 4.0).clamp(0.0, 255.0)) as u8,
                    ((sum_cr / 4.0).clamp(0.0, 255.0)) as u8,
                )
            })
            .collect();
        for (k, (c, r)) in chroma.into_iter().enumerate() {
            cb[k] = c;
            cr[k] = r;
        }

        // ── keyframe vs inter ──
        let mut ftype: u8 = if self.prev.is_none() || self.n.is_multiple_of(KEY) {
            0
        } else {
            1
        };
        // Scene-cut detection: if the luma barely resembles the previous
        // reconstruction, motion prediction is worthless — re-encode the frame
        // as a fresh keyframe (self-describing, so every decoder handles it).
        if ftype == 1 && self.scene_cut_mad > 0.0 {
            let prev_y = &self.prev.as_ref().unwrap()[0].buf;
            if mad(&y, prev_y) > self.scene_cut_mad {
                ftype = 0;
            }
        }

        let (ql, qc) = qtables(self.qf as i64);
        let planes = [y, cb, cr];

        // Tag 5 (AQ) prep: per-block quant-scale map for the luma plane, packed
        // MSB-first. Deterministic — a pure function of the luma plane.
        let aq: (u8, Option<Vec<u8>>) = if self.aq_levels > 0 {
            let idxs = aq_indices(&planes[0], w, h, self.aq_levels);
            let bits = (self.aq_levels as u32).ilog2() as u8;
            (self.aq_levels, Some(pack_aq_map(&idxs, bits)))
        } else {
            (0, None)
        };

        let mut payload: Vec<u8> = Vec::new();
        payload.push(ftype);
        if ftype == 0 {
            // keyframe self-describes: [QF][cols u16][rows u16][aq_levels?]
            // Tag 5 pushes the byte only when AQ is on; tag 6 ALWAYS pushes
            // it (0 when off) so the decoder never has to guess whether the
            // half-pel header carries it.
            payload.push(self.qf);
            payload.extend_from_slice(&(w as u16).to_be_bytes());
            payload.extend_from_slice(&(h as u16).to_be_bytes());
            if aq.0 > 0 || self.hpel {
                payload.push(aq.0);
            }
        }

        let mut recons: [Plane; 3] = [Plane::new(0, 0), Plane::new(0, 0), Plane::new(0, 0)];
        for pi in 0..3usize {
            let (pw, ph) = if pi == 0 { (w, h) } else { (cw, ch) };
            let qm = if pi == 0 { &ql } else { &qc };
            let prev_plane = if ftype == 1 {
                Some(&self.prev.as_ref().unwrap()[pi])
            } else {
                None
            };
            // The AQ map is luma-only (chroma always uses the base tables).
            let (al, am) = if pi == 0 {
                (aq.0, aq.1.as_deref())
            } else {
                (0, None)
            };
            // Half-pel motion applies to the luma plane only (the same planes
            // that carry motion vectors) — chroma stays co-located.
            let hp = self.hpel && pi == 0;
            let (body, rec) = enc_plane(
                &planes[pi],
                prev_plane,
                pw,
                ph,
                ftype,
                pi == 0,
                hp,
                qm,
                self.dz,
                self.skip_t,
                self.r_search,
                self.rdo_lambda,
                al,
                am,
            );
            payload.extend_from_slice(&body);
            recons[pi] = rec;
        }

        let z = zlib_compress(&payload, self.level);
        let mut msg = Vec::with_capacity(5 + z.len());
        msg.extend_from_slice(&self.n.to_be_bytes());
        // Tag 6 (half-pel) subsumes tag 5 (its keyframe header layout is
        // identical, including the optional aq_levels byte); when both hpel
        // and AQ are off we emit the plain tag 4 for full compat.
        msg.push(if self.hpel {
            TAG_PROFILE_HPEL
        } else if aq.0 > 0 {
            TAG_PROFILE_AQ
        } else {
            TAG_PROFILE
        });
        msg.extend_from_slice(&z);

        self.prev = Some(recons);

        let shown = yuv_to_bgr(
            &self.prev.as_ref().unwrap()[0].buf,
            &self.prev.as_ref().unwrap()[1].buf,
            &self.prev.as_ref().unwrap()[2].buf,
            w,
            h,
        );

        // Quality report: luma source vs luma reconstruction (the signal the
        // DCT actually transforms), plus the full displayed BGR vs the source.
        // `self.n` is still THIS frame's index here (incremented below).
        // Skipped entirely when `collect_stats` is off (`--no-quality`): the
        // SSIM is the single biggest per-frame cost, so this is what makes
        // the flag actually skip the work, not just the printed report.
        if self.collect_stats {
            let src_y = &planes[0];
            let rec_y = &self.prev.as_ref().unwrap()[0].buf;
            self.stats.push(
                self.n as u64,
                psnr(src_y, rec_y),
                ssim(src_y, rec_y, w, h),
                psnr(frame_bgr, &shown),
            );
        }
        self.n += 1;

        (msg, shown)
    }
}

/// Mean absolute deviation of one byte plane vs another (scene-cut signal).
fn mad(a: &[u8], b: &[u8]) -> f64 {
    let mut sum: u64 = 0;
    for (&x, &y) in a.iter().zip(b) {
        sum += (x as i64 - y as i64).unsigned_abs();
    }
    sum as f64 / a.len() as f64
}

/// Sum of absolute differences for one 8×8 block against an edge-clamped
/// previous frame shifted by (dx, dy). Sequential i32 accumulation.
#[inline]
#[allow(clippy::too_many_arguments)]
fn block_sad(
    cur: &[u8],
    prev: &[u8],
    w: usize,
    h: usize,
    by: usize,
    bx: usize,
    dx: i32,
    dy: i32,
) -> i32 {
    let w32 = w as i32;
    let h32 = h as i32;
    let mut sad = 0i32;
    for y in 0..8usize {
        let sy = ((by * 8 + y) as i32 + dy).clamp(0, h32 - 1) as usize;
        let row = sy * w;
        for x in 0..8usize {
            let sx = ((bx * 8 + x) as i32 + dx).clamp(0, w32 - 1) as usize;
            let cv = cur[(by * 8 + y) * w + bx * 8 + x] as i32;
            let pv = prev[row + sx] as i32;
            sad += (cv - pv).abs();
        }
    }
    sad
}

/// Per-block encode result (phase 1): motion vector, skip flag, quantized
/// coefficients (for DC DPCM + zigzag RLE) and the reconstructed 8×8 block.
#[derive(Clone, Copy)]
struct BlockEnc {
    skip: bool,
    mvx: i32,
    mvy: i32,
    cq: [[i64; 8]; 8],
    rec: [[u8; 8]; 8],
}

/// Prediction block for a candidate motion vector: the edge-clamped previous
/// frame shifted by (dx, dy) — integer, or half-pel bilinear when `hpel` — or
/// a flat 128 predictor on keyframes / no-MV planes. Identical math to the
/// inline decoder version (and codec.js).
#[inline]
#[allow(clippy::too_many_arguments)]
fn build_pred(
    prev: Option<&Plane>,
    w: usize,
    h: usize,
    by: usize,
    bx: usize,
    ftype: u8,
    use_mv: bool,
    hpel: bool,
    mvx: i32,
    mvy: i32,
) -> [[f64; 8]; 8] {
    let mut pred = [[0f64; 8]; 8];
    if ftype == 0 {
        for y in 0..8 {
            for x in 0..8 {
                pred[y][x] = 128.0;
            }
        }
    } else {
        let prev_buf = &prev.as_ref().unwrap().buf;
        for y in 0..8 {
            for x in 0..8 {
                let v = if use_mv {
                    if hpel {
                        hpel_sample(prev_buf, w, h, bx * 8 + x, by * 8 + y, mvx, mvy)
                    } else {
                        let w32 = w as i32;
                        let h32 = h as i32;
                        let sx = ((bx * 8 + x) as i32 + mvx).clamp(0, w32 - 1) as usize;
                        let sy = ((by * 8 + y) as i32 + mvy).clamp(0, h32 - 1) as usize;
                        prev_buf[sy * w + sx]
                    }
                } else {
                    prev_buf[(by * 8 + y) * w + bx * 8 + x]
                };
                pred[y][x] = v as f64;
            }
        }
    }
    pred
}

/// Half-pel bilinear sample of `prev` at output pixel (px, py) shifted by the
/// half-pel displacement (hdx, hdy). Integer part is `hdx>>1` (arithmetic
/// shift = floor for negatives), fractional bit is `hdx&1`; the four integer
/// neighbors are edge-clamped to the plane. Identical integer math on the
/// encoder and decoder sides (and in codec.js), so prediction is bit-exact:
/// ```text
/// fx,fy = 0,0 -> A
///         1,0 -> (A+B+1)>>1      0,1 -> (A+C+1)>>1
///         1,1 -> (A+B+C+D+2)>>2
/// ```
/// This is plain bilinear interpolation (not H.264's 6-tap half-pel filter);
/// cheap and enough for the prototype.
#[inline]
fn hpel_sample(prev: &[u8], w: usize, h: usize, px: usize, py: usize, hdx: i32, hdy: i32) -> u8 {
    let w32 = w as i32;
    let h32 = h as i32;
    let ix = ((px as i32) + (hdx >> 1)).clamp(0, w32 - 1) as usize;
    let iy = ((py as i32) + (hdy >> 1)).clamp(0, h32 - 1) as usize;
    let fx = hdx & 1;
    let fy = hdy & 1;
    if fx == 0 && fy == 0 {
        return prev[iy * w + ix];
    }
    let ix1 = ((ix as i32) + 1).min(w32 - 1) as usize;
    let iy1 = ((iy as i32) + 1).min(h32 - 1) as usize;
    let a = prev[iy * w + ix] as i32;
    let b = prev[iy * w + ix1] as i32;
    let c = prev[iy1 * w + ix] as i32;
    let d = prev[iy1 * w + ix1] as i32;
    let v = match (fx, fy) {
        (1, 0) => (a + b + 1) >> 1,
        (0, 1) => (a + c + 1) >> 1,
        (1, 1) => (a + b + c + d + 2) >> 2,
        _ => unreachable!(),
    };
    v.clamp(0, 255) as u8
}

/// SAD of one 8×8 block against the half-pel-shifted reference (tag 6).
#[inline]
#[allow(clippy::too_many_arguments)]
fn block_sad_hpel(
    cur: &[u8],
    prev: &[u8],
    w: usize,
    h: usize,
    by: usize,
    bx: usize,
    hdx: i32,
    hdy: i32,
) -> i32 {
    let mut sad = 0i32;
    for y in 0..8usize {
        for x in 0..8usize {
            let cv = cur[(by * 8 + y) * w + bx * 8 + x] as i32;
            let pv = hpel_sample(prev, w, h, bx * 8 + x, by * 8 + y, hdx, hdy) as i32;
            sad += (cv - pv).abs();
        }
    }
    sad
}

/// Refine the best integer motion vector (mx, my) to half-pel precision:
/// score the 9 half-pel displacements around it (2mx+fx, 2my+fy) by
/// interpolated SAD and return the best, in half-pel units. The integer
/// displacement itself (2mx, 2my) is included, so refinement never picks a
/// worse vector than the integer search did.
#[inline]
#[allow(clippy::too_many_arguments)]
fn refine_hpel(
    cur: &[u8],
    prev: &[u8],
    w: usize,
    h: usize,
    by: usize,
    bx: usize,
    mx: i32,
    my: i32,
) -> (i32, i32) {
    let mut best = (2 * mx, 2 * my);
    let mut best_sad = block_sad_hpel(cur, prev, w, h, by, bx, best.0, best.1);
    for fy in -HPEL_REFINE..=HPEL_REFINE {
        for fx in -HPEL_REFINE..=HPEL_REFINE {
            if fx == 0 && fy == 0 {
                continue;
            }
            let (hdx, hdy) = (2 * mx + fx, 2 * my + fy);
            let s = block_sad_hpel(cur, prev, w, h, by, bx, hdx, hdy);
            if s < best_sad {
                best_sad = s;
                best = (hdx, hdy);
            }
        }
    }
    best
}

/// Residual + forward DCT + dead-zone quantize for one block against a given
/// predictor. Returns the SSE (distortion), the quantized coefficient matrix
/// (for DC DPCM + zigzag RLE) and the count of non-zero coefficients — a
/// first-order rate estimate used by rate-distortion vector selection.
#[inline]
fn dct_quant(
    cur_b: &[[f64; 8]; 8],
    pred: &[[f64; 8]; 8],
    qm: &[i64; 64],
    dz: f64,
    f: &[[f64; 8]; 8],
) -> (f64, [[i64; 8]; 8], u32) {
    let mut resid = [[0f64; 8]; 8];
    let mut sse = 0f64;
    for y in 0..8 {
        for x in 0..8 {
            let d = cur_b[y][x] - pred[y][x];
            resid[y][x] = d;
            sse += d * d;
        }
    }

    // forward DCT: tmp = F @ resid; t = tmp @ Fᵀ
    let mut tmp = [[0f64; 8]; 8];
    for k in 0..8 {
        for n in 0..8 {
            let mut s = 0.0;
            for m in 0..8 {
                s += f[k][m] * resid[m][n];
            }
            tmp[k][n] = s;
        }
    }
    let mut t = [[0f64; 8]; 8];
    for k in 0..8 {
        for v in 0..8 {
            let mut s = 0.0;
            for m in 0..8 {
                s += tmp[k][m] * f[v][m];
            }
            t[k][v] = s;
        }
    }

    // quantize with dead-zone
    let mut cq = [[0i64; 8]; 8];
    let mut nnz = 0u32;
    for y in 0..8 {
        for x in 0..8 {
            let tv = t[y][x] / qm[y * 8 + x] as f64;
            let q = if tv.abs() < dz { 0.0 } else { np_round(tv) };
            let qi = q as i64;
            cq[y][x] = qi;
            if qi != 0 {
                nnz += 1;
            }
        }
    }
    (sse, cq, nnz)
}

/// Sum of absolute Hadamard-transformed differences (SATD) for one 8×8 block
/// against a predictor. The 8×8 Hadamard is separable with ±1 coefficients
/// (no multiplies); SATD predicts coefficient survival under quantization
/// better than SAD/SSE, which is why codecs use it in the motion/mode-decision
/// ladder (x264: SAD → SATD → RD). The transform scale is absorbed by the
/// RDO lambda.
#[inline]
fn satd8(cur_b: &[[f64; 8]; 8], pred: &[[f64; 8]; 8]) -> f64 {
    // Hadamard H8 (Sylvester construction), entries ±1.
    const H: [[i64; 8]; 8] = [
        [1, 1, 1, 1, 1, 1, 1, 1],
        [1, -1, 1, -1, 1, -1, 1, -1],
        [1, 1, -1, -1, 1, 1, -1, -1],
        [1, -1, -1, 1, 1, -1, -1, 1],
        [1, 1, 1, 1, -1, -1, -1, -1],
        [1, -1, 1, -1, -1, 1, -1, 1],
        [1, 1, -1, -1, -1, -1, 1, 1],
        [1, -1, -1, 1, -1, 1, 1, -1],
    ];
    let mut r = [[0f64; 8]; 8];
    for y in 0..8 {
        for x in 0..8 {
            r[y][x] = cur_b[y][x] - pred[y][x];
        }
    }
    // tmp = H @ r
    let mut tmp = [[0f64; 8]; 8];
    for k in 0..8 {
        for n in 0..8 {
            let mut s = 0.0;
            for m in 0..8 {
                s += H[k][m] as f64 * r[m][n];
            }
            tmp[k][n] = s;
        }
    }
    // t = tmp @ Hᵀ; sum |t| / 64
    let mut sum = 0.0;
    for k in 0..8 {
        for v in 0..8 {
            let mut s = 0.0;
            for m in 0..8 {
                s += tmp[k][m] * H[v][m] as f64;
            }
            sum += s.abs();
        }
    }
    sum / 64.0
}

/// Compute one block's motion vector, quantized coefficients, skip decision
/// and reconstruction. A pure function of the block's inputs (current plane,
/// previous reconstruction, quant table) — every block is independent, so
/// this runs in parallel; results are bit-identical to the serial loop.
#[allow(clippy::too_many_arguments)]
fn enc_block(
    cur: &[u8],
    prev: Option<&Plane>,
    w: usize,
    h: usize,
    by: usize,
    bx: usize,
    ftype: u8,
    use_mv: bool,
    hpel: bool,
    qm: &[i64; 64],
    dz: f64,
    skip_t: f64,
    f: &[[f64; 8]; 8],
    r_search: i32,
    rdo_lambda: f64,
    aq_num: i64,
) -> BlockEnc {
    // AQ (tag 5): scale this block's quant table by aq_num/4. num = 4 is the
    // identity (tag-4 table), so the tag-4 path takes the zero-cost branch.
    let qm_b: [i64; 64];
    let qm_ref: &[i64; 64] = if aq_num == 4 {
        qm
    } else {
        qm_b = scale_qm(qm, aq_num);
        &qm_b
    };

    // current block as f64
    let mut cur_b = [[0f64; 8]; 8];
    for y in 0..8 {
        for x in 0..8 {
            cur_b[y][x] = cur[(by * 8 + y) * w + bx * 8 + x] as f64;
        }
    }

    // motion search (luma inter only): scan dy outer, dx inner, (0,0)
    // preferred on ties. When RDO is enabled, rank every candidate by SAD
    // first, keep the lowest K, then refine with a full DCT + quantize +
    // rate cost so the vector minimizes distortion + λ·bits, not just SAD.
    // Tag 6 (half-pel) refines the integer winner(s) to half-pel precision
    // with the interpolated SAD, so smooth motion lands on the true minimum.
    let mut mvx: i32 = 0;
    let mut mvy: i32 = 0;
    if ftype == 1 && use_mv {
        let prev_buf = &prev.as_ref().unwrap().buf;
        if rdo_lambda > 0.0 {
            let n = (2 * r_search + 1) as usize;
            let mut cands: Vec<(i32, i32, i32)> = Vec::with_capacity(n * n);
            for dy in -r_search..=r_search {
                for dx in -r_search..=r_search {
                    let s = block_sad(cur, prev_buf, w, h, by, bx, dx, dy);
                    cands.push((dx, dy, s));
                }
            }
            cands.sort_by_key(|&(_, _, s)| s);
            cands.truncate(RDO_K);
            // Half-pel: expand each integer candidate by its refined neighbor
            // so the RD cost sees sub-pixel vectors too.
            if hpel {
                let refined: Vec<(i32, i32)> = cands
                    .iter()
                    .map(|&(dx, dy, _)| refine_hpel(cur, prev_buf, w, h, by, bx, dx, dy))
                    .collect();
                cands.extend(refined.into_iter().map(|(hdx, hdy)| (hdx, hdy, 0)));
            }
            let mut best_cost = f64::INFINITY;
            for (dx, dy, _) in cands {
                let pred = build_pred(prev, w, h, by, bx, ftype, use_mv, hpel, dx, dy);
                let (_, _, nnz) = dct_quant(&cur_b, &pred, qm_ref, dz, f);
                // SATD distortion + rate: 1 (n_pairs) + 3 × nnz (run + 2-byte
                // value per pair); the MV bytes are constant across candidates.
                let satd = satd8(&cur_b, &pred);
                let cost = satd + rdo_lambda * (3 * nnz + 1) as f64;
                if cost < best_cost {
                    best_cost = cost;
                    mvx = dx;
                    mvy = dy;
                }
            }
        } else {
            let mut best = block_sad(cur, prev_buf, w, h, by, bx, 0, 0);
            for dy in -r_search..=r_search {
                for dx in -r_search..=r_search {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let s = block_sad(cur, prev_buf, w, h, by, bx, dx, dy);
                    if s < best {
                        best = s;
                        mvx = dx;
                        mvy = dy;
                    }
                }
            }
            if hpel {
                let (hdx, hdy) = refine_hpel(cur, prev_buf, w, h, by, bx, mvx, mvy);
                mvx = hdx;
                mvy = hdy;
            }
        }
    }

    // predict + residual + DCT + dead-zone quantize (shared with the RDO path)
    let pred = build_pred(prev, w, h, by, bx, ftype, use_mv, hpel, mvx, mvy);
    let (sse, cq, nnz) = dct_quant(&cur_b, &pred, qm_ref, dz, f);

    // skip decision (inter only). Tag 6 MVs are half-pel units, but the zero
    // vector is still exactly (0,0) — identical skip semantics.
    let is_skip =
        ftype == 1 && mvx == 0 && mvy == 0 && (nnz == 0 || (skip_t > 0.0 && sse < skip_t));

    // reconstruct
    let mut rec_b = [[0u8; 8]; 8];
    if is_skip {
        let prev_buf = &prev.as_ref().unwrap().buf;
        for y in 0..8 {
            for x in 0..8 {
                rec_b[y][x] = prev_buf[(by * 8 + y) * w + bx * 8 + x];
            }
        }
    } else {
        let mut c = [[0i64; 8]; 8];
        for y in 0..8 {
            for x in 0..8 {
                c[y][x] = cq[y][x] * qm_ref[y * 8 + x];
            }
        }
        let idct = idct_int(&c);
        for y in 0..8 {
            for x in 0..8 {
                let val = (pred[y][x] as i64 + idct[y][x]).clamp(0, 255);
                rec_b[y][x] = val as u8;
            }
        }
    }

    BlockEnc {
        skip: is_skip,
        mvx,
        mvy,
        cq,
        rec: rec_b,
    }
}

/// Encode one plane. `prev` is `None` on keyframes (prediction = 128).
///
/// Two phases: (1) every block is computed in parallel with rayon — motion
/// search, prediction, DCT, quantization, skip decision and reconstruction
/// touch only that block's own inputs, so results are order-independent;
/// (2) the serial phase assembles the wire bytes in raster order: the skip
/// mask, motion vectors, and the zigzag RLE with the DC DPCM predictor chain
/// (which is inherently serial). Because the parallel phase produces exactly
/// the same per-block results and phase 2 walks blocks in the same order,
/// the output is bit-identical to the original single-threaded loop.
#[allow(clippy::too_many_arguments)]
fn enc_plane(
    cur: &[u8],
    prev: Option<&Plane>,
    w: usize,
    h: usize,
    ftype: u8,
    use_mv: bool,
    hpel: bool,
    qm: &[i64; 64],
    dz: f64,
    skip_t: f64,
    r_search: i32,
    rdo_lambda: f64,
    aq_levels: u8,
    aq_map: Option<&[u8]>,
) -> (Vec<u8>, Plane) {
    let nbx = w / 8;
    let nby = h / 8;
    let nb = nbx * nby;
    let f = dct_basis();

    // Per-block quant-scale numerator (denominator 4); identity when AQ is off.
    let nums: Vec<i64> = if aq_levels > 0 {
        unpack_aq_map(aq_map.expect("aq map missing"), nb, aq_levels)
    } else {
        Vec::new()
    };

    // ── phase 1: parallel per-block compute (rayon preserves raster order) ──
    let blocks: Vec<BlockEnc> = (0..nb)
        .into_par_iter()
        .map(|bi| {
            let by = bi / nbx;
            let bx = bi % nbx;
            let aq_num = if aq_levels > 0 { nums[bi] } else { 4 };
            enc_block(
                cur, prev, w, h, by, bx, ftype, use_mv, hpel, qm, dz, skip_t, f, r_search,
                rdo_lambda, aq_num,
            )
        })
        .collect();

    // ── phase 2: serial assembly (recon plane, skip mask, DC-DPCM'd body) ──
    let mut recon = vec![0u8; w * h];
    let mut body: Vec<u8> = Vec::new();
    let mut dc_pred: i64 = 0;

    for (bi, blk) in blocks.iter().enumerate() {
        let by = bi / nbx;
        let bx = bi % nbx;

        // reconstruction (disjoint 8×8 regions, any order)
        for y in 0..8 {
            for x in 0..8 {
                recon[(by * 8 + y) * w + bx * 8 + x] = blk.rec[y][x];
            }
        }

        if blk.skip {
            continue;
        }
        // emit coded blocks
        if ftype == 1 && use_mv {
            body.push(blk.mvx as i8 as u8);
            body.push(blk.mvy as i8 as u8);
        }
        // zigzag + DC DPCM (over coded blocks, raster order)
        let dc_old = blk.cq[0][0];
        let dc_diff = dc_old - dc_pred;
        dc_pred = dc_old;
        let mut pairs: Vec<(u8, i16)> = Vec::new();
        let mut pos: i32 = 0;
        let mut prev_pos: i32 = -1;
        for k in 0..64usize {
            let id = ZZ[k];
            let v = if k == 0 {
                dc_diff
            } else {
                blk.cq[id / 8][id % 8]
            };
            if v != 0 {
                assert!(
                    (-32767..=32767).contains(&v),
                    "profile: coefficient out of int16 range"
                );
                let run = (pos - prev_pos - 1) as u8;
                pairs.push((run, v as i16));
                prev_pos = pos;
            }
            pos += 1;
        }
        body.push(pairs.len() as u8);
        for (run, val) in pairs {
            body.push(run);
            body.extend_from_slice(&val.to_le_bytes());
        }
    }

    // AQ map (tag 5, luma only) precedes the skip mask: same MSB-first order
    // the decoder reads it in.
    let mut head: Vec<u8> = Vec::new();
    if let Some(map) = aq_map {
        head.extend_from_slice(map);
    }
    if ftype == 1 {
        let mut mask = vec![0u8; nb.div_ceil(8)];
        for (bi, blk) in blocks.iter().enumerate() {
            if blk.skip {
                mask[bi >> 3] |= 0x80 >> (bi & 7);
            }
        }
        head.extend_from_slice(&mask);
    }
    head.extend_from_slice(&body);

    (head, Plane { w, h, buf: recon })
}

/// Integer YUV 4:2:0 → BGR bytes (BT.601 full range, arithmetic shifts).
fn yuv_to_bgr(y: &[u8], cb: &[u8], cr: &[u8], w: usize, h: usize) -> Vec<u8> {
    let cw = w / 2;
    let mut out = vec![0u8; w * h * 3];
    // Each output pixel depends only on its own luma/chroma samples and writes
    // a disjoint 3-byte run — parallel over pixels, bit-identical to serial.
    out.par_chunks_exact_mut(3).enumerate().for_each(|(i, px)| {
        let yy = i / w;
        let x = i % w;
        let cy = yy >> 1;
        let cx = x >> 1;
        let yv = y[yy * w + x] as i32;
        let cbv = cb[cy * cw + cx] as i32 - 128;
        let crv = cr[cy * cw + cx] as i32 - 128;
        let r = yv + ((359 * crv + 128) >> 8);
        let g = yv - ((88 * cbv + 183 * crv + 128) >> 8);
        let b = yv + ((454 * cbv + 128) >> 8);
        px[0] = b.clamp(0, 255) as u8;
        px[1] = g.clamp(0, 255) as u8;
        px[2] = r.clamp(0, 255) as u8;
    });
    out
}

// ────────────────────────────────────────────────────────────────────────────
// Decoder (mirrors codec.js `makeProfileDecoder`)
// ────────────────────────────────────────────────────────────────────────────

/// Stateful profile decoder. Messages must arrive in stream order (deltas
/// predict from the previous frame). Returns `(frame_index, BGR bytes)`.
pub struct ProfileDecoder {
    ql: Option<[i64; 64]>,
    qc: Option<[i64; 64]>,
    cur: Option<[Plane; 3]>,
    spare: Option<[Plane; 3]>,
    /// AQ levels signaled by the last tag-5/6 keyframe (0 = tag-4 stream).
    aq_levels: u8,
    /// Half-pel motion signaled by the last tag-6 keyframe (inter MVs are
    /// half-pel units and the luma reference is interpolated bilinearly).
    hpel: bool,
}

impl ProfileDecoder {
    pub fn new() -> ProfileDecoder {
        ProfileDecoder {
            ql: None,
            qc: None,
            cur: None,
            spare: None,
            aq_levels: 0,
            hpel: false,
        }
    }

    pub fn reset(&mut self) {
        self.ql = None;
        self.qc = None;
        self.cur = None;
        self.spare = None;
        self.aq_levels = 0;
        self.hpel = false;
    }

    /// Decode one wire message → `(frame_index, full BGR frame)`.
    pub fn decode(&mut self, msg: &[u8]) -> Result<(u32, Vec<u8>)> {
        if msg.len() < 5 {
            bail!("profile message too short");
        }
        let tag = msg[4];
        let idx = u32::from_be_bytes([msg[0], msg[1], msg[2], msg[3]]);
        let payload = zlib_decompress(&msg[5..])?;
        if payload.is_empty() {
            bail!("profile payload empty");
        }
        let ftype = payload[0];
        let mut off = 1usize;

        if ftype == 0 {
            if payload.len() < 6 {
                bail!("profile keyframe truncated");
            }
            let qf = payload[1] as i64;
            let w = ((payload[2] as usize) << 8) | payload[3] as usize;
            let h = ((payload[4] as usize) << 8) | payload[5] as usize;
            // Reject absurd grids BEFORE allocating planes: a crafted keyframe
            // header previously requested up to 25 GB (65535² × 3 planes × 2
            // buffers), which aborts on OOM. Also require w,h >= 2 and EVEN
            // (both found by fuzzing): the chroma planes are w/2 × h/2, so a
            // 1×N grid has EMPTY chroma planes and an odd w/h leaves the last
            // luma row/column without chroma — either way yuv_to_bgr indexes
            // out of bounds and panics. Real grids are multiples of 16 (the
            // compiler pads).
            if w < 2
                || h < 2
                || !w.is_multiple_of(2)
                || !h.is_multiple_of(2)
                || (w as u64) * (h as u64) > MAX_GRID_PIXELS
            {
                bail!("profile grid {w}x{h} out of bounds");
            }
            // Tags 5/6 keyframes carry one extra header byte: the AQ level
            // count. Tag 5 pushes it only when AQ is on (2|4); tag 6 always
            // carries it (0 = AQ off) so parsing is unambiguous.
            self.aq_levels = if tag == TAG_PROFILE_AQ || tag == TAG_PROFILE_HPEL {
                if payload.len() < 7 {
                    bail!("profile AQ keyframe truncated");
                }
                let lv = payload[6];
                // Only the levels the encoder can emit are accepted: tag 5
                // carries 2|4, tag 6 additionally carries 0 (AQ off). Anything
                // else (e.g. 1, which would make log2(levels) = 0 and divide
                // by zero in the map unpacker) is a clean bail, not a panic.
                if tag == TAG_PROFILE_HPEL {
                    if lv != 0 && lv != 2 && lv != 4 {
                        bail!("profile AQ levels {lv} out of bounds (0, 2 or 4)");
                    }
                } else if lv != 2 && lv != 4 {
                    bail!("profile AQ levels {lv} out of bounds (2 or 4)");
                }
                off = 7;
                lv
            } else {
                off = 6;
                0
            };
            self.hpel = tag == TAG_PROFILE_HPEL;
            let (ql, qc) = qtables(qf);
            self.ql = Some(ql);
            self.qc = Some(qc);
            let need_alloc = match &self.cur {
                Some(p) => p[0].w != w || p[0].h != h,
                None => true,
            };
            if need_alloc {
                let cw = w / 2;
                let ch = h / 2;
                let mk = || [Plane::new(w, h), Plane::new(cw, ch), Plane::new(cw, ch)];
                self.cur = Some(mk());
                self.spare = Some(mk());
            }
        } else if self.cur.is_none() {
            bail!("profile inter frame before any keyframe");
        }

        // ping-pong: decode into a copy of the current planes
        let cur = self.cur.as_mut().unwrap();
        let spare = self.spare.as_mut().unwrap();
        for i in 0..3 {
            spare[i].buf.copy_from_slice(&cur[i].buf);
        }
        // AQ applies to the luma plane only; chroma always uses the base
        // tables. Half-pel motion likewise applies to the luma plane only
        // (the plane that carries motion vectors).
        let frame_aq = if tag == TAG_PROFILE_AQ || tag == TAG_PROFILE_HPEL {
            self.aq_levels
        } else {
            0
        };
        for pi in 0..3usize {
            let qm = if pi == 0 {
                self.ql.as_ref().unwrap()
            } else {
                self.qc.as_ref().unwrap()
            };
            let aq = if pi == 0 { frame_aq } else { 0 };
            off = dec_plane(
                &payload,
                off,
                &cur[pi],
                &mut spare[pi],
                ftype,
                pi == 0,
                pi == 0 && self.hpel,
                qm,
                aq,
            )?;
        }
        std::mem::swap(&mut self.cur, &mut self.spare);

        let c = self.cur.as_ref().unwrap();
        let frame = yuv_to_bgr(&c[0].buf, &c[1].buf, &c[2].buf, c[0].w, c[0].h);
        Ok((idx, frame))
    }
}

impl Default for ProfileDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Decode one plane into `out` (predicted from `cur`). Returns the new offset.
#[allow(clippy::too_many_arguments)]
fn dec_plane(
    data: &[u8],
    mut off: usize,
    cur: &Plane,
    out: &mut Plane,
    ftype: u8,
    use_mv: bool,
    hpel: bool,
    qm: &[i64; 64],
    aq_levels: u8,
) -> Result<usize> {
    let w = cur.w;
    let h = cur.h;
    let nbx = w / 8;
    let nby = h / 8;
    let nb = nbx * nby;

    // Tag-5 AQ map (luma only): `log2(levels)` bits per block, MSB-first,
    // preceding the skip mask. Unpacked once into per-block quant numerators.
    // `levels` is validated at the keyframe header (2 or 4), but guard the
    // division anyway so a crafted `dec_plane` call can never panic.
    let aq_nums: Vec<i64> = if aq_levels == 2 || aq_levels == 4 {
        let bits = (aq_levels as u32).ilog2() as usize;
        let nbytes = nb.div_ceil(8 / bits);
        if off + nbytes > data.len() {
            bail!("profile plane truncated (AQ map)");
        }
        let nums = unpack_aq_map(&data[off..off + nbytes], nb, aq_levels);
        off += nbytes;
        nums
    } else {
        Vec::new()
    };

    let skip: Option<&[u8]> = if ftype == 1 {
        let mb = nb.div_ceil(8);

        if off + mb > data.len() {
            bail!("profile plane truncated (skip mask)");
        }
        let s = &data[off..off + mb];
        off += mb;
        Some(s)
    } else {
        None
    };

    let mut bi = 0usize;
    let mut dc_pred: i64 = 0;
    let mut z = [0i64; 64];
    let mut c = [0i64; 64];

    for by in 0..nby {
        for bx in 0..nbx {
            if let Some(s) = skip {
                if (s[bi >> 3] & (0x80 >> (bi & 7))) != 0 {
                    bi += 1;
                    continue;
                }
            }

            // Per-block scaled quant table (AQ) — identical integer math to the
            // encoder's and the JS decoder's.
            let num = if aq_levels > 0 { aq_nums[bi] } else { 4 };
            let qm_b: [i64; 64];
            let qm_ref: &[i64; 64] = if num == 4 {
                qm
            } else {
                qm_b = scale_qm(qm, num);
                &qm_b
            };

            let (mut dx, mut dy) = (0i32, 0i32);
            if ftype == 1 && use_mv {
                if off + 2 > data.len() {
                    bail!("profile plane truncated (motion vector)");
                }
                dx = data[off] as i8 as i32;
                dy = data[off + 1] as i8 as i32;
                off += 2;
            }

            if off >= data.len() {
                bail!("profile plane truncated (block header)");
            }
            let n_pairs = data[off] as usize;
            off += 1;

            z.fill(0);
            let mut pos = 0usize;
            let mut last_nz: i32 = -1;
            for _ in 0..n_pairs {
                if off + 3 > data.len() {
                    bail!("profile plane truncated (pair)");
                }
                let run = data[off] as usize;
                let v = i16::from_le_bytes([data[off + 1], data[off + 2]]);
                off += 3;
                pos += run;
                if pos >= 64 {
                    bail!("profile pair run out of zigzag bounds");
                }
                z[pos] = v as i64;
                last_nz = pos as i32;
                pos += 1;
            }

            // DC DPCM
            z[0] += dc_pred;
            dc_pred = z[0];

            let w32 = w as i32;
            let h32 = h as i32;
            if last_nz <= 0 {
                // DC-only block: IDCT collapses to a flat value
                let flat = (529 * (z[0] * qm_ref[0]) + 2048).div_euclid(4096);
                for y in 0..8 {
                    for x in 0..8 {
                        let pred = if ftype == 0 {
                            128
                        } else if use_mv && hpel {
                            hpel_sample(&cur.buf, w, h, bx * 8 + x, by * 8 + y, dx, dy) as i64
                        } else {
                            let sx = ((bx * 8 + x) as i32 + dx).clamp(0, w32 - 1) as usize;
                            let sy = ((by * 8 + y) as i32 + dy).clamp(0, h32 - 1) as usize;
                            cur.buf[sy * w + sx] as i64
                        };
                        out.buf[(by * 8 + y) * w + bx * 8 + x] = (pred + flat).clamp(0, 255) as u8;
                    }
                }
            } else {
                // de-zigzag: c[spatial] = z[zigzag] * qm[spatial]
                for k in 0..64usize {
                    c[ZZ[k]] = z[k] * qm_ref[ZZ[k]];
                }
                let mut cm = [[0i64; 8]; 8];
                for y in 0..8 {
                    for x in 0..8 {
                        cm[y][x] = c[y * 8 + x];
                    }
                }
                let idct = idct_int(&cm);
                for y in 0..8 {
                    for x in 0..8 {
                        let pred = if ftype == 0 {
                            128
                        } else if use_mv && hpel {
                            hpel_sample(&cur.buf, w, h, bx * 8 + x, by * 8 + y, dx, dy) as i64
                        } else {
                            let sx = ((bx * 8 + x) as i32 + dx).clamp(0, w32 - 1) as usize;
                            let sy = ((by * 8 + y) as i32 + dy).clamp(0, h32 - 1) as usize;
                            cur.buf[sy * w + sx] as i64
                        };
                        out.buf[(by * 8 + y) * w + bx * 8 + x] =
                            (pred + idct[y][x]).clamp(0, 255) as u8;
                    }
                }
            }
            bi += 1;
        }
    }
    Ok(off)
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_bgr(w: usize, h: usize, i: u32, seed: u64) -> Vec<u8> {
        // deterministic LCG noise + moving blob
        let mut state = seed.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(i as u64);
        let mut next = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        };
        let mut f = vec![0u8; w * h * 3];
        for px in f.iter_mut() {
            *px = next();
        }
        let cx = (w / 2 + (i as usize * 4) % (w / 2).max(1)) as i32;
        let cy = (h / 2) as i32;
        let r = (h / 8).max(2) as i32;
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                if (x - cx) * (x - cx) + (y - cy) * (y - cy) <= r * r {
                    let o = ((y as usize) * w + x as usize) * 3;
                    f[o] = 0;
                    f[o + 1] = 128;
                    f[o + 2] = 255;
                }
            }
        }
        f
    }

    #[test]
    fn qtables_match_python() {
        // QF=70: S = 200-2*70 = 60 → QL[0]=floor((16*60+50)/100)=10, QC[0]=10,
        // QL[1]=floor((11*60+50)/100)=7, QL[2]=floor((10*60+50)/100)=6
        let (ql, qc) = qtables(70);
        assert_eq!(ql[0], 10);
        assert_eq!(ql[1], 7);
        assert_eq!(ql[2], 6);
        assert_eq!(qc[0], 10);
        assert!(ql.iter().all(|&v| (1..=255).contains(&v)));
        assert!(qc.iter().all(|&v| (1..=255).contains(&v)));
    }

    #[test]
    fn np_round_is_bankers() {
        assert_eq!(np_round(2.5), 2.0);
        assert_eq!(np_round(3.5), 4.0);
        assert_eq!(np_round(-2.5), -2.0);
        assert_eq!(np_round(-3.5), -4.0);
        assert_eq!(np_round(2.4), 2.0);
        assert_eq!(np_round(-2.4), -2.0);
        assert_eq!(np_round(0.5), 0.0);
    }

    #[test]
    fn roundtrip_small() {
        let (w, h) = (48usize, 32usize);
        let mut enc = ProfileEncoder::new(w, h, 70);
        let mut dec = ProfileDecoder::new();
        for i in 0..10u32 {
            let f = synth_bgr(w, h, i, 42);
            let (msg, shown) = enc.encode(&f);
            let (idx, out) = dec.decode(&msg).unwrap();
            assert_eq!(idx, i);
            assert_eq!(out, shown, "decoder must reproduce encoder's shown frame");
        }
    }

    #[test]
    fn keyframe_resync_at_48() {
        let (w, h) = (32usize, 16usize);
        let mut enc = ProfileEncoder::new(w, h, 60);
        let mut dec = ProfileDecoder::new();
        for i in 0..100u32 {
            let f = synth_bgr(w, h, i, 7);
            let (msg, shown) = enc.encode(&f);
            let (_, out) = dec.decode(&msg).unwrap();
            assert_eq!(out, shown, "frame {i} mismatch (covers keyframe resync)");
        }
    }

    #[test]
    fn aq_roundtrip_2_and_4_levels() {
        // Tag-5 AQ streams must round-trip through OUR decoder for both level
        // counts, across keyframes, inter frames, skips and forced scene cuts.
        for levels in [2u8, 4u8] {
            let (w, h) = (48usize, 32usize);
            let mut enc = ProfileEncoder::new(w, h, 70);
            enc.aq_levels = levels;
            enc.scene_cut_mad = 20.0;
            let mut dec = ProfileDecoder::new();
            let mut frames: Vec<Vec<u8>> = (0..10u32).map(|i| synth_bgr(w, h, i, 42)).collect();
            frames.push(ramp_bgr(w, h, 9)); // hard scene change → forced keyframe
            for (i, f) in frames.iter().enumerate() {
                let (msg, shown) = enc.encode(f);
                assert_eq!(
                    msg[4], TAG_PROFILE_AQ,
                    "AQ encoder must emit tag 5 (levels={levels}, frame {i})"
                );
                let (idx, out) = dec
                    .decode(&msg)
                    .unwrap_or_else(|e| panic!("levels={levels} frame {i}: {e}"));
                assert_eq!(idx, i as u32);
                assert_eq!(
                    out, shown,
                    "AQ levels={levels} frame {i}: decoder must reproduce shown frame"
                );
            }
        }
    }

    #[test]
    fn hpel_roundtrip_with_and_without_aq() {
        // Tag-6 half-pel streams must round-trip through OUR decoder for both
        // plain and AQ-combined modes, across keyframes, inter frames, skips
        // and forced scene cuts — the encoder and decoder must interpolate the
        // half-pel reference with identical integer math.
        for hpel_aq in [(true, 0u8), (true, 2u8), (true, 4u8)] {
            let (hpel, aq) = hpel_aq;
            let (w, h) = (48usize, 32usize);
            let mut enc = ProfileEncoder::new(w, h, 70);
            enc.hpel = hpel;
            enc.aq_levels = aq;
            enc.scene_cut_mad = 20.0;
            let mut dec = ProfileDecoder::new();
            let mut frames: Vec<Vec<u8>> = (0..10u32).map(|i| synth_bgr(w, h, i, 42)).collect();
            frames.push(ramp_bgr(w, h, 9)); // hard scene change → forced keyframe
            for (i, f) in frames.iter().enumerate() {
                let (msg, shown) = enc.encode(f);
                assert_eq!(
                    msg[4], TAG_PROFILE_HPEL,
                    "hpel encoder must emit tag 6 (aq={aq}, frame {i})"
                );
                let (idx, out) = dec
                    .decode(&msg)
                    .unwrap_or_else(|e| panic!("hpel aq={aq} frame {i}: {e}"));
                assert_eq!(idx, i as u32);
                assert_eq!(
                    out, shown,
                    "hpel aq={aq} frame {i}: decoder must reproduce shown frame"
                );
            }
        }
    }

    #[test]
    fn hpel_emits_tag6_and_keeps_keyframe_layout() {
        // `hpel = false` keeps tags 4/5 byte-identical (the compat contract;
        // also pinned by the fuzz corpus and the aq_off test). With `hpel =
        // true` the tag is 6, and the KEYFRAME wire layout is unchanged from
        // tags 4/5 except that the AQ byte is always present (0 when AQ is
        // off) — only inter-frame payloads differ (that is the point of
        // half-pel: different MVs and residuals).
        let (w, h) = (48usize, 32usize);
        let frames: Vec<Vec<u8>> = (0..50u32).map(|i| synth_bgr(w, h, i, 42)).collect();

        let mut enc_ref = ProfileEncoder::new(w, h, 70);
        let mut enc_hp = ProfileEncoder::new(w, h, 70);
        enc_hp.hpel = true;
        for (i, f) in frames.iter().enumerate() {
            let m_ref = enc_ref.encode(f).0;
            let m_hp = enc_hp.encode(f).0;
            assert_eq!(
                m_hp[4], TAG_PROFILE_HPEL,
                "hpel must emit tag 6 (frame {i})"
            );
            if i % 48 == 0 {
                // keyframes: the uncompressed tag-6 body is the tag-4 body
                // with the always-present AQ byte (0) INSERTED into the
                // header — nothing else drifts.
                let hp_body = zlib_decompress(&m_hp[5..]).unwrap();
                let ref_body = zlib_decompress(&m_ref[5..]).unwrap();
                assert_eq!(
                    hp_body.len(),
                    ref_body.len() + 1,
                    "tag-6 keyframe = tag-4 keyframe + 1 AQ byte (frame {i})"
                );
                assert_eq!(
                    &hp_body[..6],
                    &ref_body[..6],
                    "header prefix drifted (frame {i})"
                );
                assert_eq!(hp_body[6], 0, "AQ off must signal 0 (frame {i})");
                assert_eq!(
                    &hp_body[7..],
                    &ref_body[6..],
                    "plane data drifted (frame {i})"
                );
            } else {
                assert_ne!(
                    &m_ref[5..],
                    &m_hp[5..],
                    "inter payloads should differ under half-pel (frame {i})"
                );
            }
        }
    }

    #[test]
    fn aq_off_is_bit_exact_tag4() {
        // `aq_levels = 0` must leave the tag-4 stream byte-identical, and the
        // AQ bit-packing must be stable under round-trips.
        let (w, h) = (48usize, 32usize);
        let mut enc = ProfileEncoder::new(w, h, 70);
        assert_eq!(enc.aq_levels, 0, "AQ must default off");
        let mut dec = ProfileDecoder::new();
        for i in 0..8u32 {
            let f = synth_bgr(w, h, i, 99);
            let (msg, shown) = enc.encode(&f);
            assert_eq!(msg[4], TAG_PROFILE, "AQ off must emit tag 4");
            let (_, out) = dec.decode(&msg).unwrap();
            assert_eq!(out, shown);
        }
    }

    #[test]
    fn malformed_aq_level_bails_not_panics() {
        // Fuzz-found: a tag-6 keyframe with aq_levels = 1 (or 3) must bail
        // cleanly — 1 would make log2(levels) = 0 and divide by zero in the
        // map unpacker. The decoder must never panic on a crafted stream.
        for (tag, bad_lv) in [
            (TAG_PROFILE_HPEL, 1u8),
            (TAG_PROFILE_HPEL, 3u8),
            (TAG_PROFILE_AQ, 0u8),
        ] {
            let (w, h) = (48usize, 32usize);
            let mut body = vec![0u8]; // ftype = keyframe
            body.push(70);
            body.extend_from_slice(&(w as u16).to_be_bytes());
            body.extend_from_slice(&(h as u16).to_be_bytes());
            body.push(bad_lv);
            let z = zlib_compress(&body, 6);
            let mut msg = Vec::new();
            msg.extend_from_slice(&0u32.to_be_bytes());
            msg.push(tag);
            msg.extend_from_slice(&z);

            let mut dec = ProfileDecoder::new();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dec.decode(&msg)));
            match r {
                Ok(Err(e)) => {
                    assert!(
                        e.to_string().contains("out of bounds"),
                        "expected a clean levels bail, got: {e}"
                    );
                }
                Ok(Ok(_)) => panic!("tag {tag} with aq level {bad_lv} must not decode"),
                Err(_) => panic!("tag {tag} with aq level {bad_lv} must not panic"),
            }
        }
    }

    #[test]
    fn aq_map_pack_unpack_roundtrip() {
        // Packing must be lossless for both widths so Rust and JS decode the
        // same per-block scales (MSB-first, `ilog2(levels)` bits per block).
        for (levels, indices) in [
            (2u8, vec![0u8, 1, 1, 0, 1, 0, 1, 1, 0, 0, 1, 0, 1, 1, 0, 1]),
            (4u8, vec![3u8, 0, 1, 2, 3, 3, 0, 2, 1, 1, 0, 3, 2, 2, 1, 0]),
        ] {
            let bits = (levels as u32).ilog2() as u8;
            let packed = pack_aq_map(&indices, bits);
            let nums = unpack_aq_map(&packed, indices.len(), levels);
            let want: Vec<i64> = indices.iter().map(|&i| aq_num(levels, i)).collect();
            assert_eq!(nums, want, "AQ map round-trip (levels={levels})");
        }
    }

    #[test]
    fn scale_qm_identity_and_halving() {
        // num=4 must be the identity table; num=2 must halve each step.
        let (ql, _) = qtables(70);
        let id = scale_qm(&ql, 4);
        assert_eq!(id, ql, "num=4 is the identity scale");
        let half = scale_qm(&ql, 2);
        for i in 0..64 {
            assert_eq!(half[i], ((ql[i] * 2 + 2) / 4).max(1));
        }
    }

    /// Smooth diagonal ramp — encodes cleanly so the recon error is tiny
    /// (the scene-cut signal then cleanly separates "same scene" from "cut").
    fn ramp_bgr(w: usize, h: usize, phase: u32) -> Vec<u8> {
        let mut f = vec![0u8; w * h * 3];
        for (i, px) in f.chunks_exact_mut(3).enumerate() {
            let v = ((i as u32 * 3 + phase * 40) & 0xff) as u8;
            px[0] = v;
            px[1] = v.wrapping_add(40);
            px[2] = v.wrapping_add(80);
        }
        f
    }

    /// Frame type from a wire message: `[u32 index][tag 4][zlib(payload)]`,
    /// `payload[0]` is the ftype byte.
    fn payload_ftype(msg: &[u8]) -> u8 {
        let payload = zlib_decompress(&msg[5..]).expect("zlib payload");
        payload[0]
    }

    #[test]
    fn scene_cut_forces_keyframe() {
        let (w, h) = (48usize, 32usize);
        let mut enc = ProfileEncoder::new(w, h, 70);
        enc.scene_cut_mad = 20.0;
        let mut dec = ProfileDecoder::new();
        let a = ramp_bgr(w, h, 0);
        let b = ramp_bgr(w, h, 3); // clearly a different scene

        let (m0, s0) = enc.encode(&a);
        assert_eq!(dec.decode(&m0).unwrap().1, s0);
        assert_eq!(payload_ftype(&m0), 0, "first frame must be a keyframe");

        // identical next frame: tiny deviation from the recon → inter
        let (m1, s1) = enc.encode(&a);
        assert_eq!(dec.decode(&m1).unwrap().1, s1);
        assert_eq!(payload_ftype(&m1), 1, "static frame must stay inter");

        // scene change: luma deviates massively → forced keyframe
        let (m2, s2) = enc.encode(&b);
        assert_eq!(
            dec.decode(&m2).unwrap().1,
            s2,
            "decoder must reproduce the forced keyframe"
        );
        assert_eq!(payload_ftype(&m2), 0, "scene change must force a keyframe");
    }

    #[test]
    fn scene_cut_disabled_by_default() {
        let (w, h) = (48usize, 32usize);
        let mut enc = ProfileEncoder::new(w, h, 70);
        assert_eq!(
            enc.scene_cut_mad, 0.0,
            "detection must default off so the codec stays bit-exact with the original"
        );
        let a = ramp_bgr(w, h, 0);
        let b = ramp_bgr(w, h, 3);
        enc.encode(&a);
        enc.encode(&a);
        let (m2, _) = enc.encode(&b);
        assert_eq!(
            payload_ftype(&m2),
            1,
            "without detection the cut frame stays inter"
        );
    }

    #[test]
    fn parallel_output_is_bit_identical_across_thread_counts() {
        let (w, h) = (64usize, 48usize); // 8×6 = 48 blocks, real threading
        let run = |threads: usize, frames: Vec<Vec<u8>>| -> Vec<(Vec<u8>, Vec<u8>)> {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            pool.install(|| {
                let mut enc = ProfileEncoder::new(w, h, 70);
                enc.scene_cut_mad = 20.0;
                frames.iter().map(|f| enc.encode(f)).collect()
            })
        };
        // inter-heavy noise sequence + a forced keyframe via a hard scene change
        let mut frames: Vec<Vec<u8>> = (0..10u32).map(|i| synth_bgr(w, h, i, 42)).collect();
        frames.push(ramp_bgr(w, h, 9)); // clearly a different scene → forced keyframe
        let single = run(1, frames.clone());
        let multi = run(8, frames);
        assert_eq!(single.len(), multi.len());
        for (i, (a, b)) in single.iter().zip(&multi).enumerate() {
            assert_eq!(
                a.0, b.0,
                "frame {i}: wire bytes differ across thread counts"
            );
            assert_eq!(
                a.1, b.1,
                "frame {i}: shown frame differs across thread counts"
            );
        }
    }

    #[test]
    fn encoder_tracks_quality_stats() {
        let (w, h) = (48usize, 32usize);
        let mut enc = ProfileEncoder::new(w, h, 70);
        for i in 0..5u32 {
            let f = synth_bgr(w, h, i, 42);
            enc.encode(&f);
        }
        let s = enc.stats();
        assert_eq!(s.frames(), 5);
        // Lossy DCT on noisy synthetic content must deviate measurably but
        // plausibly: finite PSNR below 60 dB, SSIM in (0, 1).
        assert!(s.psnr_y_mean() > 0.0 && s.psnr_y_mean() < 60.0);
        assert!(s.psnr_rgb_mean() > 0.0 && s.psnr_rgb_mean() < 60.0);
        assert!(s.ssim_y_mean() > 0.0 && s.ssim_y_mean() < 1.0);
        // reset() clears the accumulated statistics too
        enc.reset();
        assert_eq!(enc.stats().frames(), 0);
    }

    #[test]
    #[should_panic(expected = "multiples of 16")]
    fn rejects_non_16_grid() {
        let _ = ProfileEncoder::new(40, 32, 70);
    }

    #[test]
    fn rejects_tiny_keyframe_grids() {
        // A crafted keyframe declaring a 1×1 grid has EMPTY chroma planes
        // (w/2 × h/2 = 0); yuv_to_bgr used to index cb[0] and panic. Must bail.
        let mut dec = ProfileDecoder::new();
        let mut msg = vec![0u8, 0, 0, 0, TAG_PROFILE];
        // payload: ftype=0 (keyframe), QF=70, w=1, h=1, then three empty planes
        msg.extend_from_slice(&zlib_compress(&[0, 70, 0, 1, 0, 1], 6));
        assert!(
            dec.decode(&msg).is_err(),
            "1x1 keyframe grid must be rejected, not panic"
        );
        // 1×N and N×1 are just as invalid (chroma is empty either way)
        let mut msg2 = vec![0u8, 0, 0, 0, TAG_PROFILE];
        msg2.extend_from_slice(&zlib_compress(&[0, 70, 0, 1, 1, 0], 6));
        assert!(dec.decode(&msg2).is_err(), "1xN grid must be rejected");
        let mut msg3 = vec![0u8, 0, 0, 0, TAG_PROFILE];
        msg3.extend_from_slice(&zlib_compress(&[0, 70, 1, 0, 0, 1], 6));
        assert!(dec.decode(&msg3).is_err(), "Nx1 grid must be rejected");
        // odd w or h: the last luma row/column has no chroma (w/2 × h/2),
        // so yuv_to_bgr would index one past the chroma plane — must bail
        for (w, h) in [(3u8, 2u8), (2, 3), (5, 5), (3, 7)] {
            let mut m = vec![0u8, 0, 0, 0, TAG_PROFILE];
            m.extend_from_slice(&zlib_compress(&[0, 70, 0, w, 0, h], 6));
            assert!(
                dec.decode(&m).is_err(),
                "odd grid {w}x{h} must be rejected, not panic"
            );
        }
        // a legal tiny grid (2×2) still decodes without panicking
        let mut msg4 = vec![0u8, 0, 0, 0, TAG_PROFILE];
        msg4.extend_from_slice(&zlib_compress(&[0, 70, 0, 2, 0, 2], 6));
        assert!(dec.decode(&msg4).is_ok(), "2x2 grid must decode");
    }

    #[test]
    fn decoder_needs_keyframe_first() {
        let mut dec = ProfileDecoder::new();
        // fake an inter message: index 0, tag 4, zlib("[1]")
        let mut msg = vec![0u8, 0, 0, 0, TAG_PROFILE];
        msg.extend_from_slice(&zlib_compress(&[1], 6));
        assert!(dec.decode(&msg).is_err());
    }
}
