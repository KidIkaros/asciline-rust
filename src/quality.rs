//! PSNR / SSIM quality metrics for lossy-reconstruction reports.
//!
//! `asciline-compile --profile` uses the lossy DCT codec, so its output is
//! never pixel-identical to the source. These helpers quantify the deviation
//! with the two metrics video tooling reports most often:
//!
//! * **PSNR** — peak signal-to-noise ratio in dB from mean squared error over
//!   a byte plane (`inf` when the planes are identical).
//! * **SSIM** — structural similarity (Wang et al., 2004) with an 11×11
//!   Gaussian window (σ = 1.5), K1 = 0.01, K2 = 0.03 on a 0..255 scale,
//!   computed via separable convolution with mirror (reflect) boundaries and
//!   averaged over the whole plane. 1.0 = identical.
//!
//! All arithmetic is f64, so results are deterministic across runs.

use rayon::prelude::*;

/// Mean squared error between two equal-length byte planes.
pub fn mse(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len(), "mse: plane length mismatch");
    if a.is_empty() {
        return 0.0;
    }
    let mut acc: u64 = 0;
    for (&x, &y) in a.iter().zip(b) {
        let d = (x as i64) - (y as i64);
        acc += (d * d) as u64;
    }
    acc as f64 / a.len() as f64
}

/// Peak signal-to-noise ratio in dB (`inf` for identical planes).
pub fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let m = mse(a, b);
    if m == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (65025.0 / m).log10()
    }
}

/// 11-tap Gaussian kernel, σ = 1.5, normalized to sum to 1.
fn gauss_11() -> [f64; 11] {
    const SIGMA: f64 = 1.5;
    let mut k = [0.0f64; 11];
    let mut sum = 0.0;
    for (i, v) in k.iter_mut().enumerate() {
        let d = i as f64 - 5.0;
        *v = (-d * d / (2.0 * SIGMA * SIGMA)).exp();
        sum += *v;
    }
    for v in k.iter_mut() {
        *v /= sum;
    }
    k
}

/// Mirror (reflect) index for window boundaries: ..., 3, 2, 1, 0, 1, 2, 3, ...
#[inline]
fn reflect(i: i64, n: usize) -> usize {
    if n <= 1 {
        return 0;
    }
    let period = 2 * (n as i64 - 1);
    let mut x = i.rem_euclid(period);
    if x >= n as i64 {
        x = period - x;
    }
    x as usize
}

/// Mirror-index LUTs for one plane geometry. Built once per SSIM call and
/// shared by all five blur passes — replaces the per-pixel `rem_euclid` (i64
/// division, ~30 cycles) in the hot loop with table lookups. Same samples,
/// same results, ~5-10x less arithmetic per pixel.
struct BlurLuts {
    w: usize,
    h: usize,
    kx: Vec<[usize; 11]>,
    ky: Vec<[usize; 11]>,
}

impl BlurLuts {
    fn new(w: usize, h: usize) -> BlurLuts {
        let kx: Vec<[usize; 11]> = (0..w)
            .map(|x| std::array::from_fn(|ki| reflect(x as i64 - 5 + ki as i64, w)))
            .collect();
        let ky: Vec<[usize; 11]> = (0..h)
            .map(|y| std::array::from_fn(|ki| reflect(y as i64 - 5 + ki as i64, h)))
            .collect();
        BlurLuts { w, h, kx, ky }
    }

