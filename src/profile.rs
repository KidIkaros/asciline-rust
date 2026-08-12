//! Opt-in lossy DCT profile (tag 4, pixel mode) — a faithful port of the
//! `ProfileEncoder` / `makeProfileDecoder` pair from `codec.py` / `codec.js`.
//!
//! This is the maximum-compression `.ascf` profile: frames are converted to
//! YUV 4:2:0, every 8×8 block is motion-compensated (luma, ±3 integer search),
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

//! Wire format (per frame message, after the shared `[u32 BE index][u8 tag=4]`):
//! ```text
//! payload = zlib( body )
//! body:
//!   [u8 ftype]                       0 = keyframe, 1 = inter
//!   keyframe only: [u8 QF][u16 BE cols][u16 BE rows]
//!   then 3 planes (Y full, Cb/Cr half), each:
//!     inter only: [ceil(nb/8) bytes skip mask, MSB-first]
//!     per coded block, raster order:
//!       luma inter: [i8 dx][i8 dy]
//!       [u8 n_pairs][ (u8 run)(i16 LE value) × n_pairs ]
//! ```

use anyhow::{bail, Result};
use rayon::prelude::*;

use crate::codec::{zlib_compress, zlib_decompress, TAG_PROFILE};
use crate::quality::{psnr, ssim, QualityStats};

/// Forced keyframe interval (same as the adaptive codec's).
const KEY: u32 = 48;
/// Motion search radius (integer pixels).
const R_SEARCH: i32 = 3;
/// Default dead-zone: coefficients with |t| below this round to zero.
const DEFAULT_DZ: f64 = 0.75;
/// Default skip threshold: inter blocks with SSE below this and zero motion
/// are skipped even if they have non-zero coefficients.
const DEFAULT_SKIP_T: f64 = 256.0;
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
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69,
    56, 14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104,
    113, 92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];

