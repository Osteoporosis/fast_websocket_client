// try this example with
// $ cargo run --example wss_client

use std::time::{Duration, Instant};

use fast_websocket_client::{client, connect, OpCode};

#[derive(serde::Serialize)]
struct Subscription {
    method: String,
    params: Vec<String>,
    id: u128,
}

async fn subscribe(
    client: &mut client::Online,
    started_at: Instant,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let data = Subscription {
        method: "SUBSCRIBE".to_string(),
        params: vec!["btcusdt@bookTicker".to_string()],
        id: started_at.elapsed().as_nanos(),
    };
    tokio::time::timeout(Duration::from_millis(0), client.send_json(&data)).await??;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let started_at = Instant::now();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // the lowest volume example
    let url = "wss://data-stream.binance.vision:9443/ws/bakeusdt@bookTicker";

    let handle = runtime.spawn(async move {
        'reconnect_loop: loop {
            let future = connect(url);
            /*
                alternative code for an example:
                    1. make a Offline client
                    2. apply an intentional error raising setting before `connect`
                    3. call `connect` to get a future
            */
            // let mut client = client::Offline::new();
            // client.set_max_message_size(64);
            // let future = client.connect(url);

            let mut client: client::Online = match future.await {
                Ok(client) => {
                    println!("conneted");
                    client
                }
                Err(e) => {
                    eprintln!("Reconnecting from an Error: {e:?}");
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    continue;
                }
            };

            // we can modify settings while running.
            // without pong, this app stops in about 15 minutes.(by the binance API spec.)
            client.set_auto_pong(false);

            // add one more example subscription here after connect
            if let Err(e) = subscribe(&mut client, started_at).await {
                eprintln!("Reconnecting from an Error: {e:?}");
                let _ = client.send_close(&[]).await;
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            };

            // message processing loop
            loop {
                let message = if let Ok(result) =
                    tokio::time::timeout(Duration::from_millis(100), client.receive_frame()).await
                {
                    match result {
                        Ok(message) => message,
                        Err(e) => {
                            eprintln!("Reconnecting from an Error: {e:?}");
                            let _ = client.send_close(&[]).await;
                            break; // break the message loop then reconnect
                        }
                    }
                } else {
                    println!("timeout");
                    continue;
                };

                match message.opcode {
                    OpCode::Text => {
                        let payload = match simdutf8::basic::from_utf8(message.payload.as_ref()) {
                            Ok(payload) => payload,
                            Err(e) => {
                                eprintln!("Reconnecting from an Error: {e:?}");
                                let _ = client.send_close(&[]).await;
                                break; // break the message loop then reconnect
                            }
                        };
                        println!("{payload}");
                    }
                    OpCode::Close => {
                        println!("{:?}", String::from_utf8_lossy(message.payload.as_ref()));
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