    /// Separable Gaussian blur (horizontal then vertical) with reflect
    /// boundaries. Parallelized over rows with rayon: each output pixel is an
    /// independent dot product of fixed inputs, so results are bit-identical
    /// to the serial loop regardless of thread count (the SSIM report is
    /// deterministic). This is the dominant cost of the per-frame quality
    /// report when enabled, so it scales with cores. The mirror-index LUTs
    /// are built once per SSIM call and shared across the five blurred
    /// fields; each plane is processed contiguously (row-major, plane by
    /// plane) for cache locality.
    #[allow(clippy::needless_range_loop)]
    fn blur(&self, src: &[f64], k: &[f64; 11]) -> Vec<f64> {
        let w = self.w;
        let h = self.h;
        let mut horiz = vec![0.0f64; w * h];
        horiz
            .par_chunks_exact_mut(w)
            .enumerate()
            .for_each(|(y, row)| {
                for (x, px) in row.iter_mut().enumerate() {
                    let kx = &self.kx[x];
                    let base = y * w;
                    let mut acc = 0.0;
                    for (ki, &kw) in k.iter().enumerate() {
                        acc += kw * src[base + kx[ki]];
                    }
                    *px = acc;
                }
            });
        let mut out = vec![0.0f64; w * h];
        out.par_chunks_exact_mut(w)
            .enumerate()
            .for_each(|(y, row)| {
                let ky = &self.ky[y];
                for (x, px) in row.iter_mut().enumerate() {
                    let mut acc = 0.0;
                    for (ki, &kw) in k.iter().enumerate() {
                        acc += kw * horiz[ky[ki] * w + x];
                    }
                    *px = acc;
                }
            });
        out
    }

    /// Same blur without rayon (single-threaded pools: the parallel iterator's
    /// scheduling overhead exceeds the win). Bit-identical results.
    #[allow(clippy::needless_range_loop)]
    fn blur_serial(&self, src: &[f64], k: &[f64; 11]) -> Vec<f64> {
        let w = self.w;
        let h = self.h;
        let mut horiz = vec![0.0f64; w * h];
        for y in 0..h {
            let base = y * w;
            for x in 0..w {
                let kx = &self.kx[x];
                let mut acc = 0.0;
                for (ki, &kw) in k.iter().enumerate() {
                    acc += kw * src[base + kx[ki]];
                }
                horiz[base + x] = acc;
            }
        }
        let mut out = vec![0.0f64; w * h];
        for y in 0..h {
            let ky = &self.ky[y];
            for x in 0..w {
                let mut acc = 0.0;
                for (ki, &kw) in k.iter().enumerate() {
                    acc += kw * horiz[ky[ki] * w + x];
                }
                out[y * w + x] = acc;
            }
        }
        out
    }
}



/// Structural similarity between two `w`×`h` byte planes (1.0 = identical).
///
/// Standard windowed SSIM: per-pixel 11×11 Gaussian means, variances and
/// covariance, averaged over the whole plane. Planes smaller than 11 px in
/// either dimension fall back to whole-image statistics.
pub fn ssim(a: &[u8], b: &[u8], w: usize, h: usize) -> f64 {
    assert_eq!(a.len(), b.len(), "ssim: plane length mismatch");
    assert_eq!(a.len(), w * h, "ssim: buffer/geometry mismatch");
    if w < 11 || h < 11 {
        return ssim_global(a, b);
    }
    let k = gauss_11();
    let n = w * h;
    let af: Vec<f64> = a.iter().map(|&v| v as f64).collect();
    let bf: Vec<f64> = b.iter().map(|&v| v as f64).collect();
    let a2: Vec<f64> = af.iter().map(|&v| v * v).collect();
    let b2: Vec<f64> = bf.iter().map(|&v| v * v).collect();
    let ab: Vec<f64> = af.iter().zip(&bf).map(|(&x, &y)| x * y).collect();
    // Mirror LUTs built once and shared by all five blur passes; on a
    // single-threaded pool the parallel iterator's overhead exceeds the win,
    // so fall back to the plain loop — same results, no slowdown.
    let luts = BlurLuts::new(w, h);
    let parallel = rayon::current_num_threads() > 1;
    let b = |src: &[f64]| if parallel { luts.blur(src, &k) } else { luts.blur_serial(src, &k) };
    let mu_a = b(&af);
    let mu_b = b(&bf);
    let e_a2 = b(&a2);
    let e_b2 = b(&b2);
    let e_ab = b(&ab);

    const C1: f64 = 6.5025; // (0.01 * 255)²
    const C2: f64 = 58.5225; // (0.03 * 255)²
    let mut total = 0.0f64;
    for ((((&mu_a, &mu_b), &e_a2), &e_b2), &e_ab) in
        mu_a.iter().zip(&mu_b).zip(&e_a2).zip(&e_b2).zip(&e_ab)
    {
        // variances can land tiny-negative on flat regions via E[x²]−μ²
        // cancellation; clamp them (covariance may legitimately be negative)
        let vx = (e_a2 - mu_a * mu_a).max(0.0);
        let vy = (e_b2 - mu_b * mu_b).max(0.0);
        let vxy = e_ab - mu_a * mu_b;
        let num = (2.0 * mu_a * mu_b + C1) * (2.0 * vxy + C2);
        let den = (mu_a * mu_a + mu_b * mu_b + C1) * (vx + vy + C2);
        total += num / den;
    }
    total / n as f64
}

