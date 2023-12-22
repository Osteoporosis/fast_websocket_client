// Copyright 2023 Hyoungjun Son
// Copyright 2023 Divy Srivastava <dj.srivastava23@gmail.com>
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
// from fastwebsockets 0.6.0

use hyper::body::Incoming;
use hyper::upgrade::Upgraded;
use hyper::Request;
use hyper::Response;
use hyper::StatusCode;

use hyper_util::rt::TokioIo;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;

use std::future::Future;
use std::pin::Pin;

use crate::websocket::WebSocket;
use fastwebsockets::Role;
use fastwebsockets::WebSocketError;

/// Perform the client handshake.
///
/// This function is used to perform the client handshake. It takes a hyper
/// executor, a `hyper::Request` and a stream.
pub async fn get_websocket<S, E, B>(
    executor: &E,
    request: Request<B>,
    socket: S,
) -> Result<(WebSocket<TokioIo<Upgraded>>, Response<Incoming>), WebSocketError>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    E: hyper::rt::Executor<Pin<Box<dyn Future<Output = ()> + Send>>>,
    B: hyper::body::Body + 'static + Send,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(socket)).await?;
    let fut = Box::pin(async move {
        if let Err(e) = conn.with_upgrades().await {
            eprintln!("Error polling connection: {}", e);
        }
    });
    executor.execute(fut);

    let mut response = sender.send_request(request).await?;
    verify(&response)?;

    match hyper::upgrade::on(&mut response).await {
        Ok(upgraded) => Ok((
            WebSocket::after_handshake(TokioIo::new(upgraded), Role::Client),
            response,
        )),
        Err(e) => Err(e.into()),
    }
}

// https://github.com/snapview/tungstenite-rs/blob/314feea3055a93e585882fb769854a912a7e6dae/src/handshake/client.rs#L189
fn verify(response: &Response<Incoming>) -> Result<(), WebSocketError> {
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        return Err(WebSocketError::InvalidStatusCode(
            response.status().as_u16(),
        ));
    }

    let headers = response.headers();

    if !headers
        .get("Upgrade")
        .and_then(|h| h.to_str().ok())
        .map(|h| h.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false)
    {
        return Err(WebSocketError::InvalidUpgradeHeader);
    }

    if !headers
        .get("Connection")
        .and_then(|h| h.to_str().ok())
        .map(|h| h.eq_ignore_ascii_case("Upgrade"))
        .unwrap_or(false)
    {
        return Err(WebSocketError::InvalidConnectionHeader);
    }

    Ok(())
}
