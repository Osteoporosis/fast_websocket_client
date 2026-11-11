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

async fn receive(frames: Vec<TestFrame>, limit: usize) -> ReceiveResult {
    let (listener, url) = common::listener().await;
    let server = tokio::spawn(async move {
        let mut peer = common::accept_ws(&listener).await;
        for (fin, opcode, payload) in frames {
            common::write_frame(&mut peer, fin, opcode, &payload).await;
        }
        common::reply_until_close(peer).await;
    });
    let mut offline = Offline::new();
    offline.set_max_message_size(limit);
    let mut client = timeout(DEADLINE, offline.connect(&url))
        .await
        .unwrap()
        .unwrap();
    let received = timeout(DEADLINE, client.receive_frame())
        .await
        .unwrap()
        .map(|frame| (frame.opcode, frame.payload.to_vec()));
    drop(client);
    timeout(DEADLINE, server).await.unwrap().unwrap();
    received
}

#[tokio::test]
async fn handshake_rejects_missing_or_incorrect_accept() {
    for accept in ["", "Sec-WebSocket-Accept: incorrect\r\n"] {
        let (listener, url) = common::listener().await;
        let server = tokio::spawn(async move {
            let (mut peer, _) = listener.accept().await.unwrap();
            common::read_headers(&mut peer).await;
            let response = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n{accept}\r\n"
            );
            peer.write_all(response.as_bytes()).await.unwrap();
            common::reply_until_close(peer).await;
        });
        let result = timeout(DEADLINE, Offline::new().connect(&url))
            .await
            .unwrap();
        let error = result.err().expect("invalid handshake was accepted");
        assert!(
            error.to_string().contains("Sec-WebSocket-Accept"),
            "{error}"
        );
        timeout(DEADLINE, server).await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn selected_subprotocol_must_be_single_and_previously_offered() {
    let cases = [
        (
            "Sec-WebSocket-Protocol: chat\r\n",
            Some("chat, other"),
            true,
        ),
        ("Sec-WebSocket-Protocol: unknown\r\n", Some("chat"), false),
        ("Sec-WebSocket-Protocol: chat\r\n", None, false),
        (
            "Sec-WebSocket-Protocol: chat\r\nSec-WebSocket-Protocol: chat\r\n",
            Some("chat"),
            false,
        ),
    ];
    for (selected, offered, should_succeed) in cases {
        let (listener, url) = common::listener().await;
        let server = tokio::spawn(async move {
            let (mut peer, _) = listener.accept().await.unwrap();
            let headers = common::read_headers(&mut peer).await;
            let key = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("sec-websocket-key")
                        .then_some(value.trim())
                })
                .unwrap();
            let mut request = hyper::Request::builder()
                .header("Sec-WebSocket-Key", key)
                .header("Sec-WebSocket-Version", "13")
                .body(())
                .unwrap();
            let (response, _) = fastwebsockets::upgrade::upgrade(&mut request).unwrap();
            let accept = response.headers()["Sec-WebSocket-Accept"].to_str().unwrap();
            let response = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n{selected}\r\n"
            );
            peer.write_all(response.as_bytes()).await.unwrap();
            common::reply_until_close(peer).await;
        });
        let mut offline = Offline::new();
        if let Some(offered) = offered {
            offline.add_header("Sec-WebSocket-Protocol", offered);
        }
        let result = timeout(DEADLINE, offline.connect(&url)).await.unwrap();
        assert_eq!(
            result.is_ok(),
            should_succeed,
            "selection {selected:?}, offer {offered:?}"
        );
        drop(result);
        timeout(DEADLINE, server).await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn request_target_contains_only_the_original_path_and_query() {
    let (listener, url) = common::listener().await;
    let address = listener.local_addr().unwrap();
    let url = format!("{url}chat?x=%2F&token=a");
    let (headers_tx, headers_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut peer, _) = listener.accept().await.unwrap();
        headers_tx.send(common::handshake(&mut peer).await).unwrap();
        common::reply_until_close(peer).await;
    });
    let client = timeout(DEADLINE, Offline::new().connect(&url))
        .await
        .unwrap()
        .unwrap();
    let headers = timeout(DEADLINE, headers_rx).await.unwrap().unwrap();
    assert_eq!(
        headers.lines().next(),
        Some("GET /chat?x=%2F&token=a HTTP/1.1")
    );
    assert!(
        headers
            .lines()
            .any(|line| line.eq_ignore_ascii_case(&format!("Host: {address}")))
    );
    drop(client);
    timeout(DEADLINE, server).await.unwrap().unwrap();
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
async fn fragmented_text_validates_the_complete_utf8_sequence() {
    let valid = receive(
        vec![
            (false, 1, vec![0xe2]),
            (false, 0, vec![0x82]),
            (true, 0, vec![0xac]),
        ],
        3,
    )
    .await
    .unwrap();
    assert_eq!(valid, (OpCode::Text, "€".as_bytes().to_vec()));
    let invalid = receive(vec![(false, 1, b"A".to_vec()), (true, 0, vec![0xe2])], 3)
        .await
        .expect_err("unfinished UTF-8 suffix was accepted");
    assert!(matches!(
        invalid.downcast_ref::<WebSocketError>(),
        Some(WebSocketError::InvalidUTF8)
    ));
}

