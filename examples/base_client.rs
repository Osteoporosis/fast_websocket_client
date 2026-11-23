//! Subscribe to Binance book tickers using the low-level client.
//! Run with `cargo run --example base_client`.

use std::time::{Duration, Instant};

use fast_websocket_client::{OpCode, base_client};
use tokio::time::timeout;

#[derive(serde::Serialize)]
struct Subscription {
    method: String,
    params: Vec<String>,
    id: u128,
}

async fn subscribe(
    client: &mut base_client::Online,
    started_at: Instant,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let data = Subscription {
        method: "SUBSCRIBE".to_string(),
        params: vec!["btcusdt@bookTicker".to_string()],
        id: started_at.elapsed().as_nanos(),
    };
    client.send_json(&data).await
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let started_at = Instant::now();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let url = "wss://data-stream.binance.vision:9443/ws/bakeusdt@bookTicker";

    let handle = runtime.spawn(async move {
        'reconnect_loop: loop {
            // To try a custom receive limit, replace the connect call below with
            // `base_client::Offline::new().set_max_message_size(64).connect(url).await`.
            let mut client = match base_client::connect(url).await {
                Ok(client) => {
                    println!("connected");
                    client
                }
                Err(e) => {
                    eprintln!("Reconnecting from an Error: {e:?}");
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    continue;
                }
            };

            // The URL selects one ticker. Subscribe to a second ticker on each new connection.
            if let Err(e) = subscribe(&mut client, started_at).await {
                eprintln!("Reconnecting from an Error: {e:?}");
                drop(client);
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            };

            loop {
                let result = {
                    // Report quiet intervals while allowing a partially read frame to finish.
                    let mut receive = std::pin::pin!(client.receive_frame());
                    loop {
                        match timeout(Duration::from_millis(100), &mut receive).await {
                            Ok(result) => break result,
                            Err(_) => println!("timeout"),
                        }
                    }
                };
                let message = match result {
                    Ok(frame) => frame,
                    Err(e) => {
                        eprintln!("Reconnecting from an Error: {e:?}");
                        break;
                    }
                };

                match message.opcode {
                    OpCode::Text => {
                        let payload = match simdutf8::basic::from_utf8(message.payload.as_ref()) {
                            Ok(payload) => payload,
                            Err(e) => {
                                eprintln!("Reconnecting from an Error: {e:?}");
                                let _ = client.send_close("").await;
                                break;
                            }
                        };
                        println!("{payload}");
                    }
                    OpCode::Close => {
                        let reason = message.payload.get(2..).unwrap_or_default();
                        println!("{:?}", String::from_utf8_lossy(reason));
                        break 'reconnect_loop;
                    }
                    _ => {}
                }
            }
        }
    });
    runtime.block_on(handle)?;
    Ok(())
}
