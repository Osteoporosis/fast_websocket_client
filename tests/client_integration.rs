#![cfg(feature = "callback_client")]

mod common;

use std::sync::Arc;
use std::time::Duration;

use fast_websocket_client::{ClientConfig, WebSocket, WebSocketBuilder};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::time::{Instant, sleep, timeout};

const DEADLINE: Duration = Duration::from_secs(3);

#[tokio::test]
async fn close_interrupts_failed_connection_backoff() {
    let (listener, url) = common::listener().await;
    drop(listener);
    let (errors, mut observed) = mpsc::unbounded_channel();
    let socket = WebSocket::connect(&url).await.unwrap();
    socket
        .on_error(move |error| {
            let errors = errors.clone();
            async move { errors.send(error).unwrap() }
        })
        .await;
    timeout(DEADLINE, observed.recv()).await.unwrap().unwrap();
    socket.close().await;
    timeout(DEADLINE, socket.await_shutdown())
        .await
        .expect("Close must interrupt retry backoff")
        .unwrap();
}

#[tokio::test]
async fn close_interrupts_an_unfinished_handshake() {
    let (listener, url) = common::listener().await;
    let socket = WebSocket::connect(&url).await.unwrap();
    let (peer, _) = timeout(DEADLINE, listener.accept()).await.unwrap().unwrap();
    socket.close().await;
    timeout(DEADLINE, socket.await_shutdown())
        .await
        .expect("Close must cancel an active handshake")
        .unwrap();
    drop(peer);
}

