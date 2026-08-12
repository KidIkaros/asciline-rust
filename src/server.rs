//! Drop-in ASCILINE web server (Rust port of `stream_server.py`).
//!
//! Speaks the exact original wire protocol so the *unchanged* browser client
//! (`web/index.html` + `app.js` + `codec.js`) works against it:
//!
//! - `GET /`                    → player HTML
//! - `GET /static/{file}`       → whitelisted `app.js` / `style.css` / `codec.js`
//! - `GET /audio?v=&start=`     → MP3 transcode of the current video (ffmpeg)
//! - `GET /scrub`, `/scrub_sprite` → seek-bar hover thumbnails
//! - `WS /ws?codec=adaptive`    → INIT text + binary frame stream
//!
//! The stream pipeline is explicitly designed to beat the 30 fps ceiling of the
//! Python original: a dedicated OS thread decodes frames from ffmpeg into a
//! bounded channel (decode overlaps map/encode/send), mapping + adaptive
//! encoding run on the tokio blocking pool, and the async task only does
//! pacing + WebSocket sends. No per-frame Python/NumPy overhead, no GIL.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::header;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::io::AsyncReadExt;

use crate::audio;
use crate::codec::CodecEncoder;
use crate::filters;
use crate::mapper::{Mapper, Palette};
use crate::protocol::init_message;
use crate::queue::QueueEntry;
use crate::video::{probe_video, FrameReader, SourceParams};

// ────────────────────────────────────────────────────────────────────────────
// App state
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ScrubSprite {
    pub meta: serde_json::Value,
    pub jpeg: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct AppState {
    pub queue: Vec<QueueEntry>,
    pub current_index: Arc<AtomicUsize>,
    pub loop_playback: bool,
    pub debug: bool,
    /// Colour drift tolerance for the adaptive codec (lossless/high/balanced/low).
    pub tolerance: u32,
    pub thumbnails: bool,
    /// Global target fps override (`--fps`). `None` = run at source fps (no 30 cap).
    pub fps_override: Option<f64>,
    pub web_dir: PathBuf,
    pub scrub_cache: Arc<tokio::sync::Mutex<std::collections::HashMap<String, ScrubSprite>>>,
}

/// Build the axum router. Also used by the e2e integration test.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/static/{filename}", get(static_file))
        .route("/audio", get(audio_stream))
        .route("/scrub", get(scrub_meta))
        .route("/scrub_sprite", get(scrub_sprite))
        .route("/ws", get(ws_handler))
        .with_state(Arc::new(state))
}

// ────────────────────────────────────────────────────────────────────────────
// HTTP endpoints
// ────────────────────────────────────────────────────────────────────────────

async fn root(State(state): State<Arc<AppState>>) -> Result<Html<String>, StatusCode> {
    let html = tokio::fs::read_to_string(state.web_dir.join("index.html"))
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    Ok(Html(html))
}

const STATIC_WHITELIST: &[&str] = &["app.js", "style.css", "codec.js"];

