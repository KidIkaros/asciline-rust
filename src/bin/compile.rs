//! asciline-compile — compile a video into a self-contained `.ascf` file.
//!
//! Port of `compiler.py`: decodes the video, maps it to the ASCILINE framebuffer
//! layouts, encodes every frame with the adaptive codec (RAW/ZLIB/DELTA/RLE_FULL),
//! and writes the v2 `ASC2` container (18-byte header + length-prefixed records)
//! that `static_player/reader.js` and `asciline-player` can play. Audio is
//! extracted to a sibling `.mp3`.
//!
//! ```text
//! asciline-compile your_video.mp4 --cols 250 --pixel --quantize 2
//! asciline-compile your_video.mp4 --mode 6 --hard
//! asciline-compile your_video.mp4 --profile --qf 70   # max compression (tag 4)
//! ```
//!
//! With `--profile` the opt-in lossy DCT encoder (tag 4) is used for the
//! smallest `.ascf` files — a faithful port of `codec.py`'s `ProfileEncoder`.
//! The browser client (`codec.js`) and `asciline-player` decode it back to
//! reconstructed pixels.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use asciline::codec::{CodecEncoder, DEFAULT_LEVEL};
use asciline::mapper::Mapper;
use asciline::profile::ProfileEncoder;
use asciline::protocol::{write_ascf_header, AscfHeader};
use asciline::quality::{rgb_vs_bgr, QualityStats};
use asciline::video::{probe_video, FrameReader, SourceParams};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "asciline-compile",
    version,
    about = "Compile a video into a self-contained .ascf static file (Rust port)"
)]
struct Args {
    /// Input video.
    video: String,

    /// Grid columns (default 300).
    #[arg(long, default_value_t = 300)]
    cols: u32,
    /// Grid rows (0 = auto).
    #[arg(long, default_value_t = 0)]
    rows: u32,
    /// Color quality: 1=B&W 2=64c 3=512c 4=32Kc 5=262Kc 6=16M Ultra.
    #[arg(long, default_value_t = 6)]
    mode: u8,
    /// Pixel mode (colored blocks).
    #[arg(long)]
    pixel: bool,

    /// Colour drift tolerance (0 = lossless).
    #[arg(long, default_value_t = 0)]
    tolerance: u32,
    /// Pixel-mode color quantization (0 = lossless, 3 = aggressive).
    #[arg(long, default_value_t = 0)]
    quantize: u8,
    /// Opt-in lossy DCT compression profile (tag 4, implies --pixel).
    #[arg(long)]
    profile: bool,
    /// Profile quality factor 1-100 (default 70).
    #[arg(long, default_value_t = 70)]
    qf: u8,
    /// Maximum zlib compression (level 9) — slower, smaller file.
    #[arg(long)]
    hard: bool,
    /// Skip the PSNR/SSIM quality report (faster compiles, scripting).
    #[arg(long)]
    no_quality: bool,
    /// Disable scene-cut keyframe insertion (strict every-48-frames schedule).
    #[arg(long)]
    no_scene_cut: bool,

    /// Output base name (no extension).
    #[arg(long)]
    out: Option<String>,
    /// Output directory (default: current directory).
    #[arg(long, default_value = ".")]
    out_dir: String,
    /// Target FPS for the clip (default: 30fps decimation of the source).
    #[arg(long)]
    fps: Option<f64>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let mut mode = args.mode;
    let mut pixel = args.pixel;
    if args.profile {
        // The lossy DCT profile is pixel mode only (like compiler.py).
        pixel = true;
        if mode == 1 {
            mode = 6;
        }
    }
    if !(1..=6).contains(&mode) {
        bail!("--mode must be 1-6");
    }
    if !args.video_exists() {
        bail!("file not found: {:?}", args.video);
    }

    let info = probe_video(&args.video, false)
        .with_context(|| format!("cannot open {:?}", args.video))?;

    // Target FPS: explicit --fps, else the original's decimation (cap ~30).
    let source_fps = info.fps.max(1.0);
    let effective_fps = if let Some(f) = args.fps {
        f
    } else if source_fps > 30.0 {
        let skip_n = (source_fps / 30.0).round().max(1.0);
        source_fps / skip_n
    } else {
        source_fps
    };

    let (cols, rows) = if args.rows > 0 {
        (args.cols, args.rows)
    } else {
        asciline::protocol::calc_auto_dimensions(args.cols, info.width, info.height, pixel)
    };
    println!(
        "[Compiler] {}x{} → grid {}x{} | mode={} | pixel={} | fps={:.3}",
        info.width, info.height, cols, rows, mode, pixel, effective_fps
    );

    // ── Opt-in lossy DCT profile (tag 4): 8x8 blocks over 4:2:0 planes need
    //    cols/rows to be multiples of 16, so the grid is padded up (like compiler.py).
    let profile = args.profile;
    let (cols, rows) = if profile {
        let pc = cols.div_ceil(16) * 16;
        let pr = rows.div_ceil(16) * 16;
        if pc != cols || pr != rows {
            println!(
                "[Compiler] Lossy DCT profile (tag 4) ON | QF={} | grid padded {}x{} → {}x{}",
                args.qf, cols, rows, pc, pr
            );
            (pc, pr)
        } else {
            println!("[Compiler] Lossy DCT profile (tag 4) ON | QF={}", args.qf);
            (cols, rows)
        }
    } else {
        (cols, rows)
    };