// ────────────────────────────────────────────────────────────────────────────
// Frame-level comparison helpers
// ────────────────────────────────────────────────────────────────────────────

/// BT.601 luma plane from an RGB24 buffer (the profile codec's exact formula,
/// f32 left-fold so luma numbers match the `--profile` report).
fn luma_from_rgb(rgb: &[u8]) -> Vec<u8> {
    rgb.chunks_exact(3)
        .map(|px| {
            let (r, g, b) = (px[0] as f32, px[1] as f32, px[2] as f32);
            ((0.299 * r + 0.587 * g) + 0.114 * b).clamp(0.0, 255.0) as u8
        })
        .collect()
}

/// BT.601 luma plane from a BGR framebuffer (pixel mode).
fn luma_from_bgr(bgr: &[u8]) -> Vec<u8> {
    bgr.chunks_exact(3)
        .map(|px| {
            let (b, g, r) = (px[0] as f32, px[1] as f32, px[2] as f32);
            ((0.299 * r + 0.587 * g) + 0.114 * b).clamp(0.0, 255.0) as u8
        })
        .collect()
}

/// Compare a source RGB24 frame against a reconstructed BGR framebuffer (e.g.
/// the adaptive codec's shown frame) → `(psnr_y, ssim_y, psnr_rgb)`, the same
/// triple the `--profile` report uses. Channel order is aligned internally, so
/// both the luma and full-colour metrics measure the true deviation.
pub fn rgb_vs_bgr(src_rgb: &[u8], recon_bgr: &[u8], w: usize, h: usize) -> (f64, f64, f64) {
    assert_eq!(src_rgb.len(), w * h * 3, "rgb_vs_bgr: source geometry mismatch");
    assert_eq!(
        recon_bgr.len(),
        w * h * 3,
        "rgb_vs_bgr: reconstruction geometry mismatch"
    );
    let src_y = luma_from_rgb(src_rgb);
    let rec_y = luma_from_bgr(recon_bgr);
    let psnr_y = psnr(&src_y, &rec_y);
    let ssim_y = ssim(&src_y, &rec_y, w, h);
    // channel-align source RGB → BGR so the byte-wise PSNR is per-channel
    let src_bgr: Vec<u8> = src_rgb
        .chunks_exact(3)
        .flat_map(|px| [px[2], px[1], px[0]])
        .collect();
    let psnr_rgb = psnr(&src_bgr, recon_bgr);
    (psnr_y, ssim_y, psnr_rgb)
}

/// Whole-image statistics fallback for planes smaller than the 11×11 window.
fn ssim_global(a: &[u8], b: &[u8]) -> f64 {
    let n = a.len() as f64;
    let (mut ma, mut mb) = (0.0f64, 0.0f64);
    for (&x, &y) in a.iter().zip(b) {
        ma += x as f64;
        mb += y as f64;
    }
    ma /= n;
    mb /= n;
    let (mut va, mut vb, mut vab) = (0.0f64, 0.0f64, 0.0f64);
    for (&x, &y) in a.iter().zip(b) {
        let x = x as f64 - ma;
        let y = y as f64 - mb;
        va += x * x;
        vb += y * y;
        vab += x * y;
    }
    va = (va / n).max(0.0);
    vb = (vb / n).max(0.0);
    vab /= n;
    const C1: f64 = 6.5025;
    const C2: f64 = 58.5225;
    let num = (2.0 * ma * mb + C1) * (2.0 * vab + C2);
    let den = (ma * ma + mb * mb + C1) * (va + vb + C2);
    num / den
}

