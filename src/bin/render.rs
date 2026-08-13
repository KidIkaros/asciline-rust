//! asciline-render — headless `.ascf` → PPM image renderer.
//!
//! Decodes a compiled `.ascf` clip with the same codecs as `asciline-player`
//! and writes every frame as a P6 PPM (RGB), which ffmpeg converts to
//! PNG/mp4/GIF:
//!
//! ```sh
//! asciline-render clip.ascf --out frames
//! ffmpeg -framerate 30 -i frames/frame_%06d.ppm -vf scale=640:-2 out.mp4
//! ```
//!
//! Rasterization: pixel cells are solid `scale`×`scale` blocks; ASCII cells
//! draw the palette character in its cell colour on black, using the
//! public-domain 8×8 `font8x8` glyphs (glyph pixels are `scale`×`scale`).
//! `experiments/make_samples.sh` uses this to generate the README
//! format-evidence stills and the animated demo GIF.

use std::fs;
use std::io::{BufReader, Read};
use std::path::Path;

use anyhow::{bail, Context, Result};
use asciline::codec::{CodecDecoder, MAX_DECOMPRESSED, TAG_PROFILE};
use asciline::profile::ProfileDecoder;
use asciline::protocol::{parse_ascf_header, ASCF_MAGIC_V2};
use clap::Parser;
use font8x8::UnicodeFonts;

#[derive(Parser, Debug)]
#[command(
    name = "asciline-render",
    version,
    about = "Headless .ascf -> PPM frame renderer (pixel blocks / ASCII glyphs)"
)]
struct Args {
    /// Compiled .ascf clip to render.
    ascf: String,
    /// Output directory for frame_%06d.ppm files (created if missing).
    #[arg(long, default_value = "render_out")]
    out: String,
    /// Cell size in pixels (solid block for pixel mode, glyph scale for ASCII).
    #[arg(long)]
    scale: Option<u8>,
    /// Render only this one frame (0-based), then stop.
    #[arg(long)]
    frame: Option<u32>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let file = fs::File::open(&args.ascf).with_context(|| format!("open {}", args.ascf))?;
    let mut reader = BufReader::new(file);

    let mut header_bytes = [0u8; 18];
    reader
        .read_exact(&mut header_bytes[..14])
        .context("read ascf header")?;
    let is_v2 = &header_bytes[..4] == ASCF_MAGIC_V2;
    if is_v2 {
        reader
            .read_exact(&mut header_bytes[14..])
            .context("read v2 header tail")?;
    }
    let header = parse_ascf_header(&header_bytes)?;
    let cell_bytes = if header.pixel { 3 } else { 4 };
    let (cols, rows) = (header.cols as usize, header.rows as usize);
    let scale = args.scale.unwrap_or(if header.pixel { 8 } else { 2 }) as usize;
    let (img_w, img_h) = (
        cols * cell_px(scale, header.pixel),
        rows * cell_px(scale, header.pixel),
    );
    fs::create_dir_all(&args.out).with_context(|| format!("create {}", args.out))?;

    let mut adec = CodecDecoder::new(cell_bytes);
    let mut pdec = ProfileDecoder::new();
    let mut n = 0u32;

    loop {
        let mut len_buf = [0u8; 4];
        if reader.read_exact(&mut len_buf).is_err() {
            break; // clean EOF
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len == 0 || len > MAX_DECOMPRESSED {
            bail!("record {n}: bad length {len}");
        }
        let mut msg = vec![0u8; len];
        reader.read_exact(&mut msg).context("read record")?;

        // mode 1 streams plain text (the live-server INIT path), never a frame
        let frame: Vec<u8> = if header.mode == 1 {
            continue;
        } else if msg.len() >= 5 && msg[4] == TAG_PROFILE {
            pdec.decode(&msg)?.1
        } else {
            adec.decode(&msg)?.1
        };

        let ppm = rasterize(&frame, cols, rows, cell_bytes, scale, header.pixel);
        fs::write(Path::new(&args.out).join(format!("frame_{n:06}.ppm")), &ppm)?;

        if let Some(f) = args.frame {
            if n == f {
                eprintln!("wrote frame {n} ({img_w}x{img_h}) to {}/", args.out);
                return Ok(());
            }
        }
        n += 1;
    }
    eprintln!("wrote {n} frames ({img_w}x{img_h}) to {}/", args.out);
    Ok(())
}

fn cell_px(scale: usize, pixel: bool) -> usize {
    if pixel {
        scale
    } else {
        8 * scale // font8x8 glyphs are 8×8 bitmaps
    }
}

/// Rasterize one decoded frame (BGR, `cell_bytes` per cell) into a P6 PPM.
#[allow(clippy::needless_range_loop)]
fn rasterize(
    fb: &[u8],
    cols: usize,
    rows: usize,
    cell_bytes: usize,
    scale: usize,
    pixel: bool,
) -> Vec<u8> {
    let cell = cell_px(scale, pixel);
    let (w, h) = (cols * cell, rows * cell);
    let mut img = vec![0u8; w * h * 3];

    for y in 0..rows {
        for x in 0..cols {
            let o = (y * cols + x) * cell_bytes;
            // cell layout: ASCII [char,R,G,B], pixel/profile BGR
            let (r, g, b) = if cell_bytes == 4 {
                (fb[o + 1], fb[o + 2], fb[o])
            } else {
                (fb[o + 2], fb[o + 1], fb[o])
            };
            if pixel {
                for dy in 0..cell {
                    for dx in 0..cell {
                        let p = ((y * cell + dy) * w + (x * cell + dx)) * 3;
                        img[p] = r;
                        img[p + 1] = g;
                        img[p + 2] = b;
                    }
                }
            } else {
                let glyph = font8x8::BASIC_FONTS.get(fb[o] as char).unwrap_or([0u8; 8]);
                for gy in 0..8 {
                    for gx in 0..8 {
                        if (glyph[gy] >> (7 - gx)) & 1 == 0 {
                            continue;
                        }
                        for dy in 0..scale {
                            for dx in 0..scale {
                                let px = ((y * cell + gy * scale + dy) * w
                                    + (x * cell + gx * scale + dx))
                                    * 3;
                                img[px] = r;
                                img[px + 1] = g;
                                img[px + 2] = b;
                            }
                        }
                    }
                }
            }
        }
    }

    let mut ppm = Vec::with_capacity(w * h * 3 + 64);
    ppm.extend_from_slice(format!("P6\n{w} {h}\n255\n").as_bytes());
    ppm.extend_from_slice(&img);
    ppm
}
