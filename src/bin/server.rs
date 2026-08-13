//! asciline-server — drop-in Rust replacement for `stream_server.py`.
//!
//! ```text
//! asciline-server video.mp4 --cols 240
//! asciline-server --folder videos --cols 200 --loop
//! asciline-server --playlist playlist.json --mode 4
//! asciline-server --webcam --cols 240
//! ```
//!
//! Open http://localhost:8000 — the original browser client works unchanged.
//! Unlike the Python original there is no hard 30 FPS cap: sources play at
//! their native rate (or whatever `--fps N` asks for).

use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use anyhow::Result;
use asciline::queue::{load_folder, load_playlist, resolve_video_path, QueueEntry};
use asciline::server::{app, AppState};
use clap::Parser;
use tokio::sync::Mutex;

#[derive(Parser, Debug)]
#[command(
    name = "asciline-server",
    version,
    about = "Real-time ASCII video Web server (Rust port of ASCILINE)"
)]
struct Args {
    /// Single video file to stream.
    #[arg(default_value = "video.mp4")]
    video: String,

    /// JSON playlist file.
    #[arg(long)]
    playlist: Option<String>,

    /// Folder of videos, played in filesystem order.
    #[arg(long)]
    folder: Option<String>,

    /// Use a webcam instead of a file.
    #[arg(long)]
    webcam: bool,
    #[arg(long, default_value_t = 0)]
    webcam_device: u32,
    #[arg(long, default_value_t = 30)]
    webcam_fps: u32,
    #[arg(long)]
    no_mirror: bool,

    /// Color quality: 1=B&W 2=64c 3=512c 4=32Kc 5=262Kc 6=16M Ultra.
    #[arg(long, default_value_t = 1)]
    mode: u8,
    /// Pixel mode: colored blocks instead of characters.
    #[arg(long)]
    pixel: bool,
    /// Grid columns (default: 200 text / 450 pixel).
    #[arg(long)]
    cols: Option<u32>,
    /// Grid rows (0 = auto from aspect ratio).
    #[arg(long, default_value_t = 0)]
    rows: u32,

    /// Volume 0-5 (0 = muted, 1 = normal, 5 = double).
    #[arg(long, default_value_t = 1)]
    vol: u8,
    /// Loop the queue infinitely.
    #[arg(long, alias = "loop")]
    loop_playback: bool,
    /// Adaptive-codec color fidelity (lossless = bit-exact).
    #[arg(long, default_value = "lossless")]
    quality: String,

    /// Target streaming FPS. Default = source FPS (no 30fps cap in this port).
    #[arg(long)]
    fps: Option<f64>,

    /// Bind address.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 8000)]
    port: u16,
    /// Bandwidth debug logging (RAW vs WIRE).
    #[arg(long)]
    debug: bool,
    /// Disable seek-bar hover thumbnails.
    #[arg(long)]
    no_thumbnails: bool,

    /// Maximum concurrent WebSocket clients (each runs its own ffmpeg child +
    /// decode thread + encode work). Extra connections get a 503.
    #[arg(long, default_value_t = 8)]
    max_clients: usize,

    /// Concurrent ffmpeg spawns for /audio transcodes and scrub-sprite builds.
    #[arg(long, default_value_t = 4)]
    max_ffmpeg: usize,

    /// Optional shared secret: when set, /ws, /audio and /scrub* require
    /// `?token=<secret>`. Note the original browser client does not send one,
    /// so the token must be appended to the URL (see README security section).
    /// Also settable via the ASCILINE_TOKEN environment variable (systemd
    /// EnvironmentFile / docker-compose `environment:`).
    #[arg(long, env = "ASCILINE_TOKEN", hide_env_values = true)]
    token: Option<String>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();

    // Python-compatible guardrails
    let mode = if args.pixel && args.mode == 1 {
        6
    } else {
        args.mode
    };
    let pixel = args.pixel;
    if pixel && args.quality != "lossless" {
        anyhow::bail!("--pixel mode sends raw data and does not support the adaptive codec. Remove the --quality flag.");
    }
    if !(1..=6).contains(&mode) {
        anyhow::bail!("--mode must be 1-6");
    }
    let tolerance = match args.quality.as_str() {
        "lossless" => 0,
        "high" => 4,
        "balanced" => 8,
        "low" => 16,
        other => anyhow::bail!("unknown --quality {other:?} (lossless|high|balanced|low)"),
    };
    // An empty ASCILINE_TOKEN env var (e.g. `ASCILINE_TOKEN: ${ASCILINE_TOKEN:-}`
    // in compose when .env omits it) must mean "unset", not "auth with an
    // unusable empty secret".
    let token = args.token.filter(|t| !t.is_empty());

    let defaults =
        QueueEntry::from_file(String::new(), mode, args.vol, pixel, args.rows, args.cols);

    // ── Build the queue (priority: webcam > playlist > folder > single file) ──
    let queue: Vec<QueueEntry> = if args.webcam {
        vec![QueueEntry {
            video: format!("/dev/video{}", args.webcam_device),
            mode,
            vol: args.vol,
            pixel,
            rows: args.rows,
            cols_override: args.cols,
            is_webcam: true,
            mirror: !args.no_mirror,
            fallback_fps: args.webcam_fps as f64,
        }]
    } else if let Some(p) = &args.playlist {
        println!("[PLAYLIST] Loading: {p}");
        let mut items = load_playlist(p, &defaults)?;
        if let Some(c) = args.cols {
            for it in items.iter_mut() {
                it.cols_override = Some(c);
            }
        }
        items
    } else if let Some(f) = &args.folder {
        println!("[FOLDER] Scanning: {f}");
        let mut items = load_folder(f, &defaults)?;
        if let Some(c) = args.cols {
            for it in items.iter_mut() {
                it.cols_override = Some(c);
            }
        }
        items
    } else {
        let resolved = resolve_video_path(&args.video);
        vec![QueueEntry {
            video: resolved,
            mode,
            vol: args.vol,
            pixel,
            rows: args.rows,
            cols_override: args.cols,
            is_webcam: false,
            mirror: false,
            fallback_fps: 0.0,
        }]
    };

