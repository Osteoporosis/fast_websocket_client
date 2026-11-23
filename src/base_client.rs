//! Direct access to a WebSocket connection. Use [`connect`] with the defaults,
//! or configure an [`Offline`] client before connecting. Both return an [`Online`]
//! connection whose reads and writes are driven by the caller.

use crate::OpCode;
use base64::Engine;
use fastwebsockets::{FragmentCollector, Frame, WebSocket, WebSocketError, handshake};
use hyper_util::rt::{TokioExecutor, TokioIo};
use sha1::{Digest, Sha1};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub(crate) type Stream = TokioIo<hyper::upgrade::Upgraded>;
pub(crate) type Socket = WebSocket<Stream>;

/// Opens a WebSocket connection with default options.
///
/// Waits for the handshake and returns a connection ready for I/O.
/// Use [`Offline::connect`] to supply custom request headers or connection settings.
pub async fn connect(url: &str) -> Result<self::Online, Box<dyn std::error::Error + Send + Sync>> {
    self::Offline::new().connect(url).await
}

/// Configures a connection before the handshake. The same options can be reused
/// to open another connection with [`Offline::connect`].
///
/// ```no_run
/// use fast_websocket_client::{OpCode, base_client::Offline};
///
/// #[tokio::main(flavor = "current_thread")]
/// async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
///     let mut options = Offline::new();
///     options
///         .add_header("Authorization", "Bearer mytoken")
///         .set_max_message_size(1024 * 1024);
///
///     let mut socket = options.connect("wss://example.com/events").await?;
///     loop {
///         let frame = socket.receive_frame().await?;
///         match frame.opcode {
///             OpCode::Text => println!("{}", String::from_utf8_lossy(&frame.payload)),
///             OpCode::Close => break,
///             _ => {}
///         }
///     }
///     Ok(())
/// }
/// ```
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

    /// Sets a handshake request header, replacing any previous value for that name.
    /// Headers that the client generates, such as `Host` and `Sec-WebSocket-Key`,
    /// are rejected by [`Self::connect`].
    ///
    /// # Panics
    ///
    /// Panics if the name or value is invalid.
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

    #[cfg(feature = "callback_client")]
    pub(crate) fn set_headers(&mut self, headers: crate::HeaderMap) {
        self.custom_headers = headers;
    }

    /// Allows the frame header and payload to be written together in separate buffers.
    /// Enabled by default. Configure the payload cutoff with [`Self::set_writev_threshold`].
    pub fn set_writev(&mut self, vectored: bool) -> &mut Self {
        self.vectored = vectored;
        self
    }

    /// Sets the payload size above which vectored writes can be used. Defaults to 1024 bytes.
    pub fn set_writev_threshold(&mut self, threshold: usize) -> &mut Self {
        self.writev_threshold = threshold;
        self
    }

    /// Replies to a peer's Close frame while reading it. Enabled by default.
    /// When disabled, the caller can reply with [`Online::send_close`].
    pub fn set_auto_close(&mut self, auto_close: bool) -> &mut Self {
        self.auto_close = auto_close;
        self
    }

    /// Answers incoming Pings with Pongs while reading. Enabled by default.
    /// Disable this to receive Ping frames from [`Online::receive_frame`].
    pub fn set_auto_pong(&mut self, auto_pong: bool) -> &mut Self {
        self.auto_pong = auto_pong;
        self
    }

    /// Sets the maximum size in bytes of a text or binary message returned to the caller.
    /// Defaults to 64 MiB.
    pub fn set_max_message_size(&mut self, max_message_size: usize) -> &mut Self {
        self.max_message_size = max_message_size;
        self
    }

    /// Masks outgoing client frames as required by the WebSocket protocol.
    /// Enabled by default.
    pub fn set_auto_apply_mask(&mut self, auto_apply_mask: bool) -> &mut Self {
        self.auto_apply_mask = auto_apply_mask;
        self
    }

    #[cfg(feature = "proxy")]
    /// Sets the proxy for this connection.
    pub fn set_proxy(&mut self, proxy: Option<crate::proxy::Proxy>) -> &mut Self {
        self.proxy = proxy;
        self
    }

    /// Opens a WebSocket connection with these options and waits for the handshake.
    ///
    /// # Errors
    ///
    /// Returns an error if the URL or headers are invalid, the transport fails
    /// to connect, or the peer rejects the handshake.
    pub async fn connect(
        &mut self,
        url: &str,
    ) -> Result<Online, Box<dyn std::error::Error + Send + Sync>> {
        let socket = self.connect_socket(url).await?;
        Ok(Online::new(socket, self.max_message_size))
    }

    pub(crate) async fn connect_socket(&self, url: &str) -> Result<Socket, BoxError> {
        self.open_socket(url, crate::tls_connector::get_tls_connector)
            .await
    }

    #[cfg(all(test, feature = "proxy", any(feature = "ring", feature = "aws_lc_rs")))]
    pub(crate) async fn connect_with_tls_connector(
        &self,
        url: &str,
        tls_connector: impl Fn() -> Result<tokio_rustls::TlsConnector, BoxError> + Sync,
    ) -> Result<Online, BoxError> {
        let socket = self.open_socket(url, tls_connector).await?;
        Ok(Online::new(socket, self.max_message_size))
    }

    async fn open_socket(
        &self,
        url: &str,
        tls_connector: impl Fn() -> Result<tokio_rustls::TlsConnector, BoxError> + Sync,
    ) -> Result<Socket, BoxError> {
        validate_headers(&self.custom_headers)?;
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
                handshake::client(&TokioExecutor::new(), request, tls_stream).await?
            }
            _ => handshake::client(&TokioExecutor::new(), request, stream).await?,
        };

        validate_response(response.headers(), &expected_accept, &self.custom_headers)?;

        ws.set_writev(self.vectored);
        ws.set_writev_threshold(self.writev_threshold);
        ws.set_auto_close(self.auto_close);
        ws.set_auto_pong(self.auto_pong);
        // The frame limit is exclusive and must allow 125-byte control frames.
        ws.set_max_message_size(self.max_message_size.max(125).saturating_add(1));
        ws.set_auto_apply_mask(self.auto_apply_mask);
        Ok(ws)
    }
}

