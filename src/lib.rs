#![cfg_attr(any(not(doctest), feature = "callback_client"), doc = include_str!("../README.md"))]

pub mod base_client;
pub use base_client::connect;

#[cfg(feature = "callback_client")]
pub mod client;
#[cfg(feature = "callback_client")]
pub use client as callback_client;
#[cfg(feature = "callback_client")]
pub use client::{
    ClientConfig, CloseEvent, ConnectionInitOptions, WebSocket, WebSocketBuilder,
    WebSocketClientError,
};

#[cfg(feature = "proxy")]
pub mod proxy;

pub use fastwebsockets::OpCode;
pub use hyper::HeaderMap;

mod tls_connector;

#[cfg(all(test, feature = "proxy", any(feature = "aws_lc_rs", feature = "ring")))]
mod tls_proxy_tests;
