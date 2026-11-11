use base64::Engine;
use fastwebsockets::{Role, WebSocket, WebSocketError};
use hyper_util::rt::TokioIo;
use sha1::{Digest, Sha1};

use crate::connection::{Reader, Writer};

/// Convenience helper: connect to `url` and return an [`Online`] client.
///
/// This is equivalent to:
/// ```no_run
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
///     let mut offline = fast_websocket_client::base_client::Offline::new();
///     let online = offline.connect("wss://echo.websocket.org").await?;
///     let _ = online;
///     Ok(())
/// }
/// ```
pub async fn connect(url: &str) -> Result<self::Online, Box<dyn std::error::Error + Send + Sync>> {
    self::Offline::new().connect(url).await
}

/// Builder-like struct holding connection options **before** the handshake.
///
/// Use [`Offline::new`] and its `set_*` / `add_header` methods, then call
/// [`Offline::connect`] to obtain an [`Online`] WebSocket.
pub struct Offline {
    vectored: bool,
    auto_close: bool,
    auto_pong: bool,
    max_message_size: usize,
    writev_threshold: usize,
    auto_apply_mask: bool,
    custom_headers: crate::HeaderMap,
    #[cfg(feature = "proxy")]
    proxy: Option<crate::proxy::Proxy>,
}

impl Default for Offline {
    fn default() -> Self {
        Self::new()
    }
}

impl Offline {
    #[must_use]
    pub fn new() -> Self {
        Self {
            vectored: true,
            auto_close: true,
            auto_pong: true,
            max_message_size: 64 << 20,
            writev_threshold: 1024,
            auto_apply_mask: true,
            custom_headers: crate::HeaderMap::new(),
            #[cfg(feature = "proxy")]
            proxy: None,
        }
    }

    /// Adds a custom HTTP header to be included in the WebSocket handshake request.
    ///
    /// ```
    /// use fast_websocket_client::base_client::Offline;
    ///
    /// let mut client = Offline::new();
    /// client
    ///     .add_header("Authorization", "Bearer mytoken")
    ///     .add_header("X-Custom-Header", "custom-value");
    /// ```
    pub fn add_header<K, V>(&mut self, key: K, value: V) -> &mut Self
    where
        K: std::convert::TryInto<hyper::header::HeaderName>,
        K::Error: std::fmt::Debug,
        V: std::convert::TryInto<hyper::header::HeaderValue>,
        V::Error: std::fmt::Debug,
    {
        let name = key.try_into().expect("invalid header name");
        let val = value.try_into().expect("invalid header value");
        self.custom_headers.insert(name, val);
        self
    }

    /// Sets whether to use vectored writes. This option does not guarantee that vectored writes will be always used.
    ///
    /// Default: `true`
    pub fn set_writev(&mut self, vectored: bool) -> &mut Self {
        self.vectored = vectored;
        self
    }

    pub fn set_writev_threshold(&mut self, threshold: usize) -> &mut Self {
        self.writev_threshold = threshold;
        self
    }

    /// Sets whether to automatically close the connection when a close frame is received. When set to `false`, the application will have to manually send close frames.
    ///
    /// Default: `true`
    pub fn set_auto_close(&mut self, auto_close: bool) -> &mut Self {
        self.auto_close = auto_close;
        self
    }

    /// Sets whether to automatically send a pong frame when a ping frame is received.
    ///
    /// Default: `true`
    pub fn set_auto_pong(&mut self, auto_pong: bool) -> &mut Self {
        self.auto_pong = auto_pong;
        self
    }

    /// Sets the maximum complete message size in bytes, including all fragments.
    /// A larger message returns an error; discard the connection after that error.
    ///
    /// Default: 64 MiB
    pub fn set_max_message_size(&mut self, max_message_size: usize) -> &mut Self {
        self.max_message_size = max_message_size;
        self
    }

    /// Sets whether to automatically apply the mask to the frame payload.
    ///
    /// Default: `true`
    pub fn set_auto_apply_mask(&mut self, auto_apply_mask: bool) -> &mut Self {
        self.auto_apply_mask = auto_apply_mask;
        self
    }

    #[cfg(feature = "proxy")]
    /// Sets a proxy to be used when establishing a connection.
    ///
    /// # Arguments
    ///
    /// * `proxy` - The proxy to be used.
    pub fn set_proxy(&mut self, proxy: Option<crate::proxy::Proxy>) -> &mut Self {
        self.proxy = proxy;
        self
    }

    /// Perform the TCP/TLS/WebSocket handshake with the stored options.
    ///
    /// * Respects the `proxy` feature and custom headers.  
    /// * Applies `fastwebsockets` tuning parameters immediately after
    ///   the handshake.
    ///
    /// # Errors
    ///
    /// * URL parse / DNS / TCP / TLS failures  
    /// * WebSocket handshake errors
    pub async fn connect(
        &mut self,
        url: &str,
    ) -> Result<Online, Box<dyn std::error::Error + Send + Sync>> {
        self.connect_with_tls_connector(url, crate::tls_connector::get_tls_connector)
            .await
    }

