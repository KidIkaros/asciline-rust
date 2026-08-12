//! Video decoding via the ffmpeg CLI.
//!
//! The original Python project decodes with OpenCV. We decode by spawning the
//! `ffmpeg` binary with `-vf scale=WxH,fps=N` so the resize + decimation happen
//! inside ffmpeg's SIMD-optimized filter graph and only a tiny raw RGB24 frame
//! (`cols*rows*3` bytes) crosses the pipe — no C bindings, no system dev
//! libraries required.
//!
//! Decoding runs on a dedicated OS thread so it overlaps with mapping/encoding
//! and network sends (this is one of the key >30fps wins over the serial
//! Python loop).

use std::io::Read;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::AtomicU32;

use anyhow::{bail, Context, Result};

/// Metadata probed via `ffprobe`.
#[derive(Debug, Clone)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub duration: f64,
    pub frame_count: u64,
}

/// Parse an ffprobe `r_frame_rate`-style "num/den" string.
fn parse_frac(s: &str) -> Option<f64> {
    let (n, d) = s.split_once('/')?;
    let n: f64 = n.parse().ok()?;
    let d: f64 = d.parse().ok()?;
    if d == 0.0 {
        None
    } else {
        Some(n / d)
    }
}

/// Probe a media file (or v4l2 device) for width/height/fps/duration.
pub fn probe_video(src: &str, is_webcam: bool) -> Result<VideoInfo> {
    let mut cmd = Command::new("ffprobe");
    cmd.arg("-v").arg("error");
    cmd.arg("-select_streams").arg("v:0");
    cmd.arg("-show_entries")
        .arg("stream=width,height,r_frame_rate,avg_frame_rate,nb_frames,duration");
    cmd.arg("-of").arg("json");
    if is_webcam {
        // v4l2 input: force a frame rate so ffprobe doesn't hang waiting for input.
        cmd.args(["-f", "v4l2", "-framerate", "30"]);
    }
    cmd.arg(src);

    let out = cmd
        .output()
        .with_context(|| format!("failed to run ffprobe on {src:?}"))?;
    if !out.status.success() {
        bail!(
            "ffprobe failed on {:?}: {}",
            src,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("ffprobe returned invalid JSON")?;
    let stream = json["streams"]
        .as_array()
        .and_then(|a| a.first())
        .context("no video stream found")?;

    let width = stream["width"].as_u64().unwrap_or(0) as u32;
    let height = stream["height"].as_u64().unwrap_or(0) as u32;
    let fps = stream["avg_frame_rate"]
        .as_str()
        .and_then(parse_frac)
        .or_else(|| stream["r_frame_rate"].as_str().and_then(parse_frac))
        .unwrap_or(24.0);
    let duration = stream["duration"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| stream["duration"].as_f64())
        .unwrap_or(0.0);
    let frame_count = stream["nb_frames"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    Ok(VideoInfo {
        width,
        height,
        fps,
        duration,
        frame_count,
    })
}

/// Parameters for spawning the ffmpeg decode pipe.
#[derive(Debug, Clone)]
pub struct SourceParams {
    /// Path to the video file, or e.g. `/dev/video0` for webcam mode.
    pub src: String,
    pub is_webcam: bool,
    pub cols: u32,
    pub rows: u32,
    /// Target output fps (`fps=` filter). `None` = native source rate.
    pub target_fps: Option<f64>,
    /// Seek to this second before decoding.
    pub seek_secs: f64,
    /// Horizontal mirror (webcam selfie view).
    pub mirror: bool,
}

fn build_ffmpeg_cmd(p: &SourceParams) -> Command {
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-nostdin").arg("-v").arg("error");
    cmd.arg("-fflags").arg("+genpts");

    if p.is_webcam {
        // v4l2 input options must precede `-i`.
        cmd.args(["-f", "v4l2", "-framerate", "30"]);
    } else if p.seek_secs > 0.0 {
        cmd.args(["-ss", &format!("{:.3}", p.seek_secs)]);
    }

    cmd.arg("-i").arg(&p.src);

    let mut vf = format!("scale={}:{}:flags=bilinear", p.cols, p.rows);
    if let Some(fps) = p.target_fps {
        vf.push_str(&format!(",fps={}", format_fps(fps)));
    }
    if p.mirror {
        vf.push_str(",hflip");
    }
    cmd.args(["-vf", &vf])
        .args(["-pix_fmt", "rgb24", "-f", "rawvideo"])
        .arg("pipe:1");

    cmd
}

/// Format an fps value the way ffmpeg's `fps=` filter accepts it.
fn format_fps(fps: f64) -> String {
    if fps.fract() == 0.0 {
        format!("{:.0}", fps)
    } else {
        format!("{:.6}", fps)
    }
}

/// Reads raw RGB24 frames (`cols*rows*3` bytes each) from an ffmpeg child.
pub struct FrameReader {
    child: Child,
    stdout: ChildStdout,
    pub cols: u32,
    pub rows: u32,
    frame_bytes: usize,
}

impl FrameReader {
    /// Spawn the ffmpeg decode pipe. The child PID is published into `pid` the
    /// instant the process spawns (before any blocking read) so a concurrent
    /// shutdown can always interrupt a blocked pipe read.
    pub fn new(p: &SourceParams, pid: &AtomicU32) -> Result<FrameReader> {
        if !Path::new(&p.src).exists() && !p.is_webcam {
            bail!("video file not found: {:?}", p.src);
        }
        let mut child = build_ffmpeg_cmd(p)
            .stdin(Stdio::null())
            // stderr is drained to null: `-v error` output is diagnostic only, and
            // leaving the pipe unread would let ffmpeg block forever once the
            // 64KB pipe buffer fills (pathological error-heavy input).
            .stderr(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .context("failed to spawn ffmpeg")?;
        pid.store(child.id(), std::sync::atomic::Ordering::SeqCst);

        let stdout = child.stdout.take().context("no stdout on ffmpeg")?;
        let frame_bytes = (p.cols * p.rows * 3) as usize;
        Ok(FrameReader {
            child,
            stdout,
            cols: p.cols,
            rows: p.rows,
            frame_bytes,
        })
    }

    /// Read one frame. Returns `None` at EOF / decode error.
    pub fn read_frame(&mut self) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; self.frame_bytes];
        let mut filled = 0usize;
        while filled < self.frame_bytes {
            match self.stdout.read(&mut buf[filled..]) {
                Ok(0) => return None, // EOF
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return None,
            }
        }
        Some(buf)
    }
}

impl Drop for FrameReader {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
