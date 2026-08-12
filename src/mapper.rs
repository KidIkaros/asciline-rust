//! Pixel → ASCII mapping, mirroring the original `AsciiMapper`.
//!
//! The framebuffer layouts are byte-identical to the Python project:
//!   - ASCII color modes: 4 bytes per cell `[char_code, R, G, B]`
//!   - Pixel mode:        3 bytes per cell `[B, G, R]`
//!   - Mode 1 (B&W):      text frame `"{index}\n" + rows joined by '\n'`
//!
//! The mapper is parallelized over rows with rayon so mapping large grids
//! scales across cores — a direct contributor to >30fps throughput.

use rayon::prelude::*;

pub use crate::{BLOCK_PALETTE, DEFAULT_PALETTE, FLAT_PALETTE};

/// Named palettes the server filter can switch between (matches `FILTER_PALETTES`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Palette {
    Default,
    Flat,
    Block,
}

impl Palette {
    pub fn chars(self) -> Vec<char> {
        match self {
            Palette::Default => DEFAULT_PALETTE.chars().collect(),
            Palette::Flat => FLAT_PALETTE.chars().collect(),
            Palette::Block => BLOCK_PALETTE.chars().collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Mapper {
    n: usize,
    /// 256-entry LUT: gray value → palette index.
    char_lut: Vec<u8>,
    /// palette index → ASCII byte.
    char_codes: Vec<u8>,
    pub quantize_bits: u8,
}

impl Mapper {
    pub fn new(palette: &[char], quantize_bits: u8) -> Mapper {
        let n = palette.len().max(1);
        let mut char_lut = vec![0u8; 256];
        for (g, slot) in char_lut.iter_mut().enumerate() {
            let idx = (g * (n - 1)) / 255;
            *slot = idx.min(n - 1) as u8;
        }
        let char_codes = palette
            .iter()
            .map(|c| {
                let mut b = [0u8; 4];
                c.encode_utf8(&mut b);
                b[0]
            })
            .collect();
        Mapper {
            n,
            char_lut,
            char_codes,
            quantize_bits,
        }
    }

    pub fn default(quantize_bits: u8) -> Mapper {
        Mapper::new(&Palette::Default.chars(), quantize_bits)
    }

    /// The compiler's gray→index LUT: `floor_divide(gray, max(1, 256 // n))`,
    /// matching `compiler.py` bit-for-bit (distinct from the live server's
    /// proportional `(g * (n-1)) / 255` mapping).
    pub fn compiler_lut(n: usize) -> Vec<u8> {
        let n = n.max(1);
        let divisor = (256 / n).max(1);
        (0..256)
            .map(|g| ((g / divisor) as u8).min(n.saturating_sub(1) as u8))
            .collect()
    }

    /// Build a mapper with an explicit gray→index LUT (used by the compiler).
    pub fn new_with_lut(palette: &[char], char_lut: Vec<u8>, quantize_bits: u8) -> Mapper {
        let n = palette.len().max(1);
        let char_codes = palette
            .iter()
            .map(|c| {
                let mut b = [0u8; 4];
                c.encode_utf8(&mut b);
                b[0]
            })
            .collect();
        Mapper {
            n,
            char_lut,
            char_codes,
            quantize_bits,
        }
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Rec.601 luma from RGB — the character plane (like OpenCV BGR→GRAY).
    #[inline]
    pub fn gray(r: u8, g: u8, b: u8) -> u8 {
        ((77 * r as u32 + 150 * g as u32 + 29 * b as u32) >> 8) as u8
    }

    /// Compute the Rec.601 luma plane for an RGB24 frame (parallel over rows).
    pub fn gray_plane(rgb: &[u8], cols: usize, rows: usize) -> Vec<u8> {
        debug_assert_eq!(rgb.len(), cols * rows * 3);
        rgb.par_chunks_exact(cols * 3)
            .flat_map_iter(|row| {
                row.chunks_exact(3)
                    .map(|px| Self::gray(px[0], px[1], px[2]))
            })
            .collect()
    }

    /// Map an RGB24 frame into a `[char,R,G,B]` framebuffer (`cols*rows*4` bytes).
    pub fn map_ascii(&self, rgb: &[u8], cols: usize, rows: usize, out: &mut [u8]) {
        let gray = Self::gray_plane(rgb, cols, rows);
        self.map_ascii_with_gray(rgb, &gray, cols, rows, out);
    }

    /// Like [`Mapper::map_ascii`] but uses a caller-provided (filtered) gray plane
    /// so runtime filters (sharpen / LUT) can affect the character plane.
    pub fn map_ascii_with_gray(
        &self,
        rgb: &[u8],
        gray: &[u8],
        cols: usize,
        rows: usize,
        out: &mut [u8],
    ) {
        debug_assert_eq!(rgb.len(), cols * rows * 3);
        debug_assert_eq!(gray.len(), cols * rows);
        debug_assert_eq!(out.len(), cols * rows * 4);
        let qb = self.quantize_bits;
        rgb.par_chunks_exact(cols * 3)
            .zip(gray.par_chunks_exact(cols))
            .zip(out.par_chunks_exact_mut(cols * 4))
            .for_each(|((row_in, row_gray), row_out)| {
                for (i, cell) in row_out.chunks_exact_mut(4).enumerate() {
                    let px = &row_in[i * 3..i * 3 + 3];
                    let (r, g, b) = (px[0], px[1], px[2]);
                    // cell[0] is the ASCII byte of the palette character (wire format)
                    cell[0] = self.char_codes[self.char_lut[row_gray[i] as usize] as usize];
                    cell[1] = if qb == 0 { r } else { (r >> qb) << qb };
                    cell[2] = if qb == 0 { g } else { (g >> qb) << qb };
                    cell[3] = if qb == 0 { b } else { (b >> qb) << qb };
                }
            });
    }

    /// Map an RGB24 frame into a BGR framebuffer (`cols*rows*3` bytes) for pixel mode.
    pub fn map_pixel(&self, rgb: &[u8], cols: usize, rows: usize, out: &mut [u8]) {
        debug_assert_eq!(rgb.len(), cols * rows * 3);
        debug_assert_eq!(out.len(), cols * rows * 3);
        let qb = self.quantize_bits;
        rgb.par_chunks_exact(cols * 3)
            .zip(out.par_chunks_exact_mut(cols * 3))
            .for_each(|(row_in, row_out)| {
                for (i, cell) in row_out.chunks_exact_mut(3).enumerate() {
                    let px = &row_in[i * 3..i * 3 + 3];
                    cell[0] = if qb == 0 { px[2] } else { (px[2] >> qb) << qb };
                    cell[1] = if qb == 0 { px[1] } else { (px[1] >> qb) << qb };
                    cell[2] = if qb == 0 { px[0] } else { (px[0] >> qb) << qb };
                }
            });
    }

    /// Build a mode-1 B&W text frame: `"{index}\n" + rows joined by '\n'`.
    pub fn text_frame(&self, rgb: &[u8], cols: usize, rows: usize, frame_index: u32) -> String {
        let gray = Self::gray_plane(rgb, cols, rows);
        self.text_frame_with_gray(&gray, cols, rows, frame_index)
    }

    /// Like [`Mapper::text_frame`] but uses a caller-provided (filtered) gray plane.
    pub fn text_frame_with_gray(
        &self,
        gray: &[u8],
        cols: usize,
        rows: usize,
        frame_index: u32,
    ) -> String {
        let mut s = String::with_capacity(cols * rows + 16);
        s.push_str(&frame_index.to_string());
        s.push('\n');
        for row in gray.chunks_exact(cols) {
            for &g in row {
                let idx = self.char_lut[g as usize] as usize;
                s.push(self.char_codes[idx] as char);
            }
            s.push('\n');
        }
        s.pop(); // drop trailing '\n'
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lut_bounds() {
        let m = Mapper::default(0);
        assert_eq!(m.len(), DEFAULT_PALETTE.chars().count());
        assert!(m.char_lut.iter().all(|&i| (i as usize) < m.n));
        assert_eq!(m.char_lut[0], 0);
        assert_eq!(m.char_lut[255] as usize, m.n - 1);
    }

    #[test]
    fn quantize_masks_low_bits() {
        let qb = 2u8;
        let q = |v: u8| (v >> qb) << qb;
        assert_eq!(q(0b1111_1111), 0b1111_1100);
        assert_eq!(q(0b0000_0011), 0);
    }

    #[test]
    fn map_ascii_layout() {
        let m = Mapper::default(0);
        let cols = 2;
        let rows = 1;
        let rgb: Vec<u8> = vec![255, 0, 0, 0, 255, 0];
        let mut out = vec![0u8; cols * rows * 4];
        m.map_ascii(&rgb, cols, rows, &mut out);
        // cell 0: bright red → the ASCII byte of the palette char, RGB (255,0,0)
        let expect_char = m.char_codes[m.char_lut[Mapper::gray(255, 0, 0) as usize] as usize];
        assert_eq!(out[0], expect_char);
        assert_eq!(&out[1..4], &[255, 0, 0]);
        // cell 1: green
        assert_eq!(&out[5..8], &[0, 255, 0]);
    }

    #[test]
    fn map_pixel_is_bgr() {
        let m = Mapper::default(0);
        let rgb: Vec<u8> = vec![10, 20, 30]; // one pixel
        let mut out = vec![0u8; 3];
        m.map_pixel(&rgb, 1, 1, &mut out);
        assert_eq!(out, vec![30, 20, 10]);
    }
}