fn validate_response(
    headers: &crate::HeaderMap,
    expected_accept: &str,
    request_headers: &crate::HeaderMap,
) -> Result<(), BoxError> {
    if headers.get_all(hyper::header::UPGRADE).iter().count() != 1 {
        return Err(WebSocketError::InvalidUpgradeHeader.into());
    }
    for value in headers.get_all(hyper::header::CONNECTION) {
        if !value
            .to_str()
            .map_err(|_| WebSocketError::InvalidConnectionHeader)?
            .split(',')
            .map(str::trim)
            .all(|token| token.is_empty() || is_token(token))
        {
            return Err(WebSocketError::InvalidConnectionHeader.into());
        }
    }
    let mut accept = headers.get_all("sec-websocket-accept").iter();
    if accept.next().and_then(|v| v.to_str().ok()) != Some(expected_accept)
        || accept.next().is_some()
    {
        return Err("invalid Sec-WebSocket-Accept".into());
    }
    if headers.contains_key("sec-websocket-extensions") {
        return Err("unsupported WebSocket extension".into());
    }
    let mut selected = headers.get_all("sec-websocket-protocol").iter();
    if let Some(protocol) = selected.next() {
        let protocol = protocol.to_str()?;
        let offered = request_headers.get_all("sec-websocket-protocol");
        if selected.next().is_some()
            || !offered
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|v| v.split(',').map(str::trim))
                .any(|token| token == protocol)
        {
            return Err("unsolicited WebSocket subprotocol".into());
        }
    }
    Ok(())
}

// HTTP field names and WebSocket subprotocols share the HTTP token grammar.
fn is_token(token: &str) -> bool {
    hyper::header::HeaderName::from_bytes(token.as_bytes()).is_ok()
}

pub(crate) fn validate_headers(headers: &crate::HeaderMap) -> Result<(), BoxError> {
    for name in headers.keys() {
        if matches!(
            name.as_str(),
            "host"
                | "upgrade"
                | "connection"
                | "sec-websocket-key"
                | "sec-websocket-version"
                | "sec-websocket-extensions"
        ) {
            return Err(format!("reserved WebSocket handshake header: {name}").into());
        }
    }
    let mut protocols = std::collections::HashSet::new();
    for value in headers.get_all("sec-websocket-protocol") {
        for protocol in value.to_str()?.split(',').map(str::trim) {
            if !is_token(protocol) || !protocols.insert(protocol) {
                return Err("expected unique WebSocket subprotocol tokens".into());
            }
        }
    }
    Ok(())
}