async fn static_file(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(filename): axum::extract::Path<String>,
) -> Response {
    if !STATIC_WHITELIST.contains(&filename.as_str()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let mime = match filename.as_str() {
        "app.js" | "codec.js" => "text/javascript",
        "style.css" => "text/css",
        _ => "application/octet-stream",
    };
    match tokio::fs::read(state.web_dir.join(&filename)).await {
        Ok(bytes) => Response::builder()
            .header(header::CONTENT_TYPE, mime)
            .body(Body::from(bytes))
            .unwrap(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Deserialize, Default)]
struct AudioQuery {
    v: Option<u32>,
    start: Option<f64>,
}

async fn audio_stream(
    State(state): State<Arc<AppState>>,
    Query(q): Query<AudioQuery>,
) -> Response {
    let idx = q.v.unwrap_or_else(|| state.current_index.load(Ordering::SeqCst) as u32) as usize;
    let Some(entry) = state.queue.get(idx) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Webcam has no audio file; vol 0 → no ffmpeg run (CPU/bandwidth saving).
    if entry.is_webcam || entry.vol == 0 {
        return StatusCode::NO_CONTENT.into_response();
    }
    if !Path::new(&entry.video).exists() {
        return StatusCode::NOT_FOUND.into_response();
    }

    let start = q.start.unwrap_or(0.0);
    let vol = entry.vol;
    let video_path = entry.video.clone();

    let mut args: Vec<String> = vec!["-nostdin".to_string()];
    if start > 0.0 {
        args.push("-ss".to_string());
        args.push(format!("{:.3}", start));
    }
    args.push("-i".to_string());
    args.push(video_path.clone());
    args.extend([
        "-vn".to_string(),
        "-filter:a".to_string(),
        format!("volume={:.3}", audio::ffmpeg_volume(vol)),
        "-acodec".to_string(),
        "libmp3lame".to_string(),
        "-ab".to_string(),
        "128k".to_string(),
        "-ar".to_string(),
        "44100".to_string(),
        "-f".to_string(),
        "mp3".to_string(),
        "-loglevel".to_string(),
        "quiet".to_string(),
        "pipe:1".to_string(),
    ]);
    let mut child = match tokio::process::Command::new("ffmpeg")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let stdout = child.stdout.take().expect("piped stdout");
    let stream = futures_util::stream::unfold((stdout, Some(child)), |(mut s, mut c)| async move {
        let mut buf = vec![0u8; 8192];
        match s.read(&mut buf).await {
            Ok(0) | Err(_) => {
                if let Some(mut c) = c.take() {
                    let _ = c.kill().await;
                    let _ = c.wait().await;
                }
                None
            }
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<_, std::io::Error>(buf), (s, c)))
            }
        }
    });

    Response::builder()
        .header(header::CONTENT_TYPE, "audio/mpeg")
        .header("Accept-Ranges", "bytes")
        .body(Body::from_stream(stream))
        .unwrap()
}

async fn scrub_meta(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ScrubQuery>,
) -> Response {
    if !state.thumbnails {
        return json_response(&serde_json::json!({"available": false}));
    }
    let idx = q.v.unwrap_or_else(|| state.current_index.load(Ordering::SeqCst) as u32) as usize;
    let Some(entry) = state.queue.get(idx) else {
        return json_response(&serde_json::json!({"available": false}));
    };
    if entry.is_webcam || !Path::new(&entry.video).exists() {
        return json_response(&serde_json::json!({"available": false}));
    }
    let path = entry.video.clone();
    let cache = state.scrub_cache.clone();
    let sprite = {
        let guard = cache.lock().await;
        guard.get(&path).cloned()
    };
    let sprite = match sprite {
        Some(s) => s,
        None => {
            // Build once per video on first request (off the async path).
            let path_for_build = path.clone();
            let built = tokio::task::spawn_blocking(move || build_scrub_sprite(&path_for_build, 64, 160))
                .await
                .ok()
                .flatten();
            if let Some(s) = built {
                let mut guard = cache.lock().await;
                guard.insert(path.clone(), s.clone());
                s
            } else {
                return json_response(&serde_json::json!({"available": false}));
            }
        }
    };
    let mut meta = sprite.meta.as_object().cloned().unwrap_or_default();
    let vid_id = Path::new(&path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    meta.insert(
        "sprite".into(),
        serde_json::Value::String(format!("/scrub_sprite?v={}&id={}", idx, vid_id)),
    );
    json_response(&serde_json::Value::Object(meta))
}

async fn scrub_sprite(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ScrubQuery>,
) -> Response {
    let idx = q.v.unwrap_or_else(|| state.current_index.load(Ordering::SeqCst) as u32) as usize;
    let Some(entry) = state.queue.get(idx) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let cache = state.scrub_cache.lock().await;
    match cache.get(&entry.video) {
        Some(s) => Response::builder()
            .header(header::CONTENT_TYPE, "image/jpeg")
            .body(Body::from(s.jpeg.clone()))
            .unwrap(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Deserialize, Default)]
struct ScrubQuery {
    v: Option<u32>,
}

fn json_response(v: &serde_json::Value) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(v.to_string()))
        .unwrap()
}

