/// Convenience helper: connect to `url` and return an [`Online`] client.
///
/// This is equivalent to:
/// ```
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

    /// Sets the maximum message size in bytes. If a message is received that is larger than this, the connection will be closed.
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
        let url = url::Url::parse(url).expect("invalid url");
        let host = url.host_str().expect("invalid host").to_owned();
        let port = url.port_or_known_default().expect("the port is unknown");
        let address = format!("{host}:{port}");

        #[cfg(feature = "proxy")]
        let tcp_stream = if let Some(proxy) = &self.proxy {
            proxy.tunnel(&host, port).await?
        } else {
            tokio::net::TcpStream::connect(&address).await?
        };
        #[cfg(not(feature = "proxy"))]
        let tcp_stream = tokio::net::TcpStream::connect(&address).await?;

        let mut req_builder = hyper::Request::builder()
            .method("GET")
            .uri(url.to_string())
            .header("Host", &address)
            .header(hyper::header::UPGRADE, "websocket")
            .header(hyper::header::CONNECTION, "upgrade")
            .header(
                "Sec-WebSocket-Key",
                fastwebsockets::handshake::generate_key(),
            )
            .header("Sec-WebSocket-Version", "13");

        for (key, value) in self.custom_headers.iter() {
            req_builder = req_builder.header(key, value);
        }

        let request = req_builder.body(http_body_util::Empty::<hyper::body::Bytes>::new())?;

        let (mut ws, _) = match url.scheme() {
            "wss" | "https" => {
                let tls_connector = crate::tls_connector::get_tls_connector();
                let server_name = rustls_pki_types::ServerName::try_from(host).map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid server name")
                })?;
                let tls_stream = tls_connector.connect(server_name, tcp_stream).await?;
                fastwebsockets::handshake::client(&SpawnExecutor, request, tls_stream).await?
            }
            _ => fastwebsockets::handshake::client(&SpawnExecutor, request, tcp_stream).await?,
        };

        ws.set_writev(self.vectored);
        ws.set_writev_threshold(self.writev_threshold);
        ws.set_auto_close(self.auto_close);
        ws.set_auto_pong(self.auto_pong);
        ws.set_max_message_size(self.max_message_size);
        ws.set_auto_apply_mask(self.auto_apply_mask);

        Ok(Online(fastwebsockets::FragmentCollector::new(ws)))
    }
}

/// Provides receive/send functions and configuration setters.
pub struct Online(
    fastwebsockets::FragmentCollector<hyper_util::rt::tokio::TokioIo<hyper::upgrade::Upgraded>>,
);

impl Online {
    /// Receive the next WebSocket frame from the server.
    ///
    /// *Text frame payloads are validated as UTF-8* by
    /// `fastwebsockets::FragmentCollector`.
    #[inline]
    pub async fn receive_frame(
        &mut self,
    ) -> Result<fastwebsockets::Frame, Box<dyn std::error::Error + Send + Sync>> {
        Ok(self.0.read_frame().await?)
    }

    #[inline]
    async fn _send_frame(
        &mut self,
        frame: fastwebsockets::Frame<'_>,
    ) -> Result<(), fastwebsockets::WebSocketError> {
        self.0.write_frame(frame).await
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
        let json_bytes = serde_json::to_vec(&data)
            .expect("Failed to serialize data passed to send_json into JSON");
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

struct SpawnExecutor;

impl<Fut> hyper::rt::Executor<Fut> for SpawnExecutor
where
    Fut: std::future::Future + Send + 'static,
    Fut::Output: Send + 'static,
{
    fn execute(&self, future: Fut) {
        tokio::task::spawn(future);
    }
}