/// Chroma quantization table (JPEG style).
const QC_BASE: [i64; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99,
    99, 47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
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
    pub fn new_with(w: usize, h: usize, qf: u8, dz: f64, skip_t: f64, level: u32) -> ProfileEncoder {
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
        assert_eq!(frame_bgr.len(), self.w * self.h * 3, "profile frame size mismatch");

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
        y.par_iter_mut()
            .enumerate()
            .for_each(|(i, out)| {
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
                for (i, (dy, dx)) in [(0usize, 0usize), (0, 1), (1, 0), (1, 1)].iter().enumerate() {
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

        let mut payload: Vec<u8> = Vec::new();
        payload.push(ftype);
        if ftype == 0 {
            // keyframe self-describes: [QF][cols u16][rows u16]
            payload.push(self.qf);
            payload.extend_from_slice(&(w as u16).to_be_bytes());
            payload.extend_from_slice(&(h as u16).to_be_bytes());
        }

        let (ql, qc) = qtables(self.qf as i64);
        let planes = [y, cb, cr];
        let mut recons: [Plane; 3] = [Plane::new(0, 0), Plane::new(0, 0), Plane::new(0, 0)];
        for pi in 0..3usize {
            let (pw, ph) = if pi == 0 { (w, h) } else { (cw, ch) };
            let qm = if pi == 0 { &ql } else { &qc };
            let prev_plane = if ftype == 1 {
                Some(&self.prev.as_ref().unwrap()[pi])
            } else {
                None
            };
            let (body, rec) = enc_plane(
                &planes[pi],
                prev_plane,
                pw,
                ph,
                ftype,
                pi == 0,
                qm,
                self.dz,
                self.skip_t,
            );
            payload.extend_from_slice(&body);
            recons[pi] = rec;
        }

        let z = zlib_compress(&payload, self.level);
        let mut msg = Vec::with_capacity(5 + z.len());
        msg.extend_from_slice(&self.n.to_be_bytes());
        msg.push(TAG_PROFILE);
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
    qm: &[i64; 64],
    dz: f64,
    skip_t: f64,
    f: &[[f64; 8]; 8],
) -> BlockEnc {
    // current block as f64
    let mut cur_b = [[0f64; 8]; 8];
    for y in 0..8 {
        for x in 0..8 {
            cur_b[y][x] = cur[(by * 8 + y) * w + bx * 8 + x] as f64;
        }
    }

    // motion search (luma inter only): scan dy outer, dx inner, (0,0)
    // preferred on ties — identical ordering to codec.py
    let mut mvx: i32 = 0;
    let mut mvy: i32 = 0;
    if ftype == 1 && use_mv {
        let prev_buf = &prev.as_ref().unwrap().buf;
        let mut best = block_sad(cur, prev_buf, w, h, by, bx, 0, 0);
        for dy in -R_SEARCH..=R_SEARCH {
            for dx in -R_SEARCH..=R_SEARCH {
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
    }

    // predict
    let mut pred = [[0f64; 8]; 8];
    if ftype == 0 {
        for y in 0..8 {
            for x in 0..8 {
                pred[y][x] = 128.0;
            }
        }
    } else {
        let prev_buf = &prev.as_ref().unwrap().buf;
        let w32 = w as i32;
        let h32 = h as i32;
        for y in 0..8 {
            for x in 0..8 {
                if use_mv {
                    let sx = ((bx * 8 + x) as i32 + mvx).clamp(0, w32 - 1) as usize;
                    let sy = ((by * 8 + y) as i32 + mvy).clamp(0, h32 - 1) as usize;
                    pred[y][x] = prev_buf[sy * w + sx] as f64;
                } else {
                    pred[y][x] = prev_buf[(by * 8 + y) * w + bx * 8 + x] as f64;
                }
            }
        }
    }

    // residual + SSE
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
    let mut all_zero = true;
    for y in 0..8 {
        for x in 0..8 {
            let tv = t[y][x] / qm[y * 8 + x] as f64;
            let q = if tv.abs() < dz { 0.0 } else { np_round(tv) };
            let qi = q as i64;
            cq[y][x] = qi;
            if qi != 0 {
                all_zero = false;
            }
        }
    }

    // skip decision (inter only)
    let is_skip = ftype == 1
        && mvx == 0
        && mvy == 0
        && (all_zero || (skip_t > 0.0 && sse < skip_t));

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
                c[y][x] = cq[y][x] * qm[y * 8 + x];
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
    qm: &[i64; 64],
    dz: f64,
    skip_t: f64,
) -> (Vec<u8>, Plane) {
    let nbx = w / 8;
    let nby = h / 8;
    let nb = nbx * nby;
    let f = dct_basis();

    // ── phase 1: parallel per-block compute (rayon preserves raster order) ──
    let blocks: Vec<BlockEnc> = (0..nb)
        .into_par_iter()
        .map(|bi| {
            let by = bi / nbx;
            let bx = bi % nbx;
            enc_block(cur, prev, w, h, by, bx, ftype, use_mv, qm, dz, skip_t, f)
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
            let v = if k == 0 { dc_diff } else { blk.cq[id / 8][id % 8] };
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

    // skip mask (MSB-first), then block data
    let mut head: Vec<u8> = Vec::new();
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
    out.par_chunks_exact_mut(3)
        .enumerate()
        .for_each(|(i, px)| {
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
}

impl ProfileDecoder {
    pub fn new() -> ProfileDecoder {
        ProfileDecoder {
            ql: None,
            qc: None,
            cur: None,
            spare: None,
        }
    }

    pub fn reset(&mut self) {
        self.ql = None;
        self.qc = None;
        self.cur = None;
        self.spare = None;
    }

    /// Decode one wire message → `(frame_index, full BGR frame)`.
    pub fn decode(&mut self, msg: &[u8]) -> Result<(u32, Vec<u8>)> {
        if msg.len() < 5 {
            bail!("profile message too short");
        }
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
            // buffers), which aborts on OOM.
            if w == 0 || h == 0 || (w as u64) * (h as u64) > MAX_GRID_PIXELS {
                bail!("profile grid {w}x{h} out of bounds");
            }
            off = 6;
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
                let mk = || {
                    [
                        Plane::new(w, h),
                        Plane::new(cw, ch),
                        Plane::new(cw, ch),
                    ]
                };
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
        for pi in 0..3usize {
            let qm = if pi == 0 {
                self.ql.as_ref().unwrap()
            } else {
                self.qc.as_ref().unwrap()
            };
            off = dec_plane(&payload, off, &cur[pi], &mut spare[pi], ftype, pi == 0, qm)?;
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
    qm: &[i64; 64],
) -> Result<usize> {
    let w = cur.w;
    let h = cur.h;
    let nbx = w / 8;
    let nby = h / 8;
    let nb = nbx * nby;

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
                let flat = (529 * (z[0] * qm[0]) + 2048).div_euclid(4096);
                for y in 0..8 {
                    for x in 0..8 {
                        let pred = if ftype == 0 {
                            128
                        } else {
                            let sx = ((bx * 8 + x) as i32 + dx).clamp(0, w32 - 1) as usize;
                            let sy = ((by * 8 + y) as i32 + dy).clamp(0, h32 - 1) as usize;
                            cur.buf[sy * w + sx] as i64
                        };
                        out.buf[(by * 8 + y) * w + bx * 8 + x] =
                            (pred + flat).clamp(0, 255) as u8;
                    }
                }
            } else {
                // de-zigzag: c[spatial] = z[zigzag] * qm[spatial]
                for k in 0..64usize {
                    c[ZZ[k]] = z[k] * qm[ZZ[k]];
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
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
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
        assert_eq!(dec.decode(&m2).unwrap().1, s2, "decoder must reproduce the forced keyframe");
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
        assert_eq!(payload_ftype(&m2), 1, "without detection the cut frame stays inter");
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
            assert_eq!(a.0, b.0, "frame {i}: wire bytes differ across thread counts");
            assert_eq!(a.1, b.1, "frame {i}: shown frame differs across thread counts");
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
    fn decoder_needs_keyframe_first() {
        let mut dec = ProfileDecoder::new();
        // fake an inter message: index 0, tag 4, zlib("[1]")
        let mut msg = vec![0u8, 0, 0, 0, TAG_PROFILE];
        msg.extend_from_slice(&zlib_compress(&[1], 6));
        assert!(dec.decode(&msg).is_err());
    }
}
