#![cfg(feature = "callback_client")]

mod common;

use std::time::Duration;

use fast_websocket_client::{CloseEvent, ConnectionInitOptions, WebSocketBuilder};
use tokio::sync::mpsc;
use tokio::time::timeout;

const DEADLINE: Duration = Duration::from_secs(3);

async fn receive_peer_close(payload: &[u8]) -> CloseEvent {
    let (listener, url) = common::listener().await;
    let (close_tx, mut close_rx) = mpsc::unbounded_channel();
    let socket = WebSocketBuilder::new()
        .on_close(move |event| {
            let tx = close_tx.clone();
            async move {
                let _ = tx.send(event);
            }
        })
        .connect(&url)
        .await
        .unwrap();
    let mut peer = common::accept_ws(&listener).await;
    common::write_frame(&mut peer, true, 8, payload).await;
    loop {
        let frame = common::read_frame(&mut peer).await.unwrap();
        if frame.0 == 8 {
            assert_eq!(frame.1, payload);
            break;
        }
        assert_eq!(frame.0, 9);
    }
    let event = close_rx.recv().await.unwrap();
    socket.close().await;
    socket.await_shutdown().await.unwrap();
    assert_eq!(close_rx.recv().await, None);
    event
}

#[tokio::test]
async fn peer_close_reports_code_and_utf8_reason() {
    timeout(DEADLINE, async {
        let reason = "maintenance ☕";
        let mut payload = 4001_u16.to_be_bytes().to_vec();
        payload.extend_from_slice(reason.as_bytes());
        let event = receive_peer_close(&payload).await;
        assert_eq!(event.code, 4001);
        assert_eq!(event.reason, reason);
        assert!(event.was_clean);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn empty_peer_close_reports_no_status_code() {
    timeout(DEADLINE, async {
        let event = receive_peer_close(&[]).await;
        assert_eq!(event.code, 1005);
        assert!(event.reason.is_empty());
        assert!(event.was_clean);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn runtime_close_callback_reports_an_unanswered_peer_close() {
    timeout(DEADLINE, async {
        let (listener, url) = common::listener().await;
        let (close_tx, mut close_rx) = mpsc::unbounded_channel();
        let socket = WebSocketBuilder::new()
            .with_options(ConnectionInitOptions::new().auto_close(false))
            .connect(&url)
            .await
            .unwrap();
        let mut peer = common::accept_ws(&listener).await;
        socket
            .on_close(move |event| {
                let tx = close_tx.clone();
                async move {
                    let _ = tx.send(event);
                }
            })
            .await;
        socket.send("ready").await.unwrap();
        loop {
            let frame = common::read_frame(&mut peer).await.unwrap();
            if frame.0 == 1 {
                assert_eq!(frame.1, b"ready");
                break;
            }
            assert_eq!(frame.0, 9);
        }

        let reason = "server restarting";
        let mut payload = 1012_u16.to_be_bytes().to_vec();
        payload.extend_from_slice(reason.as_bytes());
        common::write_frame(&mut peer, true, 8, &payload).await;
        let event = close_rx.recv().await.unwrap();
        assert_eq!(event.code, 1012);
        assert_eq!(event.reason, reason);
        assert!(!event.was_clean);
        while let Ok(frame) = common::read_frame(&mut peer).await {
            assert_eq!(frame.0, 9, "automatic Close reply was disabled");
        }
        socket.close().await;
        socket.await_shutdown().await.unwrap();
        assert_eq!(close_rx.recv().await, None);
    })
    .await
    .unwrap();
}
