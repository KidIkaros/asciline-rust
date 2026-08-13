//! Decode-only throughput benchmark for `.ascf` clips.
//!
//! Parses the container exactly like `asciline-player` (header, per-record
//! length prefix, tag dispatch to the profile or adaptive decoder) but
//! discards the reconstructed pixels — no terminal or PPM I/O — so the
//! number is the codec's real decode ceiling.
//!
//! This is the evidence tool for the "decode stays real-time on 60 fps
//! footage" claim: a tag-7 (quarter-pel) clip at the source's 60 fps decodes
//! faster than real time if the reported fps ≥ the clip's display rate.
//!
//! ```text
//! cargo run --release --example decode_bench -- samples/drone_profile.ascf
//! ```
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use asciline::codec::{
    CodecDecoder, MAX_DECOMPRESSED, TAG_PROFILE, TAG_PROFILE_AQ, TAG_PROFILE_HPEL, TAG_PROFILE_QPEL,
};
use asciline::profile::ProfileDecoder;
use asciline::protocol::{parse_ascf_header, ASCF_MAGIC_V2};

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: decode_bench <clip.ascf>")?;
    let file = std::fs::File::open(&path).with_context(|| format!("cannot open {path:?}"))?;
    let mut reader = BufReader::new(file);

    let mut header = [0u8; 18];
    reader.read_exact(&mut header[..14])?;
    let is_v2 = &header[..4] == ASCF_MAGIC_V2;
    if is_v2 {
        reader.read_exact(&mut header[14..])?;
    }
    let h = parse_ascf_header(&header)?;
    let cell_bytes = if h.pixel { 3 } else { 4 };
    let header_len: u64 = if is_v2 { 18 } else { 14 };
    reader.seek(SeekFrom::Start(header_len))?;

    let mut decoder = CodecDecoder::new(cell_bytes);
    let mut pdec = ProfileDecoder::new();

    let mut frames = 0u64;
    let mut profile_frames = 0u64;
    let mut bytes = 0u64;
    let t0 = Instant::now();
    let mut buf = Vec::new();
    loop {
        let mut lenb = [0u8; 4];
        match reader.read_exact(&mut lenb) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e).context("read length prefix"),
        }
        let len = u32::from_be_bytes(lenb) as usize;
        if len > MAX_DECOMPRESSED {
            bail!("frame length {len} exceeds MAX_DECOMPRESSED");
        }
        buf.resize(len, 0);
        reader.read_exact(&mut buf)?;
        let is_profile = buf.len() > 4
            && matches!(
                buf[4],
                TAG_PROFILE | TAG_PROFILE_AQ | TAG_PROFILE_HPEL | TAG_PROFILE_QPEL
            );
        if is_profile {
            pdec.decode(&buf)?;
            profile_frames += 1;
        } else {
            decoder.decode(&buf)?;
        }
        frames += 1;
        bytes += len as u64;
    }
    let wall = t0.elapsed().as_secs_f64();
    println!(
        "{}: {} frames ({} profile) | {:.2} MB | {:.1} s | {:.0} fps | {:.1} MB/s",
        path,
        frames,
        profile_frames,
        bytes as f64 / 1024.0 / 1024.0,
        wall,
        frames as f64 / wall,
        bytes as f64 / 1024.0 / 1024.0 / wall
    );
    Ok(())
}