/// Build a tiled JPEG hover-preview sprite with a single ffmpeg pass.
fn build_scrub_sprite(video_path: &str, max_count: usize, cell_w: u32) -> Option<ScrubSprite> {
    let info = probe_video(video_path, false).ok()?;
    let (w0, h0) = (info.width, info.height);
    let fps = if info.fps > 0.0 { info.fps } else { 25.0 };
    let total = if info.frame_count > 0 {
        info.frame_count
    } else {
        (info.duration * fps).round() as u64
    };
    let duration = if total > 0 { total as f64 / fps } else { info.duration };
    if duration <= 0.0 || w0 == 0 || h0 == 0 {
        return None;
    }
    let cell_h = (cell_w as f64 * h0 as f64 / w0 as f64).round().max(1.0) as u32;
    let n = (duration as usize).clamp(1, max_count);
    let cols = (n as f64).sqrt().ceil().max(1.0) as u32;
    let rows = (n as f64 / cols as f64).ceil().max(1.0) as u32;
    let interval = duration / n as f64;

    let vf = format!(
        "fps={}/{:0.3},scale={}:{},tile={}x{}",
        n, duration, cell_w, cell_h, cols, rows
    );
    let out = std::process::Command::new("ffmpeg")
        .arg("-nostdin")
        .arg("-i")
        .arg(video_path)
        .args(["-vf", &vf, "-frames:v", "1", "-q:v", "4", "-f", "image2", "-c:v", "mjpeg"])
        .arg("-loglevel")
        .arg("error")
        .arg("pipe:1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    let meta = serde_json::json!({
        "available": true, "count": n, "gridCols": cols, "gridRows": rows,
        "cellW": cell_w, "cellH": cell_h, "interval": interval, "duration": duration,
    });
    Some(ScrubSprite {
        meta,
        jpeg: out.stdout,
    })
}

// ────────────────────────────────────────────────────────────────────────────
// WebSocket origin check (port of `_origin_allowed`)
// ────────────────────────────────────────────────────────────────────────────

fn origin_hostname(origin: &str) -> Option<&str> {
    let after = origin.split_once("//").map(|(_, h)| h).unwrap_or(origin);
    let host = after.split(['/', '?']).next()?;
    let host = host.strip_prefix('[').and_then(|h| h.split(']').next()).unwrap_or(host);
    Some(host.split(':').next().unwrap_or(host).trim())
}

fn origin_allowed(origin: Option<&str>, host_header: Option<&str>) -> bool {
    let Some(origin) = origin else {
        return true; // non-browser clients / tests send no Origin
    };
    let Some(oh) = origin_hostname(origin) else {
        return false;
    };
    if oh.is_empty() || oh == "localhost" || oh == "127.0.0.1" {
        return true;
    }
    // Same-origin over LAN: the Origin hostname equals our Host header hostname.
    if let Some(hh) = host_header {
        if let Some(hhn) = hh.split(':').next() {
            if oh == hhn {
                return true;
            }
        }
    }
    false
}

// ────────────────────────────────────────────────────────────────────────────
// Stream pipeline
// ────────────────────────────────────────────────────────────────────────────

enum FrameMsg {
    Frame(Vec<u8>),
    Eof,
    Error(String),
}

enum SendKind {
    Text(String),
    Bytes(Vec<u8>),
}

enum EncodedMsg {
    Frame {
        index: u32,
        kind: SendKind,
        raw_size: usize,
    },
}

