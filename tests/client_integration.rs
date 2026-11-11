#![cfg(feature = "callback_client")]

mod common;

use std::time::Duration;

use fast_websocket_client::{ClientConfig, WebSocket, WebSocketBuilder};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, sleep, timeout};

const DEADLINE: Duration = Duration::from_secs(3);

#[tokio::test]
async fn close_interrupts_failed_connection_backoff() {
    let (listener, url) = common::listener().await;
    drop(listener);
    let (errors_tx, mut errors_rx) = mpsc::unbounded_channel();
    let socket = WebSocketBuilder::new()
        .on_error(move |error| {
            let tx = errors_tx.clone();
            async move {
                let _ = tx.send(error);
            }
        })
        .connect_with_config(
            &url,
            ClientConfig::new().with_reconnect_delay(Duration::from_secs(30)),
        )
        .await
        .unwrap();
    timeout(DEADLINE, errors_rx.recv()).await.unwrap().unwrap();
    socket.close().await;
    timeout(DEADLINE, socket.await_shutdown())
        .await
        .expect("Close must interrupt retry backoff")
        .unwrap();
}

#[tokio::test]
async fn close_interrupts_an_unfinished_handshake() {
    let (listener, url) = common::listener().await;
    let socket = WebSocketBuilder::new()
        .connect_with_config(
            &url,
            ClientConfig::new().with_connect_timeout(Duration::from_secs(30)),
        )
        .await
        .unwrap();
    let (peer, _) = timeout(DEADLINE, listener.accept()).await.unwrap().unwrap();
    socket.close().await;
    timeout(DEADLINE, socket.await_shutdown())
        .await
        .expect("Close must cancel an active handshake")
        .unwrap();
    drop(peer);
}

