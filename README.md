# fast_websocket_client

[![Crates.io](https://img.shields.io/crates/v/fast_websocket_client)](https://crates.io/crates/fast_websocket_client)
[![docs.rs](https://docs.rs/fast_websocket_client/badge.svg)](https://docs.rs/fast_websocket_client)

**Tokio-native WebSocket client for Rust** – high-throughput, low-latency, callback-driven, proxy-ready.

Supports two modes of operation:

- **High-level callback-based client** for ergonomic event-driven use.
- **Low-level direct API** for fine-tuned control with minimal dependencies.

Quick Example: [examples/async_callback_client.rs](https://github.com/Osteoporosis/fast_websocket_client/blob/main/examples/async_callback_client.rs)

## Features

- JavaScript-like callback API for event handling
- Separate text (`String`) and binary (`Vec<u8>`) message callbacks
- Async/await support via `tokio`
- Powered by [`fastwebsockets`](https://github.com/denoland/fastwebsockets)
- Built-in reconnection and ping loop
- TLS support
- Custom HTTP headers for handshake (e.g., Authorization)
- Proxy support (HTTP/HTTPS CONNECT & SOCKS5)

## Installation

```bash
cargo add fast_websocket_client
```

## High-Level Callback API

An ergonomic, JavaScript-like API with built-in reconnect, ping, and lifecycle hooks.

`WebSocket::connect().await`, `WebSocket::new().await`, and the builder's connect methods validate the URL and configuration, start a background task, and return a handle. Connection success is reported through `on_open`; network and handshake failures are reported through `on_error`. Use `WebSocketBuilder` to register callbacks before the first connection attempt.

`on_message` receives text as `String`; `on_binary` receives binary data as `Vec<u8>`.

`on_close` receives a `CloseEvent` with the close code, reason, and whether the closing handshake completed successfully (`was_clean`).

```rust
// try this example with
// `cargo run --example wss_client`

use tokio::time::{Duration, sleep};
use fast_websocket_client::WebSocket;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), fast_websocket_client::WebSocketClientError> {
    let ws = WebSocket::new("wss://echo.websocket.org").await?;

    ws.on_open(|_| async move {
        println!("[OPEN] WebSocket connection opened.");
    })
    .await;
    ws.on_close(|event| async move {
        println!(
            "[CLOSE] code={}, reason={:?}, clean={}",
            event.code, event.reason, event.was_clean
        );
    })
    .await;
    ws.on_message(|message| async move {
        println!("[MESSAGE] {}", message);
    })
    .await;
    ws.on_binary(|bytes| async move {
        println!("[BINARY] {:?}", bytes);
    })
    .await;
    ws.on_error(|error| async move {
        eprintln!("[ERROR] {}", error);
    })
    .await;

    sleep(Duration::from_secs(2)).await;
    ws.send_binary(&[0, 1, 255]).await?;
    for i in 1..5 {
        let message = format!("#{}", i);
        if let Err(e) = ws.send(&message).await {
            eprintln!("[ERROR] Send error: {:?}", e);
            break;
        }
        println!("[SEND] {}", message);
        sleep(Duration::from_secs(2)).await;
    }

    ws.close().await;
    ws.await_shutdown().await?;
    Ok(())
}
```

`close().await` requests shutdown and stops reconnection. An established connection has up to five seconds to complete its locally requested Close handshake. Set this limit with `ClientConfig::new().with_close_timeout(Duration::from_secs(3))`, passed to `connect_with_config` or `update_config`. Use `await_shutdown().await?` to wait for the task and propagate a task failure.

## Low-Level API

The free `connect().await` helper waits for the TCP/TLS/WebSocket handshake and returns an established connection. If `receive_frame()` is cancelled, including by a timeout, discard that connection before receiving again. See `examples/base_client.rs` for reconnecting after a receive timeout.

```rust
use fast_websocket_client::{connect, OpCode};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut client = connect("wss://echo.websocket.org").await?;

    client.send_string("Hello, WebSocket!").await?;

    let frame = client.receive_frame().await?;
    if frame.opcode == OpCode::Text {
        println!("Received: {}", String::from_utf8_lossy(&frame.payload));
    }

    client.send_close("bye").await?;
    Ok(())
}
```

## Running the Example

Clone the repo and run:

```bash
cargo run --example wss_client
```

## Migration Guide (from `0.2.0`)

| Old                                     | New                    |
|-----------------------------------------|------------------------|
| `client::Offline`                       | `base_client::Offline` |
| `client::Online`                        | `base_client::Online`  |
| `on_close` callback argument `()`       | `CloseEvent`; callbacks using `\|_\|` need no change. |
| Runtime settings via `Online`'s methods | Must now be set before connect via `ConnectionInitOptions`.<br>Changes to the running `WebSocket` take effect on the next (re)connection. |

**New users:** We recommend starting with the `WebSocket` API for best experience.

## Documentation

- [docs.rs/fast_websocket_client](https://docs.rs/fast_websocket_client)
- [Examples](https://github.com/Osteoporosis/fast_websocket_client/blob/main/examples/)
- [fastwebsockets upstream](https://github.com/denoland/fastwebsockets)

---

💡 Actively maintained - **contributions are welcome!**
