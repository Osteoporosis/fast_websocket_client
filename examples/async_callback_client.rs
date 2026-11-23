//! Run with `cargo run --example wss_client`. A browser equivalent follows the Rust example.

use fast_websocket_client::WebSocket;
use tokio::time::{Duration, sleep};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), fast_websocket_client::WebSocketClientError> {
    let ws = WebSocket::new("wss://echo.websocket.org").await?;

    ws.on_open(|_| async move {
        println!("[OPEN] WebSocket connection opened.");
    })
    .await;
    ws.on_close(|event| async move {
        println!("[CLOSE] {} {}", event.code, event.reason);
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

/* JavaScript equivalent
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <title>WebSocket Client</title>
</head>
<body>
  <script>
    function sleep(ms) {
      return new Promise(resolve => setTimeout(resolve, ms));
    }

    function main() {
      const ws = new WebSocket("wss://echo.websocket.org");
      ws.binaryType = "arraybuffer";

      ws.onclose = (event) => {
        console.log("[CLOSE]", event.code, event.reason);
      };
      ws.onmessage = (event) => {
        if (typeof event.data === "string") {
          console.log("[MESSAGE]", event.data);
        } else {
          console.log("[BINARY]", new Uint8Array(event.data));
        }
      };
      ws.onerror = (event) => {
        console.error("[ERROR]", event);
      };

      ws.onopen = async () => {
        console.log("[OPEN] WebSocket connection opened.");
        ws.send(new Uint8Array([0, 1, 255]));
        for (let i = 1; i < 5; i++) {
          const message = `#${i}`;
          try {
            ws.send(message);
            console.log("[SEND]", message);
          } catch (err) {
            console.error("[ERROR] Send error:", err);
            break;
          }
          await sleep(2000);
        }

        ws.close();
      };
    }

    main();
  </script>
</body>
</html>
*/
