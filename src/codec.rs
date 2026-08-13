//! Adaptive per-frame codec — a faithful Rust port of `codec.py` / `codec.js`.
//!
//! Wire format per frame message:
//! ```text
//! [4 bytes: frame_index, big-endian u32][1 byte: tag][payload]
//! tag 0 RAW      payload = framebuffer bytes
//! tag 1 ZLIB     payload = zlib(framebuffer bytes)
//! tag 2 DELTA    payload = zlib(indices[uint32 LE] ++ changed cell values)
//! tag 3 RLE_FULL payload = zlib(runs: [uint16 count LE][cell bytes]...)
//! ```
//!
//! The encoder races candidate encodings per frame and ships the smallest:
//! DELTA when few cells changed, full-frame ZLIB vs RLE+ZLIB otherwise, and
//! falls back to RAW when compression can't beat the raw frame. A keyframe is
//! forced every 48 frames so late joiners / dropped packets resync. Lossy
//! temporal delta (`tolerance`) skips re-sending cells whose channels drifted by
//! less than the tolerance — character plane exact in ASCII mode, every channel
//! toleranced in pixel mode (matching `codec.py`).

use std::io::{Read, Write};

use anyhow::{anyhow, bail, Result};

pub const TAG_RAW: u8 = 0;
pub const TAG_ZLIB: u8 = 1;
pub const TAG_DELTA: u8 = 2;
pub const TAG_RLE_FULL: u8 = 3;
pub const TAG_PROFILE: u8 = 4;
/// Tag 5: the lossy DCT profile with per-block adaptive quantization (AQ).
/// Same payload as tag 4 except the keyframe header carries an extra
/// `[aq_levels u8]` byte and the luma plane leads with a packed per-block
/// quant-scale map. Self-describing at keyframes, so unknown decoders can
/// still sync; see `src/profile.rs` and `web/PROFILE.md`.
pub const TAG_PROFILE_AQ: u8 = 5;
/// Tag 6: the lossy DCT profile with half-pixel motion compensation. Same
/// keyframe header as tag 5 (`[QF][cols u16][rows u16][aq_levels?]`); the tag
/// itself signals that inter-frame motion vectors are half-pel units and the
/// decoder interpolates the reference bilinearly (luma plane only, like the
/// integer MVs it extends). A strict superset of tag 5: even half-pel
/// displacements are plain integer motion, so the encoder is never worse.
pub const TAG_PROFILE_HPEL: u8 = 6;
/// Tag 7: the lossy DCT profile with quarter-pixel motion compensation
/// (H.264-style). Same keyframe header as tag 6 (`[QF][cols u16][rows u16]
/// [aq_levels u8]`, always present); the tag signals that inter-frame motion
/// vectors are quarter-pel units and the decoder interpolates the luma
/// reference with H.264's 6-tap half-pel filter + bilinear quarter-pel step
/// (luma plane only, identical integer math in Rust and web/codec.js). A
/// strict superset of tag 6 — every half-pel displacement is representable in
/// quarter-pel units, and the encoder falls back to half-pel/integer vectors
/// whenever sub-pixel motion does not help — so it is never worse.
pub const TAG_PROFILE_QPEL: u8 = 7;

pub const KEYFRAME_INTERVAL: u32 = 48;
pub const DEFAULT_LEVEL: u32 = 3;

/// Fraction of changed cells above which DELTA loses (don't even build it).
const DELTA_MAX_FRAC: f64 = 0.60;
/// Fraction below which full-frame zlib loses (don't race it).
const ZLIB_MIN_FRAC: f64 = 0.10;

// ────────────────────────────────────────────────────────────────────────────
// zlib helpers (flate2, pure-Rust backend)
// ────────────────────────────────────────────────────────────────────────────

pub fn zlib_compress(data: &[u8], level: u32) -> Vec<u8> {
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(level));
    enc.write_all(data).expect("zlib write");
    enc.finish().expect("zlib finish")
}

