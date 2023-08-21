//! # `fast_websocket_client`
//!
//! A fast asynchronous websocket client built on top of `fastwebsockets` library
//!
//! ## `use fast_websocket_client::{client, connect, OpCode};`
//!
//! Just grap a slick toolbox and go.  
//! Please read `examples/wss_client.rs`

pub mod client;
pub use fastwebsockets::OpCode;

/// Connects to the url and returns a Online client.
pub async fn connect(url: &str) -> Result<self::client::Online, Box<dyn std::error::Error>> {
    self::client::Offline::new().connect(url).await
}

mod fragment;
mod tls_connector;
