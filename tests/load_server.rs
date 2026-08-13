//! Concurrent-client load test for `--max-clients`.
//!
//! Every WebSocket client owns an ffmpeg child, a decode thread and encode
//! work, so the cap is the resource guard. This test boots the real server
//! with `--max-clients 2` and verifies under real contention that:
//!
//!   1. both in-cap clients connect and stream (INIT + frames);
//!   2. `/healthz` reports the exact in-use count;
//!   3. the overflow connection is rejected (503 at the upgrade);
//!   4. disconnecting a client frees its slot — a new client can connect.

mod common;

use std::time::Duration;

use common::{free_port, http_get, make_test_video, spawn_server, wait_for_server};
use futures_util::StreamExt;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

const MAX: usize = 2;

/// Wait for the next message and assert it is a `Text` starting with `INIT:`.
async fn expect_init(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> String {
    let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("INIT timeout")
        .unwrap()
        .unwrap();
    match msg {
        Message::Text(t) => {
            assert!(t.starts_with("INIT:"), "bad INIT: {t}");
            t.to_string()
        }
        other => panic!("expected INIT text, got {other:?}"),
    }
}

/// Wait for the next message and assert it is a binary codec frame.
async fn expect_frame(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) {
    let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("frame timeout")
        .unwrap()
        .unwrap();
    assert!(
        matches!(msg, Message::Binary(_)),
        "expected a binary frame, got {msg:?}"
    );
}

#[tokio::test]
async fn concurrent_clients_respect_the_cap() {
    let Some(video) = make_test_video("load") else {
        eprintln!("ffmpeg not available — skipping load test");
        return;
    };
    let port = free_port();
    let mut child = spawn_server(
        video.to_str().unwrap(),
        port,
        &["--cols", "40", "--max-clients", &MAX.to_string()],
    );

    wait_for_server(port).await;
    let url = format!("ws://127.0.0.1:{port}/ws?codec=adaptive");

    // ── all in-cap clients connect and stream ──
    let mut clients: Vec<_> = Vec::new();
    for _ in 0..MAX {
        let n = clients.len();
        let (ws, _) = connect_async(&url)
            .await
            .unwrap_or_else(|e| panic!("in-cap client {n} must connect: {e}"));
        clients.push(ws);
    }
    for ws in &mut clients {
        expect_init(ws).await;
    }
    assert!(
        http_get(port, "/healthz").contains(&format!("\"in_use\":{MAX}")),
        "healthz must report {MAX} in use"
    );

    // ── overflow connection: 503 at the upgrade ──
    let overflow = connect_async(&url).await;
    assert!(
        overflow.is_err(),
        "the ({}+1)-th connection must be rejected by the cap",
        MAX
    );

    // ── frames actually flow while both are connected ──
    for ws in &mut clients {
        expect_frame(ws).await;
    }

    // ── disconnect frees the slot: a new client can connect and stream ──
    clients[0].close(None).await.ok();
    drop(clients.remove(0)); // closes TCP immediately → server frees the permit
    tokio::time::sleep(Duration::from_millis(500)).await; // let the server notice

    let (mut ws2, _) = connect_async(&url)
        .await
        .expect("a freed slot must be reusable");
    expect_init(&mut ws2).await;
    expect_frame(&mut ws2).await;
    assert!(
        http_get(port, "/healthz").contains(&format!("\"in_use\":{MAX}")),
        "healthz must report {MAX} in use again after reconnect"
    );
    let _ = ws2.close(None).await;
    for mut ws in clients {
        let _ = ws.close(None).await;
    }

    let _ = child.kill().await;
    let _ = child.wait().await;
    let _ = std::fs::remove_file(video);
}