/// Cap on a single decompressed frame payload. Legitimate frames are at most a
/// few MB (a 2048×2048 pixel grid is ~12.6 MB raw); 64 MiB bounds any real
/// stream while stopping decompression bombs (a few KB of zlib can expand to
/// gigabytes) from exhausting memory in the decoders.
pub const MAX_DECOMPRESSED: usize = 64 << 20;

/// Inflate with a hard size cap. Checksum/truncation errors still surface
/// exactly as before (we read to `Ok(0)`, then stop); the only behavior change
/// is that output over `MAX_DECOMPRESSED` fails instead of allocating forever.
pub fn zlib_decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut dec = flate2::read::ZlibDecoder::new(data);
    let mut out = Vec::with_capacity(data.len().saturating_mul(4).min(1 << 20));
    let mut buf = [0u8; 65536];
    loop {
        match dec.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() > MAX_DECOMPRESSED {
                    bail!("zlib output exceeds {} bytes", MAX_DECOMPRESSED);
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(out)
}

// ────────────────────────────────────────────────────────────────────────────
// RLE
// ────────────────────────────────────────────────────────────────────────────

/// Run-length encode a whole framebuffer into `[u16 count LE][cell bytes]` runs.
/// Runs longer than 65535 are split, like the Python fallback path.
fn rle_encode(frame: &[u8], cell_bytes: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let n_cells = frame.len() / cell_bytes;
    let mut i = 0usize;
    while i < n_cells {
        let cell = &frame[i * cell_bytes..(i + 1) * cell_bytes];
        let mut j = i + 1;
        while j < n_cells && j - i < 65535 && &frame[j * cell_bytes..(j + 1) * cell_bytes] == cell {
            j += 1;
        }
        out.extend_from_slice(&((j - i) as u16).to_le_bytes()); // u16 count, little-endian
        out.extend_from_slice(cell);
        i = j;
    }
    out
}

fn write_message(out: &mut Vec<u8>, frame_index: u32, tag: u8, payload: &[u8]) {
    out.extend_from_slice(&frame_index.to_be_bytes());
    out.push(tag);
    out.extend_from_slice(payload);
}

// ────────────────────────────────────────────────────────────────────────────
// Encoder
// ────────────────────────────────────────────────────────────────────────────

/// Stateful adaptive encoder. `cell_bytes` is 4 (ASCII colour) or 3 (pixel).
pub struct CodecEncoder {
    pub level: u32,
    pub tolerance: u32,
    pub keyframe_interval: u32,
    cell_bytes: usize,
    /// The previously-SHOWN frame (what the client currently displays).
    prev: Option<Vec<u8>>,
}

impl CodecEncoder {
    pub fn new(cell_bytes: usize, level: u32, tolerance: u32) -> CodecEncoder {
        CodecEncoder {
            level,
            tolerance,
            keyframe_interval: KEYFRAME_INTERVAL,
            cell_bytes,
            prev: None,
        }
    }

    pub fn reset(&mut self) {
        self.prev = None;
    }

    /// How many cells differ (same predicate as the DELTA body builder).
    fn diff_count(&self, frame: &[u8]) -> usize {
        let prev = self.prev.as_ref().expect("prev set");
        let cell = self.cell_bytes;
        frame
            .chunks_exact(cell)
            .zip(prev.chunks_exact(cell))
            .filter(|(f, p)| self.cell_differs(f, p))
            .count()
    }

    /// Full-frame encoding: race ZLIB vs RLE+ZLIB, fall back to RAW.
    fn encode_full(&self, frame: &[u8], frame_index: u32) -> Vec<u8> {
        let raw = frame;
        let z_raw = zlib_compress(raw, self.level);
        let rle = rle_encode(frame, self.cell_bytes);
        let z_rle = zlib_compress(&rle, self.level);

        let mut msg = Vec::with_capacity(5 + z_raw.len());
        if z_rle.len() < z_raw.len() && z_rle.len() < raw.len() {
            write_message(&mut msg, frame_index, TAG_RLE_FULL, &z_rle);
        } else if z_raw.len() < raw.len() {
            write_message(&mut msg, frame_index, TAG_ZLIB, &z_raw);
        } else {
            write_message(&mut msg, frame_index, TAG_RAW, raw);
        }
        msg
    }

    /// Encode one framebuffer. Returns the wire message and advances internal state.
    pub fn encode(&mut self, frame: &[u8], frame_index: u32) -> Vec<u8> {
        let keyframe = self.prev.is_none() || frame_index.is_multiple_of(self.keyframe_interval);
        let same_size = self
            .prev
            .as_ref()
            .map(|p| p.len() == frame.len())
            .unwrap_or(false);

        if keyframe || !same_size {
            let msg = self.encode_full(frame, frame_index);
            self.prev = Some(frame.to_vec());
            return msg;
        }

        let n_cells = frame.len() / self.cell_bytes;
        let changed = self.diff_count(frame);
        let frac = changed as f64 / n_cells.max(1) as f64;

        // Candidate encodings: (tag, payload). The delta body is kept separately
        // so the SHOWN frame can be patched without re-inflating.
        let mut best: Option<(u8, Vec<u8>)> = None;
        let mut delta_body: Option<Vec<u8>> = None;
        if frac < DELTA_MAX_FRAC {
            // DELTA wire body: [k × u32 LE cell indices][k × cell bytes] — the
            // block layout codec.py (`ci.tobytes() + vals.tobytes()`) and
            // codec.js (valuesOffset = k*4) both use. Interleaving would corrupt
            // every delta on every decoder.
            let mut indices = Vec::with_capacity(changed * 4);
            let mut values = Vec::with_capacity(changed * self.cell_bytes);
            for (i, (f, p)) in frame
                .chunks_exact(self.cell_bytes)
                .zip(self.prev.as_ref().unwrap().chunks_exact(self.cell_bytes))
                .enumerate()
            {
                if self.cell_differs(f, p) {
                    indices.extend_from_slice(&(i as u32).to_le_bytes());
                    values.extend_from_slice(f);
                }
            }
            indices.extend_from_slice(&values);
            let body = indices;
            let delta = zlib_compress(&body, self.level);
            best = Some((TAG_DELTA, delta));
            delta_body = Some(body);
        }

        // Full-frame candidates still race when they might win.
        if frac >= ZLIB_MIN_FRAC || best.is_none() {
            let z_raw = zlib_compress(frame, self.level);
            let rle = rle_encode(frame, self.cell_bytes);
            let z_rle = zlib_compress(&rle, self.level);
            let full = if z_rle.len() < z_raw.len() {
                (TAG_RLE_FULL, z_rle)
            } else {
                (TAG_ZLIB, z_raw)
            };
            match &best {
                Some((_, b)) if b.len() <= full.1.len() => {}
                _ => best = Some(full),
            }
        }

        // Never exceed the raw frame (zlib can inflate incompressible data).
        let (tag, payload) = match best {
            Some((t, p)) if p.len() < frame.len() => (t, p),
            _ => (TAG_RAW, frame.to_vec()),
        };

        // Track the SHOWN frame: deltas patch prev, full frames replace it.
        if tag == TAG_DELTA {
            if let Some(body) = delta_body {
                let prev = self.prev.as_mut().unwrap();
                let cell = self.cell_bytes;
                let k = body.len() / (4 + cell);
                for j in 0..k {
                    let off = j * 4;
                    let cell_idx = u32::from_le_bytes([
                        body[off],
                        body[off + 1],
                        body[off + 2],
                        body[off + 3],
                    ]) as usize;
                    let src = k * 4 + j * cell;
                    let dst = cell_idx * cell;
                    prev[dst..dst + cell].copy_from_slice(&body[src..src + cell]);
                }
            }
        } else {
            self.prev = Some(frame.to_vec());
        }

        let mut msg = Vec::with_capacity(5 + payload.len());
        write_message(&mut msg, frame_index, tag, &payload);
        msg
    }

    /// Whether a cell differs for DELTA purposes.
    ///
    /// ASCII colour mode (4 bytes): channel 0 is the character plane — always
    /// exact; tolerance applies to the colour channels. Pixel mode (3 bytes):
    /// tolerance applies to every channel, matching `codec.py`'s C==3 branch
    /// (`np.any(diff > tolerance, axis=2)`).
    #[inline]
    fn cell_differs(&self, f: &[u8], p: &[u8]) -> bool {
        let tol = self.tolerance as i16;
        let start = if self.cell_bytes == 4 { 1 } else { 0 };
        if start == 1 && f[0] != p[0] {
            return true;
        }
        for c in start..self.cell_bytes {
            if (f[c] as i16 - p[c] as i16).abs() > tol {
                return true;
            }
        }
        false
    }

    pub fn prev(&self) -> Option<&[u8]> {
        self.prev.as_deref()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Decoder (mirrors codec.js `makeDecoder`)
// ────────────────────────────────────────────────────────────────────────────

/// Stateful adaptive decoder. Deltas patch the previous frame, so messages
/// must be fed in arrival order (the browser does this via a sequential queue).
pub struct CodecDecoder {
    cell_bytes: usize,
    prev: Option<Vec<u8>>,
}

impl CodecDecoder {
    pub fn new(cell_bytes: usize) -> CodecDecoder {
        CodecDecoder {
            cell_bytes,
            prev: None,
        }
    }

    pub fn reset(&mut self) {
        self.prev = None;
    }

    /// Decode one wire message → `(frame_index, full framebuffer)`.
    pub fn decode(&mut self, msg: &[u8]) -> Result<(u32, Vec<u8>)> {
        if msg.len() < 5 {
            bail!("frame message too short");
        }
        let frame_index = u32::from_be_bytes([msg[0], msg[1], msg[2], msg[3]]);
        let tag = msg[4];
        let payload = &msg[5..];
        let cell = self.cell_bytes;

        let frame: Vec<u8> = match tag {
            TAG_RAW => payload.to_vec(),
            TAG_ZLIB => zlib_decompress(payload)?,
            TAG_DELTA => {
                let body = zlib_decompress(payload)?;
                let k = body.len() / (4 + cell);
                let mut frame = self
                    .prev
                    .clone()
                    .ok_or_else(|| anyhow!("DELTA before any keyframe"))?;
                for j in 0..k {
                    let off = j * 4;
                    let cell_idx = u32::from_le_bytes([
                        body[off],
                        body[off + 1],
                        body[off + 2],
                        body[off + 3],
                    ]) as usize;
                    let src = k * 4 + j * cell;
                    let dst = cell_idx * cell;
                    if dst + cell > frame.len() {
                        bail!("DELTA index out of range");
                    }
                    frame[dst..dst + cell].copy_from_slice(&body[src..src + cell]);
                }
                frame
            }
            TAG_RLE_FULL => {
                let body = zlib_decompress(payload)?;
                let mut frame = Vec::new();
                let mut off = 0usize;
                while off < body.len() {
                    // Every run must be fully present — slicing without this
                    // check panics on a truncated final run (`off + 2 + cell`
                    // past the end), which a malformed frame used to trigger.
                    if off + 2 + cell > body.len() {
                        bail!("RLE run truncated at offset {off}");
                    }
                    let count = u16::from_le_bytes([body[off], body[off + 1]]) as usize;
                    let val = &body[off + 2..off + 2 + cell];
                    // Cap the expanded output too: a few run headers can claim
                    // 65535 cells each, i.e. a tiny body → hundreds of MB.
                    if frame.len().saturating_add(count * cell) > MAX_DECOMPRESSED {
                        bail!("RLE frame exceeds {} bytes", MAX_DECOMPRESSED);
                    }
                    for _ in 0..count {
                        frame.extend_from_slice(val);
                    }
                    off += 2 + cell;
                }
                frame
            }
            TAG_PROFILE => bail!("tag 4 (lossy DCT profile) is not supported by this decoder"),
            other => bail!("unknown codec tag {other}"),
        };

        self.prev = Some(frame.clone());
        Ok((frame_index, frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapper::Mapper;

    fn fake_frame(seed: u8, cols: usize, rows: usize, cell: usize) -> Vec<u8> {
        let mut f = vec![0u8; cols * rows * cell];
        for (i, b) in f.iter_mut().enumerate() {
            *b = seed.wrapping_mul(31).wrapping_add(i as u8);
        }
        f
    }

    #[test]
    fn rle_roundtrip() {
        let frame = fake_frame(7, 40, 10, 4);
        let rle = rle_encode(&frame, 4);
        // decode manually
        let mut out = Vec::new();
        let mut off = 0;
        while off < rle.len() {
            let count = u16::from_le_bytes([rle[off], rle[off + 1]]) as usize;
            out.extend_from_slice(&rle[off + 2..off + 6].repeat(count));
            off += 6;
        }
        assert_eq!(out, frame);
    }

    #[test]
    fn rle_splits_long_runs() {
        // 100k identical cells → split into 65535-max runs; still decodes exactly
        let cell = [1u8, 2, 3, 4];
        let frame = cell.repeat(100_000);
        let rle = rle_encode(&frame, 4);
        // every run count <= 65535
        let mut off = 0;
        let mut out = Vec::new();
        while off < rle.len() {
            let count = u16::from_le_bytes([rle[off], rle[off + 1]]) as usize;
            assert!(count <= 65535);
            out.extend_from_slice(&rle[off + 2..off + 6].repeat(count));
            off += 2 + 4;
        }
        assert_eq!(out, frame);
    }

    #[test]
    fn roundtrip_static_frame() {
        let cols = 40;
        let rows = 12;
        let mut enc = CodecEncoder::new(4, DEFAULT_LEVEL, 0);
        let mut dec = CodecDecoder::new(4);
        let frame = fake_frame(3, cols, rows, 4);
        for i in 0..3 {
            let msg = enc.encode(&frame, i);
            let (idx, out) = dec.decode(&msg).unwrap();
            assert_eq!(idx, i);
            assert_eq!(out, frame);
        }
    }

    #[test]
    fn delta_wire_format_roundtrips() {
        // Static background + a small moving blob: only a handful of cells change
        // per frame, so the DELTA encoding wins the race and the wire path is
        // exercised end-to-end (the interleaved-vs-block bug used to corrupt it).
        let cols = 32usize;
        let rows = 16usize;
        let cell = 3usize; // pixel mode
        let mut enc = CodecEncoder::new(cell, DEFAULT_LEVEL, 0);
        let mut dec = CodecDecoder::new(cell);
        let mut delta_seen = false;
        for i in 0..80usize {
            let mut f = vec![0u8; cols * rows * cell];
            for (j, px) in f.chunks_exact_mut(cell).enumerate() {
                let x = j % cols;
                let y = j / cols;
                px.copy_from_slice(&[((x * 7 + y * 3) & 0xff) as u8, 128, 96]);
            }
            let cx = 2 + (i % (cols - 4)) as i32;
            for dy in -1..=1i32 {
                for dx in -1..=1i32 {
                    let (x, y) = (cx + dx, 5 + dy);
                    let off = ((y * cols as i32 + x) as usize) * cell;
                    f[off] = 255;
                    f[off + 1] = 255;
                    f[off + 2] = 0;
                }
            }
            let msg = enc.encode(&f, i as u32);
            if msg[4] == TAG_DELTA {
                delta_seen = true;
            }
            let (_, shown) = dec.decode(&msg).unwrap();
            assert_eq!(shown, f, "frame {i} must round-trip exactly through DELTA");
        }
        assert!(
            delta_seen,
            "static+blob content must actually emit DELTA frames"
        );
    }

    #[test]
    fn roundtrip_motion_with_deltas() {
        let cols = 32;
        let rows = 16;
        let mut enc = CodecEncoder::new(4, DEFAULT_LEVEL, 0);
        let mut dec = CodecDecoder::new(4);
        let mut delta_seen = false;
        for i in 0..100usize {
            let mut f = fake_frame(1, cols, rows, 4);
            // move a 2×2 "ball": only a few char cells change per frame, so
            // the DELTA encoding wins (all-distinct backgrounds keep it real)
            let bx = 2 + (i % (cols - 4));
            for (dy, dx) in [(0usize, 0usize), (0, 1), (1, 0), (1, 1)] {
                let c = (bx + dx + i) % cols;
                let off = ((3 + dy) * cols + bx + dx) * 4;
                f[off] = c as u8;
            }
            let msg = enc.encode(&f, i as u32);
            if msg[4] == TAG_DELTA {
                delta_seen = true;
            }
            let (_, out) = dec.decode(&msg).unwrap();
            assert_eq!(out, f, "frame {i} must round-trip exactly");
        }
        assert!(
            delta_seen,
            "moving 2x2 ball must actually emit DELTA frames"
        );
    }

    #[test]
    fn pixel_tolerance_applies_to_all_channels() {
        // codec.py's C==3 branch applies tolerance to EVERY channel — a B-only
        // drift within tolerance must NOT force a re-send (a B-exact rule would).
        let cols = 8;
        let rows = 4;
        let cell = 3;
        let mut enc = CodecEncoder::new(cell, DEFAULT_LEVEL, 8);
        let mut dec = CodecDecoder::new(cell);
        let mut f = vec![10u8; cols * rows * cell];
        let msg0 = enc.encode(&f, 0);
        dec.decode(&msg0).unwrap();
        // drift channel 0 (B) by 5, within tolerance 8; G/R untouched
        for px in f.chunks_exact_mut(cell) {
            px[0] = px[0].wrapping_add(5);
        }
        let msg = enc.encode(&f, 1);
        assert_eq!(
            msg[4], TAG_DELTA,
            "sub-tolerance drift must encode as DELTA"
        );
        let (_, shown) = dec.decode(&msg).unwrap();
        assert_eq!(
            shown,
            vec![10u8; cols * rows * cell],
            "B drift within tolerance must be skipped (shown stays at the old frame)"
        );
    }

    #[test]
    fn roundtrip_pixel_mode() {
        let cols = 24;
        let rows = 9;
        let mut enc = CodecEncoder::new(3, DEFAULT_LEVEL, 0);
        let mut dec = CodecDecoder::new(3);
        let frame = fake_frame(9, cols, rows, 3);
        let msg = enc.encode(&frame, 0);
        let (_, out) = dec.decode(&msg).unwrap();
        assert_eq!(out, frame);
    }

    #[test]
    fn tolerance_keeps_char_plane_exact() {
        let cols = 20;
        let rows = 5;
        // build a real mapper-style frame so chars are stable
        let m = Mapper::default(0);
        let rgb = vec![128u8; cols * rows * 3];
        let mut fb = vec![0u8; cols * rows * 4];
        m.map_ascii(&rgb, cols, rows, &mut fb);
        let mut enc = CodecEncoder::new(4, DEFAULT_LEVEL, 8);
        let mut dec = CodecDecoder::new(4);
        let msg0 = enc.encode(&fb, 0);
        dec.decode(&msg0).unwrap(); // prime the decoder with the keyframe
                                    // jitter colours within tolerance (<=8): no cell crosses the drift budget,
                                    // so the wire gets a zero-cell DELTA and the client keeps its previous view.
        for i in (1..fb.len()).step_by(4) {
            fb[i] = fb[i].saturating_add(5);
        }
        let msg = enc.encode(&fb, 1);
        assert_eq!(
            msg[4], TAG_DELTA,
            "sub-tolerance colour drift must encode as an empty DELTA"
        );
        let (_, shown) = dec.decode(&msg).unwrap();
        // chars stay exact against the true frame; colours lag by <= 8 per channel
        for (s, t) in shown.chunks_exact(4).zip(fb.chunks_exact(4)) {
            assert_eq!(s[0], t[0], "character plane must stay exact");
            for c in 1..4 {
                assert!(i16::abs(s[c] as i16 - t[c] as i16) <= 8);
            }
        }
    }
}
