# fast_websocket_client

![Crates.io](https://img.shields.io/crates/v/fast_websocket_client)
![docs.rs](https://docs.rs/fast_websocket_client/badge.svg)

**A blazing-fast, async-native WebSocket client for Rust**, built on top of [`fastwebsockets`](https://github.com/denoland/fastwebsockets) and `tokio`.

Supports two modes of operation:
- 🔁 **High-level callback-based client** for ergonomic event-driven use.
- ⚙️ **Low-level direct API** for fine-tuned control with minimal dependencies.

Quick Example: [examples/async_callback_client.rs](https://github.com/Osteoporosis/fast_websocket_client/blob/main/examples/async_callback_client.rs)

## 📦 Features

- Async/await support via `tokio`
- Built-in reconnection and ping loop
- Optional callback-driven lifecycle management
- Custom HTTP headers for handshake (e.g., Authorization)

## 🛠 Installation

```bash
cargo add fast_websocket_client
```

## 🔁 High-Level Callback API

An ergonomic, JavaScript-like API with built-in reconnect, ping, and lifecycle hooks.

```rust
// try this example with
// `cargo run --example wss_client`

use tokio::time::{Duration, sleep};
use fast_websocket_client::WebSocket;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), fast_websocket_client::WebSocketClientError> {
    let ws = WebSocket::new("wss://echo.websocket.org").await?;

    ws.on_close(|_| async move {
        println!("[CLOSE] WebSocket connection closed.");
    })
    .await;
    ws.on_message(|message| async move {
        println!("[MESSAGE] {}", message);
    })
    .await;

    sleep(Duration::from_secs(1)).await;
    for i in 1..5 {
        let message = format!("#{}", i);
        if let Err(e) = ws.send(&message).await {
            eprintln!("[ERROR] Send error: {:?}", e);
            break;
        }
        println!("[SEND] {}", message);
        sleep(Duration::from_secs(5)).await;
    }

    ws.close().await;
    ws.await_shutdown().await;
    Ok(())
}
```

## 🧵 Low-Level API

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

## 🧪 Running the Example

Clone the repo and run:

```bash
cargo run --example wss_client
```

## 🔄 Migration Guide (from `0.2.0`)

| Old                                     | New                    |
|-----------------------------------------|------------------------|
| `client::Offline`                       | `base_client::Offline` |
| `client::Online`                        | `base_client::Online`  |
| Runtime settings via `Online`'s methods | Must now be set before connect via `ConnectionInitOptions`.<br>Changes to the running `WebSocket` take effect on the next (re)connection. |

**New users:** We recommend starting with the `WebSocket` API for best experience.

## 📚 Documentation

- [docs.rs/fast_websocket_client](https://docs.rs/fast_websocket_client)
- [Examples](https://github.com/Osteoporosis/fast_websocket_client/blob/main/examples/)
- [fastwebsockets upstream](https://github.com/denoland/fastwebsockets)

---

💡 Actively maintained – **contributions are welcome!**
