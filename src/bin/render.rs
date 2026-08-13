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
use std::time::Instant;

use anyhow::{bail, Context, Result};
use asciline::codec::{
    CodecDecoder, MAX_DECOMPRESSED, TAG_PROFILE, TAG_PROFILE_AQ, TAG_PROFILE_HPEL,
};
use asciline::profile::ProfileDecoder;
use asciline::protocol::{parse_ascf_header, AscfSeekIndex, ASCF_MAGIC_V2};
use clap::Parser;
use font8x8::UnicodeFonts;
use futures_util::StreamExt;
use tokio_tungstenite::tungstenite::Message;

#[derive(Parser, Debug)]
#[command(
    name = "asciline-render",
    version,
    about = "Headless .ascf -> PPM frame renderer (pixel blocks / ASCII glyphs)"
)]
struct Args {
    /// Compiled .ascf clip to render (or use `--live` for a WebSocket stream).
    ascf: Option<String>,
    /// Output directory for frame_%06d.ppm files (created if missing).
    #[arg(long, default_value = "render_out")]
    out: String,
    /// Cell size in pixels (solid block for pixel mode, glyph scale for ASCII).
    #[arg(long)]
    scale: Option<u8>,
    /// Render only this one frame (0-based), then stop. Uses the .ascf seek
    /// index to jump straight to the frame's keyframe instead of decoding
    /// every earlier frame.
    #[arg(long)]
    frame: Option<u32>,
    /// Start rendering from this time (seconds) instead of the beginning
    /// (uses the .ascf seek index — nearest keyframe + forward decode).
    #[arg(long, default_value_t = 0.0)]
    seek: f64,
    /// Render a LIVE WebSocket stream instead of a file: connect, parse the
    /// INIT handshake, decode the binary frames as they arrive and write a
    /// PPM per frame (plus a measured-fps line on stderr). Use with a timeout
    /// or `--max-frames` for a bounded capture, e.g.
    /// `asciline-render --live ws://127.0.0.1:8000/ws?codec=adaptive`.
    #[arg(long)]
    live: Option<String>,
    /// Stop after this many frames (live mode).
    #[arg(long)]
    max_frames: Option<u32>,
    /// Write per-frame latency timestamps (`frame_index t_recv t_decode
    /// t_render`, monotonic wall ns) to this file (live mode). Join with the
    /// server's `--latency-log` on the same host via
    /// `experiments/analyze_latency.py`.
    #[arg(long)]
    latency_log: Option<String>,
}

/// Measurement-only per-frame latency logger (mirror of the server's; the
/// timestamps are comparable across processes on the same host — see the
/// `LatencyLog` docs in `src/server.rs`).
struct ClientLatencyLog {
    w: std::io::BufWriter<std::fs::File>,
    mono0: Instant,
    wall0_ns: u128,
}

impl ClientLatencyLog {
    fn open(path: &str) -> Result<ClientLatencyLog> {
        Ok(ClientLatencyLog {
            w: std::io::BufWriter::new(
                std::fs::File::create(path).with_context(|| format!("--latency-log {path:?}"))?,
            ),
            mono0: Instant::now(),
            wall0_ns: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        })
    }

    fn record(&mut self, index: u32, t_recv: Instant, t_decode: Instant, t_render: Instant) {
        use std::io::Write;
        let ns = |t: Instant| self.wall0_ns + t.duration_since(self.mono0).as_nanos();
        let _ = writeln!(
            self.w,
            "{index} {} {} {}",
            ns(t_recv),
            ns(t_decode),
            ns(t_render)
        );
        // Flush every record (see the server's `LatencyLog`): a measurement
        // log that loses its tail on process exit corrupts the join.
        let _ = self.w.flush();
    }

    fn flush(&mut self) {
        use std::io::Write;
        let _ = self.w.flush();
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(url) = &args.live {
        return live_render(url, &args);
    }
    let Some(ascf) = &args.ascf else {
        bail!("provide an .ascf file to render or --live <ws://url> for a stream");
    };
    let file = fs::File::open(ascf).with_context(|| format!("open {ascf}"))?;
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
    let header_len: u64 = if is_v2 { 18 } else { 14 };

    // ── seek index: jump to the keyframe at/before the target frame, then
    //    decode forward (deterministic — same bytes as sequential playback). ──
    let mut n = 0u32;
    let mut written = 0u32;
    let mut skip_until: Option<u32> = None;
    if args.seek > 0.0 || args.frame.is_some() {
        use std::io::{Seek, SeekFrom};
        let target = match args.frame {
            Some(f) => f,
            None => (args.seek * header.fps as f64).round().max(0.0) as u32,
        };
        let idx = AscfSeekIndex::scan(&mut reader, header_len)
            .with_context(|| format!("seek: cannot index {ascf:?}"))?;
        match idx.floor(target) {
            Some((kf, off)) => {
                reader.seek(SeekFrom::Start(off))?;
                n = kf;
                skip_until = Some(target);
                eprintln!("[Seek] frame {target}: jumped to keyframe {kf} @ {off}B");
            }
            None => eprintln!(
                "[Seek] target {target} past end ({} frames)",
                idx.total_frames
            ),
        }
    }
    let scale = args.scale.unwrap_or(if header.pixel { 8 } else { 2 }) as usize;
    let (img_w, img_h) = (
        cols * cell_px(scale, header.pixel),
        rows * cell_px(scale, header.pixel),
    );
    fs::create_dir_all(&args.out).with_context(|| format!("create {}", args.out))?;

    let mut adec = CodecDecoder::new(cell_bytes);
    let mut pdec = ProfileDecoder::new();

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
        } else if msg.len() >= 5
            && matches!(msg[4], TAG_PROFILE | TAG_PROFILE_AQ | TAG_PROFILE_HPEL)
        {
            pdec.decode(&msg)?.1
        } else {
            adec.decode(&msg)?.1
        };