    pub(crate) async fn connect_with_tls_connector<F>(
        &mut self,
        url: &str,
        tls_connector: F,
    ) -> Result<Online, Box<dyn std::error::Error + Send + Sync>>
    where
        F: Fn() -> Result<tokio_rustls::TlsConnector, Box<dyn std::error::Error + Send + Sync>>
            + Sync,
    {
        let url = parse_url(url)?;
        let host = match url.host().unwrap() {
            url::Host::Domain(host) => host.to_owned(),
            url::Host::Ipv4(ip) => ip.to_string(),
            url::Host::Ipv6(ip) => ip.to_string(),
        };
        let port = url.port_or_known_default().unwrap();
        let authority = &url[url::Position::BeforeHost..url::Position::AfterPort];
        let key = fastwebsockets::handshake::generate_key();
        let expected_accept = base64::engine::general_purpose::STANDARD.encode(Sha1::digest(
            format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
        ));

        let mut req_builder = hyper::Request::builder()
            .method("GET")
            .uri(&url[url::Position::BeforePath..url::Position::AfterQuery])
            .header("Host", authority)
            .header(hyper::header::UPGRADE, "websocket")
            .header(hyper::header::CONNECTION, "upgrade")
            .header("Sec-WebSocket-Key", key)
            .header("Sec-WebSocket-Version", "13");

        for (key, value) in self.custom_headers.iter() {
            if matches!(
                key.as_str(),
                "host"
                    | "upgrade"
                    | "connection"
                    | "sec-websocket-key"
                    | "sec-websocket-version"
                    | "sec-websocket-extensions"
            ) {
                return Err(format!("reserved WebSocket handshake header: {key}").into());
            }
            req_builder = req_builder.header(key, value);
        }

        let request = req_builder.body(http_body_util::Empty::<hyper::body::Bytes>::new())?;

        #[cfg(feature = "proxy")]
        let stream = if let Some(proxy) = &self.proxy {
            proxy.tunnel(&host, port, &tls_connector).await?
        } else {
            crate::proxy::ProxyStream::Plain(
                tokio::net::TcpStream::connect((host.as_str(), port)).await?,
            )
        };
        #[cfg(not(feature = "proxy"))]
        let stream = tokio::net::TcpStream::connect((host.as_str(), port)).await?;

        let (mut ws, response) = match url.scheme() {
            "wss" | "https" => {
                let tls_connector = tls_connector()?;
                let server_name = rustls_pki_types::ServerName::try_from(host).map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid server name")
                })?;
                let tls_stream = tls_connector.connect(server_name, stream).await?;
                handshake(request, tls_stream).await?
            }
            _ => handshake(request, stream).await?,
        };

        let accept = response.headers().get_all("sec-websocket-accept");
        if accept.iter().count() != 1
            || accept
                .iter()
                .next()
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                != Some(expected_accept.as_str())
        {
            return Err("invalid Sec-WebSocket-Accept".into());
        }
        if response.headers().contains_key("sec-websocket-extensions") {
            return Err("unsupported WebSocket extension".into());
        }
        let protocols = response.headers().get_all("sec-websocket-protocol");
        if let Some(protocol) = protocols.iter().next() {
            let selected = protocol.to_str()?;
            let offered = self.custom_headers.get_all("sec-websocket-protocol");
            if protocols.iter().count() != 1
                || selected.is_empty()
                || !offered
                    .iter()
                    .filter_map(|v| v.to_str().ok())
                    .flat_map(|v| v.split(',').map(str::trim))
                    .any(|v| v == selected)
            {
                return Err("unsolicited WebSocket subprotocol".into());
            }
        }

        ws.set_writev(self.vectored);
        ws.set_writev_threshold(self.writev_threshold);
        ws.set_auto_close(false);
        ws.set_auto_pong(false);
        // The backend limit is exclusive and applies to individual frames.
        // Control frames remain valid even with a smaller application limit.
        ws.set_max_message_size(self.max_message_size.max(125).saturating_add(1));
        ws.set_auto_apply_mask(self.auto_apply_mask);

        let (reader, writer) = ws.split(tokio::io::split);
        Ok(Online {
            reader: Reader::new(reader, self.max_message_size),
            writer: Writer::new(writer, self.auto_close, self.auto_pong),
        })
    }
}

async fn handshake<S>(
    request: hyper::Request<http_body_util::Empty<hyper::body::Bytes>>,
    stream: S,
) -> Result<
    (
        WebSocket<TokioIo<hyper::upgrade::Upgraded>>,
        hyper::Response<hyper::body::Incoming>,
    ),
    WebSocketError,
