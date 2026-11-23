mod common;

use std::error::Error;
use std::time::Duration;

use fast_websocket_client::{OpCode, base_client::Offline};
use fastwebsockets::WebSocketError;
use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;
use tokio::time::timeout;

const DEADLINE: Duration = Duration::from_secs(3);
type TestFrame = (bool, u8, Vec<u8>);
type ReceiveResult = Result<(OpCode, Vec<u8>), Box<dyn Error + Send + Sync>>;

async fn exchange<P: Future, C: Future>(peer: P, client: C) -> (P::Output, C::Output) {
    timeout(DEADLINE, async { tokio::join!(peer, client) })
        .await
        .expect("client and peer did not complete the exchange")
}

async fn receive(frames: Vec<TestFrame>, limit: usize) -> ReceiveResult {
    let (listener, url) = common::listener().await;
    let peer = async {
        let mut peer = common::accept_ws(&listener).await;
        for (fin, opcode, payload) in frames {
            common::write_frame(&mut peer, fin, opcode, &payload).await;
        }
    };
    let client = async {
        let mut offline = Offline::new();
        offline.set_max_message_size(limit);
        let mut client = offline.connect(&url).await.unwrap();
        client
            .receive_frame()
            .await
            .map(|frame| (frame.opcode, frame.payload.to_vec()))
    };
    let ((), received) = exchange(peer, client).await;
    received
}

#[tokio::test]
async fn handshake_rejects_missing_or_incorrect_accept() {
    for accept in ["", "Sec-WebSocket-Accept: incorrect\r\n"] {
        let (listener, url) = common::listener().await;
        let server = async {
            let (mut peer, _) = listener.accept().await.unwrap();
            common::read_headers(&mut peer).await;
            let response = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n{accept}\r\n"
            );
            peer.write_all(response.as_bytes()).await.unwrap();
        };
        let ((), result) = exchange(server, Offline::new().connect(&url)).await;
        let error = result.err().expect("invalid handshake was accepted");
        assert!(
            error.to_string().contains("Sec-WebSocket-Accept"),
            "{error}"
        );
    }
}

#[tokio::test]
async fn upgrade_validates_negotiated_headers() {
    for (offered, extra, accepted) in [
        (Some("chat, other"), "Sec-WebSocket-Protocol: chat", true),
        (Some("chat"), "Sec-WebSocket-Protocol: other", false),
        (None, "Sec-WebSocket-Protocol: chat", false),
        (
            Some("chat"),
            "Sec-WebSocket-Protocol: chat\r\nSec-WebSocket-Protocol: chat",
            false,
        ),
        (None, "Sec-WebSocket-Extensions: permessage-deflate", false),
        (None, "Sec-WebSocket-Accept: duplicate", false),
        (None, "Upgrade: h2c", false),
        (None, "Connection: invalid option", false),
    ] {
        let (listener, url) = common::listener().await;
        let mut offline = Offline::new();
        if let Some(offered) = offered {
            offline.add_header("Sec-WebSocket-Protocol", offered);
        }
        let server = async {
            let (mut peer, _) = listener.accept().await.unwrap();
            let request = common::read_headers(&mut peer).await;
            let response = common::handshake_response(&request);
            let response = format!("{}\r\n{extra}\r\n\r\n", response.trim_end());
            peer.write_all(response.as_bytes()).await.unwrap();
        };
        let ((), result) = exchange(server, offline.connect(&url)).await;
        assert_eq!(result.is_ok(), accepted, "{extra:?}, offered {offered:?}");
    }
}

#[tokio::test]
async fn request_target_contains_only_the_original_path_and_query() {
    let (listener, url) = common::listener().await;
    let address = listener.local_addr().unwrap();
    let url = format!("{url}chat?x=%2F&token=a");
    let peer = async {
        let (mut peer, _) = listener.accept().await.unwrap();
        common::handshake(&mut peer).await
    };
    let (headers, connected) = exchange(peer, Offline::new().connect(&url)).await;
    connected.unwrap();
    assert_eq!(
        headers.lines().next(),
        Some("GET /chat?x=%2F&token=a HTTP/1.1")
    );
    assert!(
        headers
            .lines()
            .any(|line| line.eq_ignore_ascii_case(&format!("Host: {address}")))
    );
}