    if queue.is_empty() {
        anyhow::bail!("no videos found — check --playlist / --folder / the video argument");
    }

    // Warm up the first video's page cache so the initial connect is instant.
    if let Some(first) = queue.first() {
        if !first.is_webcam {
            println!("> Warming up cache for first video...");
            let _ = asciline::video::probe_video(&first.video, false);
            let _ = std::process::Command::new("ffmpeg")
                .args([
                    "-nostdin",
                    "-v",
                    "error",
                    "-i",
                    &first.video,
                    "-frames:v",
                    "1",
                    "-f",
                    "null",
                    "-",
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }

    // ── Banner ──
    println!();
    println!("{}", banner());
    println!(" {}", "═".repeat(55));
    println!(" > Queue      : {} video(s)", queue.len());
    println!(
        " > Loop       : {}",
        if args.loop_playback { "ON" } else { "OFF" }
    );
    println!(
        " > Resolution  : {}x({})",
        args.cols.unwrap_or(if pixel { 450 } else { 200 }),
        if args.rows > 0 {
            args.rows.to_string()
        } else {
            "auto".into()
        }
    );
    println!(
        " > Target FPS : {}",
        args.fps
            .map(|f| f.to_string())
            .unwrap_or_else(|| "source (no cap)".into())
    );
    for (i, e) in queue.iter().enumerate() {
        println!(
            "  {:2}. {}  (mode={}{} vol={})",
            i + 1,
            e.video,
            e.mode,
            if e.pixel { " [PIXEL]" } else { "" },
            e.vol
        );
    }
    println!(" {}", "═".repeat(55));
    println!();
    println!(" [+] Server live → http://{}:{}", args.host, args.port);
    println!();

    // ── web/ assets dir ──
    let web_dir = find_web_dir();
    if !web_dir.join("index.html").exists() {
        anyhow::bail!("web/ assets not found (looked in {:?})", web_dir);
    }

    let state = AppState {
        queue,
        current_index: Arc::new(AtomicUsize::new(0)),
        loop_playback: args.loop_playback,
        debug: args.debug,
        tolerance,
        thumbnails: !args.no_thumbnails,
        fps_override: args.fps,
        web_dir,
        scrub_cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
        client_permits: Arc::new(tokio::sync::Semaphore::new(args.max_clients.max(1))),
        max_clients: args.max_clients.max(1),
        ffmpeg_permits: Arc::new(tokio::sync::Semaphore::new(args.max_ffmpeg.max(1))),
        token,
    };
    if state.token.is_some() {
        println!(" > Auth       : token required (?token=... on /ws, /audio, /scrub*)");
    } else {
        println!(" > Auth       : none (bind 127.0.0.1 by default; see README security section)");
    }
    println!(" > Max clients: {}", args.max_clients.max(1));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(4)
        .build()?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind((args.host.as_str(), args.port)).await?;
        println!("[+] Listening on {}", listener.local_addr()?);
        tracing::info!(addr = %listener.local_addr()?, "asciline-server listening");
        axum::serve(listener, app(state))
            .with_graceful_shutdown(shutdown_signal())
            .await?;
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

/// Wait for SIGINT, then let axum drain in-flight connections before exiting.
async fn shutdown_signal() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::error!(%e, "failed to install SIGINT handler");
    }
    tracing::info!("shutdown signal received — draining connections");
}

fn find_web_dir() -> PathBuf {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_default();
    let candidates = [
        exe.join("web"),
        exe.join("../web"),
        PathBuf::from("web"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web"),
    ];
    candidates
        .into_iter()
        .find(|p| p.join("index.html").exists())
        .unwrap_or_else(|| PathBuf::from("web"))
}

fn banner() -> &'static str {
    "  ▄▄▄▄▄▄▄ ▄▄▄▄▄▄▄ ▄▄▄▄▄▄  ▄▄▄  ▄▄▄▄▄▄▄ ▄    ▄ ▄▄▄▄▄▄▄ ▄▄▄▄▄▄  ▄▄▄▄▄▄  \n  █       █       █      \\█   █ █       █ █  █ █ █       █      \\█      \\ \n  █    ▄▄▄█   ▄   █  ▄▄▄▄▄█   █ █    ▄▄▄█ █  █▄█ █   ▄   █  ▄▄▄▄▄█  ▄▄▄▄▄█ \n  █   █▄▄▄█  █▄█  █ █▄▄▄▄▄█   █ █   █▄▄▄█ █       █  █▄█  █ █▄▄▄▄▄█ █▄▄▄▄▄  \n  █    ▄▄▄█       █▄▄▄▄▄  █   █▄█    ▄▄▄█ █▄▄▄▄▄▄▄█       █▄▄▄▄▄  █▄▄▄▄▄  \n  █   █▄▄▄█   ▄   █▄▄▄▄▄█  █▄▄█    █▄▄▄█       █   ▄   █▄▄▄▄▄█  █▄▄▄▄▄█ \n  █▄▄▄▄▄▄▄█▄▄█ █▄▄█▄▄▄▄▄▄▄█▄▄▄▄▄▄▄█▄▄▄▄▄▄▄█       █▄▄█ █▄▄█▄▄▄▄▄▄▄█▄▄▄▄▄▄  \n  ASCILINE·RS  —  Rust real-time ASCII streaming server"
}
