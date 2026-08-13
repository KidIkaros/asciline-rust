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

/// Seek index: an in-memory table of (frame_index, byte offset) for every
/// forced keyframe in a `.ascf` file, so players can jump to an arbitrary
/// frame by seeking to the nearest keyframe and decoding forward.
///
/// Deliberately NOT a wire change: both codecs guarantee a full, self-
/// describing keyframe exactly when `frame_index % KEYFRAME_INTERVAL == 0`
/// (adaptive `CodecEncoder` and the profile encoder both force `ftype 0`),
/// and the record framing exposes the frame index in its first 4 bytes — so
/// the scan needs no decompression and works on every existing `.ascf` file,
/// including legacy `ASCF` containers. Scene-cut keyframes (extra, beyond the
/// interval) are found too: the scan records any record whose index lands on
/// the interval; a scene-cut keyframe mid-interval is skipped, but the
/// interval keyframes alone are always sufficient to reach any frame.
pub struct AscfSeekIndex {
    /// (frame_index, byte offset of that record's 4-byte length prefix), ascending.
    pub keyframes: Vec<(u32, u64)>,
    /// Total frames seen while scanning (records past the last keyframe).
    pub total_frames: u32,
}

impl AscfSeekIndex {
    /// Scan a `.ascf` stream. `reader` must be positioned just after the
    /// header (caller already consumed it); `first_offset` is the byte offset
    /// of the first frame record. Reads every record's length + 4-byte frame
    /// index and skips the rest — one cheap sequential pass, no decompression.
    pub fn scan(reader: &mut impl std::io::Read, first_offset: u64) -> Result<AscfSeekIndex> {
        use std::io::Read;
        let mut keyframes = Vec::new();
        let mut total_frames = 0u32;
        let mut offset = first_offset;
        let mut len_buf = [0u8; 4];
        let mut idx_buf = [0u8; 4];
        loop {
            if reader.read_exact(&mut len_buf).is_err() {
                break; // clean EOF
            }
            let len = u32::from_be_bytes(len_buf) as usize;
            if len == 0 || len > crate::codec::MAX_DECOMPRESSED {
                bail!("ascf seek scan: bad record length {len} at offset {offset}");
            }
            if len < 4 {
                bail!("ascf seek scan: record too short for frame index at offset {offset}");
            }
            reader.read_exact(&mut idx_buf)?;
            let idx = u32::from_be_bytes(idx_buf);
            if idx % crate::codec::KEYFRAME_INTERVAL == 0 {
                keyframes.push((idx, offset));
            }
            total_frames = total_frames.max(idx + 1);
            // Skip the rest of the record without materializing it.
            std::io::copy(
                &mut reader.by_ref().take((len - 4) as u64),
                &mut std::io::sink(),
            )?;
            offset += 4 + len as u64;
        }
        keyframes.sort_unstable();
        Ok(AscfSeekIndex {
            keyframes,
            total_frames,
        })
    }

    /// The largest keyframe at or before `target` — the decoder can start
    /// there and decode forward to `target`. `None` when the stream is empty.
    pub fn floor(&self, target: u32) -> Option<(u32, u64)> {
        // keyframes are ascending; find the last with frame <= target
        self.keyframes
            .iter()
            .rev()
            .find(|&&(f, _)| f <= target)
            .copied()
    }
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
    fn ascf_seek_index_scan() {
        use crate::codec::KEYFRAME_INTERVAL;
        // Build a tiny synthetic .ascf: header + 100 records whose frame
        // indices are exactly the record order (as the compilers emit), with
        // payloads of varying length so offsets are non-trivial.
        let mut bytes = write_ascf_header(&AscfHeader {
            fps: 30.0,
            mode: 6,
            pixel: true,
            cols: 48,
            rows: 32,
            total_frames: 100,
        });
        for i in 0..100u32 {
            let payload_len = 5 + (i % 7) as usize; // vary the record length
            bytes.extend_from_slice(&(payload_len as u32).to_be_bytes());
            bytes.extend_from_slice(&i.to_be_bytes());
            bytes.resize(bytes.len() + payload_len - 4, 0u8); // dummy tail
        }
        let mut cursor = std::io::Cursor::new(&bytes);
        let mut reader = std::io::BufReader::new(&mut cursor);
        let mut hdr = [0u8; 18];
        use std::io::Read;
        reader.read_exact(&mut hdr).unwrap();
        let idx = AscfSeekIndex::scan(&mut reader, 18).unwrap();
        assert_eq!(idx.total_frames, 100);
        // exactly the interval frames, at offsets that account for the header
        let expected: Vec<(u32, u64)> = (0..100u32)
            .filter(|i| i % KEYFRAME_INTERVAL == 0)
            .map(|i| {
                // offset of record i: header + sum of (4 + len) over records < i
                let before: u64 = (0..i).map(|j| 4 + (5 + (j % 7)) as u64).sum();
                (i, 18 + before)
            })
            .collect();
        assert_eq!(idx.keyframes, expected);
        // floor(): largest keyframe <= target, so any frame is reachable
        assert_eq!(idx.floor(0), Some((0, 18)));
        assert_eq!(idx.floor(47), Some((0, 18)));
        assert_eq!(idx.floor(48), Some((48, idx.keyframes[1].1)));
        assert_eq!(idx.floor(99), Some((96, idx.keyframes.last().unwrap().1)));
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
