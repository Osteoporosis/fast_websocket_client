/// A struct actually provides `connect` function. This also holds settings before `connect`.
pub struct Offline {
    vectored: bool,
    auto_close: bool,
    auto_pong: bool,
    max_message_size: usize,
    writev_threshold: usize,
    auto_apply_mask: bool,
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
        }
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

    pub async fn connect(&mut self, url: &str) -> Result<Online, Box<dyn std::error::Error>> {
        let url = url::Url::parse(url).expect("invalid url");
        let host = url.host_str().expect("invalid host").to_owned();
        let port = url.port_or_known_default().expect("the port is unknown");
        let address = format!("{host}:{port}");
        let tcp_stream = tokio::net::TcpStream::connect(&address).await?;

        let request = hyper::Request::builder()
            .method("GET")
            .uri(url.to_string())
            .header("Host", &address)
            .header(hyper::header::UPGRADE, "websocket")
            .header(hyper::header::CONNECTION, "upgrade")
            .header(
                "Sec-WebSocket-Key",
                fastwebsockets::handshake::generate_key(),
            )
            .header("Sec-WebSocket-Version", "13")
            .body(http_body_util::Empty::<hyper::body::Bytes>::new())?;

        let (mut ws, _) = match url.scheme() {
            "wss" | "https" => {
                let tls_connector = crate::tls_connector::get_tls_connector();
                let server_name = rustls_pki_types::ServerName::try_from(host).map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid server name")
                })?;
                let tls_stream = tls_connector.connect(server_name, tcp_stream).await?;
                crate::handshake::get_websocket(&SpawnExecutor, request, tls_stream).await?
            }
            _ => crate::handshake::get_websocket(&SpawnExecutor, request, tcp_stream).await?,
        };

        ws.set_writev(self.vectored);
        ws.set_writev_threshold(self.writev_threshold);
        ws.set_auto_close(self.auto_close);
        ws.set_auto_pong(self.auto_pong);
        ws.set_max_message_size(self.max_message_size);
        ws.set_auto_apply_mask(self.auto_apply_mask);

        Ok(Online(crate::fragment::FragmentCollector::new(ws)))
    }
}

/// Provides receive/send functions and configuration setters.
pub struct Online(
    crate::fragment::FragmentCollector<hyper_util::rt::tokio::TokioIo<hyper::upgrade::Upgraded>>,
);

impl Online {
    /// Sets whether to use vectored writes. This option does not guarantee that vectored writes will be always used.
    ///
    /// Default: `true`
    pub fn set_writev(&mut self, vectored: bool) -> &mut Self {
        self.0.set_writev(vectored);
        self
    }

    pub fn set_writev_threshold(&mut self, threshold: usize) -> &mut Self {
        self.0.set_writev_threshold(threshold);
        self
    }

    /// Sets whether to automatically close the connection when a close frame is received. When set to `false`, the application will have to manually send close frames.
    ///
    /// Default: `true`
    pub fn set_auto_close(&mut self, auto_close: bool) -> &mut Self {
        self.0.set_auto_close(auto_close);
        self
    }

    /// Sets whether to automatically send a pong frame when a ping frame is received.
    ///
    /// Default: `true`
    pub fn set_auto_pong(&mut self, auto_pong: bool) -> &mut Self {
        self.0.set_auto_pong(auto_pong);
        self
    }

    /// Sets the maximum message size in bytes. If a message is received that is larger than this, the connection will be closed.
    ///
    /// Default: 64 MiB
    pub fn set_max_message_size(&mut self, max_message_size: usize) -> &mut Self {
        self.0.set_max_message_size(max_message_size);
        self
    }

    /// Sets whether to automatically apply the mask to the frame payload.
    ///
    /// Default: `true`
    pub fn set_auto_apply_mask(&mut self, auto_apply_mask: bool) -> &mut Self {
        self.0.set_auto_apply_mask(auto_apply_mask);
        self
    }

    pub fn is_closed(&self) -> bool {
        self.0.is_closed()
    }

    /// Reads a frame. Text frames payload is guaranteed to be valid UTF-8.
    pub async fn receive_frame(
        &mut self,
    ) -> Result<fastwebsockets::Frame, fastwebsockets::WebSocketError> {
        self.0.read_frame().await
    }

    async fn _send_frame(
        &mut self,
        opcode: crate::OpCode,
        byte_array: &[u8],
    ) -> Result<(), fastwebsockets::WebSocketError> {
        self.0
            .write_frame(fastwebsockets::Frame::new(
                true,
                opcode,
                None,
                byte_array.into(),
            ))
            .await
    }

    /// Sends a ping frame to the stream.
    pub async fn send_ping(&mut self, data: &str) -> Result<(), fastwebsockets::WebSocketError> {
        self._send_frame(crate::OpCode::Ping, data.as_bytes()).await
    }

    /// Sends a pong frame to the stream.
    pub async fn send_pong(&mut self, data: &str) -> Result<(), fastwebsockets::WebSocketError> {
        self._send_frame(crate::OpCode::Pong, data.as_bytes()).await
    }

    /// Sends a string to the stream.
    pub async fn send_string(&mut self, data: &str) -> Result<(), fastwebsockets::WebSocketError> {
        self._send_frame(crate::OpCode::Text, data.as_bytes()).await
    }

    /// Sends a serialized json to the stream.
    pub async fn send_json(
        &mut self,
        data: impl serde::Serialize,
    ) -> Result<(), fastwebsockets::WebSocketError> {
        let json_bytes = serde_json::to_vec(&data)
            .expect("Failed to serialize data passed to send_json into JSON");
        self._send_frame(crate::OpCode::Text, json_bytes.as_slice())
            .await
    }

    /// Sends binary data to the stream.
    pub async fn send_binary(&mut self, data: &[u8]) -> Result<(), fastwebsockets::WebSocketError> {
        self._send_frame(crate::OpCode::Text, data).await
    }

    /// Sends a close frmae to the stream.
    pub async fn send_close(&mut self, data: &[u8]) -> Result<(), fastwebsockets::WebSocketError> {
        self._send_frame(crate::OpCode::Close, data).await
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