#[tokio::test]
async fn text_and_binary_message_limits_are_inclusive_and_apply_to_all_fragments() {
    for opcode in [1, 2] {
        for fragmented in [false, true] {
            for size in [4, 5] {
                let frames = if fragmented {
                    vec![
                        (false, opcode, b"ab".to_vec()),
                        (true, 0, vec![b'c'; size - 2]),
                    ]
                } else {
                    vec![(true, opcode, vec![b'a'; size])]
                };
                let result = receive(frames, 4).await;
                if size == 4 {
                    let (received_opcode, payload) = result.unwrap();
                    assert_eq!(
                        received_opcode,
                        if opcode == 1 {
                            OpCode::Text
                        } else {
                            OpCode::Binary
                        }
                    );
                    assert_eq!(payload.len(), 4);
                } else {
                    let error = result.expect_err("oversized message was accepted");
                    assert!(matches!(
                        error.downcast_ref::<WebSocketError>(),
                        Some(WebSocketError::FrameTooLarge)
                    ));
                }
            }
        }
    }
}

#[tokio::test]
async fn ping_interleaves_with_fragments_and_is_not_limited_by_message_size() {
    let (listener, url) = common::listener().await;
    let peer = async {
        let mut peer = common::accept_ws(&listener).await;
        common::write_frame(&mut peer, false, 1, b"A").await;
        common::write_frame(&mut peer, true, 9, &[b'p'; 125]).await;
        let pong = common::read_frame(&mut peer).await.unwrap();
        assert_eq!(pong, (10, vec![b'p'; 125]));
        common::write_frame(&mut peer, true, 0, b"B").await;
    };
    let client = async {
        let mut offline = Offline::new();
        offline.set_max_message_size(2);
        let mut client = offline.connect(&url).await.unwrap();
        let frame = client.receive_frame().await.unwrap();
        assert_eq!(frame.opcode, OpCode::Text);
        assert_eq!(&frame.payload[..], b"AB");
    };
    exchange(peer, client).await;
}

#[tokio::test]
async fn invalid_outgoing_payloads_do_not_write_or_close_the_socket() {
    let (listener, url) = common::listener().await;
    let peer = async {
        let mut peer = common::accept_ws(&listener).await;
        assert_eq!(
            common::read_frame(&mut peer).await.unwrap(),
            (1, b"still open".to_vec())
        );
        common::reply_until_close(peer).await;
    };
    let client = async {
        let mut client = Offline::new().connect(&url).await.unwrap();
        assert!(client.send_ping(&"p".repeat(126)).await.is_err());
        assert!(client.send_pong(&"p".repeat(126)).await.is_err());
        assert!(client.send_close(&"r".repeat(124)).await.is_err());
        let invalid_json = std::collections::BTreeMap::from([((1, 2), 3)]);
        assert!(client.send_json(&invalid_json).await.is_err());
        client.send_string("still open").await.unwrap();
        client.send_close("").await.unwrap();
        let frame = client.receive_frame().await.unwrap();
        assert_eq!(frame.opcode, OpCode::Close);
    };
    exchange(peer, client).await;
}

#[tokio::test]
async fn idle_ticks_preserve_a_partially_received_message_on_the_same_socket() {
    let (listener, url) = common::listener().await;
    let (sent, header_sent) = oneshot::channel();
    let (resume, finish_frame) = oneshot::channel();
    let peer = async {
        let mut peer = common::accept_ws(&listener).await;
        peer.write_all(b"\x81\x05he").await.unwrap();
        sent.send(()).unwrap();
        finish_frame.await.unwrap();
        peer.write_all(b"llo").await.unwrap();
    };

    let client = async {
        let mut online = Offline::new().connect(&url).await.unwrap();
        header_sent.await.unwrap();
        let mut receive = std::pin::pin!(online.receive_frame());
        for _ in 0..2 {
            assert!(
                timeout(Duration::from_millis(10), &mut receive)
                    .await
                    .is_err()
            );
        }
        resume.send(()).unwrap();
        assert_eq!(&receive.await.unwrap().payload[..], b"hello");
    };
    exchange(peer, client).await;
}
