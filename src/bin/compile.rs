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
//! asciline-compile your_video.mp4 --profile --target-size 450K  # rate control
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
    /// Profile motion search radius (±N integer pixels; default 7). Larger
    /// radii improve fast/large motion (e.g. 30 fps footage); 3 = codec.py.
    #[arg(long, default_value_t = 7)]
    r_search: i32,
    /// Profile rate-distortion λ for motion-vector selection (0 = pure SAD).
    /// A positive value enables SAD-prefilter + RDO refinement.
    #[arg(long, default_value_t = 0.0)]
    rdo_lambda: f64,
    /// Profile adaptive quantization levels: 2 = per-block quant-scale map
    /// on the luma plane (tag 5, x264-style: detail blocks quantize finer,
    /// flat blocks stay at the base table — measured +2 dB PSNR-Y at QF=70,
    /// ~10% smaller at equal quality), 4 = 4-level map (2 bits/block),
    /// 0 = off (tag 4, bit-exact with codec.py). Requires the browser
    /// decoder at web/codec.js >= this commit.
    #[arg(long, default_value_t = 2)]
    aq: u8,
    /// Disable half-pixel motion compensation: emit tag 5 (AQ) / tag 4
    /// (bit-exact with codec.py) instead of the default tag 6. Half-pel
    /// refines the best integer motion vector to half-pel precision and the
    /// decoder interpolates the luma reference bilinearly — a strict superset
    /// of integer motion (never worse), but the browser decoder at
    /// web/codec.js must be >= this commit (older decoders freeze on the
    /// unknown tag).
    #[arg(long)]
    no_hpel: bool,
    /// Disable quarter-pixel motion compensation: emit tag 6 (half-pel)
    /// instead of the default tag 7. Quarter-pel refines through the half-pel
    /// ladder to quarter-pel precision and the decoder interpolates the luma
    /// reference with H.264's 6-tap half-pel filter + bilinear quarter-pel
    /// step — a strict superset of half-pel (every half-pel vector is
    /// representable in quarter-pel units, so it is never worse). Requires
    /// the browser decoder at web/codec.js >= this commit.
    #[arg(long)]
    no_qpel: bool,
    /// Use plain bilinear interpolation for quarter-pel motion instead of
    /// H.264's 6-tap filter (encoder-side experiment: the 6-tap is sharper
    /// but can ring on tiny grids; measured quality/size differ per content).
    /// Decoders always implement the 6-tap tag-7 spec, so streams stay
    /// interoperable — this only changes which vectors the encoder picks.
    #[arg(long)]
    qpel_bilinear: bool,
    /// Maximum zlib compression (level 9) — slower, smaller file.
    #[arg(long)]
    hard: bool,
    /// Skip the PSNR/SSIM quality report (faster compiles, scripting).
    #[arg(long)]
    no_quality: bool,
    /// Exit non-zero when the mean PSNR-Y of the lossy reconstruction falls
    /// below this dB floor (CI quality gate; requires the quality report).
    #[arg(long)]
    quality_threshold: Option<f64>,
    /// Disable scene-cut keyframe insertion (strict every-48-frames schedule).
    #[arg(long)]
    no_scene_cut: bool,
    /// Rate control: target .ascf size, e.g. `450K`, `1.2M` or plain bytes.
    /// Requires --profile. The compiler probes at --qf, allocates a
    /// per-keyframe QF schedule to hit the target, and re-encodes — complex
    /// GOPs get more bits (higher QF), simple ones fewer. Wire-compatible:
    /// each keyframe already self-describes its QF, so every decoder plays
    /// the stream.
    #[arg(long, value_name = "SIZE")]
    target_size: Option<String>,

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

    let info =
        probe_video(&args.video, false).with_context(|| format!("cannot open {:?}", args.video))?;

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
        (
            args.cols.clamp(1, asciline::protocol::MAX_GRID_COLS),
            args.rows.clamp(1, asciline::protocol::MAX_GRID_ROWS),
        )
    } else {
        asciline::protocol::calc_auto_dimensions(
            args.cols.clamp(1, asciline::protocol::MAX_GRID_COLS),
            info.width,
            info.height,
            pixel,
        )
    };
    println!(
        "[Compiler] {}x{} → grid {}x{} | mode={} | pixel={} | fps={:.3}",
        info.width, info.height, cols, rows, mode, pixel, effective_fps
    );

    // ── Opt-in lossy DCT profile (tag 4): 8x8 blocks over 4:2:0 planes need
    //    cols/rows to be multiples of 16, so the grid is padded up (like compiler.py).
    let profile = args.profile;
    if profile && !matches!(args.aq, 0 | 2 | 4) {
        bail!("--aq must be 0, 2 or 4 (0 = off, tag 4)");
    }
    let (cols, rows) = if profile {
        let pc = cols.div_ceil(16) * 16;
        let pr = rows.div_ceil(16) * 16;
        // Sub-pel motion ladder: tag 7 (quarter-pel) is the default — a strict
        // superset of tag 6 (half-pel), which is a strict superset of the
        // original integer search; --no-qpel falls back to tag 6, and
        // --no-hpel to tags 5/4.
        let qpel = !args.no_qpel && !args.no_hpel;
        let hpel = !args.no_hpel && !qpel;
        let tag = if qpel {
            7
        } else if hpel {
            6
        } else if args.aq > 0 {
            5
        } else {
            4
        };
        let mut aq_note = String::new();
        if args.aq > 0 {
            aq_note.push_str(&format!(" | AQ={} levels", args.aq));
        }
        if qpel {
            aq_note.push_str(" | quarter-pel motion");
        } else if hpel {
            aq_note.push_str(" | half-pel motion");
        }
        aq_note.push_str(&format!(" (tag {tag})"));
        if pc != cols || pr != rows {
            println!(
                "[Compiler] Lossy DCT profile ON | QF={}{aq_note} | grid padded {}x{} → {}x{}",
                args.qf, cols, rows, pc, pr
            );
            (pc, pr)
        } else {
            println!("[Compiler] Lossy DCT profile ON | QF={}{aq_note}", args.qf);
            (cols, rows)
        }
    } else {
        (cols, rows)
    };

    // ── audio extraction (best-effort, like the original) ──
    let base = args.out.clone().unwrap_or_else(|| {
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
    let adaptive_quality =
        !profile && pixel && !args.no_quality && (args.tolerance > 0 || args.quantize > 0);
    let mut adaptive_stats = QualityStats::new();
    let mut profile_enc = if profile {
        let mut pe = ProfileEncoder::new(cols as usize, rows as usize, args.qf.clamp(1, 100));
        pe.r_search = args.r_search.max(0);
        pe.rdo_lambda = args.rdo_lambda.max(0.0);
        pe.aq_levels = args.aq;
        // Sub-pel ladder: tag 7 (quarter-pel) is the compiler default; qpel
        // wins over hpel (a strict superset), and --no-hpel disables both.
        pe.qpel = !args.no_qpel && !args.no_hpel;
        pe.hpel = !args.no_hpel && !pe.qpel;
        pe.qpel_6tap = !args.qpel_bilinear;
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

    // ── decode + encode loop ── (one pass = full decode + encode + write).
    //    Normal compiles run a single pass; rate control (--target-size)
    //    runs a probe pass, then re-encodes with a per-keyframe QF schedule.
    let rate_target: Option<u64> = args.target_size.as_deref().map(parse_size).transpose()?;
    if rate_target.is_some() && !profile {
        bail!("--target-size requires --profile (rate control allocates the per-keyframe QF)");
    }
    if let Some(t) = rate_target {
        if t < 32 {
            bail!("--target-size too small: minimum 32 bytes (18-byte header + one frame)");
        }
    }

    let mut write_pass = |pe: &mut Option<ProfileEncoder>,
                          adaptive_stats: &mut QualityStats,
                          probe: bool,
                          collect_stats: bool|
     -> Result<(u64, u32)> {
        // Each pass is an independent stream: reset encoder state (prev frame,
        // keyframe counter, probe data, stats) and re-decode from the source.
        if let Some(pen) = pe.as_mut() {
            pen.reset();
            pen.probe = probe;
            pen.collect_stats = collect_stats;
        }
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

        let file =
            File::create(&ascf_path).with_context(|| format!("cannot create {:?}", ascf_path))?;
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
            let msg: Vec<u8> = if let Some(pen) = pe.as_mut() {
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
                        let (py, sy, pr) = rgb_vs_bgr(&rgb, shown, cols as usize, rows as usize);
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
        Ok((bytes_written, frame_index))
    };

    let (bytes_written, frame_index) = if let Some(target_bytes) = rate_target {
        // ── rate control: probe → per-GOP QF schedule → iterate to target ──
        //    Keyframes already self-describe their QF (`payload[1]`), so a
        //    per-GOP QF schedule is wire-compatible with every decoder.
        let q0 = args.qf.clamp(1, 100);
        let scene_cut = if args.no_scene_cut {
            0.0
        } else {
            asciline::profile::SCENE_CUT_MAD
        };
        // Pass 1: probe at the base QF with the natural scene-cut grid.
        {
            let pe = profile_enc.as_mut().expect("rate control requires profile");
            pe.qf = q0;
            pe.qf_schedule = None;
            pe.force_keyframes = None;
            pe.scene_cut_mad = scene_cut;
        }
        let (base_bytes, _) = write_pass(&mut profile_enc, &mut adaptive_stats, true, false)?;
        println!(
            "\n[Rate control] pass 1 (probe): QF={q0} → {:.1} KB",
            base_bytes as f64 / 1024.0
        );
        // The probe pass fixes the keyframe grid (every-48 + scene cuts);
        // schedule passes reproduce it exactly via force_keyframes, so the
        // QF schedule (indexed by keyframe) stays aligned pass to pass.
        let (force_kf, model) = {
            let pe = profile_enc.as_mut().expect("rate control requires profile");
            (
                std::mem::take(&mut pe.probe_keyframes),
                std::mem::take(&mut pe.probe_gop_sizes)
                    .into_iter()
                    .map(|s| s as f64)
                    .collect::<Vec<_>>(),
            )
        };
        if model.is_empty() {
            bail!("[Rate control] no frames encoded — nothing to size");
        }
        // size(q) ≈ a_k · r(q)^β_k with r(q) = s(q0)/s(q) and a per-GOP
        // exponent β. The plain 1/step model (β=1) overpredicts badly — this
        // codec's dead-zone quantize + skip threshold make size grow like
        // step^(−0.3…−0.55) — so β starts at an empirical 0.5 and is refit
        // per GOP from each measured pass (two points → exact power law).
        // Per-GOP rate curves: each GOP starts with the probe point (q0, a_k)
        // and accumulates a measured (qf, size) point per schedule pass.
        // Piecewise-linear interpolation between measured points needs no
        // functional form, so it tracks the codec's real curve — which is NOT
        // a power law (dead-zone quantize + skip threshold flatten it at low
        // QF and steepen it near QF=100). Before a second point exists, a
        // default β=0.5 power law fills in.
        let mut curves: Vec<GopCurve> =
            model.iter().map(|&a| GopCurve::new(q0 as i32, a)).collect();
        // ±5% is the acceptance bar; the measured points below close the gap
        // on subsequent passes.
        let sched = if (base_bytes as f64 - target_bytes as f64).abs() <= target_bytes as f64 * 0.05
        {
            vec![q0; model.len()] // already on target: keep the base QF
        } else {
            // Passes 2..4: encode with the schedule; if the measured size
            // misses the target by >5%, fold the measurements into each GOP's
            // curve, reallocate, and move the schedule HALFWAY toward the new
            // proposal. The damped step turns the extreme-case oscillation
            // (2×+ growth targets) into geometric convergence while keeping
            // the common cases to 1-2 schedule passes.
            let mut sched = allocate_qfs(&curves, q0 as i32, target_bytes as f64);
            let mut used: Vec<u8> = sched.clone();
            // The probe's error is the reference sign for the first move.
            let mut prev_err = (base_bytes as f64 - target_bytes as f64) / target_bytes as f64;
            for pass in 0..3u32 {
                {
                    let pe = profile_enc.as_mut().expect("rate control requires profile");
                    pe.qf_schedule = Some(used.clone());
                    pe.force_keyframes = Some(force_kf.clone());
                    pe.scene_cut_mad = 0.0; // grid fully determined by the force set
                }
                let (bytes, _) = write_pass(&mut profile_enc, &mut adaptive_stats, true, false)?;
                let err = (bytes as f64 - target_bytes as f64) / target_bytes as f64;
                println!(
                    "[Rate control] pass {}: {:.1} KB (target {:.1} KB, {:+.1}%)",
                    pass + 2,
                    bytes as f64 / 1024.0,
                    target_bytes as f64 / 1024.0,
                    err * 100.0
                );
                if err.abs() <= 0.05 || pass == 2 {
                    sched = used;
                    break;
                }
                // Fold this pass's per-GOP measurements into the curves.
                let actual = std::mem::take(&mut profile_enc.as_mut().unwrap().probe_gop_sizes);
                if actual.len() == used.len() {
                    for (k, &a) in actual.iter().enumerate() {
                        curves[k].add(used[k] as i32, a as f64);
                    }
                }
                let proposal = allocate_qfs(&curves, q0 as i32, target_bytes as f64);
                // Move fully toward the proposal while the error shrinks
                // monotonically; when the sign flips (an overshoot — the
                // model swung past the target), move only halfway so the
                // next pass lands between the two, converging geometrically.
                let alpha = if err.signum() != prev_err.signum() {
                    0.5
                } else {
                    1.0
                };
                prev_err = err;
                used = proposal
                    .iter()
                    .zip(&used)
                    .map(|(&p, &u)| {
                        let pq = p as i32;
                        let uq = u as i32;
                        (uq + ((pq - uq) as f64 * alpha).round() as i32).clamp(1, 100) as u8
                    })
                    .collect();
            }
            sched
        };
        // Final pass: the converged schedule, quality stats on (feeds the
        // report and --quality-threshold), file kept.
        {
            let pe = profile_enc.as_mut().expect("rate control requires profile");
            pe.qf_schedule = Some(sched);
            pe.force_keyframes = Some(force_kf);
            pe.scene_cut_mad = 0.0;
        }
        let (bytes_written, frame_index) = write_pass(
            &mut profile_enc,
            &mut adaptive_stats,
            false,
            !args.no_quality,
        )?;
        let err = (bytes_written as f64 - target_bytes as f64) / target_bytes as f64;
        println!(
            "[Rate control] final: {:.1} KB (target {:.1} KB, {:+.1}%)",
            bytes_written as f64 / 1024.0,
            target_bytes as f64 / 1024.0,
            err * 100.0
        );
        if err.abs() > 0.05 {
            println!(
                "[Rate control] WARNING: {:+.1}% off target — QFs bottomed/tapped out; pick a --qf closer to the natural size",
                err * 100.0
            );
        } else {
            println!("[Rate control] target hit within ±5%");
        }
        (bytes_written, frame_index)
    } else {
        write_pass(
            &mut profile_enc,
            &mut adaptive_stats,
            false,
            !args.no_quality,
        )?
    };

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
                let qf_label = match rate_target {
                    Some(t) => format!("rate-controlled → {:.1} KB", t as f64 / 1024.0),
                    None => args.qf.clamp(1, 100).to_string(),
                };
                print_quality_report(
                    s,
                    &format!(
                        "Lossy DCT reconstruction vs source ({} frames, QF={}):",
                        s.frames(),
                        qf_label
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

    // ── CI quality gate: exit non-zero when the reconstruction is worse than
    //    the requested floor (mean PSNR-Y over the whole clip). ──
    if let Some(floor) = args.quality_threshold {
        if args.no_quality {
            bail!("--quality-threshold requires the quality report (drop --no-quality)");
        }
        let mean = if let Some(pen) = profile_enc.as_ref() {
            pen.stats().psnr_y_mean()
        } else if adaptive_quality && adaptive_stats.frames() > 0 {
            adaptive_stats.psnr_y_mean()
        } else {
            bail!("--quality-threshold needs a lossy compile (--profile, or --pixel with --tolerance/--quantize)");
        };
        // ∞ mean (lossless reconstruction) is strictly better than any finite
        // floor, so it passes; anything else — a finite mean below the floor,
        // or NaN (e.g. a zero-frame compile) — fails closed.
        let ok = mean.is_infinite() || (mean.is_finite() && mean >= floor);
        if !ok {
            bail!("quality gate failed: mean PSNR-Y {mean:?} < required {floor:.2} dB");
        }
        println!("[Quality] gate passed: mean PSNR-Y {mean:.2} dB ≥ {floor:.2} dB");
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
        .args([
            "-vn",
            "-acodec",
            "libmp3lame",
            "-ab",
            "128k",
            "-ar",
            "44100",
        ])
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

/// Parse a size like `450K`, `1.2M` or plain bytes into a byte count.
fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim().to_ascii_lowercase();
    let (num, mult) = if let Some(n) = s.strip_suffix('k') {
        (n, 1024.0)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 1024.0 * 1024.0)
    } else {
        (s.as_str(), 1.0)
    };
    let v: f64 = num
        .trim()
        .parse()
        .with_context(|| format!("invalid --target-size: {s:?}"))?;
    let bytes = v * mult;
    if !bytes.is_finite() || bytes < 1.0 || bytes > u64::MAX as f64 {
        bail!("--target-size out of range: {s:?}");
    }
    Ok(bytes.round() as u64)
}

/// JPEG-style quant scale factor for a QF (mirrors `qtables` in profile.rs):
/// `s = 5000/qf` below 50, `s = 200 − 2·qf` at/above. Floored at 1 so the
/// rate model stays finite at QF=100, where every quant step clamps to 1.
fn qf_scale(q: i32) -> f64 {
    if q < 50 {
        5000.0 / q as f64
    } else {
        ((200 - 2 * q) as f64).max(1.0)
    }
}

/// One GOP's measured size curve: `(qf, wire bytes)` points interpolated
/// piecewise-linearly; outside the measured range the nearest segment's
/// slope is extended. With a single point (the probe at q0) a default
/// β=0.5 power law fills in until a second measurement exists.
struct GopCurve {
    points: Vec<(i32, f64)>, // sorted by qf; first is always (q0, a)
}

impl GopCurve {
    fn new(q0: i32, a: f64) -> GopCurve {
        GopCurve {
            points: vec![(q0, a)],
        }
    }

    /// Add a measured point, replacing any existing point at the same QF.
    fn add(&mut self, q: i32, s: f64) {
        if let Some(i) = self.points.iter().position(|&(qq, _)| qq == q) {
            self.points[i] = (q, s);
            return;
        }
        let i = self.points.partition_point(|&(qq, _)| qq < q);
        self.points.insert(i, (q, s));
    }

    /// Predicted wire size at QF `q`.
    fn size(&self, q: i32) -> f64 {
        if self.points.len() == 1 {
            let (q0, a) = self.points[0];
            return a * (qf_scale(q0) / qf_scale(q)).powf(0.5);
        }
        let pts = &self.points;
        if q < pts[0].0 {
            let (q1, s1) = pts[0];
            let (q2, s2) = pts[1];
            let slope = (s2 - s1) / (q2 - q1) as f64;
            return (s1 + slope * (q - q1) as f64).max(1.0);
        }
        for w in pts.windows(2) {
            let (q1, s1) = w[0];
            let (q2, s2) = w[1];
            if q <= q2 {
                let t = (q - q1) as f64 / (q2 - q1) as f64;
                return s1 + t * (s2 - s1);
            }
        }
        let (q1, s1) = pts[pts.len() - 2];
        let (q2, s2) = pts[pts.len() - 1];
        let slope = (s2 - s1) / (q2 - q1) as f64;
        (s2 + slope * (q - q2) as f64).max(1.0)
    }
}

/// Marginal-allocation rate control: pick per-GOP QFs so the predicted total
/// (`Σ curves[k].size(q_k)`) lands on `target` bytes while keeping QFs as
/// close to `q0` as possible — complex GOPs get more bits (higher QF),
/// simple ones fewer. Greedy marginal exchange: each step moves the GOP
/// with the best bits-per-quality-point ratio, terminating on target within
/// one marginal unit.
fn allocate_qfs(curves: &[GopCurve], q0: i32, target: f64) -> Vec<u8> {
    use std::cmp::{Ordering, Reverse};
    use std::collections::BinaryHeap;

    #[derive(Clone, Copy)]
    struct Entry {
        ratio: f64,
        k: usize,
        q: i32,
    }
    impl PartialEq for Entry {
        fn eq(&self, o: &Self) -> bool {
            self.k == o.k && self.q == o.q
        }
    }
    impl Eq for Entry {}
    impl PartialOrd for Entry {
        fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
            Some(self.cmp(o))
        }
    }
    impl Ord for Entry {
        fn cmp(&self, o: &Self) -> Ordering {
            self.ratio.total_cmp(&o.ratio)
        }
    }

    let n = curves.len();
    let mut qs = vec![q0 as u8; n];
    let size = |qs: &[u8]| -> f64 {
        qs.iter()
            .enumerate()
            .map(|(k, &q)| curves[k].size(q as i32))
            .sum()
    };
    let mut cur = size(&qs);

    if target <= cur {
        // Shrink toward QF=1: drop the GOP that loses the least size per
        // quality point saved.
        let push = |heap: &mut BinaryHeap<Reverse<Entry>>, qs: &[u8], k: usize| {
            let q = qs[k] as i32;
            if q <= 1 {
                return;
            }
            let lost = curves[k].size(q) - curves[k].size(q - 1);
            let saved = (2 * (q - q0) - 1) as f64; // cost removed by q → q−1
            heap.push(Reverse(Entry {
                ratio: lost / saved.max(1e-9),
                k,
                q,
            }));
        };
        let mut heap: BinaryHeap<Reverse<Entry>> = BinaryHeap::new();
        for k in 0..n {
            push(&mut heap, &qs, k);
        }
        while cur > target {
            let Some(Reverse(e)) = heap.pop() else { break };
            if e.q != qs[e.k] as i32 || e.q <= 1 {
                continue; // stale entry
            }
            qs[e.k] = (e.q - 1) as u8;
            cur -= curves[e.k].size(e.q) - curves[e.k].size(e.q - 1);
            push(&mut heap, &qs, e.k);
        }
    } else {
        // Grow toward QF=100: bump the GOP with the best bits-per-quality-
        // point ratio.
        let push = |heap: &mut BinaryHeap<Entry>, qs: &[u8], k: usize| {
            let q = qs[k] as i32;
            if q >= 100 {
                return;
            }
            let gain = curves[k].size(q + 1) - curves[k].size(q);
            let cost = (2 * (q - q0) + 1) as f64;
            heap.push(Entry {
                ratio: gain / cost.max(1e-9),
                k,
                q,
            });
        };
        let mut heap: BinaryHeap<Entry> = BinaryHeap::new();
        for k in 0..n {
            push(&mut heap, &qs, k);
        }
        while cur < target {
            let Some(e) = heap.pop() else { break };
            if e.q != qs[e.k] as i32 || e.q >= 100 {
                continue; // stale entry
            }
            qs[e.k] = (e.q + 1) as u8;
            cur += curves[e.k].size(e.q + 1) - curves[e.k].size(e.q);
            push(&mut heap, &qs, e.k);
        }
    }
    qs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_suffixes() {
        assert_eq!(parse_size("500").unwrap(), 500);
        assert_eq!(parse_size("450K").unwrap(), 460_800);
        assert_eq!(parse_size("1.2M").unwrap(), 1_258_291);
        assert_eq!(parse_size(" 64k ").unwrap(), 65_536);
        assert!(parse_size("abc").is_err());
        assert!(parse_size("-5").is_err());
    }

    #[test]
    fn gop_curve_single_point_uses_power_law() {
        let c = GopCurve::new(70, 1000.0);
        assert_eq!(c.size(70), 1000.0);
        assert!(c.size(80) > 1000.0, "finer quant must be bigger");
        assert!(c.size(60) < 1000.0, "coarser quant must be smaller");
    }

    #[test]
    fn gop_curve_interpolates_through_measured_points() {
        let mut c = GopCurve::new(70, 100.0);
        c.add(85, 300.0);
        c.add(95, 900.0);
        assert_eq!(c.size(70), 100.0);
        assert_eq!(c.size(85), 300.0);
        assert_eq!(c.size(95), 900.0);
        assert!((c.size(90) - 600.0).abs() < 1e-9, "linear midpoint");
        // Re-adding a point at the same QF replaces it.
        c.add(85, 350.0);
        assert_eq!(c.size(85), 350.0);
        for q in 1..100 {
            assert!(c.size(q + 1) >= c.size(q), "non-monotone at q={q}");
        }
    }

    #[test]
    fn allocate_lands_total_on_target() {
        let curves: Vec<GopCurve> = vec![100.0, 200.0, 400.0]
            .into_iter()
            .map(|a| GopCurve::new(70, a))
            .collect();
        let q0 = 70;
        let base: f64 = curves.iter().map(|c| c.size(q0)).sum();
        for target in [350.0, 700.0, 1400.0] {
            let qs = allocate_qfs(&curves, q0, target);
            assert!(
                qs.iter().all(|&q| (1..=100).contains(&q)),
                "QF out of range"
            );
            let total: f64 = qs
                .iter()
                .enumerate()
                .map(|(k, &q)| curves[k].size(q as i32))
                .sum();
            // Greedy granularity: within one marginal unit of the target, and
            // always on the correct side of the base size.
            if target > base {
                assert!(total >= base - 1e-9, "grow must not shrink");
            } else {
                assert!(total <= base + 1e-9, "shrink must not grow");
            }
            assert!(
                (total - target).abs() / target < 0.15,
                "target {target}: predicted total {total} too far off"
            );
        }
    }

    #[test]
    fn allocate_keeps_q0_when_target_equals_base() {
        let curves: Vec<GopCurve> = vec![100.0, 200.0, 400.0]
            .into_iter()
            .map(|a| GopCurve::new(70, a))
            .collect();
        let q0 = 70;
        let base: f64 = curves.iter().map(|c| c.size(q0)).sum();
        let qs = allocate_qfs(&curves, q0, base);
        assert!(qs.iter().all(|&q| q == q0 as u8));
    }
}