    // ── audio extraction (best-effort, like the original) ──
    let base = args
        .out
        .clone()
        .unwrap_or_else(|| {
            std::path::Path::new(&args.video)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "output".into())
        });
    let out_dir = PathBuf::from(&args.out_dir);
    std::fs::create_dir_all(&out_dir).ok();
    let ascf_path = out_dir.join(format!("{base}.ascf"));
    let audio_path = out_dir.join(format!("{base}.mp3"));
    extract_audio(&args.video, &audio_path);

    // ── mapper with the compiler's floor-divide LUT ──
    let palette: Vec<char> = asciline::DEFAULT_PALETTE.chars().collect();
    let lut = Mapper::compiler_lut(palette.len());
    let ascii_qb = match mode {
        6 => 0,
        5 => 2,
        4 => 3,
        3 => 5,
        2 => 6,
        _ => 0,
    };
    let mapper = if pixel {
        Mapper::new_with_lut(&palette, lut, args.quantize.min(3))
    } else {
        Mapper::new_with_lut(&palette, lut, ascii_qb)
    };

    let level = if args.hard { 9 } else { DEFAULT_LEVEL };
    let cell_bytes = if pixel { 3 } else { 4 };
    let mut enc = CodecEncoder::new(cell_bytes, level, args.tolerance);

    // Adaptive pixel quality report: only when the lossless path is actually
    // lossy (colour quantization or temporal drift tolerance). Measured against
    // the ORIGINAL video frame, so both loss sources show up.
    // Adaptive pixel quality report: only when the lossless path is actually
    // lossy AND the report wasn't suppressed — `--no-quality` must skip the
    // computation too (it is the dominant per-frame cost when enabled).
    let adaptive_quality = !profile
        && pixel
        && !args.no_quality
        && (args.tolerance > 0 || args.quantize > 0);
    let mut adaptive_stats = QualityStats::new();
    let mut profile_enc = if profile {
        let mut pe = ProfileEncoder::new(cols as usize, rows as usize, args.qf.clamp(1, 100));
        if args.hard {
            pe.level = 9; // --hard applies to the profile's zlib stage too
        }
        if args.no_quality {
            pe.collect_stats = false; // skip the SSIM work, not just the print
        }
        if !args.no_scene_cut {
            // scene-cut keyframes: when luma stops resembling the previous
            // reconstruction, motion prediction is useless — re-encode the
            // frame as a fresh keyframe (better quality at cuts, usually a
            // smaller file too). Keyframes are self-describing, so every
            // ASCILINE decoder handles them at any point in the stream.
            pe.scene_cut_mad = asciline::profile::SCENE_CUT_MAD;
        }
        Some(pe)
    } else {
        None
    };

    // ── decode + encode loop ──
    let params = SourceParams {
        src: args.video.clone(),
        is_webcam: false,
        cols,
        rows,
        target_fps: Some(effective_fps),
        seek_secs: 0.0,
        mirror: false,
    };
    let pid = std::sync::atomic::AtomicU32::new(0);
    let mut reader = FrameReader::new(&params, &pid)?;

    let file = File::create(&ascf_path).with_context(|| format!("cannot create {:?}", ascf_path))?;
    let mut out = BufWriter::new(file);

    let header = AscfHeader {
        fps: effective_fps as f32,
        mode,
        pixel,
        cols: cols as u16,
        rows: rows as u16,
        total_frames: 0,
    };
    out.write_all(&write_ascf_header(&header))?;

    let mut frame_index: u32 = 0;
    let mut bytes_written: u64 = 18;
    while let Some(rgb) = reader.read_frame() {
        let msg: Vec<u8> = if let Some(pen) = profile_enc.as_mut() {
            // RGB24 → BGR (profile expects BGR), then optional color quantization
            let mut bgr = Vec::with_capacity(rgb.len());
            for c in rgb.chunks_exact(3) {
                bgr.push(c[2]);
                bgr.push(c[1]);
                bgr.push(c[0]);
            }
            if args.quantize > 0 {
                let q = args.quantize;
                for v in bgr.iter_mut() {
                    *v = (*v >> q) << q;
                }
            }
            pen.encode(&bgr).0
        } else if mode == 1 {
            let text = mapper.text_frame(&rgb, cols as usize, rows as usize, frame_index);
            text.into_bytes()
        } else if pixel {
            let mut fb = vec![0u8; (cols * rows * 3) as usize];
            mapper.map_pixel(&rgb, cols as usize, rows as usize, &mut fb);
            let m = enc.encode(&fb, frame_index);
            if adaptive_quality {
                // enc.prev() is the SHOWN framebuffer — what players display
                // after delta/tolerance skipping (and colour quantization).
                if let Some(shown) = enc.prev() {
                    let (py, sy, pr) =
                        rgb_vs_bgr(&rgb, shown, cols as usize, rows as usize);
                    adaptive_stats.push(frame_index as u64, py, sy, pr);
                }
            }
            m
        } else {
            let mut fb = vec![0u8; (cols * rows * 4) as usize];
            mapper.map_ascii(&rgb, cols as usize, rows as usize, &mut fb);
            enc.encode(&fb, frame_index)
        };

        out.write_all(&(msg.len() as u32).to_be_bytes())?;
        out.write_all(&msg)?;
        bytes_written += 4 + msg.len() as u64;
        frame_index += 1;

        if frame_index.is_multiple_of(50) {
            print!(
                "\r[Compiler] {} frames ({} MB)...",
                frame_index,
                bytes_written as f64 / 1024.0 / 1024.0
            );
            let _ = std::io::stdout().flush();
        }
    }
    out.flush()?;
    drop(out);

    // ── patch the total frame count into the header ──
    {
        let f = File::options()
            .write(true)
            .open(&ascf_path)
            .with_context(|| format!("cannot reopen {:?} for patching", ascf_path))?;
        let mut w = BufWriter::new(f);
        use std::io::{Seek, SeekFrom};
        w.seek(SeekFrom::Start(14))?;
        w.write_all(&frame_index.to_be_bytes())?;
        w.flush()?;
    }

    println!(
        "\n[Compiler] Done! {} frames → {} ({:.2} MB)",
        frame_index,
        ascf_path.display(),
        bytes_written as f64 / 1024.0 / 1024.0
    );

    // ── quality report: how far the lossy reconstruction drifts ──
    if !args.no_quality {
        if let Some(pen) = profile_enc.as_ref() {
            let s = pen.stats();
            if s.frames() > 0 {
                print_quality_report(
                    s,
                    &format!(
                        "Lossy DCT reconstruction vs source ({} frames, QF={}):",
                        s.frames(),
                        args.qf.clamp(1, 100)
                    ),
                );
            }
        }
        if adaptive_quality && adaptive_stats.frames() > 0 {
            print_quality_report(
                &adaptive_stats,
                &format!(
                    "Adaptive pixel reconstruction vs source ({} frames, tolerance={}, quantize={}):",
                    adaptive_stats.frames(),
                    args.tolerance,
                    args.quantize
                ),
            );
        }
    }
    Ok(())
}

