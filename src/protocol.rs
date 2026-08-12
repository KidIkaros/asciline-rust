//! Wire-protocol helpers shared by the server and the compiler.

use anyhow::{bail, Result};

/// Format a float the way Python's `f"{x}"` prints it (used by `INIT:`).
/// Python prints `30.0` for whole floats; `parseFloat` in the client accepts anything.
pub fn py_float(x: f64) -> String {
    if x.fract() == 0.0 {
        format!("{:.1}", x)
    } else {
        let s = format!("{:.6}", x);
        let trimmed = s.trim_end_matches('0').trim_end_matches('.');
        trimmed.to_string()
    }
}

/// Build the `INIT:` handshake text, byte-identical in layout to the Python server:
/// `INIT:{fps}:{mode}:{cols}:{rows}:{pixel}:{queue_idx}:{duration:.3f}:{seek}:{webcam}`
#[allow(clippy::too_many_arguments)]
pub fn init_message(
    fps: f64,
    mode: u8,
    cols: u32,
    rows: u32,
    pixel: bool,
    queue_idx: u32,
    duration: f64,
    seek_secs: f64,
    webcam: bool,
) -> String {
    format!(
        "INIT:{}:{}:{}:{}:{}:{}:{:.3}:{}:{}",
        py_float(fps),
        mode,
        cols,
        rows,
        u8::from(pixel),
        queue_idx,
        duration,
        py_float(seek_secs),
        u8::from(webcam)
    )
}

/// Hard cap on the configured grid: a playlist entry or CLI flag must not be
/// able to ask ffmpeg for a gigantic scale (100k cols → GBs of frame traffic).
/// Real deployments run 200-500 cols; 2000 is far beyond any sane config.
pub const MAX_GRID_COLS: u32 = 2000;
pub const MAX_GRID_ROWS: u32 = 2000;

/// Grid auto-sizing, mirroring `calc_auto_dimensions`.
/// ASCII mode chars are ~2× taller than wide → divide rows by 2; pixel cells are square.
pub fn calc_auto_dimensions(cols: u32, vid_w: u32, vid_h: u32, pixel: bool) -> (u32, u32) {
    let max_rows = if pixel { 1080u32 } else { 300u32 };
    let ratio = vid_w as f64 / (vid_h.max(1) as f64);
    let mut rows = if pixel {
        (cols as f64 / ratio).round() as u32
    } else {
        (cols as f64 / ratio / 2.0).round() as u32
    };
    rows = rows.max(1);
    if rows > max_rows {
        let scale = max_rows as f64 / rows as f64;
        rows = max_rows;
        let cols_scaled = ((cols as f64) * scale).round() as u32;
        return (cols_scaled.max(1), rows);
    }
    (cols, rows)
}

// ────────────────────────────────────────────────────────────────────────────
// .ascf static file format (v2 "ASC2")
// ────────────────────────────────────────────────────────────────────────────

pub const ASCF_MAGIC_V2: &[u8; 4] = b"ASC2";
pub const ASCF_MAGIC_LEGACY: &[u8; 4] = b"ASCF";

#[derive(Debug, Clone)]
pub struct AscfHeader {
    pub fps: f32,
    pub mode: u8,
    pub pixel: bool,
    pub cols: u16,
    pub rows: u16,
    pub total_frames: u32,
}

/// Serialize the 18-byte v2 header.
pub fn write_ascf_header(h: &AscfHeader) -> Vec<u8> {
    let mut out = Vec::with_capacity(18);
    out.extend_from_slice(ASCF_MAGIC_V2);
    out.extend_from_slice(&h.fps.to_be_bytes());
    out.push(h.mode);
    out.push(u8::from(h.pixel));
    out.extend_from_slice(&h.cols.to_be_bytes());
    out.extend_from_slice(&h.rows.to_be_bytes());
    out.extend_from_slice(&h.total_frames.to_be_bytes());
    out
}

/// Parse an .ascf header (18 bytes). Accepts legacy `ASCF` (14-byte) headers too.
pub fn parse_ascf_header(bytes: &[u8]) -> Result<AscfHeader> {
    if bytes.len() < 14 {
        bail!("ascf header too short");
    }
    let magic = &bytes[0..4];
    let (fps, mode, pixel, cols, rows) = if magic == ASCF_MAGIC_V2 {
        if bytes.len() < 18 {
            bail!("ASC2 header truncated");
        }
        (
            f32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            bytes[8],
            bytes[9] == 1,
            u16::from_be_bytes([bytes[10], bytes[11]]),
            u16::from_be_bytes([bytes[12], bytes[13]]),
        )
    } else if magic == ASCF_MAGIC_LEGACY {
        (
            f32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            bytes[8],
            bytes[9] == 1,
            u16::from_be_bytes([bytes[10], bytes[11]]),
            u16::from_be_bytes([bytes[12], bytes[13]]),
        )
    } else {
        bail!("invalid ascf magic");
    };
    let total_frames = if magic == ASCF_MAGIC_V2 && bytes.len() >= 18 {
        u32::from_be_bytes([bytes[14], bytes[15], bytes[16], bytes[17]])
    } else {
        0
    };
    Ok(AscfHeader {
        fps,
        mode,
        pixel,
        cols,
        rows,
        total_frames,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn py_float_formatting() {
        assert_eq!(py_float(30.0), "30.0");
        assert_eq!(py_float(23.976), "23.976");
        assert_eq!(py_float(60.0), "60.0");
    }

    #[test]
    fn init_format() {
        let s = init_message(30.0, 6, 240, 67, false, 0, 10.5, 0.0, false);
        assert_eq!(s, "INIT:30.0:6:240:67:0:0:10.500:0.0:0");
    }

    #[test]
    fn ascf_header_roundtrip() {
        let h = AscfHeader {
            fps: 30.0,
            mode: 6,
            pixel: false,
            cols: 240,
            rows: 67,
            total_frames: 1234,
        };
        let bytes = write_ascf_header(&h);
        assert_eq!(bytes.len(), 18);
        let parsed = parse_ascf_header(&bytes).unwrap();
        assert_eq!(parsed.fps, 30.0);
        assert_eq!(parsed.mode, 6);
        assert!(!parsed.pixel);
        assert_eq!(parsed.cols, 240);
        assert_eq!(parsed.rows, 67);
        assert_eq!(parsed.total_frames, 1234);
    }

    #[test]
    fn auto_dims() {
        // 1920x1080, 240 cols ascii → 240x68 (1920/1080=1.777; 240/1.777/2=67.5→68)
        let (c, r) = calc_auto_dimensions(240, 1920, 1080, false);
        assert_eq!(c, 240);
        assert_eq!(r, 68);
        // pixel: 240/1.777 = 135
        let (c, r) = calc_auto_dimensions(240, 1920, 1080, true);
        assert_eq!(c, 240);
        assert_eq!(r, 135);
    }
}
