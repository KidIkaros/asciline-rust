//! asciline-player — zero-flicker true-color ASCII video player for the terminal.
//!
//! Port of `ascii_video_player2.py`: ffmpeg decodes the video to a tiny grid,
//! pixels are mapped to palette characters + 24-bit ANSI colors (run-length
//! compressed escape codes), and frames are paced at the source FPS.
//!
//! ```text
//! asciline-player video.mp4 --cols 100
//! asciline-player --webcam --cols 100
//! asciline-player movie.ascf            # compiled static clip
//! ```
//!
//! Playback runs at the source frame rate — 60fps sources play at 60fps.

use std::io::{BufWriter, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use asciline::codec::{CodecDecoder, MAX_DECOMPRESSED, TAG_PROFILE};
use asciline::mapper::Mapper;
use asciline::profile::ProfileDecoder;
use asciline::protocol::{parse_ascf_header, ASCF_MAGIC_V2};
use asciline::video::{probe_video, FrameReader, SourceParams};
use clap::Parser;

const CHAR_RATIO: f64 = 0.45;

// terminal control sequences (same as the Python original)
const CURSOR_HOME: &str = "\x1b[H";
const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";
const DISABLE_WRAP: &str = "\x1b[?7l";
const ENABLE_WRAP: &str = "\x1b[?7h";
const BLACK_BG: &str = "\x1b[40m";
const RESET: &str = "\x1b[0m";
const CLEAR_SCREEN: &str = "\x1b[2J";

#[derive(Parser, Debug)]
#[command(
    name = "asciline-player",
    version,
    about = "True-color ANSI ASCII video player (Rust port of ASCILINE)"
)]
struct Args {
    /// Path to a video file or .ascf clip.
    #[arg(default_value = "")]
    video: String,

    /// Custom character palette, space-separated.
    #[arg(long)]
    palette: Option<String>,

    /// Color quality: 0=max, 3=max speed (bit-shift quantization).
    #[arg(short = 'q', long, default_value_t = 0)]
    quality: u8,

    /// Fixed grid width (0 = auto-fit to terminal).
    #[arg(short = 'c', long, default_value_t = 0)]
    cols: u32,

    /// Use a webcam instead of a video file.
    #[arg(long)]
    webcam: bool,
    #[arg(long, default_value_t = 0)]
    webcam_device: u32,
    #[arg(long, default_value_t = 30)]
    webcam_fps: u32,
    #[arg(long)]
    no_mirror: bool,

    /// Target playback FPS (default: source FPS — no 30fps cap).
    #[arg(long)]
    fps: Option<f64>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let quantize_bits = args.quality.min(3);
    let palette: Vec<char> = args
        .palette
        .clone()
        .map(|p| {
            p.split_whitespace()
                .filter_map(|c| c.chars().next())
                .collect()
        })
        .unwrap_or_else(|| asciline::DEFAULT_PALETTE.chars().collect());

    if !args.webcam && args.video.is_empty() {
        bail!("a video file is required (or use --webcam)");
    }
    let src = if args.webcam {
        format!("/dev/video{}", args.webcam_device)
    } else {
        args.video.clone()
    };

    // Graceful Ctrl+C: set a flag, exit the loop, restore the terminal.
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let _ = ctrlc::set_handler(move || s2.store(true, Ordering::SeqCst));

    let mapper = Mapper::new(&palette, quantize_bits);

    if is_ascf_file(&src) {
        play_ascf(&src, &stop)?;
    } else {
        play_video(&src, &args, &mapper, quantize_bits, &stop)?;
    }
    Ok(())
}

fn is_ascf_file(path: &str) -> bool {
    Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().eq_ignore_ascii_case("ascf"))
        .unwrap_or(false)
}

/// Grid sizing mirroring the original: fixed --cols, or auto-fit (capped at 160).
fn compute_grid(t_cols: u32, t_lines: u32, vid_w: u32, vid_h: u32, fixed_cols: u32) -> (u32, u32) {
    let aspect = vid_h as f64 / vid_w.max(1) as f64;
    if fixed_cols > 0 {
        return (
            fixed_cols,
            ((fixed_cols as f64 * aspect * CHAR_RATIO).round()).max(1.0) as u32,
        );
    }
    let safe_cols = t_cols.clamp(1, 160);
    if vid_h > vid_w {
        // portrait: fit height first
        let rows = t_lines.max(1);
        let cols = ((rows as f64) / (aspect * CHAR_RATIO)).round().max(1.0) as u32;
        if cols > safe_cols {
            (
                safe_cols,
                ((safe_cols as f64 * aspect * CHAR_RATIO).round()).max(1.0) as u32,
            )
        } else {
            (cols, rows)
        }
    } else {
        // landscape: fit width first
        let cols = safe_cols;
        let rows = ((cols as f64 * aspect * CHAR_RATIO).round()).max(1.0) as u32;
        if rows > t_lines {
            let rows = t_lines.max(1);
            let cols = ((rows as f64) / (aspect * CHAR_RATIO)).round().max(1.0) as u32;
            (cols.max(1), rows)
        } else {
            (cols, rows)
        }
    }
}