#[tokio::test]
async fn ping_interleaves_with_fragments_and_is_not_limited_by_message_size() {
    let (listener, url) = common::listener().await;
    let server = tokio::spawn(async move {
        let mut peer = common::accept_ws(&listener).await;
        common::write_frame(&mut peer, false, 1, b"A").await;
        common::write_frame(&mut peer, true, 9, &[b'p'; 125]).await;
        let pong = common::read_frame(&mut peer).await.unwrap();
        assert_eq!(pong, (10, vec![b'p'; 125]));
        common::write_frame(&mut peer, true, 0, b"B").await;
        common::reply_until_close(peer).await;
    });
    let mut offline = Offline::new();
    offline.set_max_message_size(2);
    let mut client = timeout(DEADLINE, offline.connect(&url))
        .await
        .unwrap()
        .unwrap();
    let frame = timeout(DEADLINE, client.receive_frame())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.opcode, OpCode::Text);
    assert_eq!(&frame.payload[..], b"AB");
    drop(frame);
    drop(client);
    timeout(DEADLINE, server).await.unwrap().unwrap();
}

#[tokio::test]
async fn in_flight_ping_after_local_close_is_consumed_without_another_write() {
    let (listener, url) = common::listener().await;
    let server = tokio::spawn(async move {
        let mut peer = common::accept_ws(&listener).await;
        let close = common::read_frame(&mut peer).await.unwrap();
        assert_eq!(close, (8, vec![0x03, 0xe8]));
        common::write_frame(&mut peer, true, 9, b"in flight").await;
        common::write_frame(&mut peer, true, 8, &close.1).await;
        assert!(
            common::read_frame(&mut peer).await.is_err(),
            "the client must not send another frame after its Close"
        );
    });
    let mut client = timeout(DEADLINE, Offline::new().connect(&url))
        .await
        .unwrap()
        .unwrap();
    timeout(DEADLINE, client.send_close(""))
        .await
        .unwrap()
        .unwrap();
    let frame = timeout(DEADLINE, client.receive_frame())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.opcode, OpCode::Close);
    assert_eq!(&frame.payload[..], &[0x03, 0xe8]);
    drop(frame);
    drop(client);
    timeout(DEADLINE, server).await.unwrap().unwrap();
}

#[tokio::test]
async fn malformed_control_frames_are_rejected() {
    let cases = vec![
        (true, 8, vec![0]), // Close status is either absent or at least two bytes.
        (true, 8, vec![0x03, 0xee]), // 1006 cannot appear on the wire.
        (true, 8, vec![0x03, 0xe8, 0xff]), // Invalid UTF-8 reason.
        (true, 8, vec![0; 126]),
        (true, 9, vec![0; 126]),
        (true, 10, vec![0; 126]),
        (false, 9, vec![]), // Control frames cannot be fragmented.
    ];
    for frame in cases {
        assert!(receive(vec![frame], 1024).await.is_err());
    }
}

#[tokio::test]
async fn invalid_outgoing_payloads_do_not_write_or_close_the_socket() {
    let (listener, url) = common::listener().await;
    let server = tokio::spawn(async move {
        let mut peer = common::accept_ws(&listener).await;
        assert_eq!(
            common::read_frame(&mut peer).await.unwrap(),
            (1, b"still open".to_vec())
        );
        common::reply_until_close(peer).await;
    });
    let mut client = timeout(DEADLINE, Offline::new().connect(&url))
        .await
        .unwrap()
        .unwrap();
    assert!(client.send_ping(&"p".repeat(126)).await.is_err());
    assert!(client.send_pong(&"p".repeat(126)).await.is_err());
    assert!(client.send_close(&"r".repeat(124)).await.is_err());
    let invalid_json = std::collections::BTreeMap::from([((1, 2), 3)]);
    assert!(client.send_json(&invalid_json).await.is_err());
    client.send_string("still open").await.unwrap();
    client.send_close("").await.unwrap();
    let frame = timeout(DEADLINE, client.receive_frame())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.opcode, OpCode::Close);
    drop(frame);
    drop(client);
    timeout(DEADLINE, server).await.unwrap().unwrap();
}
