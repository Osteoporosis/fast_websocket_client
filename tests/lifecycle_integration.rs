#![cfg(feature = "callback_client")]

mod common;

use std::sync::Arc;
use std::time::Duration;

use fast_websocket_client::{ClientConfig, WebSocketBuilder, WebSocketClientError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Notify, mpsc};
use tokio::time::{Instant, sleep, timeout};

const DEADLINE: Duration = Duration::from_secs(3);

#[tokio::test]
async fn local_close_waits_for_peer_acknowledgment_and_emits_one_close() {
    timeout(DEADLINE, async {
        let (listener, url) = common::listener().await;
        let (open_tx, mut open_rx) = mpsc::unbounded_channel();
        let (close_tx, mut close_rx) = mpsc::unbounded_channel();
        let (error_tx, mut error_rx) = mpsc::unbounded_channel();
        let socket = WebSocketBuilder::new()
            .on_open(move |_| {
                let tx = open_tx.clone();
                async move {
                    let _ = tx.send(());
                }
            })
            .on_close(move |event| {
                let tx = close_tx.clone();
                async move {
                    let _ = tx.send(event);
                }
            })
            .on_error(move |error| {
                let tx = error_tx.clone();
                async move {
                    let _ = tx.send(error);
                }
            })
            .connect_with_config(&url, ClientConfig::new().with_close_timeout(Duration::MAX))
            .await
            .unwrap();
        let mut peer = common::accept_ws(&listener).await;
        open_rx.recv().await.unwrap();
        socket.send("before close").await.unwrap();
        socket.send_binary(&[1, 2]).await.unwrap();
        socket.close().await;
        assert!(socket.send("after close").await.is_err());
        assert!(socket.send_binary(&[3]).await.is_err());
        assert_eq!(
            common::read_data_or_close(&mut peer).await,
            (1, b"before close".to_vec())
        );
        assert_eq!(common::read_data_or_close(&mut peer).await, (2, vec![1, 2]));
        let (opcode, close_payload) = common::read_data_or_close(&mut peer).await;
        assert_eq!((opcode, close_payload.as_slice()), (8, [3, 232].as_slice()));

        assert!(matches!(
            close_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        common::write_frame(&mut peer, true, 8, b"\x03\xe8received").await;
        socket.await_shutdown().await.unwrap();
        let event = close_rx.recv().await.unwrap();
        assert_eq!(event.code, 1000);
        assert_eq!(event.reason, "received");
        assert!(event.was_clean);
        assert_eq!(close_rx.recv().await, None);
        assert!(error_rx.try_recv().is_err());
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incoming_callbacks_finish_in_wire_order_even_when_one_awaits() {
    timeout(DEADLINE, async {
        let (listener, url) = common::listener().await;
        let release = Arc::new(Notify::new());
        let wait = release.clone();
        let (text, mut events) = mpsc::unbounded_channel();
        let binary = text.clone();
        let close = text.clone();
        let socket = WebSocketBuilder::new()
            .on_message(move |_| {
                let (text, wait) = (text.clone(), wait.clone());
                async move {
                    text.send("text started").unwrap();
                    wait.notified().await;
                    text.send("text finished").unwrap();
                }
            })
            .on_binary(move |_| {
                let binary = binary.clone();
                async move { binary.send("binary").unwrap() }
            })
            .on_close(move |_| {
                let close = close.clone();
                async move { close.send("close").unwrap() }
            })
            .connect(&url)
            .await
            .unwrap();
        let mut peer = common::accept_ws(&listener).await;
        common::write_frame(&mut peer, true, 1, b"first").await;
        assert_eq!(events.recv().await, Some("text started"));
        common::write_frame(&mut peer, true, 2, &[1]).await;
        common::write_frame(&mut peer, true, 8, &1000u16.to_be_bytes()).await;
        assert!(
            timeout(Duration::from_millis(25), events.recv())
                .await
                .is_err()
        );
        release.notify_one();
        for event in ["text finished", "binary", "close"] {
            assert_eq!(events.recv().await, Some(event));
        }
        socket.close().await;
        socket.await_shutdown().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn dropping_connected_handle_closes_once_without_reconnecting() {
    timeout(DEADLINE, async {
        let (listener, url) = common::listener().await;
        let (open_tx, mut open_rx) = mpsc::unbounded_channel();
        let (close_tx, mut close_rx) = mpsc::unbounded_channel();
        let socket = WebSocketBuilder::new()
            .on_open(move |_| {
                let tx = open_tx.clone();
                async move {
                    let _ = tx.send(());
                }
            })
            .on_close(move |_| {
                let tx = close_tx.clone();
                async move {
                    let _ = tx.send(());
                }
            })
            .connect(&url)
            .await
            .unwrap();
        let mut peer = common::accept_ws(&listener).await;
        open_rx.recv().await.unwrap();
        drop(socket);
        let (opcode, close_payload) = common::read_data_or_close(&mut peer).await;
        assert_eq!(opcode, 8);
        common::write_frame(&mut peer, true, 8, &close_payload).await;
        assert_eq!(close_rx.recv().await, Some(()));
        // Callback senders disappear only when the detached driver terminates.
        assert_eq!(close_rx.recv().await, None);
        assert_eq!(open_rx.recv().await, None);
        common::assert_no_pending_connection(listener);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn local_close_times_out_when_peer_never_acknowledges() {
    timeout(DEADLINE, async {
        let (listener, url) = common::listener().await;
        let (open_tx, mut open_rx) = mpsc::unbounded_channel();
        let (close_tx, mut close_rx) = mpsc::unbounded_channel();
        let close_timeout = Duration::from_millis(80);
        let socket = WebSocketBuilder::new()
            .on_open(move |_| {
                let tx = open_tx.clone();
                async move {
                    let _ = tx.send(());
                }
            })
            .on_close(move |event| {
                let tx = close_tx.clone();
                async move {
                    let _ = tx.send(event);
                }
            })
            .connect_with_config(&url, ClientConfig::new().with_close_timeout(close_timeout))
            .await
            .unwrap();
        let mut peer = common::accept_ws(&listener).await;
        open_rx.recv().await.unwrap();
        let requested_at = Instant::now();
        socket.close().await;
        assert_eq!(common::read_data_or_close(&mut peer).await.0, 8);
        socket.await_shutdown().await.unwrap();
        assert!(
            requested_at.elapsed() >= close_timeout,
            "socket dropped before the configured acknowledgment timeout"
        );
        let event = close_rx.recv().await.unwrap();
        assert_eq!(event.code, 1006);
        assert!(event.reason.is_empty());
        assert!(!event.was_clean);
        assert_eq!(close_rx.recv().await, None);
        assert_eq!(peer.read(&mut [0]).await.unwrap(), 0);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn local_close_preserves_a_partial_read_until_peer_acknowledges() {
    timeout(DEADLINE, async {
        let (listener, url) = common::listener().await;
        let (open_tx, mut open_rx) = mpsc::unbounded_channel();
        let (close_tx, mut close_rx) = mpsc::unbounded_channel();
        let (error_tx, mut error_rx) = mpsc::unbounded_channel();
        let (message_tx, mut message_rx) = mpsc::unbounded_channel();
        let socket = WebSocketBuilder::new()
            .on_open(move |_| {
                let tx = open_tx.clone();
                async move {
                    let _ = tx.send(());
                }
            })
            .on_close(move |_| {
                let tx = close_tx.clone();
                async move {
                    let _ = tx.send(());
                }
            })
            .on_error(move |error| {
                let tx = error_tx.clone();
                async move {
                    let _ = tx.send(error);
                }
            })
            .on_message(move |message| {
                let tx = message_tx.clone();
                async move {
                    let _ = tx.send(message);
                }
            })
            .connect(&url)
            .await
            .unwrap();
        let mut peer = common::accept_ws(&listener).await;
        open_rx.recv().await.unwrap();
        peer.write_all(&[0x81, 5, b'h']).await.unwrap();
        // Let the pending read consume the header and partial payload.
        sleep(Duration::from_millis(25)).await;
        socket.close().await;
        let (opcode, close_payload) = loop {
            let frame = common::read_frame(&mut peer).await.unwrap();
            // A Pong cannot be inserted inside the unfinished Text payload.
            if frame.0 != 9 {
                break frame;
            }
        };
        assert_eq!(opcode, 8);
        peer.write_all(b"ello").await.unwrap();
        common::write_frame(&mut peer, true, 8, &close_payload).await;
        socket.await_shutdown().await.unwrap();
        assert_eq!(close_rx.recv().await, Some(()));
        assert_eq!(close_rx.recv().await, None);
        let error = error_rx.try_recv();
        assert!(
            error.is_err(),
            "partial receive was corrupted by Close: {error:?}"
        );
        assert!(
            message_rx.try_recv().is_err(),
            "message delivered after Close was sent"
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn zero_ping_interval_is_rejected_before_spawning_a_connection() {
    let (listener, url) = common::listener().await;
    let result = WebSocketBuilder::new()
        .connect_with_config(&url, ClientConfig::new().with_ping_interval(Duration::ZERO))
        .await;
    assert!(
        matches!(result, Err(WebSocketClientError::InvalidConfig(ref reason))
        if reason.contains("ping_interval"))
    );
    assert!(
        timeout(Duration::from_millis(25), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn zero_ping_update_reports_error_and_keeps_the_connection_usable() {
    timeout(DEADLINE, async {
        let (listener, url) = common::listener().await;
        let (open_tx, mut open_rx) = mpsc::unbounded_channel();
        let (error_tx, mut error_rx) = mpsc::unbounded_channel();
        let socket = WebSocketBuilder::new()
            .on_open(move |_| {
                let tx = open_tx.clone();
                async move {
                    let _ = tx.send(());
                }
            })
            .on_error(move |error| {
                let tx = error_tx.clone();
                async move {
                    let _ = tx.send(error);
                }
            })
            .connect(&url)
            .await
            .unwrap();
        let mut peer = common::accept_ws(&listener).await;
        open_rx.recv().await.unwrap();
        socket
            .update_config(ClientConfig::new().with_ping_interval(Duration::ZERO))
            .await;
        assert!(error_rx.recv().await.unwrap().contains("ping_interval"));
        socket.send("still connected").await.unwrap();
        assert_eq!(
            common::read_data_or_close(&mut peer).await,
            (1, b"still connected".to_vec())
        );
        socket.close().await;
        let (opcode, close_payload) = common::read_data_or_close(&mut peer).await;
        assert_eq!(opcode, 8);
        common::write_frame(&mut peer, true, 8, &close_payload).await;
        socket.await_shutdown().await.unwrap();
        assert!(error_rx.try_recv().is_err());
    })
    .await
    .unwrap();
}
