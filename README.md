# fast_websocket_client

[![Crates.io](https://img.shields.io/crates/v/fast_websocket_client)](https://crates.io/crates/fast_websocket_client)
[![docs.rs](https://docs.rs/fast_websocket_client/badge.svg)](https://docs.rs/fast_websocket_client)

An ergonomic, JavaScript-like WebSocket client for Tokio.

- **Callback API** — async event handlers, automatic reconnection, and heartbeat pings.
- **Low-level API** — direct control over reads and writes.
- **Both APIs** support TLS, custom handshake headers, HTTP/HTTPS CONNECT, and SOCKS5 proxies.

## Installation

```sh
cargo add fast_websocket_client
cargo add tokio --features macros,rt,time
```

The default features include the callback client, proxy support, and the AWS-LC TLS provider. For just the low-level API with TLS, disable the default features and enable `aws_lc_rs` or `ring`.

## Callback API

Create the socket, then register handlers on it. Text and binary messages have separate callbacks. Each handler runs to completion before the next one starts.

```rust,no_run
use fast_websocket_client::{WebSocket, WebSocketClientError};
use tokio::time::{Duration, sleep};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), WebSocketClientError> {
    let ws = WebSocket::new("wss://echo.websocket.org").await?;

    ws.on_open(|_| async { println!("open") }).await;
    ws.on_message(|text| async move { println!("text: {text}") }).await;
    ws.on_binary(|bytes| async move { println!("binary: {bytes:?}") }).await;
    ws.on_error(|error| async move { eprintln!("error: {error}") }).await;
    ws.on_close(|event| async move {
        println!("close: {} {}", event.code, event.reason);
    }).await;

    ws.send_binary(&[1, 2, 3]).await?;
    for n in 1..=3 {
        ws.send(&format!("hello {n}")).await?;
        sleep(Duration::from_secs(1)).await;
    }

    ws.close().await;
    ws.await_shutdown().await
}
```

`WebSocket::new().await` starts the connection in the background and returns a handle. On a multithreaded runtime, use `WebSocketBuilder` to install handlers before the first connection attempt when you need to observe its outcome.

## Low-level API

The free `connect().await` waits for the handshake and returns an `Online` client. To set headers, a proxy, or a message size limit first, configure an `Offline` client and call its `connect()` method.

```rust,no_run
use fast_websocket_client::{OpCode, connect};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut ws = connect("wss://echo.websocket.org").await?;
    ws.send_string("hello").await?;

    let frame = ws.receive_frame().await?;
    if frame.opcode == OpCode::Text {
        println!("{}", String::from_utf8_lossy(&frame.payload));
    }

    ws.send_close("bye").await
}
```

## Running the examples

From a checkout of this repository:

| Example | Command |
| --- | --- |
| [Callback client and browser JavaScript equivalent](https://github.com/Osteoporosis/fast_websocket_client/blob/main/examples/async_callback_client.rs) | `cargo run --example wss_client` |
| [Builder with named handlers and channels](https://github.com/Osteoporosis/fast_websocket_client/blob/main/examples/builders_channels_handler_functions.rs) | `cargo run --example builders_channels_handler_functions` |
| [Low-level Binance subscription with idle notifications](https://github.com/Osteoporosis/fast_websocket_client/blob/main/examples/base_client.rs) | `cargo run --example base_client` |