/// Print the PSNR/SSIM quality report block (mean/min/max per metric).
fn print_quality_report(s: &QualityStats, title: &str) {
    println!();
    println!("[Quality] {title}");
    println!(
        "[Quality]   PSNR-Y  {:>7} dB   (min {:>6} / max {:>6})",
        fmt_db(s.psnr_y_mean()),
        fmt_db(s.psnr_y_min()),
        fmt_db(s.psnr_y_max())
    );
    println!(
        "[Quality]   SSIM-Y  {:>7}      (min {:>6} / max {:>6})",
        fmt_num(s.ssim_y_mean()),
        fmt_num(s.ssim_y_min()),
        fmt_num(s.ssim_y_max())
    );
    println!(
        "[Quality]   PSNR-RGB {:>6} dB   (min {:>6} / max {:>6})",
        fmt_db(s.psnr_rgb_mean()),
        fmt_db(s.psnr_rgb_min()),
        fmt_db(s.psnr_rgb_max())
    );
    // The weakest frame (lowest PSNR-Y) — usually a scene cut or motion burst.
    // Skipped when every frame decoded losslessly (all PSNR ∞).
    if s.worst_psnr_y().is_finite() {
        println!(
            "[Quality]   worst frame #{}  PSNR-Y {:>6} dB  (SSIM {:>6} / RGB {:>6} dB)",
            s.worst_idx(),
            fmt_db(s.worst_psnr_y()),
            fmt_num(s.worst_ssim_y()),
            fmt_db(s.worst_psnr_rgb())
        );
    }
}

/// Render a dB value, using ∞ when a frame decoded losslessly (PSNR = inf).
fn fmt_db(v: f64) -> String {
    if v.is_infinite() {
        "∞".to_string()
    } else {
        format!("{v:.2}")
    }
}

/// Render a unitless metric (SSIM never hits inf, but keep it uniform).
fn fmt_num(v: f64) -> String {
    if v.is_infinite() {
        "∞".to_string()
    } else {
        format!("{v:.4}")
    }
}

/// Extract the audio track to MP3 (warns and continues if there's no audio).
fn extract_audio(video_path: &str, output_path: &std::path::Path) {
    println!("[Audio] extracting to {}...", output_path.display());
    let status = std::process::Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(video_path)
        .args(["-vn", "-acodec", "libmp3lame", "-ab", "128k", "-ar", "44100"])
        .arg(output_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => println!("[Audio] extracted successfully."),
        _ => {
            let _ = std::fs::remove_file(output_path);
            println!("[Audio] WARNING: no audio track / ffmpeg failed — compiling silent clip.");
        }
    }
}

impl Args {
    fn video_exists(&self) -> bool {
        std::path::Path::new(&self.video).exists()
    }
}