/// Running PSNR/SSIM statistics over an encoded sequence.
///
/// The profile encoder feeds one sample per frame; `asciline-compile --profile`
/// prints the summary after the run.
#[derive(Clone, Debug)]
pub struct QualityStats {
    n: u64,
    psnr_y_sum: f64,
    psnr_y_min: f64,
    psnr_y_max: f64,
    ssim_y_sum: f64,
    ssim_y_min: f64,
    ssim_y_max: f64,
    psnr_rgb_sum: f64,
    psnr_rgb_min: f64,
    psnr_rgb_max: f64,
    /// Frame index with the lowest PSNR-Y (and its metrics) — typically a scene
    /// cut or a burst of motion landing between keyframes.
    worst_idx: u64,
    worst_psnr_y: f64,
    worst_ssim_y: f64,
    worst_psnr_rgb: f64,
}

impl Default for QualityStats {
    fn default() -> Self {
        QualityStats {
            n: 0,
            psnr_y_sum: 0.0,
            psnr_y_min: f64::INFINITY,
            psnr_y_max: f64::NEG_INFINITY,
            ssim_y_sum: 0.0,
            ssim_y_min: f64::INFINITY,
            ssim_y_max: f64::NEG_INFINITY,
            psnr_rgb_sum: 0.0,
            psnr_rgb_min: f64::INFINITY,
            psnr_rgb_max: f64::NEG_INFINITY,
            worst_idx: u64::MAX,
            worst_psnr_y: f64::INFINITY,
            worst_ssim_y: 0.0,
            worst_psnr_rgb: 0.0,
        }
    }
}

impl QualityStats {
    pub fn new() -> QualityStats {
        QualityStats::default()
    }

    /// Record one frame's metrics. `idx` is the frame index (0-based, matching
    /// the wire frame index). `psnr_y`/`ssim_y` compare the luma planes (the
    /// signal the codec actually transforms); `psnr_rgb` compares the
    /// reconstructed display pixels against the source.
    pub fn push(&mut self, idx: u64, psnr_y: f64, ssim_y: f64, psnr_rgb: f64) {
        self.n += 1;
        self.psnr_y_sum += psnr_y;
        self.psnr_y_min = self.psnr_y_min.min(psnr_y);
        self.psnr_y_max = self.psnr_y_max.max(psnr_y);
        self.ssim_y_sum += ssim_y;
        self.ssim_y_min = self.ssim_y_min.min(ssim_y);
        self.ssim_y_max = self.ssim_y_max.max(ssim_y);
        self.psnr_rgb_sum += psnr_rgb;
        self.psnr_rgb_min = self.psnr_rgb_min.min(psnr_rgb);
        self.psnr_rgb_max = self.psnr_rgb_max.max(psnr_rgb);
        if psnr_y < self.worst_psnr_y {
            self.worst_psnr_y = psnr_y;
            self.worst_idx = idx;
            self.worst_ssim_y = ssim_y;
            self.worst_psnr_rgb = psnr_rgb;
        }
    }