        let n_now = n;
        n += 1;
        // After a seek, decode (stateful!) but don't write frames before the
        // target — the first written file is exactly the requested frame.
        if let Some(t) = skip_until {
            if n_now < t {
                continue;
            }
            skip_until = None;
        }

        let ppm = rasterize(&frame, cols, rows, cell_bytes, scale, header.pixel);
        fs::write(
            Path::new(&args.out).join(format!("frame_{n_now:06}.ppm")),
            &ppm,
        )?;
        written += 1;

        if let Some(f) = args.frame {
            if n_now == f {
                eprintln!("wrote frame {n_now} ({img_w}x{img_h}) to {}/", args.out);
                return Ok(());
            }
        }
    }
    eprintln!("wrote {written} frames ({img_w}x{img_h}) to {}/", args.out);
    Ok(())
}

fn cell_px(scale: usize, pixel: bool) -> usize {
    if pixel {
        scale
    } else {
        8 * scale // font8x8 glyphs are 8×8 bitmaps
    }
}

/// Grid + rasterization parameters parsed from the live INIT handshake.
struct LiveHeader {
    mode: u8,
    cols: usize,
    rows: usize,
    pixel: bool,
    cell_bytes: usize,
    scale: usize,
    img_w: usize,
    img_h: usize,
}

/// Live WS capture: decode the wire frames as they arrive (the same messages
/// the browser client receives) and rasterize them to PPMs. Prints the INIT
/// header and the measured receive rate on stderr, e.g.
/// `asciline-render --live ws://127.0.0.1:8000/ws?codec=adaptive --max-frames 720`.
fn live_render(url: &str, args: &Args) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    rt.block_on(async move {
        let (mut ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .with_context(|| format!("connect {url}"))?;

        let mut hdr: Option<LiveHeader> = None;
        let mut adec: Option<CodecDecoder> = None;
        let mut pdec = ProfileDecoder::new();
        let mut n = 0u32;
        let t0 = Instant::now();
        let mut lat = match &args.latency_log {
            Some(p) => Some(ClientLatencyLog::open(p)?),
            None => None,
        };

        while let Some(msg) = ws.next().await {
            let msg = msg.context("ws read")?;
            match msg {
                Message::Text(t) if hdr.is_none() && t.starts_with("INIT:") => {
                    let p: Vec<&str> = t.split(':').collect();
                    let fps = p[1].parse::<f64>().unwrap_or(0.0);
                    let mode = p[2].parse::<u8>().unwrap_or(1);
                    let cols = p[3].parse::<usize>().unwrap_or(0);
                    let rows = p[4].parse::<usize>().unwrap_or(0);
                    let pixel = p.get(5).map(|s| *s == "1").unwrap_or(false);
                    let cell_bytes = if pixel { 3 } else { 4 };
                    let scale = args.scale.unwrap_or(if pixel { 8 } else { 2 }) as usize;
                    let h = LiveHeader {
                        mode,
                        cols,
                        rows,
                        pixel,
                        cell_bytes,
                        scale,
                        img_w: cols * cell_px(scale, pixel),
                        img_h: rows * cell_px(scale, pixel),
                    };
                    fs::create_dir_all(&args.out).with_context(|| format!("create {}", args.out))?;
                    adec = Some(CodecDecoder::new(cell_bytes));
                    eprintln!(
                        "live INIT: fps={fps} mode={mode} grid={cols}x{rows} pixel={pixel} -> {}x{}",
                        h.img_w, h.img_h
                    );
                    hdr = Some(h);
                }
                Message::Binary(b) => {
                    let Some(h) = &hdr else { continue };
                    if h.mode == 1 {
                        continue; // plain-text record, not a video frame
                    }
                    let t_recv = Instant::now();
                    let (idx, frame): (u32, Vec<u8>) = if h.pixel {
                        // pixel mode bypasses the codec: raw frames are
                        // [u32 BE index][BGR payload] with no tag byte
                        if b.len() != 4 + h.cols * h.rows * h.cell_bytes {
                            bail!("live frame: {} bytes != 4 + {}x{}x{}", b.len(), h.cols, h.rows, h.cell_bytes);
                        }
                        (u32::from_be_bytes([b[0], b[1], b[2], b[3]]), b[4..].to_vec())
                    } else if b.len() >= 5 && (b[4] == TAG_PROFILE || b[4] == TAG_PROFILE_AQ) {
                        pdec.decode(&b)?
                    } else {
                        adec.as_mut().unwrap().decode(&b)?
                    };
                    let t_decode = Instant::now();
                    let ppm = rasterize(&frame, h.cols, h.rows, h.cell_bytes, h.scale, h.pixel);
                    fs::write(Path::new(&args.out).join(format!("frame_{n:06}.ppm")), &ppm)?;
                    let t_render = Instant::now();
                    if let Some(l) = &mut lat {
                        l.record(idx, t_recv, t_decode, t_render);
                    }
                    n += 1;
                    if let Some(max) = args.max_frames {
                        if n >= max {
                            break;
                        }
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        let dt = t0.elapsed().as_secs_f64();
        let (img_w, img_h) = hdr
            .as_ref()
            .map(|h| (h.img_w, h.img_h))
            .unwrap_or((0, 0));
        eprintln!(
            "live capture: {n} frames in {dt:.2}s -> {:.1} fps ({img_w}x{img_h} per frame)",
            n as f64 / dt.max(1e-9)
        );
        if let Some(l) = &mut lat {
            l.flush();
        }
        Ok(())
    })
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
