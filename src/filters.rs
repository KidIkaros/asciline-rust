//! Runtime filters, ported from the server's `_build_gray_lut` + sharpness path.
//!
//! The gray LUT is a 256-entry table rebuilt only when a filter value changes,
//! then applied with a single pass over the gray plane. Sharpen is a 3×3
//! unsharp-mask convolution matching `cv2.filter2D` (reflect-101 borders).

/// Build a 256-entry gray LUT applying brightness, contrast, gamma, invert.
/// Returns `None` when every parameter is identity (skip the LUT call entirely).
pub fn build_gray_lut(
    contrast: f64,
    gamma: f64,
    brightness: f64,
    invert: bool,
) -> Option<[u8; 256]> {
    let identity = contrast == 1.0 && gamma == 1.0 && brightness == 0.0 && !invert;
    if identity {
        return None;
    }
    let mut lut = [0u8; 256];
    let mut v = [0f64; 256];
    for (i, slot) in v.iter_mut().enumerate() {
        *slot = i as f64;
    }
    // 1. brightness — additive shift in 0-255 space
    if brightness != 0.0 {
        let off = brightness * 2.55;
        for slot in v.iter_mut() {
            *slot = (*slot + off).clamp(0.0, 255.0);
        }
    }
    // 2. contrast — linear stretch around midpoint 128
    if contrast != 1.0 {
        for slot in v.iter_mut() {
            *slot = (128.0 + contrast * (*slot - 128.0)).clamp(0.0, 255.0);
        }
    }
    // 3. gamma — power curve
    if gamma != 1.0 {
        let inv = 1.0 / gamma;
        for slot in v.iter_mut() {
            *slot = (255.0 * ((*slot).max(0.0) / 255.0).powf(inv)).clamp(0.0, 255.0);
        }
    }
    // 4. invert
    for (i, slot) in lut.iter_mut().enumerate() {
        let val = if invert { 255.0 - v[i] } else { v[i] };
        *slot = val as u8;
    }
    Some(lut)
}

/// Apply a gray LUT in place.
pub fn apply_lut(gray: &mut [u8], lut: &[u8; 256]) {
    for g in gray.iter_mut() {
        *g = lut[*g as usize];
    }
}

/// Reflect-101 border index (cv2 BORDER_REFLECT_101).
#[inline]
fn reflect101(i: i32, n: i32) -> usize {
    if i < 0 {
        (-i).min(n - 1) as usize
    } else if i >= n {
        (2 * n - 2 - i).clamp(0, n - 1) as usize
    } else {
        i as usize
    }
}

/// Apply the 3×3 unsharp-mask kernel in place. `alpha = sharpness * 0.5`.
pub fn sharpen_gray(gray: &mut [u8], cols: usize, rows: usize, alpha: f32) {
    if cols == 0 || rows == 0 {
        return;
    }
    let k = [
        -alpha,
        -alpha,
        -alpha,
        -alpha,
        1.0 + 8.0 * alpha,
        -alpha,
        -alpha,
        -alpha,
        -alpha,
    ];
    let mut tmp = vec![0f32; cols * rows];
    let (n, m) = (rows as i32, cols as i32);
    for y in 0..rows {
        for x in 0..cols {
            let mut acc = 0f32;
            for (ky, dy) in [-1i32, 0, 1].iter().enumerate() {
                let sy = reflect101(y as i32 + dy, n);
                for (kx, dx) in [-1i32, 0, 1].iter().enumerate() {
                    let sx = reflect101(x as i32 + dx, m);
                    acc += k[ky * 3 + kx] * gray[sy * cols + sx] as f32;
                }
            }
            tmp[y * cols + x] = acc;
        }
    }
    for (g, t) in gray.iter_mut().zip(tmp.iter()) {
        *g = t.clamp(0.0, 255.0) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_none() {
        assert!(build_gray_lut(1.0, 1.0, 0.0, false).is_none());
        assert!(build_gray_lut(1.0, 1.0, 0.0, true).is_some());
    }

    #[test]
    fn invert_flips() {
        let lut = build_gray_lut(1.0, 1.0, 0.0, true).unwrap();
        assert_eq!(lut[0], 255);
        assert_eq!(lut[255], 0);
    }

    #[test]
    fn sharpen_flat_frame_is_unchanged() {
        let mut gray = vec![100u8; 6 * 6];
        sharpen_gray(&mut gray, 6, 6, 1.0);
        assert!(gray.iter().all(|&g| g == 100));
    }
}
