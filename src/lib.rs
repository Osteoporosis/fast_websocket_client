//! # fast_websocket_client
//!
//! A fast asynchronous websocket client built on top of [fastwebsockets](https://github.com/denoland/fastwebsockets) library
//!
//! ## `use fast_websocket_client::{client, connect, OpCode};`
//!
//! That's all you need to import. Just grap a slick toolbox and go.  
//! Please read [examples/wss_client.rs](https://github.com/Osteoporosis/fast_websocket_client/blob/main/examples/wss_client.rs) or see below.
//!
//! ```
//! // try this example with
//! // $ cargo run --example wss_client
//!
//! use std::time::{Duration, Instant};
//!
//! use fast_websocket_client::{client, connect, OpCode};
//!
//! #[derive(serde::Serialize)]
//! struct Subscription {
//!     method: String,
//!     params: Vec<String>,
//!     id: u128,
//! }
//!
//! async fn subscribe(
//!     client: &mut client::Online,
//!     started_at: Instant,
//! ) -> Result<(), Box<dyn std::error::Error>> {
//!     let data = Subscription {
//!         method: "SUBSCRIBE".to_string(),
//!         params: vec!["btcusdt@bookTicker".to_string()],
//!         id: started_at.elapsed().as_nanos(),
//!     };
//!     tokio::time::timeout(Duration::from_nanos(0), client.send_json(&data)).await??;
//!     Ok(())
//! }
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let started_at = Instant::now();
//!     let url = "wss://data-stream.binance.vision:9443/ws/bakeusdt@bookTicker"; // the lowest volume
//!
//!     'reconnect_loop: loop {
//!         let future = connect(url);
//!         /*
//!             alternative code for an example
//!             make a Offline client and apply settings before `connect`
//!         */
//!         // let mut client = client::Offline::new();
//!         // client.set_max_message_size(64);
//!         // let future = client.connect(url);
//!
//!         let mut client: client::Online = match future.await {
//!             Ok(client) => {
//!                 println!("conneted");
//!                 client
//!             }
//!             Err(e) => {
//!                 eprintln!("Reconnecting from an Error: {e:?}");
//!                 tokio::time::sleep(Duration::from_secs(10)).await;
//!                 continue;
//!             }
//!         };
//!
//!         // we can modify settings while running.
//!         // without pong, this app stops in about 15 minutes.(by the API spec.)
//!         client.set_auto_pong(false);
//!
//!         // add one more subscription here, or comment out below to see the timeouts
//!         if let Err(e) = subscribe(&mut client, started_at).await {
//!             eprintln!("Reconnecting from an Error: {e:?}");
//!             let _ = client.send_close(&[]).await;
//!             tokio::time::sleep(Duration::from_secs(10)).await;
//!             continue;
//!         };
//!
//!         // message processing loop
//!         loop {
//!             let message = if let Ok(result) =
//!                 tokio::time::timeout(Duration::from_secs(1), client.receive_frame()).await
//!             {
//!                 match result {
//!                     Ok(message) => message,
//!                     Err(e) => {
//!                         eprintln!("Reconnecting from an Error: {e:?}");
//!                         let _ = client.send_close(&[]).await;
//!                         break; // break the message loop then reconnect
//!                     }
//!                 }
//!             } else {
//!                 println!("timeout");
//!                 continue;
//!             };
//!
//!             match message.opcode {
//!                 OpCode::Text => {
//!                     let payload = match String::from_utf8(message.payload.to_vec()) {
//!                         Ok(payload) => payload,
//!                         Err(e) => {
//!                             eprintln!("Reconnecting from an Error: {e:?}");
//!                             let _ = client.send_close(&[]).await;
//!                             break; // break the message loop then reconnect
//!                         }
//!                     };
//!                     println!("{payload}");
//!                 }
//!                 OpCode::Close => {
//!                     println!("{:?}", String::from_utf8_lossy(message.payload.as_ref()));
//!                     break 'reconnect_loop;
//!                 }
//!                 _ => {}
//!             }
//!         }
//!     }
//!     Ok(())
//! }
//!  ```

pub mod client;
pub use fastwebsockets::OpCode;

/// Connects to the url and returns a Online client.
pub async fn connect(url: &str) -> Result<self::client::Online, Box<dyn std::error::Error>> {
    self::client::Offline::new().connect(url).await
}

mod fragment;
mod tls_connector;