pub(crate) fn check_message_size(frame: &Frame<'_>, limit: usize) -> Result<(), WebSocketError> {
    match frame.opcode {
        OpCode::Text | OpCode::Binary if frame.payload.len() > limit => {
            Err(WebSocketError::FrameTooLarge)
        }
        OpCode::Close | OpCode::Ping | OpCode::Pong if frame.payload.len() > 125 => {
            Err(WebSocketError::InvalidValue)
        }
        _ => Ok(()),
    }
}

pub(crate) fn parse_url(url: &str) -> Result<url::Url, Box<dyn std::error::Error + Send + Sync>> {
    let url = url::Url::parse(url)?;
    if !matches!(url.scheme(), "ws" | "wss" | "http" | "https")
        || url.host().is_none()
        || url.fragment().is_some()
    {
        return Err("expected a ws/wss or http/https URL with a host and no fragment".into());
    }
    Ok(url)
}

/// An established WebSocket connection for sending messages and receiving frames.
/// [`Self::receive_frame`] returns complete text and binary messages, including
/// messages the peer split across multiple frames. To reconnect, call [`Offline::connect`] again.
pub struct Online {
    socket: FragmentCollector<Stream>,
    max_message_size: usize,
}

impl Online {
    fn new(socket: Socket, max_message_size: usize) -> Self {
        Self {
            socket: FragmentCollector::new(socket),
            max_message_size,
        }
    }

    /// Receives the next complete message or control frame.
    ///
    /// By default, this read answers incoming Pings and continues waiting. A peer's
    /// Close is acknowledged and returned to the caller.
    ///
    /// For periodic idle notifications, pin the receive future once and pass
    /// `&mut receive` to each `tokio::time::timeout`. The pending read may
    /// already have consumed part of a frame.
    #[inline]
    pub async fn receive_frame(
        &mut self,
    ) -> Result<Frame<'_>, Box<dyn std::error::Error + Send + Sync>> {
        let frame = self.socket.read_frame().await?;
        check_message_size(&frame, self.max_message_size)?;
        Ok(frame)
    }

    #[inline]
    async fn _send_frame(&mut self, frame: Frame<'_>) -> Result<(), WebSocketError> {
        if matches!(frame.opcode, OpCode::Close | OpCode::Ping | OpCode::Pong)
            && frame.payload.len() > 125
        {
            return Err(WebSocketError::InvalidValue);
        }
        self.socket.write_frame(frame).await
    }

    /// Sends a Ping with a UTF-8 payload of up to 125 bytes.
    #[inline]
    pub async fn send_ping(
        &mut self,
        data: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self._send_frame(fastwebsockets::Frame::new(
            true,
            OpCode::Ping,
            None,
            data.as_bytes().into(),
        ))
        .await?;
        Ok(())
    }

    /// Sends a Pong with a UTF-8 payload of up to 125 bytes.
    #[inline]
    pub async fn send_pong(
        &mut self,
        data: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self._send_frame(fastwebsockets::Frame::pong(data.as_bytes().into()))
            .await?;
        Ok(())
    }

    /// Sends a UTF-8 text frame.
    #[inline]
    pub async fn send_string(
        &mut self,
        data: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self._send_frame(fastwebsockets::Frame::text(data.as_bytes().into()))
            .await?;
        Ok(())
    }

    /// Serializes `data` to JSON and sends it as a text frame.
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

    /// Sends a binary frame.
    #[inline]
    pub async fn send_binary(
        &mut self,
        data: &[u8],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self._send_frame(fastwebsockets::Frame::binary(data.into()))
            .await?;
        Ok(())
    }

    /// Sends a normal Close frame (code 1000) with a reason of up to 123 UTF-8 bytes.
    /// Pass an empty string to omit the reason. To wait for the peer's Close,
    /// continue reading with [`Self::receive_frame`].
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
