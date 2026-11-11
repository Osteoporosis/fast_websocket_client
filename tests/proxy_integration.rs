#![cfg(feature = "proxy")]
mod common;

use fast_websocket_client::{
    OpCode,
    base_client::{Offline, Online},
    proxy::{Proxy, ProxyBuilder},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{Duration, timeout},
};

const DEADLINE: Duration = Duration::from_secs(3);

async fn echo(stream: &mut TcpStream) {
    common::handshake(stream).await;
    let (opcode, payload) = common::read_frame(stream).await.unwrap();
    assert_eq!(
        (opcode, payload.as_slice()),
        (1, b"through proxy".as_slice())
    );
    common::write_frame(stream, true, 1, &payload).await;
    let (opcode, payload) = common::read_frame(stream).await.unwrap();
    assert_eq!(opcode, 8);
    common::write_frame(stream, true, 8, &payload).await;
}

async fn roundtrip(client: &mut Online) {
    client.send_string("through proxy").await.unwrap();
    let frame = client.receive_frame().await.unwrap();
    assert_eq!(frame.opcode, OpCode::Text);
    assert_eq!(&frame.payload[..], b"through proxy");
    client.send_close("bye").await.unwrap();
    assert_eq!(client.receive_frame().await.unwrap().opcode, OpCode::Close);
}

#[tokio::test]
async fn http_connect_waits_for_complete_split_headers_and_accepts_2xx() {
    timeout(DEADLINE, async {
        let (listener, _) = common::listener().await;
        let proxy = ProxyBuilder::new()
            .http(&format!("http://{}", listener.local_addr().unwrap()))
            .unwrap()
            .auth("user", "pass")
            .build()
            .unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = common::read_headers(&mut stream).await;
            assert!(request.starts_with("CONNECT example.invalid:80 HTTP/1.1\r\n"));
            assert!(request.contains("Proxy-Authorization: Basic dXNlcjpwYXNz\r\n"));
            stream.write_all(b"HTTP/1.1 ").await.unwrap();
            tokio::time::sleep(Duration::from_millis(25)).await;
            stream
                .write_all(b"201 Connection established\r\n")
                .await
                .unwrap();
            // No tunnel traffic is legal before the header section is complete.
            assert!(
                timeout(Duration::from_millis(25), stream.read_u8())
                    .await
                    .is_err()
            );
            stream
                .write_all(b"Proxy-Agent: local-test\r\n\r\n")
                .await
                .unwrap();
            echo(&mut stream).await;
        });
        let mut offline = Offline::new();
        offline.set_proxy(Some(proxy));
        let mut client = offline.connect("ws://example.invalid/echo").await.unwrap();
        roundtrip(&mut client).await;
        server.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn http_connect_rejects_malformed_and_oversized_headers() {
    for response in [
        b"HTTP/1.1 2000 Invalid\r\n\r\n".to_vec(),
        [
            b"HTTP/1.1 200 OK\r\nX-Long: ".as_slice(),
            &vec![b'x'; 16 * 1024],
        ]
        .concat(),
    ] {
        timeout(DEADLINE, async {
            let (listener, _) = common::listener().await;
            let proxy = ProxyBuilder::new()
                .http(&format!("http://{}", listener.local_addr().unwrap()))
                .unwrap()
                .build()
                .unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                common::read_headers(&mut stream).await;
                let _ = stream.write_all(&response).await;
            });
            let mut offline = Offline::new();
            offline.set_proxy(Some(proxy));
            let error = offline
                .connect("ws://example.invalid/")
                .await
                .err()
                .unwrap();
            assert!(error.to_string().contains("proxy CONNECT"), "{error}");
            server.await.unwrap();
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn https_proxy_is_accepted_and_unsupported_schemes_are_rejected_before_network_io() {
    assert!(ProxyBuilder::new().http("https://proxy.invalid").is_ok());
    assert!(
        ProxyBuilder::new()
            .http("socks5://proxy.invalid:1080")
            .is_err()
    );
    let (listener, _) = common::listener().await;
    let mut offline = Offline::new();
    offline.set_proxy(Some(Proxy::Http {
        uri: url::Url::parse(&format!("socks5://{}", listener.local_addr().unwrap())).unwrap(),
        auth: None,
    }));
    let error = timeout(DEADLINE, offline.connect("ws://example.invalid/"))
        .await
        .unwrap()
        .err()
        .unwrap();
    assert!(
        error.to_string().contains("unsupported proxy scheme"),
        "{error}"
    );
    assert!(
        timeout(Duration::from_millis(25), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn http_connect_keeps_ipv6_destination_bracketed() {
    timeout(DEADLINE, async {
        let (listener, _) = common::listener().await;
        let proxy = ProxyBuilder::new()
            .http(&format!("http://{}", listener.local_addr().unwrap()))
            .unwrap()
            .build()
            .unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = common::read_headers(&mut stream).await;
            assert!(request.starts_with("CONNECT [::1]:8080 HTTP/1.1\r\n"));
            assert!(request.contains("\r\nHost: [::1]:8080\r\n"));
            stream
                .write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n")
                .await
                .unwrap();
        });
        let mut offline = Offline::new();
        offline.set_proxy(Some(proxy));
        let error = offline.connect("ws://[::1]:8080/").await.err().unwrap();
        assert!(error.to_string().contains("HTTP 403"), "{error}");
        server.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn socks5_authenticated_roundtrip_uses_proxy_dns() {
    timeout(DEADLINE, async {
        let (listener, _) = common::listener().await;
        let proxy = ProxyBuilder::new()
            .socks5(&listener.local_addr().unwrap().to_string())
            .auth("user", "pass")
            .build()
            .unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert_eq!(stream.read_u8().await.unwrap(), 5);
            let mut methods = vec![0; stream.read_u8().await.unwrap() as usize];
            stream.read_exact(&mut methods).await.unwrap();
            assert!(methods.contains(&2));
            stream.write_all(&[5, 2]).await.unwrap();
            assert_eq!(stream.read_u8().await.unwrap(), 1);
            let mut user = vec![0; stream.read_u8().await.unwrap() as usize];
            stream.read_exact(&mut user).await.unwrap();
            let mut pass = vec![0; stream.read_u8().await.unwrap() as usize];
            stream.read_exact(&mut pass).await.unwrap();
            assert_eq!(
                (&user[..], &pass[..]),
                (b"user".as_slice(), b"pass".as_slice())
            );
            stream.write_all(&[1, 0]).await.unwrap();
            let mut command = [0; 4];
            stream.read_exact(&mut command).await.unwrap();
            assert_eq!(command, [5, 1, 0, 3]);
            let mut host = vec![0; stream.read_u8().await.unwrap() as usize];
            stream.read_exact(&mut host).await.unwrap();
            assert_eq!(host, b"example.invalid");
            assert_eq!(stream.read_u16().await.unwrap(), 80);
            stream
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
                .await
                .unwrap();
            echo(&mut stream).await;
        });
        let mut offline = Offline::new();
        offline.set_proxy(Some(proxy));
        let mut client = offline.connect("ws://example.invalid/echo").await.unwrap();
        roundtrip(&mut client).await;
        server.await.unwrap();
    })
    .await
    .unwrap();
}
