use fast_websocket_client::WebSocketBuilder;
use tokio::sync::mpsc;
use tokio::time::{Duration, sleep};

/// Called when the WebSocket opens
async fn handle_open(open_tx: mpsc::Sender<()>) {
    println!("[OPEN] WebSocket connection established.");
    let _ = open_tx.send(()).await;
}

/// Called when the WebSocket closes
async fn handle_close() {
    println!("[CLOSE] WebSocket connection closed.");
}

/// Called for each incoming message
async fn handle_message(msg_tx: mpsc::Sender<u32>, msg: String) {
    println!("[MESSAGE] {}", msg);
    if let Some(n) = msg.strip_prefix('#').and_then(|s| s.parse().ok()) {
        let _ = msg_tx.send(n).await;
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), fast_websocket_client::WebSocketClientError> {
    // channel for open notification
    let (open_tx, mut open_rx) = mpsc::channel::<()>(1);
    // channel for parsed message numbers
    let (msg_tx, mut msg_rx) = mpsc::channel::<u32>(1);

    // build and connect the WebSocket client
    let ws = WebSocketBuilder::new()
        .on_open(move |_| handle_open(open_tx.clone()))
        .on_close(|_| handle_close())
        .on_message(move |msg| handle_message(msg_tx.clone(), msg))
        .connect("wss://echo.websocket.org")
        .await?;

    // wait for the connection to open
    let _ = open_rx.recv().await;
    // send the first message
    ws.send("#1").await?;

    // loop until we reach #10
    while let Some(n) = msg_rx.recv().await {
        if n >= 10 {
            break;
        }
        let next = format!("#{}", n + 1);
        ws.send(&next).await?;
    }

    // wait a bit and then close
    sleep(Duration::from_secs(2)).await;
    ws.close().await;
    ws.await_shutdown().await;
    Ok(())
}