#[tokio::test]
async fn close_at_upgrade_with_a_buffered_peer_close_never_reconnects() {
    timeout(DEADLINE, async {
        let (listener, url) = common::listener().await;
        let entered = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());
        let callback_entered = entered.clone();
        let callback_resume = resume.clone();
        let socket = WebSocketBuilder::new()
            .on_error(move |error| {
                let entered = callback_entered.clone();
                let resume = callback_resume.clone();
                async move {
                    assert!(error.contains("ping_interval"), "{error}");
                    entered.notify_one();
                    resume.notified().await;
                }
            })
            .connect(&url)
            .await
            .unwrap();

        let (mut peer, _) = listener.accept().await.unwrap();
        let request = common::read_headers(&mut peer).await;
        // Hold the driver in a callback so the upgrade and both Close requests meet.
        socket
            .update_config(ClientConfig::new().with_ping_interval(Duration::ZERO))
            .await;
        entered.notified().await;
        let mut response = common::handshake_response(&request).into_bytes();
        response.extend_from_slice(b"\x88\x02\x03\xe8");
        peer.write_all(&response).await.unwrap();
        socket.close().await;
        resume.notify_one();

        socket.await_shutdown().await.unwrap();
        common::assert_no_pending_connection(listener);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn failed_attempt_backs_off_and_uses_updated_error_handler_on_retry() {
    timeout(DEADLINE, async {
        let (listener, url) = common::listener().await;
        let delay = Duration::from_millis(100);
        let (old_tx, mut old_rx) = mpsc::unbounded_channel();
        let (new_tx, mut new_rx) = mpsc::unbounded_channel();
        let socket = WebSocketBuilder::new()
            .on_error(move |error| {
                let tx = old_tx.clone();
                async move {
                    let _ = tx.send(error);
                }
            })
            .connect_with_config(&url, ClientConfig::new().with_reconnect_delay(delay))
            .await
            .unwrap();
        let (mut first, _) = listener.accept().await.unwrap();
        common::read_headers(&mut first).await;
        let failed_at = Instant::now();
        drop(first);
        old_rx.recv().await.unwrap();
        socket
            .on_error(move |error| {
                let tx = new_tx.clone();
                async move {
                    let _ = tx.send(error);
                }
            })
            .await;
        let (mut second, _) = listener.accept().await.unwrap();
        assert!(failed_at.elapsed() >= delay);
        common::read_headers(&mut second).await;
        drop(second);
        new_rx.recv().await.unwrap();
        socket.close().await;
        socket.await_shutdown().await.unwrap();
    })
    .await
    .expect("failed attempts should retry and report the error to the updated handler");
}

#[tokio::test]
async fn eof_reconnects_immediately_and_sends_waiting_messages_in_order() {
    timeout(DEADLINE, async {
        let (listener, url) = common::listener().await;
        let (closed, mut events) = mpsc::unbounded_channel();
        let socket = WebSocketBuilder::new()
            .on_close(move |event| {
                let closed = closed.clone();
                async move { closed.send(event).unwrap() }
            })
            .connect_with_config(
                &url,
                ClientConfig::new().with_reconnect_delay(Duration::from_secs(30)),
            )
            .await
            .unwrap();
        let mut first = common::accept_ws(&listener).await;
        first.shutdown().await.unwrap();
        drop(first);
        let event = events.recv().await.unwrap();
        assert_eq!(event.code, 1006);
        assert!(event.reason.is_empty());
        assert!(!event.was_clean);

        socket.send("next session").await.unwrap();
        socket.send_binary(&[1, 2, 3]).await.unwrap();
        let mut second = common::accept_ws(&listener).await;
        assert_eq!(
            common::read_data_or_close(&mut second).await,
            (1, b"next session".to_vec())
        );
        assert_eq!(
            common::read_data_or_close(&mut second).await,
            (2, vec![1, 2, 3])
        );
        socket.close().await;
        let (_, result) = tokio::join!(common::reply_until_close(second), socket.await_shutdown());
        result.unwrap();
        assert!(events.recv().await.unwrap().was_clean);
        assert_eq!(events.recv().await, None);
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn handlers_registered_after_connect_observe_open_before_messages() {
    timeout(DEADLINE, async {
        let (listener, url) = common::listener().await;
        let server = tokio::spawn(async move {
            let (mut peer, _) = listener.accept().await.unwrap();
            let request = common::read_headers(&mut peer).await;
            let mut response = common::handshake_response(&request).into_bytes();
            response.extend_from_slice(b"\x81\x07welcome");
            peer.write_all(&response).await.unwrap();
            assert_eq!(
                common::read_data_or_close(&mut peer).await,
                (1, b"first".to_vec())
            );
            common::write_frame(&mut peer, true, 1, b"first").await;
            common::reply_until_close(peer).await;
        });

        let (events, mut observed) = mpsc::unbounded_channel();
        let open = events.clone();
        let socket = WebSocket::connect(&url).await.unwrap();
        socket
            .on_open(move |_| {
                let open = open.clone();
                async move { open.send("open".to_owned()).unwrap() }
            })
            .await;
        socket
            .on_message(move |data| {
                let events = events.clone();
                async move { events.send(data).unwrap() }
            })
            .await;
        socket.send("first").await.unwrap();
        socket
            .on_open(|_| async { panic!("a later handler overtook the first send") })
            .await;
        assert_eq!(observed.recv().await.as_deref(), Some("open"));
        assert_eq!(observed.recv().await.as_deref(), Some("welcome"));
        assert_eq!(observed.recv().await.as_deref(), Some("first"));
        socket.on_open(|_| async { panic!("open replayed") }).await;
        socket.close().await;
        socket.await_shutdown().await.unwrap();
        server.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn text_and_binary_roundtrip_in_send_order() {
    timeout(DEADLINE, async {
        let (listener, url) = common::listener().await;
        let expected = vec![
            (1, b"first".to_vec()),
            (2, vec![0xff, 0, 0x80]),
            (1, b"last".to_vec()),
        ];
        let server_expected = expected.clone();
        let server = tokio::spawn(async move {
            let mut peer = common::accept_ws(&listener).await;
            for expected in server_expected {
                let frame = common::read_data_or_close(&mut peer).await;
                assert_eq!(frame, expected);
                common::write_frame(&mut peer, true, frame.0, &frame.1).await;
            }
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
        for expected in expected {
            assert_eq!(messages_rx.recv().await, Some(expected));
        }
        socket.close().await;
        socket.await_shutdown().await.unwrap();
        server.await.unwrap();
    })
    .await
    .expect("text and binary round trip timed out");
}

#[tokio::test]
async fn outgoing_command_does_not_corrupt_partial_incoming_frame() {
    timeout(DEADLINE, async {
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
        header_rx.await.unwrap();
        sleep(Duration::from_millis(50)).await;
        socket.send("outbound").await.unwrap();
        tokio::select! {
            message = messages_rx.recv() => assert_eq!(message.as_deref(), Some("hello")),
            error = errors_rx.recv() => panic!("valid frame rejected: {error:?}"),
        }
        socket.close().await;
        socket.await_shutdown().await.unwrap();
        server.await.unwrap();
    })
    .await
    .expect("client should finish the partial read and shut down");
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