/// Mapper + codec state owned by the encode side (shared across spawn_blocking calls).
struct EncoderState {
    enc: CodecEncoder,
    mapper: Mapper,
    lut: Option<[u8; 256]>,
    sharpen_alpha: Option<f32>,
    palette: Palette,
    mode: u8,
    pixel: bool,
    adaptive: bool,
    cols: usize,
    rows: usize,
}

impl EncoderState {
    fn new(cfg: &PipelineConfig) -> EncoderState {
        let qb = match cfg.mode {
            6 => 0,
            5 => 2,
            4 => 3,
            3 => 5,
            2 => 6,
            _ => 0,
        };
        let cell_bytes = if cfg.pixel { 3 } else { 4 };
        EncoderState {
            enc: CodecEncoder::new(cell_bytes, 3, cfg.tolerance),
            mapper: Mapper::default(qb),
            lut: None,
            sharpen_alpha: None,
            palette: Palette::Default,
            mode: cfg.mode,
            pixel: cfg.pixel,
            adaptive: cfg.adaptive,
            cols: cfg.cols as usize,
            rows: cfg.rows as usize,
        }
    }

    fn has_prev(&self) -> bool {
        self.enc.prev().is_some()
    }

    /// Decode + filter + map + encode one frame. Pure CPU work (runs off the async path).
    fn produce(&mut self, rgb: Vec<u8>, index: u32) -> EncodedMsg {
        let (cols, rows) = (self.cols, self.rows);
        if self.pixel {
            // Live pixel mode ships raw BGR (like the original): no adaptive codec.
            let mut fb = vec![0u8; cols * rows * 3];
            self.mapper.map_pixel(&rgb, cols, rows, &mut fb);
            let mut msg = Vec::with_capacity(4 + fb.len());
            msg.extend_from_slice(&index.to_be_bytes());
            msg.extend_from_slice(&fb);
            return EncodedMsg::Frame {
                index,
                kind: SendKind::Bytes(msg),
                raw_size: 4 + fb.len(),
            };
        }

        let mut gray = Mapper::gray_plane(&rgb, cols, rows);
        if let Some(a) = self.sharpen_alpha {
            filters::sharpen_gray(&mut gray, cols, rows, a);
        }
        if let Some(lut) = &self.lut {
            filters::apply_lut(&mut gray, lut);
        }

        if self.mode == 1 {
            let text = self.mapper.text_frame_with_gray(&gray, cols, rows, index);
            let raw_size = text.len();
            return EncodedMsg::Frame {
                index,
                kind: SendKind::Text(text),
                raw_size,
            };
        }

        let mut fb = vec![0u8; cols * rows * 4];
        self.mapper.map_ascii_with_gray(&rgb, &gray, cols, rows, &mut fb);
        let raw_size = 4 + fb.len();
        if self.adaptive {
            let msg = self.enc.encode(&fb, index);
            EncodedMsg::Frame {
                index,
                kind: SendKind::Bytes(msg),
                raw_size,
            }
        } else {
            let mut msg = Vec::with_capacity(4 + fb.len());
            msg.extend_from_slice(&index.to_be_bytes());
            msg.extend_from_slice(&fb);
            EncodedMsg::Frame {
                index,
                kind: SendKind::Bytes(msg),
                raw_size,
            }
        }
    }

    /// Apply runtime filter updates; returns true when a keyframe must be forced.
    fn set_filters(
        &mut self,
        lut: Option<[u8; 256]>,
        sharpen_alpha: Option<f32>,
        palette: Option<Palette>,
    ) -> bool {
        let mut keyframe = false;
        if let Some(p) = palette {
            if p != self.palette {
                self.palette = p;
                let qb = match self.mode {
                    6 => 0,
                    5 => 2,
                    4 => 3,
                    3 => 5,
                    2 => 6,
                    _ => 0,
                };
                self.mapper = Mapper::new(&p.chars(), qb);
                keyframe = true;
            }
        }
        if lut != self.lut {
            self.lut = lut;
            keyframe = true;
        }
        if sharpen_alpha != self.sharpen_alpha {
            self.sharpen_alpha = sharpen_alpha;
        }
        if keyframe {
            self.enc.reset();
        }
        keyframe
    }
}