fn play_video(
    src: &str,
    args: &Args,
    mapper: &Mapper,
    quantize_bits: u8,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    let info = probe_video(src, args.webcam).with_context(|| format!("cannot open {src:?}"))?;
    let source_fps = if args.webcam {
        args.webcam_fps as f64
    } else if let Some(f) = args.fps {
        f
    } else {
        info.fps.max(1.0)
    };

    let (t_cols, t_lines) = crossterm::terminal::size()
        .map(|(c, l)| (c as u32, l as u32))
        .unwrap_or((220, 50));
    let (cols, rows) = compute_grid(
        t_cols,
        t_lines.saturating_sub(2),
        info.width,
        info.height,
        args.cols,
    );
    let pad_y = (t_lines.saturating_sub(2).saturating_sub(rows)) / 2;
    let pad_x = " ".repeat((t_cols.saturating_sub(cols) / 2) as usize);

    let orientation = if info.height > info.width {
        "PORTRAIT"
    } else {
        "LANDSCAPE"
    };
    print!(
        "{CLEAR_SCREEN}\x1b[1m[ASCII Player — Rust]\x1b[0m\n  Orientation : {orientation}\n  Video       : {}x{}\n  ASCII       : {cols}x{rows}\n  FPS         : {:.1}\n  Quantization: {} levels/channel\n  Exit        : Ctrl+C\n\n",
        info.width, info.height, source_fps, 2u32.pow(8 - quantize_bits.min(7) as u32)
    );
    let _ = std::io::stdout().flush();

    let target_fps = if args.webcam {
        args.webcam_fps as f64
    } else {
        args.fps.unwrap_or(source_fps)
    };
    let params = SourceParams {
        src: src.to_string(),
        is_webcam: args.webcam,
        cols,
        rows,
        target_fps: Some(target_fps),
        seek_secs: 0.0,
        mirror: args.webcam && !args.no_mirror,
    };
    let pid = std::sync::atomic::AtomicU32::new(0);
    let mut reader = FrameReader::new(&params, &pid)?;

    let frame_t = Duration::from_secs_f64(1.0 / target_fps);
    let mut stdout = BufWriter::new(std::io::stdout());
    write!(stdout, "{DISABLE_WRAP}{HIDE_CURSOR}{BLACK_BG}")?;
    stdout.flush()?;

    while let Some(rgb) = reader.read_frame() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let t0 = Instant::now();
        let mut cells = vec![0u8; (cols * rows * 4) as usize];
        mapper.map_ascii(&rgb, cols as usize, rows as usize, &mut cells);
        let frame = render_ansi(&cells, cols as usize, rows as usize, &pad_x, pad_y as usize);
        write!(stdout, "{CURSOR_HOME}{frame}")?;
        stdout.flush()?;
        // pace: wait out the remainder of the frame slot
        let wait = frame_t.saturating_sub(t0.elapsed());
        if wait > Duration::ZERO {
            std::thread::sleep(wait);
        }
    }

    restore_terminal(&mut stdout)?;
    Ok(())
}

