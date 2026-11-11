#![allow(dead_code)]

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub async fn listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/", listener.local_addr().unwrap());
    (listener, url)
}

pub async fn read_headers<S: AsyncRead + Unpin>(stream: &mut S) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        bytes.push(stream.read_u8().await.unwrap());
        assert!(bytes.len() <= 16384, "oversized test request");
    }
    String::from_utf8(bytes).unwrap()
}

pub async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) -> String {
    let headers = read_headers(stream).await;
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
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    stream.write_all(response.as_bytes()).await.unwrap();
    headers
}

pub async fn accept_ws(listener: &TcpListener) -> TcpStream {
    let (mut stream, _) = listener.accept().await.unwrap();
    stream.set_nodelay(true).unwrap();
    handshake(&mut stream).await;
    stream
}

pub async fn write_frame<S: AsyncWrite + Unpin>(
    stream: &mut S,
    fin: bool,
    opcode: u8,
    payload: &[u8],
) {
    let mut frame = vec![(if fin { 0x80 } else { 0 }) | opcode];
    match payload.len() {
        n @ 0..=125 => frame.push(n as u8),
        n @ 126..=65535 => {
            frame.push(126);
            frame.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            frame.push(127);
            frame.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(payload);
    stream.write_all(&frame).await.unwrap();
}

pub async fn read_frame<S: AsyncRead + Unpin>(stream: &mut S) -> std::io::Result<(u8, Vec<u8>)> {
    let first = stream.read_u8().await?;
    let second = stream.read_u8().await?;
    let len = match second & 127 {
        126 => stream.read_u16().await? as usize,
        127 => stream.read_u64().await? as usize,
        n => n as usize,
    };
    assert!(len <= 16 * 1024, "oversized test frame");
    let mut mask = [0; 4];
    if second & 128 != 0 {
        stream.read_exact(&mut mask).await?;
    }
    let mut payload = vec![0; len];
    stream.read_exact(&mut payload).await?;
    if second & 128 != 0 {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[i % 4];
        }
    }
    Ok((first & 15, payload))
}

pub async fn reply_until_close<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S) {
    while let Ok((opcode, payload)) = read_frame(&mut stream).await {
        match opcode {
            8 => {
                write_frame(&mut stream, true, 8, &payload).await;
                break;
            }
            9 => write_frame(&mut stream, true, 10, &payload).await,
            _ => {}
        }
    }
}