/// Parameters for one pipeline incarnation (video → frames).
struct PipelineConfig {
    src: String,
    is_webcam: bool,
    mirror: bool,
    target_fps: Option<f64>,
    seek_secs: f64,
    cols: u32,
    rows: u32,
    mode: u8,
    pixel: bool,
    adaptive: bool,
    tolerance: u32,
}

/// Decode thread + encoder state, owned by the async stream task.
struct Pipeline {
    frame_rx: tokio::sync::mpsc::Receiver<FrameMsg>,
    reader: Option<std::thread::JoinHandle<()>>,
    reader_pid: Arc<AtomicU32>,
    state: Arc<Mutex<EncoderState>>,
    /// Set by the async task to make the next frame get skipped (backpressure).
    skip: Arc<AtomicBool>,
}

impl Pipeline {
    fn spawn(cfg: PipelineConfig) -> Pipeline {
        let (tx, frame_rx) = tokio::sync::mpsc::channel::<FrameMsg>(4);
        let reader_pid = Arc::new(AtomicU32::new(0));
        let pid = reader_pid.clone();
        let params = SourceParams {
            src: cfg.src.clone(),
            is_webcam: cfg.is_webcam,
            cols: cfg.cols,
            rows: cfg.rows,
            target_fps: cfg.target_fps,
            seek_secs: cfg.seek_secs,
            mirror: cfg.mirror,
        };
        let reader = std::thread::spawn(move || {
            // FrameReader::new publishes the ffmpeg PID before any blocking read,
            // so shutdown() can always SIGKILL it to unblock the pipe.
            let mut reader = match FrameReader::new(&params, &pid) {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.blocking_send(FrameMsg::Error(e.to_string()));
                    return;
                }
            };
            // Shutdown may have raced our ffmpeg spawn (pid was still 0 when it
            // checked); the channel close tells us to exit before reading.
            if tx.is_closed() {
                return;
            }
            loop {
                match reader.read_frame() {
                    Some(f) => {
                        if tx.blocking_send(FrameMsg::Frame(f)).is_err() {
                            break;
                        }
                    }
                    None => {
                        let _ = tx.blocking_send(FrameMsg::Eof);
                        break;
                    }
                }
            }
        });
        let state = Arc::new(Mutex::new(EncoderState::new(&cfg)));
        Pipeline {
            frame_rx,
            reader: Some(reader),
            reader_pid,
            state,
            skip: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Stop decode + encode threads (used for seek/reinit and at stream end).
    ///
    /// Invariant that makes this deadlock-free: `FrameReader::new` publishes the
    /// ffmpeg PID *before* any blocking read. So either
    ///   - pid != 0 → SIGKILL unblocks the pipe read, the thread exits, join returns; or
    ///   - pid == 0 → the thread hasn't reached a blocking read yet; closing the
    ///     channel makes its first blocking_send fail and it exits at once.
    fn shutdown(&mut self) {
        let pid = self.reader_pid.load(Ordering::SeqCst);
        if pid != 0 {
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        self.frame_rx.close();
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ────────────────────────────────────────────────────────────────────────────
// WebSocket stream
// ────────────────────────────────────────────────────────────────────────────

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    if !origin_allowed(origin, host) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let adaptive = params.get("codec").map(|s| s == "adaptive").unwrap_or(false);
    ws.on_upgrade(move |socket| run_stream(socket, state, adaptive))
}

async fn run_stream(socket: WebSocket, state: Arc<AppState>, adaptive: bool) {
    let (mut sink, mut stream) = socket.split();
    // Bounded command queue: a chatty client can't grow memory unboundedly.
    // If it overflows we drop the oldest command (stale filter/buffer updates
    // are harmless to lose).
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<serde_json::Value>(256);
    let recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            match msg {
                Message::Text(t) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                        if cmd_tx.try_send(v).is_err() {
                            // queue full: keep draining, drop this command
                        }
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    let queue = state.queue.clone();
    let mut queue_index = 0usize;

    if queue.is_empty() {
        let _ = sink.send(Message::Text("Error: No video in queue!".into())).await;
        let _ = sink.close().await;
        recv_task.abort();
        return;
    }

    loop {
        let entry = queue[queue_index].clone();
        let mode = entry.mode;
        let mut pixel = entry.pixel;
        if mode == 1 {
            pixel = false; // extra security layer, same as the original
        }

        let info = match probe_video(&entry.video, entry.is_webcam) {
            Ok(i) => i,
            Err(e) => {
                let _ = sink
                    .send(Message::Text(format!("Error: could not open '{}': {}", entry.video, e).into()))
                    .await;
                queue_index += 1;
                if queue_index >= queue.len() {
                    if state.loop_playback {
                        queue_index = 0;
                    } else {
                        break;
                    }
                }
                continue;
            }
        };

        // IMPORTANT: publish current_index before INIT so /audio serves the right video.
        state.current_index.store(queue_index, Ordering::SeqCst);
        println!(
            "[PLAYING] ({}/{}) {}  mode={}  pixel={}  vol={}",
            queue_index + 1,
            queue.len(),
            entry.video,
            mode,
            pixel,
            entry.vol
        );

        let (cols, rows) = entry.resolve_cols_rows(info.width, info.height);
        println!("[AUTO] {}x{} → grid {}x{}", info.width, info.height, cols, rows);

        let source_fps = if entry.fallback_fps > 0.0 {
            entry.fallback_fps
        } else {
            info.fps.max(1.0)
        };
        let target_fps = if entry.is_webcam {
            source_fps
        } else {
            state.fps_override.unwrap_or(source_fps)
        };
        let frame_t = 1.0 / target_fps.max(0.001);
        let duration = info.duration;

        let init = init_message(
            target_fps, mode, cols, rows, pixel, queue_index as u32, duration, 0.0, entry.is_webcam,
        );
        if sink.send(Message::Text(init.into())).await.is_err() {
            break;
        }

        // ── spawn the decode/encode pipeline ──
        let cfg = PipelineConfig {
            src: entry.video.clone(),
            is_webcam: entry.is_webcam,
            mirror: entry.mirror,
            target_fps: Some(target_fps),
            seek_secs: 0.0,
            cols,
            rows,
            mode,
            pixel,
            adaptive,
            tolerance: state.tolerance,
        };
        let mut pipeline = Pipeline::spawn(cfg);

        let mut frame_index: u32 = 0;
        let mut start_time = tokio::time::Instant::now();
        let mut paused = false;

        // backpressure
        const BACKLOG_HIGH: i32 = 15;
        let max_consec_drops = ((target_fps * 0.3).round().max(1.0)) as i32;
        let mut client_backlog: i32 = 0;
        let mut consec_high = 0;
        let mut consec_drops = 0;

        // runtime filter state (mirrors the client's filter panel)
        let mut filter_palette = Palette::Default;

        // bandwidth debug
        let mut bw_bytes: u64 = 0;
        let mut bw_raw: u64 = 0;
        let mut bw_start = tokio::time::Instant::now();

        let is_webcam = entry.is_webcam;

        // ── frame loop ──
        'stream: loop {
            // Drain client commands.
            while let Ok(cmd) = cmd_rx.try_recv() {
                let typ = cmd.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match typ {
                    "pause" => {
                        paused = cmd.get("paused").and_then(|p| p.as_bool()).unwrap_or(false);
                        if !paused {
                            start_time = tokio::time::Instant::now()
                                - Duration::from_secs_f64(frame_index as f64 * frame_t);
                        }
                    }
                    "seek" => {
                        let target = cmd.get("time").and_then(|t| t.as_f64()).unwrap_or(0.0);
                        pipeline.shutdown();
                        let cfg = PipelineConfig {
                            src: entry.video.clone(),
                            is_webcam: entry.is_webcam,
                            mirror: entry.mirror,
                            target_fps: Some(target_fps),
                            seek_secs: target,
                            cols,
                            rows,
                            mode,
                            pixel,
                            adaptive,
                            tolerance: state.tolerance,
                        };
                        pipeline = Pipeline::spawn(cfg);
                        frame_index = (target * target_fps).round() as u32;
                        start_time = tokio::time::Instant::now()
                            - Duration::from_secs_f64(frame_index as f64 * frame_t);
                        client_backlog = 0;
                        consec_high = 0;
                        consec_drops = 0;
                    }
                    "buffer" => {
                        let depth = cmd.get("depth").and_then(|d| d.as_i64()).unwrap_or(0);
                        client_backlog = depth.max(0) as i32;
                        if client_backlog > BACKLOG_HIGH {
                            consec_high += 1;
                        } else {
                            consec_high = 0;
                        }
                    }
                    "reinit" => {
                        pixel = cmd.get("pixel").and_then(|p| p.as_bool()).unwrap_or(pixel);
                        let (ncols, nrows) = entry.resolve_cols_rows(info.width, info.height);
                        let target = cmd.get("time").and_then(|t| t.as_f64()).unwrap_or(0.0);
                        let init = init_message(
                            target_fps, mode, ncols, nrows, pixel, queue_index as u32,
                            duration, target, is_webcam,
                        );
                        if sink.send(Message::Text(init.into())).await.is_err() {
                            break 'stream;
                        }
                        pipeline.shutdown();
                        let cfg = PipelineConfig {
                            src: entry.video.clone(),
                            is_webcam: entry.is_webcam,
                            mirror: entry.mirror,
                            target_fps: Some(target_fps),
                            seek_secs: target,
                            cols: ncols,
                            rows: nrows,
                            mode,
                            pixel,
                            adaptive,
                            tolerance: state.tolerance,
                        };
                        pipeline = Pipeline::spawn(cfg);
                        frame_index = (target * target_fps).round() as u32;
                        start_time = tokio::time::Instant::now()
                            - Duration::from_secs_f64(frame_index as f64 * frame_t);
                        client_backlog = 0;
                        consec_high = 0;
                        consec_drops = 0;
                        println!("[REINIT] {}x{} → grid {}x{}", info.width, info.height, ncols, nrows);
                    }
                    "filter" => {
                        let contrast = cmd.get("contrast").and_then(|v| v.as_f64()).unwrap_or(1.0)
                            .clamp(0.1, 3.0);
                        let gamma = cmd.get("gamma").and_then(|v| v.as_f64()).unwrap_or(1.0)
                            .clamp(0.1, 3.0);
                        let brightness = cmd.get("brightness").and_then(|v| v.as_f64()).unwrap_or(0.0)
                            .clamp(-100.0, 100.0);
                        let invert = cmd.get("invert").and_then(|v| v.as_bool()).unwrap_or(false);
                        let sharpness = cmd.get("sharpness").and_then(|v| v.as_i64()).unwrap_or(0)
                            .clamp(0, 10);
                        if let Some(p) = cmd.get("palette").and_then(|v| v.as_str()) {
                            filter_palette = match p {
                                "flat" => Palette::Flat,
                                "block" => Palette::Block,
                                _ => Palette::Default,
                            };
                        }
                        let lut = filters::build_gray_lut(contrast, gamma, brightness, invert);
                        let alpha = if sharpness > 0 {
                            Some(sharpness as f32 * 0.5)
                        } else {
                            None
                        };
                        pipeline
                            .state
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .set_filters(lut, alpha, Some(filter_palette));
                    }
                    _ => {}
                }
            }

            if paused {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }

            // ── Backpressure: client behind → skip a frame (keep deltas coherent) ──
            if consec_high >= 2 && consec_drops < max_consec_drops {
                println!(
                    "[Backpressure] dropping frame {}, client_backlog={}, consec_drops={}",
                    frame_index, client_backlog, consec_drops
                );
                pipeline.skip.store(true, Ordering::SeqCst);
                client_backlog -= 1;
                consec_drops += 1;
                frame_index += 1;
                let elapsed = start_time.elapsed().as_secs_f64();
                let wait = frame_index as f64 * frame_t - elapsed;
                if wait > 0.0 && !is_webcam {
                    tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                }
                continue;
            }
            consec_drops = 0;

            // ── next decoded frame ──
            let frame_msg = match pipeline.frame_rx.recv().await {
                Some(m) => m,
                None => break 'stream, // pipeline died
            };
            let frame_msg = match frame_msg {
                FrameMsg::Error(e) => {
                    let _ = sink.send(Message::Text(format!("Error: {e}").into())).await;
                    break 'stream;
                }
                FrameMsg::Eof => break 'stream,
                FrameMsg::Frame(f) => f,
            };

            // Skipped frame: don't map/encode/send; prev stays aligned for deltas.
            if pipeline.skip.swap(false, Ordering::SeqCst)
                && pipeline.state.lock().unwrap_or_else(|e| e.into_inner()).has_prev()
            {
                frame_index += 1;
                let elapsed = start_time.elapsed().as_secs_f64();
                let wait = frame_index as f64 * frame_t - elapsed;
                if wait > 0.0 && !is_webcam {
                    tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                }
                continue;
            }

            // ── map + encode on the blocking pool (CPU) ──
            let t0 = tokio::time::Instant::now();
            let st = pipeline.state.clone();
            let index = frame_index;
            let out = tokio::task::spawn_blocking(move || {
                st.lock().unwrap_or_else(|e| e.into_inner()).produce(frame_msg, index)
            })
            .await;
            let out = match out {
                Ok(o) => o,
                Err(_) => break 'stream,
            };

            // Webcam safety net: avoid 100% CPU when v4l2 returns instantly.
            if is_webcam && t0.elapsed().as_secs_f64() < 0.005 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }

            match out {
                EncodedMsg::Frame { index, kind, raw_size } => {
                    let (sent, wire_size) = match kind {
                        SendKind::Text(t) => {
                            let wire = t.len();
                            (sink.send(Message::Text(t.into())).await, wire)
                        }
                        SendKind::Bytes(b) => {
                            let wire = b.len();
                            (sink.send(Message::Binary(b.into())).await, wire)
                        }
                    };
                    if sent.is_err() {
                        break 'stream;
                    }
                    bw_bytes += wire_size as u64;
                    bw_raw += raw_size as u64;
                    if state.debug && bw_start.elapsed().as_secs_f64() >= 1.0 {
                        let raw_kbps = bw_raw as f64 / 1024.0;
                        let wire_kbps = bw_bytes as f64 / 1024.0;
                        let ratio = if wire_kbps > 0.0 { raw_kbps / wire_kbps } else { 0.0 };
                        println!("[BW] RAW: {:.1} KB/s | WIRE: {:.1} KB/s | {:.1}x compression", raw_kbps, wire_kbps, ratio);
                        bw_start = tokio::time::Instant::now();
                        bw_bytes = 0;
                        bw_raw = 0;
                    }

                    let elapsed = start_time.elapsed().as_secs_f64();
                    let wait = index as f64 * frame_t - elapsed;
                    if wait > 0.0 && !is_webcam {
                        tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                    }
                    frame_index = index + 1;
                }
            }
        }

        pipeline.shutdown();

        // Video finished → advance queue.
        queue_index += 1;
        if queue_index >= queue.len() {
            if state.loop_playback {
                println!("[LOOP] Restarting queue from the beginning.");
                queue_index = 0;
            } else {
                println!("[DONE] All videos finished.");
                break;
            }
        }
    }

    let _ = sink.close().await;
    recv_task.abort();
}
