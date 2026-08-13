//! Shared helpers for integration tests that boot the real `asciline-server`
//! binary. Import with `mod common;` from any `tests/*.rs` file.

use std::io::{Read, Write};
use std::process::Stdio;
use std::time::Duration;

pub fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Create a test video. `tag` distinguishes concurrent tests: they share a
/// process but each needs its own temp file (one test's cleanup must not
/// delete the other's while its server is still probing it).
pub fn make_test_video(tag: &str) -> Option<std::path::PathBuf> {
    if !ffmpeg_available() {
        return None;
    }
    let path =
        std::env::temp_dir().join(format!("asciline_e2e_{}_{}.mp4", std::process::id(), tag));
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x180:rate=30:duration=1",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    status.success().then_some(path)
}

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub async fn wait_for_server(port: u16) {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("server did not come up on port {port}");
}

/// Minimal HTTP GET over a raw TCP socket (no extra deps).
pub fn http_get(port: u16, path: &str) -> String {
    let mut sock = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(
        sock,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut resp = String::new();
    let _ = sock.read_to_string(&mut resp);
    resp
}

/// Spawn `asciline-server` for `video` on `port` with the given extra args.
pub fn spawn_server(video: &str, port: u16, extra: &[&str]) -> tokio::process::Child {
    let bin = env!("CARGO_BIN_EXE_asciline-server");
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg(video)
        .args(["--mode", "6", "--fps", "30", "--no-thumbnails"])
        .arg("--port")
        .arg(port.to_string());
    for a in extra {
        cmd.arg(a);
    }
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn asciline-server")
}