>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(async move {
        let _ = connection.with_upgrades().await;
    });
    let mut response = sender.send_request(request).await?;
    if response.status() != hyper::StatusCode::SWITCHING_PROTOCOLS {
        return Err(WebSocketError::InvalidStatusCode(
            response.status().as_u16(),
        ));
    }
    if !response
        .headers()
        .get(hyper::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
    {
        return Err(WebSocketError::InvalidUpgradeHeader);
    }

    let mut upgrade = false;
    for value in response.headers().get_all(hyper::header::CONNECTION) {
        let value = value
            .to_str()
            .map_err(|_| WebSocketError::InvalidConnectionHeader)?;
        for token in value
            .split(',')
            .map(|token| token.trim_matches([' ', '\t']))
        {
            if token.is_empty() {
                continue;
            }
            if !token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
            {
                return Err(WebSocketError::InvalidConnectionHeader);
            }
            upgrade |= token.eq_ignore_ascii_case("upgrade");
        }
    }
    if !upgrade {
        return Err(WebSocketError::InvalidConnectionHeader);
    }

    let upgraded = hyper::upgrade::on(&mut response).await?;
    Ok((
        WebSocket::after_handshake(TokioIo::new(upgraded), Role::Client),
        response,
    ))
}

pub(crate) fn parse_url(url: &str) -> Result<url::Url, Box<dyn std::error::Error + Send + Sync>> {
    let url = url::Url::parse(url)?;
    if !matches!(url.scheme(), "ws" | "wss" | "http" | "https")
        || url.host().is_none()
        || url.fragment().is_some()
    {
        return Err("expected a ws/wss URL with a host and no fragment".into());
    }
    Ok(url)
}

/// Provides receive/send functions for an established connection.
///
/// If a send or receive future is cancelled, discard the connection. Restarting
/// I/O after cancellation can leave a partially consumed or written frame.
pub struct Online {
    pub(crate) reader: Reader,
    pub(crate) writer: Writer,
}

impl Online {
    /// Receive the next WebSocket frame from the server.
    ///
    /// Fragmented messages are assembled within the configured size limit,
    /// and text payloads are validated as UTF-8.
    ///
    /// # Cancellation
    /// If this future is cancelled, discard the connection; do not restart a
    /// receive on it. The high-level client preserves its pending read instead.
    #[inline]
    pub async fn receive_frame(
        &mut self,
    ) -> Result<fastwebsockets::Frame<'_>, Box<dyn std::error::Error + Send + Sync>> {
        loop {
            let frame = self.reader.read().await?;
            if frame.opcode == crate::OpCode::Ping && self.writer.auto_pong {
                self.writer.respond(&frame).await?;
                continue;
            }
            if self.writer.is_closed() && frame.opcode != crate::OpCode::Close {
                return Err(fastwebsockets::WebSocketError::ConnectionClosed.into());
            }
            self.writer.respond(&frame).await?;
            return Ok(frame);
        }
    }

    #[inline]
    async fn _send_frame(
        &mut self,
        frame: fastwebsockets::Frame<'_>,
    ) -> Result<(), fastwebsockets::WebSocketError> {
        self.writer.write(frame).await
    }

    /// Sends a ping frame to the stream.
    #[inline]
    pub async fn send_ping(
        &mut self,
        data: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self._send_frame(fastwebsockets::Frame::new(
            true,
            crate::OpCode::Ping,
            None,
            data.as_bytes().into(),
        ))
        .await?;
        Ok(())
    }

    /// Sends a pong frame to the stream.
    #[inline]
    pub async fn send_pong(
        &mut self,
        data: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self._send_frame(fastwebsockets::Frame::pong(data.as_bytes().into()))
            .await?;
        Ok(())
    }

    /// Send a UTF-8 text frame.
    ///
    /// # Errors
    /// Propagates underlying I/O or protocol errors from `fastwebsockets`.
    #[inline]
    pub async fn send_string(
        &mut self,
        data: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self._send_frame(fastwebsockets::Frame::text(data.as_bytes().into()))
            .await?;
        Ok(())
    }

    /// Serialize `data` to JSON and send as a text frame.
    #[inline]
    pub async fn send_json(
        &mut self,
        data: impl serde::Serialize,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let json_bytes = serde_json::to_vec(&data)?;
        self._send_frame(fastwebsockets::Frame::text(json_bytes.into()))
            .await?;
        Ok(())
    }

    /// Send binary payload as an **unmasked** binary frame (masking is
    /// handled automatically when `auto_apply_mask` is true).
    #[inline]
    pub async fn send_binary(
        &mut self,
        data: &[u8],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self._send_frame(fastwebsockets::Frame::binary(data.into()))
            .await?;
        Ok(())
    }

    /// Transmit a WebSocket Close frame with [`fastwebsockets::CloseCode::Normal`].
    ///
    /// Pass an empty string to omit the reason.
    pub async fn send_close(
        &mut self,
        data: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self._send_frame(fastwebsockets::Frame::close(
            fastwebsockets::CloseCode::Normal.into(),
            data.as_bytes(),
        ))
        .await?;
        Ok(())
    }
}