#[tokio::test]
async fn error_callback_can_be_replaced_between_failed_attempts() {
    let (listener, url) = common::listener().await;
    drop(listener);
    let (old_tx, mut old_rx) = mpsc::unbounded_channel();
    let (new_tx, mut new_rx) = mpsc::unbounded_channel();
    let socket = WebSocketBuilder::new()
        .on_error(move |error| {
            let tx = old_tx.clone();
            async move {
                let _ = tx.send(error);
            }
        })
        .connect_with_config(
            &url,
            ClientConfig::new().with_reconnect_delay(Duration::from_millis(10)),
        )
        .await
        .unwrap();
    timeout(DEADLINE, old_rx.recv()).await.unwrap().unwrap();
    socket
        .on_error(move |error| {
            let tx = new_tx.clone();
            async move {
                let _ = tx.send(error);
            }
        })
        .await;
    timeout(DEADLINE, new_rx.recv())
        .await
        .expect("Updated callback must run while disconnected")
        .unwrap();
    socket.close().await;
    timeout(DEADLINE, socket.await_shutdown())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn eof_emits_one_close_and_obeys_reconnect_delay() {
    let (listener, url) = common::listener().await;
    let (open_tx, mut open_rx) = mpsc::unbounded_channel();
    let (close_tx, mut close_rx) = mpsc::unbounded_channel();
    let delay = Duration::from_millis(100);
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
        .connect_with_config(&url, ClientConfig::new().with_reconnect_delay(delay))
        .await
        .unwrap();
    let mut first = timeout(DEADLINE, common::accept_ws(&listener))
        .await
        .unwrap();
    timeout(DEADLINE, open_rx.recv()).await.unwrap().unwrap();
    let disconnected_at = Instant::now();
    first.shutdown().await.unwrap();
    drop(first);
    let event = timeout(DEADLINE, close_rx.recv()).await.unwrap().unwrap();
    assert_eq!(event.code, 1006);
    assert!(event.reason.is_empty());
    assert!(!event.was_clean);
    let second = timeout(DEADLINE, common::accept_ws(&listener))
        .await
        .unwrap();
    assert!(
        disconnected_at.elapsed() >= delay,
        "reconnect_delay was skipped"
    );
    timeout(DEADLINE, open_rx.recv()).await.unwrap().unwrap();
    assert!(
        matches!(close_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
        "duplicate close event"
    );
    let peer = tokio::spawn(common::reply_until_close(second));
    socket.close().await;
    timeout(DEADLINE, socket.await_shutdown())
        .await
        .unwrap()
        .unwrap();
    timeout(DEADLINE, close_rx.recv()).await.unwrap().unwrap();
    assert!(close_rx.try_recv().is_err());
    timeout(DEADLINE, peer).await.unwrap().unwrap();
}

#[tokio::test]
async fn text_and_binary_roundtrip_in_enqueue_order() {
    let (listener, url) = common::listener().await;
    let expected = vec![
        (1, b"first".to_vec()),
        (2, vec![0xff, 0, 0x80]),
        (1, b"last".to_vec()),
    ];
    let server_expected = expected.clone();
    let server = tokio::spawn(async move {
        let mut peer = common::accept_ws(&listener).await;
        let mut actual = Vec::new();
        while actual.len() < server_expected.len() {
            let (opcode, payload) = common::read_frame(&mut peer).await.unwrap();
            if opcode == 1 || opcode == 2 {
                common::write_frame(&mut peer, true, opcode, &payload).await;
                actual.push((opcode, payload));
            }
        }
        assert_eq!(actual, server_expected);
        common::reply_until_close(peer).await;
    });
    let (messages_tx, mut messages_rx) = mpsc::unbounded_channel();
    let binary_tx = messages_tx.clone();
    let socket = WebSocketBuilder::new()
        .on_message(move |text| {
            let tx = messages_tx.clone();
            async move {
                let _ = tx.send((1, text.into_bytes()));
            }
        })
        .on_binary(move |bytes| {
            let tx = binary_tx.clone();
            async move {
                let _ = tx.send((2, bytes));
            }
        })
        .connect(&url)
        .await
        .unwrap();
    socket.send("first").await.unwrap();
    socket.send_binary(&[0xff, 0, 0x80]).await.unwrap();
    socket.send("last").await.unwrap();
    let mut actual = Vec::new();
    timeout(DEADLINE, async {
        for _ in 0..expected.len() {
            actual.push(messages_rx.recv().await.unwrap());
        }
    })
    .await
    .unwrap();
    assert_eq!(actual, expected);
    socket.close().await;
    timeout(DEADLINE, socket.await_shutdown())
        .await
        .unwrap()
        .unwrap();
    timeout(DEADLINE, server).await.unwrap().unwrap();
}

#[tokio::test]
async fn queued_messages_keep_fifo_order_across_reconnect() {
    let (listener, url) = common::listener().await;
    let (close_tx, mut close_rx) = mpsc::unbounded_channel();
    let socket = WebSocketBuilder::new()
        .on_close(move |_| {
            let tx = close_tx.clone();
            async move {
                let _ = tx.send(());
            }
        })
        .connect_with_config(
            &url,
            ClientConfig::new().with_reconnect_delay(Duration::from_millis(50)),
        )
        .await
        .unwrap();
    let mut first = timeout(DEADLINE, common::accept_ws(&listener))
        .await
        .unwrap();
    first.shutdown().await.unwrap();
    drop(first);
    timeout(DEADLINE, close_rx.recv()).await.unwrap().unwrap();
    socket.send("queued text").await.unwrap();
    socket.send_binary(&[1, 2, 3]).await.unwrap();
    let (received_tx, received_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let mut second = common::accept_ws(&listener).await;
        let mut messages = Vec::new();
        while messages.len() < 2 {
            let frame = common::read_frame(&mut second).await.unwrap();
            if frame.0 == 1 || frame.0 == 2 {
                messages.push(frame);
            }
        }
        assert_eq!(
            messages,
            vec![(1, b"queued text".to_vec()), (2, vec![1, 2, 3])]
        );
        received_tx.send(()).unwrap();
        common::reply_until_close(second).await;
    });
    timeout(DEADLINE, received_rx).await.unwrap().unwrap();
    socket.close().await;
    timeout(DEADLINE, socket.await_shutdown())
        .await
        .unwrap()
        .unwrap();
    timeout(DEADLINE, server).await.unwrap().unwrap();
}

#[tokio::test]
async fn outgoing_command_does_not_corrupt_partial_incoming_frame() {
    let (listener, url) = common::listener().await;
    let (header_tx, header_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let mut peer = common::accept_ws(&listener).await;
        peer.write_all(&[0x81, 5]).await.unwrap();
        header_tx.send(()).unwrap();
        loop {
            let (opcode, payload) = common::read_frame(&mut peer).await.unwrap();
            if opcode == 1 {
                assert_eq!(payload, b"outbound");
                break;
            }
        }
        peer.write_all(b"hello").await.unwrap();
        common::reply_until_close(peer).await;
    });
    let (messages_tx, mut messages_rx) = mpsc::unbounded_channel();
    let (errors_tx, mut errors_rx) = mpsc::unbounded_channel();
    let socket = WebSocketBuilder::new()
        .on_message(move |message| {
            let tx = messages_tx.clone();
            async move {
                let _ = tx.send(message);
            }
        })
        .on_error(move |error| {
            let tx = errors_tx.clone();
            async move {
                let _ = tx.send(error);
            }
        })
        .connect(&url)
        .await
        .unwrap();
    timeout(DEADLINE, header_rx).await.unwrap().unwrap();
    sleep(Duration::from_millis(50)).await;
    socket.send("outbound").await.unwrap();
    timeout(DEADLINE, async {
        tokio::select! {
            message = messages_rx.recv() => assert_eq!(message.as_deref(), Some("hello")),
            error = errors_rx.recv() => panic!("valid frame rejected: {error:?}"),
        }
    })
    .await
    .unwrap();
    socket.close().await;
    timeout(DEADLINE, socket.await_shutdown())
        .await
        .unwrap()
        .unwrap();
    timeout(DEADLINE, server).await.unwrap().unwrap();
}

#[tokio::test]
async fn malformed_url_is_returned_as_an_error() {
    for url in ["this is not a URL", "ws://"] {
        assert!(
            WebSocket::new(url).await.is_err(),
            "invalid URL was accepted: {url}"
        );
    }
}