    pub fn frames(&self) -> u64 {
        self.n
    }
    pub fn psnr_y_mean(&self) -> f64 {
        self.psnr_y_sum / self.n as f64
    }
    pub fn psnr_y_min(&self) -> f64 {
        self.psnr_y_min
    }
    pub fn psnr_y_max(&self) -> f64 {
        self.psnr_y_max
    }
    pub fn ssim_y_mean(&self) -> f64 {
        self.ssim_y_sum / self.n as f64
    }
    pub fn ssim_y_min(&self) -> f64 {
        self.ssim_y_min
    }
    pub fn ssim_y_max(&self) -> f64 {
        self.ssim_y_max
    }
    pub fn psnr_rgb_mean(&self) -> f64 {
        self.psnr_rgb_sum / self.n as f64
    }
    pub fn psnr_rgb_min(&self) -> f64 {
        self.psnr_rgb_min
    }
    pub fn psnr_rgb_max(&self) -> f64 {
        self.psnr_rgb_max
    }
    pub fn worst_idx(&self) -> u64 {
        self.worst_idx
    }
    pub fn worst_psnr_y(&self) -> f64 {
        self.worst_psnr_y
    }
    pub fn worst_ssim_y(&self) -> f64 {
        self.worst_ssim_y
    }
    pub fn worst_psnr_rgb(&self) -> f64 {
        self.worst_psnr_rgb
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: usize, h: usize) -> Vec<u8> {
        let mut v = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                v[y * w + x] = ((x * 8 + y * 3) & 0xff) as u8;
            }
        }
        v
    }

    fn lcg_noise(seed: u64) -> impl FnMut() -> u8 {
        let mut state = seed;
        move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        }
    }

    #[test]
    fn psnr_identical_is_inf() {
        let a = [100u8; 64];
        assert!(psnr(&a, &a).is_infinite());
        let b = gradient(16, 16);
        assert!(psnr(&b, &b).is_infinite());
    }

    #[test]
    fn psnr_known_value() {
        // constant 100 vs constant 104: MSE = 16 → 10·log10(255²/16) ≈ 36.09 dB
        let a = [100u8; 64];
        let b = [104u8; 64];
        assert!((psnr(&a, &b) - 10.0 * (65025.0f64 / 16.0).log10()).abs() < 1e-9);
    }

    #[test]
    fn psnr_sensitive_to_small_errors() {
        let a = gradient(32, 32);
        let mut b = a.clone();
        b[17] = b[17].wrapping_add(1); // one pixel off by 1
        let p = psnr(&a, &b);
        assert!(p > 40.0 && p.is_finite(), "single-LSB error must still be finite PSNR, got {p}");
    }

    #[test]
    fn ssim_identical_is_one() {
        let a = gradient(32, 32);
        assert!((ssim(&a, &a, 32, 32) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn ssim_symmetric_and_bounded() {
        let a = gradient(32, 32);
        let mut noise = lcg_noise(7);
        let b: Vec<u8> = a.iter().map(|&v| v.wrapping_add(noise() % 65)).collect();
        let sab = ssim(&a, &b, 32, 32);
        let sba = ssim(&b, &a, 32, 32);
        assert!((sab - sba).abs() < 1e-12, "ssim must be symmetric");
        assert!(sab > 0.0 && sab < 1.0, "different planes must score in (0,1), got {sab}");
    }

    #[test]
    fn parallel_blur_matches_serial() {
        // ≥4096 px so blur takes the rayon path; exercise it under an 8-thread
        // pool vs a 1-thread pool (serial fallback) — the report numbers must
        // be identical regardless of thread count.
        let (w, h) = (96usize, 48usize); // 4608 px
        let a = gradient(w, h);
        let mut noise = lcg_noise(11);
        let b: Vec<u8> = a.iter().map(|&v| v.wrapping_add(noise() % 17)).collect();
        let single = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let multi = rayon::ThreadPoolBuilder::new()
            .num_threads(8)
            .build()
            .unwrap();
        let s1 = single.install(|| ssim(&a, &b, w, h));
        let s8 = multi.install(|| ssim(&a, &b, w, h));
        assert!(
            (s1 - s8).abs() < 1e-12,
            "parallel blur must be bit-identical to serial: {s1} vs {s8}"
        );
    }

    #[test]
    fn ssim_orders_by_quality() {
        let a = gradient(32, 32);
        let mut n1 = lcg_noise(1);
        let mut n2 = lcg_noise(1);
        let small: Vec<u8> = a.iter().map(|&v| v.saturating_add(n1() & 3)).collect();
        let big: Vec<u8> = a.iter().map(|&v| v.wrapping_add(n2() % 129)).collect();
        let high = ssim(&a, &small, 32, 32);
        let low = ssim(&a, &big, 32, 32);
        assert!(high > low, "more distortion must lower SSIM: {high} vs {low}");
        assert!(high > 0.9, "tiny noise on a gradient must stay near 1.0, got {high}");
    }

    #[test]
    fn reflect_mirrors_edges() {
        assert_eq!(reflect(0, 8), 0);
        assert_eq!(reflect(7, 8), 7);
        assert_eq!(reflect(-1, 8), 1);
        assert_eq!(reflect(-2, 8), 2);
        assert_eq!(reflect(8, 8), 6);
        assert_eq!(reflect(9, 8), 5);
        assert_eq!(reflect(10, 8), 4);
        assert_eq!(reflect(-7, 8), 7);
        assert_eq!(reflect(-8, 8), 6);
        assert_eq!(reflect(-9, 8), 5);
        assert_eq!(reflect(-14, 8), 0);
        assert_eq!(reflect(14, 8), 0);
    }

    #[test]
    fn rgb_vs_bgr_lossless_is_perfect() {
        // grayscale gradient → RGB24, then the lossless RGB→BGR swap
        let gray = gradient(32, 24);
        let rgb: Vec<u8> = gray
            .iter()
            .flat_map(|&v| [v, v, v])
            .collect();
        let bgr: Vec<u8> = rgb
            .chunks_exact(3)
            .flat_map(|p| [p[2], p[1], p[0]])
            .collect();
        let (py, sy, pr) = rgb_vs_bgr(&rgb, &bgr, 32, 24);
        assert!(py.is_infinite(), "lossless conversion must be PSNR ∞, got {py}");
        assert!((sy - 1.0).abs() < 1e-9, "lossless conversion must be SSIM 1.0, got {sy}");
        assert!(pr.is_infinite(), "lossless conversion must be PSNR-RGB ∞, got {pr}");
    }

    #[test]
    fn rgb_vs_bgr_detects_channel_and_luma_error() {
        let mut rgb = vec![0u8; 16 * 12 * 3];
        for (i, px) in rgb.chunks_exact_mut(3).enumerate() {
            px[0] = (i * 7) as u8; // R
            px[1] = (i * 3) as u8; // G
            px[2] = (i * 11) as u8; // B
        }
        let bgr: Vec<u8> = rgb
            .chunks_exact(3)
            .flat_map(|p| [p[2], p[1], p[0]])
            .collect();
        let (py, _sy, pr) = rgb_vs_bgr(&rgb, &bgr, 16, 12);
        assert!(py.is_infinite() && pr.is_infinite());

        // corrupt one pixel's R channel (bgr[5] is pixel 1's R) by +7
        let mut bad = bgr.clone();
        bad[5] = bad[5].wrapping_add(7);
        let (py2, sy2, pr2) = rgb_vs_bgr(&rgb, &bad, 16, 12);
        assert!(py2.is_finite() && py2 > 20.0, "luma must register the error, got {py2}");
        assert!(pr2.is_finite() && pr2 > 20.0, "RGB must register the error, got {pr2}");
        assert!(sy2 > 0.0 && sy2 < 1.0, "SSIM must dip below 1.0, got {sy2}");
    }

    #[test]
    fn stats_accumulate() {
        let mut s = QualityStats::new();
        assert_eq!(s.frames(), 0);
        s.push(0, 30.0, 0.9, 29.0);
        s.push(1, 34.0, 0.95, 33.0);
        assert_eq!(s.frames(), 2);
        assert!((s.psnr_y_mean() - 32.0).abs() < 1e-9);
        assert!((s.psnr_y_min() - 30.0).abs() < 1e-9);
        assert!((s.psnr_y_max() - 34.0).abs() < 1e-9);
        assert!((s.ssim_y_mean() - 0.925).abs() < 1e-12);
        assert!((s.ssim_y_min() - 0.9).abs() < 1e-12);
        assert!((s.ssim_y_max() - 0.95).abs() < 1e-12);
        assert!((s.psnr_rgb_mean() - 31.0).abs() < 1e-9);
        assert!((s.psnr_rgb_min() - 29.0).abs() < 1e-9);
        assert!((s.psnr_rgb_max() - 33.0).abs() < 1e-9);
        // worst frame = lowest PSNR-Y, with its own metrics
        assert_eq!(s.worst_idx(), 0);
        assert!((s.worst_psnr_y() - 30.0).abs() < 1e-9);
        assert!((s.worst_ssim_y() - 0.9).abs() < 1e-12);
        assert!((s.worst_psnr_rgb() - 29.0).abs() < 1e-9);
        // a tie for the minimum keeps the FIRST occurrence
        s.push(2, 30.0, 0.91, 29.5);
        assert_eq!(s.worst_idx(), 0);
    }
}