/// Play a compiled `.ascf` clip (v2 `ASC2` headers; legacy `ASCF` tolerated).
fn play_ascf(path: &str, stop: &Arc<AtomicBool>) -> Result<()> {
    let file = std::fs::File::open(path).with_context(|| format!("cannot open {path:?}"))?;
    let mut reader = std::io::BufReader::new(file);

    let mut header_bytes = [0u8; 18];
    reader.read_exact(&mut header_bytes[..14])?;
    let is_v2 = &header_bytes[..4] == ASCF_MAGIC_V2;
    if is_v2 {
        reader.read_exact(&mut header_bytes[14..])?;
    }
    let header = parse_ascf_header(&header_bytes)?;
    let fps = header.fps as f64;
    let (cols, rows) = (header.cols as u32, header.rows as u32);
    let cell_bytes = if header.pixel { 3 } else { 4 };

    let (t_cols, t_lines) = crossterm::terminal::size()
        .map(|(c, l)| (c as u32, l as u32))
        .unwrap_or((220, 50));
    let pad_y = (t_lines.saturating_sub(2).saturating_sub(rows)) / 2;
    let pad_x = " ".repeat((t_cols.saturating_sub(cols) / 2) as usize);

    print!(
        "{CLEAR_SCREEN}\x1b[1m[ASCII Player — Rust (.ascf)]\x1b[0m\n  File        : {path}\n  ASCII       : {cols}x{rows}\n  Mode        : {}{}\n  FPS         : {fps:.1}\n  Exit        : Ctrl+C\n\n",
        header.mode,
        if header.pixel { " [PIXEL]" } else { "" }
    );
    let _ = std::io::stdout().flush();

    let mut decoder = CodecDecoder::new(cell_bytes);
    let mut pdec = ProfileDecoder::new(); // tag-4 lossy DCT frames (--profile clips)
    let frame_t = Duration::from_secs_f64(1.0 / fps);
    let mut stdout = BufWriter::new(std::io::stdout());
    write!(stdout, "{DISABLE_WRAP}{HIDE_CURSOR}{BLACK_BG}")?;
    stdout.flush()?;

    let mut len_buf = [0u8; 4];
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if reader.read_exact(&mut len_buf).is_err() {
            break; // EOF
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        // Cap before allocating: a malformed file must not be able to claim a
        // multi-GB record. Same cap as a single decompressed frame.
        if len > MAX_DECOMPRESSED {
            bail!("ascf record too large ({len} bytes)");
        }
        let mut msg = vec![0u8; len];
        reader.read_exact(&mut msg)?;

        let t0 = Instant::now();
        if header.mode == 1 {
            let text = String::from_utf8_lossy(&msg);
            if let Some((_, body)) = text.split_once('\n') {
                write!(stdout, "{CURSOR_HOME}{}", pad_x_per_line(body, &pad_x))?;
                stdout.flush()?;
            }
        } else {
            let is_profile = msg.len() > 4 && msg[4] == TAG_PROFILE;
            // Profile frames decode to BGR pixels; adaptive frames to cells.
            let (_, frame) = if is_profile {
                pdec.decode(&msg)?
            } else {
                decoder.decode(&msg)?
            };
            if header.pixel || is_profile {
                let expect = (cols as usize) * (rows as usize) * 3;
                if frame.len() != expect {
                    bail!(
                        "decoded frame size {} != {}x{}x3 (corrupt or mismatched .ascf)",
                        frame.len(),
                        cols,
                        rows
                    );
                }
                let cells = pixel_to_ansi_cells(&frame);
                let out = render_ansi(&cells, cols as usize, rows as usize, &pad_x, pad_y as usize);
                write!(stdout, "{CURSOR_HOME}{out}")?;
                stdout.flush()?;
            } else {
                let out = render_ansi(&frame, cols as usize, rows as usize, &pad_x, pad_y as usize);
                write!(stdout, "{CURSOR_HOME}{out}")?;
                stdout.flush()?;
            }
        }
        let wait = frame_t.saturating_sub(t0.elapsed());
        if wait > Duration::ZERO {
            std::thread::sleep(wait);
        }
    }

    restore_terminal(&mut stdout)?;
    Ok(())
}

/// Pixel-mode BGR frames → `[block_char, r, g, b]` cells so the ANSI renderer works.
fn pixel_to_ansi_cells(frame: &[u8]) -> Vec<u8> {
    frame
        .chunks_exact(3)
        .flat_map(|px| [b' ', px[2], px[1], px[0]])
        .collect()
}

/// Center `body` with `pad_x` before each line (matches the Python padding).
fn pad_x_per_line(body: &str, pad_x: &str) -> String {
    if pad_x.is_empty() {
        body.to_string()
    } else {
        format!("{pad_x}{}", body.replace('\n', &format!("\n{pad_x}")))
    }
}

/// Build one ANSI frame: RLE escape codes (a new `38;2;r;g;b` only when the
/// color changes), global color tracking across rows like the original.
fn render_ansi(cells: &[u8], cols: usize, rows: usize, pad_x: &str, pad_y: usize) -> String {
    let mut s = String::with_capacity(cols * rows * 8 + pad_y);
    let mut prev = (-1i16, -1i16, -1i16);
    for y in 0..rows {
        if y == 0 {
            for _ in 0..pad_y {
                s.push('\n');
            }
        } else {
            s.push('\n');
        }
        for x in 0..cols {
            let cell = &cells[(y * cols + x) * 4..(y * cols + x) * 4 + 4];
            let (r, g, b) = (cell[1] as i16, cell[2] as i16, cell[3] as i16);
            if (r, g, b) != prev {
                s.push_str(&format!("\x1b[38;2;{};{};{}m", cell[1], cell[2], cell[3]));
                prev = (r, g, b);
            }
            // cell[0] is the ASCII byte of the palette character (wire format)
            s.push(cell[0] as char);
        }
    }
    if !pad_x.is_empty() {
        s = format!("{pad_x}{}", s.replace('\n', &format!("\n{pad_x}")));
    }
    s
}

fn restore_terminal(out: &mut BufWriter<std::io::Stdout>) -> Result<()> {
    writeln!(out, "{ENABLE_WRAP}{SHOW_CURSOR}{RESET}")?;
    out.flush()?;
    Ok(())
}
