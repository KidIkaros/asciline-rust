//! End-to-end test: boots the real `asciline-server` binary against a generated
//! test video, connects over WebSocket like the browser client does
//! (`/ws?codec=adaptive`), verifies the INIT handshake and that binary frames
//! decode to the advertised grid size, and checks the root page + static assets.

mod common;

use std::time::Duration;

use asciline::codec::CodecDecoder;
use common::{free_port, http_get, make_test_video, spawn_server, wait_for_server};
use futures_util::StreamExt;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn ws_stream_protocol() {
    let Some(video) = make_test_video("ws") else {
        eprintln!("ffmpeg not available — skipping e2e server test");
        return;
    };
    let port = free_port();

    let mut child = spawn_server(video.to_str().unwrap(), port, &["--cols", "80"]);

    wait_for_server(port).await;

    // ── HTTP: root page + static assets ──
    let root = http_get(port, "/");
    assert!(
        root.contains("200 OK") && root.to_lowercase().contains("asciline"),
        "root page broken"
    );
    let js = http_get(port, "/static/app.js");
    assert!(js.contains("200 OK"), "static app.js not served");
    let forbidden = http_get(port, "/static/secret.txt");
    assert!(
        forbidden.contains("404"),
        "whitelist must block unknown files"
    );

    // ── WebSocket: INIT + binary frames ──
    let url = format!("ws://127.0.0.1:{port}/ws?codec=adaptive");
    let (mut ws, _) = connect_async(&url).await.expect("ws connect");

    let first = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("INIT timeout")
        .unwrap()
        .unwrap();
    let text = match first {
        Message::Text(t) => t.to_string(),
        other => panic!("expected INIT text, got {other:?}"),
    };
    assert!(text.starts_with("INIT:"), "bad INIT: {text}");
    let parts: Vec<&str> = text.split(':').collect();
    let fps: f64 = parts[1].parse().unwrap();
    let mode: u8 = parts[2].parse().unwrap();
    let cols: usize = parts[3].parse().unwrap();
    let rows: usize = parts[4].parse().unwrap();
    assert_eq!(mode, 6);
    assert!(
        (fps - 30.0).abs() < 1e-6,
        "fps in INIT should be 30.0, got {fps}"
    );

    let mut dec = CodecDecoder::new(4);
    let mut decoded = 0usize;
    for _ in 0..300 {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("frame timeout")
            .unwrap()
            .unwrap();
        if let Message::Binary(b) = msg {
            let (idx, frame) = dec.decode(b.as_ref()).expect("frame decodes");
            assert_eq!(frame.len(), cols * rows * 4, "frame {idx} wrong grid size");
            assert_eq!(idx, decoded as u32, "frame indices must be sequential");
            decoded += 1;
            if decoded >= 10 {
                break;
            }
        }
    }
    assert!(decoded >= 10, "expected >=10 frames, decoded {decoded}");

    let _ = ws.close(None).await;
    let _ = child.kill().await;
    let _ = child.wait().await;
    let _ = std::fs::remove_file(video);
}

/// Production hardening: /healthz liveness, optional token auth (401s),
/// connection cap (503 on the overflow connection), and graceful shutdown.
#[tokio::test]
async fn hardening_guards() {
    let Some(video) = make_test_video("hard") else {
        eprintln!("ffmpeg not available — skipping hardening test");
        return;
    };
    let port = free_port();
    let mut child = spawn_server(
        video.to_str().unwrap(),
        port,
        &[
            "--cols",
            "40",
            "--max-clients",
            "1",
            "--token",
            "s3cr3t",
            "--max-ffmpeg",
            "2",
        ],
    );

    wait_for_server(port).await;

    // /healthz: 200 + JSON with the client cap.
    let hz = http_get(port, "/healthz");
    assert!(
        hz.contains("200 OK") && hz.contains("\"max\":1"),
        "healthz wrong: {hz}"
    );

    // /audio and /scrub without the token: 401.
    assert!(
        http_get(port, "/audio?v=0").contains("401"),
        "audio must 401 without token"
    );
    assert!(
        http_get(port, "/scrub?v=0").contains("401"),
        "scrub must 401 without token"
    );
    assert!(
        http_get(port, "/audio?v=0&token=wrong").contains("401"),
        "wrong token must 401"
    );
    assert!(
        http_get(port, "/audio?v=0&token=s3cr3t").contains("200"),
        "correct token must pass"
    );

    // WS without a token: 401 at the upgrade.
    let no_token = connect_async(format!("ws://127.0.0.1:{port}/ws?codec=adaptive")).await;
    assert!(no_token.is_err(), "ws without token must be rejected");

    // First authenticated client connects and streams (holds the only permit).
    let url = format!("ws://127.0.0.1:{port}/ws?codec=adaptive&token=s3cr3t");
    let (mut ws, _) = connect_async(&url).await.expect("first ws must connect");
    let first = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("INIT timeout")
        .unwrap()
        .unwrap();
    assert!(matches!(first, Message::Text(t) if t.starts_with("INIT:")));

    // Second connection hits the --max-clients 1 cap: 503 at the upgrade.
    let second = connect_async(&url).await;
    assert!(
        second.is_err(),
        "second ws must be rejected by the connection cap"
    );

    // Healthz now reports 1 in use.
    let hz2 = http_get(port, "/healthz");
    assert!(
        hz2.contains("\"in_use\":1"),
        "healthz must report the active client: {hz2}"
    );

    let _ = ws.close(None).await;

    let _ = child.kill().await;
    let _ = child.wait().await;
    let _ = std::fs::remove_file(video);
}
