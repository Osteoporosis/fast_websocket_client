//! JavaScript-like WebSocket callbacks with automatic reconnection.

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use fastwebsockets::{CloseCode, FragmentCollectorRead, Frame, WebSocketError, WebSocketWrite};
use futures_util::{Stream, TryStreamExt, stream};
use thiserror::Error;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time;

use crate::HeaderMap;
use crate::OpCode;
use crate::base_client;
use crate::proxy::Proxy;

trait AsyncFnMut<T>: Send {
    fn call_mut(&mut self, arg: T) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

impl<T, F, Fut> AsyncFnMut<T> for F
where
    F: FnMut(T) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    fn call_mut(&mut self, arg: T) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin((self)(arg))
    }
}

type Callback<T> = Box<dyn AsyncFnMut<T>>;
type VoidCallback = Box<dyn AsyncFnMut<()>>;

/// Errors returned by the high-level [`WebSocket`] API.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum WebSocketClientError {
    #[error("Invalid WebSocket URL: {0}")]
    InvalidUrl(String),
    #[error("Invalid client configuration: {0}")]
    InvalidConfig(String),
    #[error("WebSocket task failed: {0}")]
    TaskError(String),
    /// Raised when the WebSocket connection fails.
    #[error("WebSocket connection failed: {0}")]
    ConnectionError(String),

    /// Raised when the WebSocket connection times out.
    #[error("WebSocket connection timed out after {0:?}")]
    ConnectionTimeout(Duration),

    /// Raised when sending a message fails.
    #[error("Send failed: {0}")]
    SendError(String),

    /// Raised when receiving a message fails.
    #[error("Receive failed: {0}")]
    ReceiveError(String),

    /// Raised when no connection is established.
    #[error("Not connected")]
    NotConnected,
}

/// Details of a closed WebSocket connection.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CloseEvent {
    /// The peer's close code, 1005 if omitted, or 1006 if no valid Close frame was received.
    pub code: u16,
    /// The peer's close reason, or an empty string if none was received.
    pub reason: String,
    /// Whether both peers exchanged Close frames successfully.
    pub was_clean: bool,
}

#[derive(Default)]
struct CallbackSet {
    on_open: Option<VoidCallback>,
    on_close: Option<Callback<CloseEvent>>,
    on_error: Option<Callback<String>>,
    on_message: Option<Callback<String>>,
    on_binary: Option<Callback<Vec<u8>>>,
}

impl CallbackSet {
    pub async fn call_on_open(&mut self) {
        if let Some(cb) = &mut self.on_open {
            cb.call_mut(()).await;
        }
    }
    pub async fn call_on_message(&mut self, message: String) {
        if let Some(cb) = &mut self.on_message {
            cb.call_mut(message).await;
        }
    }
    pub async fn call_on_binary(&mut self, message: Vec<u8>) {
        if let Some(cb) = &mut self.on_binary {
            cb.call_mut(message).await;
        }
    }
    pub async fn call_on_error(&mut self, message: String) {
        if let Some(cb) = &mut self.on_error {
            cb.call_mut(message).await;
        }
    }
    pub async fn call_on_close(&mut self, event: CloseEvent) {
        if let Some(cb) = &mut self.on_close {
            cb.call_mut(event).await;
        }
    }
}

enum CallbackUpdate {
    Open(VoidCallback),
    Close(Callback<CloseEvent>),
    Error(Callback<String>),
    Message(Callback<String>),
    Binary(Callback<Vec<u8>>),
}

/// Ping, reconnect, and timeout settings for the callback client.
///
/// | Setting | Default | Applies to |
/// | --- | --- | --- |
/// | [Ping interval](Self::with_ping_interval) | 30 s | Heartbeats on an open connection |
/// | [Reconnect delay](Self::with_reconnect_delay) | 10 s | The wait after a failed connection attempt |
/// | [Connect timeout](Self::with_connect_timeout) | 10 s | Establishing one connection |
/// | [Close timeout](Self::with_close_timeout) | 5 s | Completing a locally requested closing handshake |
pub struct ClientConfig {
    ping_interval: Duration,
    reconnect_delay: Duration,
    connect_timeout: Duration,
    close_timeout: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            ping_interval: Duration::from_secs(30),
            reconnect_delay: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(10),
            close_timeout: Duration::from_secs(5),
        }
    }
}

impl ClientConfig {
    fn validate(&self) -> Result<(), WebSocketClientError> {
        if self.ping_interval.is_zero() {
            return Err(WebSocketClientError::InvalidConfig(
                "ping_interval must be nonzero".into(),
            ));
        }
        if time::Instant::now()
            .checked_add(self.ping_interval)
            .is_none()
        {
            return Err(WebSocketClientError::InvalidConfig(
                "ping_interval exceeds the supported clock range".into(),
            ));
        }
        Ok(())
    }

    /// Maximum time to finish a locally requested close handshake (default: 5 seconds).
    pub fn with_close_timeout(mut self, timeout: Duration) -> Self {
        self.close_timeout = timeout;
        self
    }

    /// Creates the default client configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the ping interval duration.
    /// The interval must be nonzero and fit the platform clock.
    ///
    /// **Default**: 30 seconds
    pub fn with_ping_interval(mut self, interval: Duration) -> Self {
        self.ping_interval = interval;
        self
    }

    /// Sets the delay after a failed connection attempt.
    ///
    /// **Default**: 10 seconds
    pub fn with_reconnect_delay(mut self, delay: Duration) -> Self {
        self.reconnect_delay = delay;
        self
    }

    /// Sets the connect timeout duration.
    ///
    /// **Default**: 10 seconds
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }
}

/// Options for opening a connection, including handshake headers and the proxy.
/// Use [`WebSocketBuilder::with_options`] for the first attempt and
/// [`WebSocket::update_options`] to change future attempts.
///
/// # Defaults
///
/// - `vectored`: `true`  
/// - `max_message_size`: `64 MiB`  
/// - `auto_close`: `true`  
/// - `writev_threshold`: `1024`  
/// - `auto_apply_mask`: `true`
/// - `custom_headers`: empty  
/// - `proxy`: `None`
#[derive(Clone)]
pub struct ConnectionInitOptions {
    vectored: bool,
    max_message_size: usize,
    auto_close: bool,
    writev_threshold: usize,
    auto_apply_mask: bool,
    custom_headers: HeaderMap,
    proxy: Option<Proxy>,
}

impl Default for ConnectionInitOptions {
    fn default() -> Self {
        Self {
            vectored: true,
            max_message_size: 64 << 20, // 64 MiB
            auto_close: true,
            writev_threshold: 1024,
            auto_apply_mask: true,
            custom_headers: HeaderMap::new(),
            proxy: None,
        }
    }
}

impl ConnectionInitOptions {
    /// Creates a new options instance with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Enables or disables vectored writes.
    ///
    /// **Default**: `true`
    pub fn vectored(mut self, value: bool) -> Self {
        self.vectored = value;
        self
    }

    /// Sets the maximum size in bytes of a text or binary message delivered to a callback.
    ///
    /// **Default**: `64 MiB`
    pub fn max_message_size(mut self, value: usize) -> Self {
        self.max_message_size = value;
        self
    }

    /// Whether to reply to the peer's Close before ending the session.
    ///
    /// **Default**: `true`
    pub fn auto_close(mut self, value: bool) -> Self {
        self.auto_close = value;
        self
    }

    /// Sets the threshold for using vectored I/O writes.
    ///
    /// **Default**: `1024`
    pub fn writev_threshold(mut self, value: usize) -> Self {
        self.writev_threshold = value;
        self
    }

    /// Enables or disables automatic masking of frames.
    ///
    /// **Default**: `true`
    pub fn auto_apply_mask(mut self, value: bool) -> Self {
        self.auto_apply_mask = value;
        self
    }

    /// Sets custom HTTP headers to be included in the handshake.
    ///
    /// **Default**: empty
    pub fn custom_headers(mut self, headers: HeaderMap) -> Self {
        self.custom_headers = headers;
        self
    }

    /// Sets a proxy for the connection. See [`ProxyBuilder`](crate::proxy::ProxyBuilder).
    ///
    /// # Example
    ///
    /// ```
    /// use fast_websocket_client::ConnectionInitOptions;
    /// use fast_websocket_client::proxy::ProxyBuilder;
    ///
    /// let proxy = ProxyBuilder::new()
    ///     .http("http://127.0.0.1:8080").unwrap()
    ///     .auth("user", "pass")
    ///     .build().unwrap();
    ///
    /// let _options = ConnectionInitOptions::new().proxy(Some(proxy));
    /// ```
    pub fn proxy(mut self, proxy: Option<Proxy>) -> Self {
        self.proxy = proxy;
        self
    }
}

enum ClientCommand {
    UpdateConfig(ClientConfig),
    UpdateOptions(Box<ConnectionInitOptions>),
    UpdateCallback(CallbackUpdate),
    Send(Frame<'static>),
}

struct Senders {
    commands: mpsc::UnboundedSender<ClientCommand>,
    // Dropping the handle also interrupts a handshake or retry delay.
    _stop: oneshot::Sender<()>,
}

/// A reconnecting WebSocket with asynchronous event handlers.
///
/// Handlers stay registered across reconnects until replaced. One task awaits each
/// handler before processing the next event.
///
/// When an open connection ends, the client calls [`on_close`](Self::on_close)
/// and immediately tries to reconnect unless [`close`](Self::close) was requested.
/// Failed connection attempts call [`on_error`](Self::on_error) and wait for the
/// [configured retry delay](ClientConfig::with_reconnect_delay).
pub struct WebSocket {
    task_handle: JoinHandle<()>,
    senders: Mutex<Option<Senders>>,
}

impl WebSocket {
    /// Starts connecting in the background and returns a handle for registering callbacks.
    /// Use [`WebSocketBuilder`] when a handler must observe the first connection event
    /// on any Tokio runtime.
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketClientError::InvalidUrl`] for an invalid URL. A connection
    /// or handshake failure is reported to [`on_error`](Self::on_error).
    ///
    /// # Example
    ///
    /// ```no_run
    /// use fast_websocket_client::{WebSocket, WebSocketClientError};
    /// use std::time::Duration;
    /// use tokio::{sync::mpsc, time::timeout};
    ///
    /// #[tokio::main(flavor = "current_thread")]
    /// async fn main() -> Result<(), WebSocketClientError> {
    ///     let ws = WebSocket::connect("wss://echo.websocket.org").await?;
    ///     let (tx, mut messages) = mpsc::unbounded_channel();
    ///     ws.on_message(move |text| {
    ///         let tx = tx.clone();
    ///         async move { let _ = tx.send(text); }
    ///     }).await;
    ///
    ///     ws.send("hello").await?;
    ///     if let Ok(Some(text)) = timeout(Duration::from_secs(5), messages.recv()).await {
    ///         println!("{text}");
    ///     }
    ///     ws.close().await;
    ///     ws.await_shutdown().await
    /// }
    /// ```
    pub async fn connect(url: &str) -> Result<Self, WebSocketClientError> {
        WebSocketBuilder::new().connect(url).await
    }

    /// Alias for [`WebSocket::connect`].
    pub async fn new(url: &str) -> Result<Self, WebSocketClientError> {
        Self::connect(url).await
    }

    /// Updates the client configuration at runtime.
    /// Invalid configurations are reported through `on_error` and leave the
    /// previous configuration in place. An ongoing connection attempt, retry
    /// wait, or close handshake keeps the timeout it started with.
    pub async fn update_config(&self, config: ClientConfig) -> &Self {
        let _ = self.post(|| ClientCommand::UpdateConfig(config));
        self
    }

    /// Updates the options for the next connection attempt.
    /// Invalid options are reported through the error callback.
    pub async fn update_options(&self, opts: ConnectionInitOptions) -> &Self {
        let _ = self.post(|| ClientCommand::UpdateOptions(Box::new(opts)));
        self
    }

    /// Updates the callback handlers.
    async fn update_callback(&self, cb: CallbackUpdate) -> &Self {
        let _ = self.post(|| ClientCommand::UpdateCallback(cb));
        self
    }

    /// Sets the handler called after each successful handshake, including reconnects.
    pub async fn on_open<F, Fut>(&self, f: F) -> &Self
    where
        F: FnMut(()) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.update_callback(CallbackUpdate::Open(Box::new(f)))
            .await;
        self
    }

    /// Sets the handler that receives a [`CloseEvent`] when an established connection ends.
    pub async fn on_close<F, Fut>(&self, f: F) -> &Self
    where
        F: FnMut(CloseEvent) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.update_callback(CallbackUpdate::Close(Box::new(f)))
            .await;
        self
    }

    /// Sets the handler for connection, transport, and runtime configuration errors.
    pub async fn on_error<F, Fut>(&self, f: F) -> &Self
    where
        F: FnMut(String) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.update_callback(CallbackUpdate::Error(Box::new(f)))
            .await;
        self
    }

    /// Sets the handler for complete text messages. See [`Self::on_binary`] for binary data.
    pub async fn on_message<F, Fut>(&self, f: F) -> &Self
    where
        F: FnMut(String) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.update_callback(CallbackUpdate::Message(Box::new(f)))
            .await;
        self
    }

    /// Sets the handler for complete binary messages.
    pub async fn on_binary<F, Fut>(&self, f: F) -> &Self
    where
        F: FnMut(Vec<u8>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.update_callback(CallbackUpdate::Binary(Box::new(f)))
            .await;
        self
    }

    /// Requests shutdown and stops reconnecting. While the connection stays open,
    /// earlier sends are processed before the Close frame. Pending sends are discarded
    /// if the client stops while disconnected. Use [`Self::await_shutdown`] to wait for completion.
    pub async fn close(&self) -> &Self {
        self.senders.lock().unwrap().take();
        self
    }

    /// Waits for shutdown and reports a task panic or cancellation.
    /// Call [`Self::close`] first to stop this reconnecting client.
    pub async fn await_shutdown(self) -> Result<(), WebSocketClientError> {
        self.task_handle
            .await
            .map_err(|e| WebSocketClientError::TaskError(e.to_string()))
    }

    /// Sends a text message. You can send before the connection opens.
    /// Text and binary messages are sent in the order submitted.
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketClientError::SendError`] once shutdown has been requested
    /// or the driver has exited. Transport errors are delivered to the error callback.
    pub async fn send(&self, message: &str) -> Result<(), WebSocketClientError> {
        self.post(|| ClientCommand::Send(Frame::text(message.as_bytes().to_vec().into())))
    }

    /// Sends a binary message with the same ordering and error behavior as [`Self::send`].
    pub async fn send_binary(&self, message: &[u8]) -> Result<(), WebSocketClientError> {
        self.post(|| ClientCommand::Send(Frame::binary(message.to_vec().into())))
    }

    // Order concurrent sends and close under the same lock.
    fn post(&self, command: impl FnOnce() -> ClientCommand) -> Result<(), WebSocketClientError> {
        let guard = self.senders.lock().unwrap();
        let closed = || WebSocketClientError::SendError("client is closed".into());
        let senders = guard.as_ref().ok_or_else(closed)?;
        senders.commands.send(command()).map_err(|_| closed())
    }
}

/// Installs handlers and connection options before the first connection attempt.
///
/// Use the builder when the first [`on_open`](Self::on_open) or [`on_error`](Self::on_error)
/// matters. [`connect`](Self::connect) starts the client with the registered handlers.
///
/// ```
/// use fast_websocket_client::{ConnectionInitOptions, HeaderMap, WebSocketBuilder};
///
/// let mut headers = HeaderMap::new();
/// headers.insert("Sec-WebSocket-Protocol", "chat".parse().unwrap());
/// let builder = WebSocketBuilder::new()
///     .with_options(ConnectionInitOptions::new().custom_headers(headers))
///     .on_open(|_| async { println!("connected") })
///     .on_error(|error| async move { eprintln!("{error}") });
/// ```
pub struct WebSocketBuilder {
    callbacks: CallbackSet,
    options: ConnectionInitOptions,
}

impl Default for WebSocketBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl WebSocketBuilder {
    /// Creates a new WebSocketBuilder with default options.
    pub fn new() -> Self {
        Self {
            callbacks: CallbackSet::default(),
            options: ConnectionInitOptions::default(),
        }
    }

    /// Registers a callback to be invoked when the connection opens.
    pub fn on_open<F, Fut>(mut self, f: F) -> Self
    where
        F: FnMut(()) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.callbacks.on_open = Some(Box::new(f));
        self
    }

    /// Registers a callback to be invoked when the connection closes.
    pub fn on_close<F, Fut>(mut self, f: F) -> Self
    where
        F: FnMut(CloseEvent) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.callbacks.on_close = Some(Box::new(f));
        self
    }

    /// Registers a callback to be invoked when an error occurs.
    pub fn on_error<F, Fut>(mut self, f: F) -> Self
    where
        F: FnMut(String) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.callbacks.on_error = Some(Box::new(f));
        self
    }

    /// Registers a callback to be invoked when a message is received.
    pub fn on_message<F, Fut>(mut self, f: F) -> Self
    where
        F: FnMut(String) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.callbacks.on_message = Some(Box::new(f));
        self
    }

    /// Registers a callback for binary messages.
    pub fn on_binary<F, Fut>(mut self, f: F) -> Self
    where
        F: FnMut(Vec<u8>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.callbacks.on_binary = Some(Box::new(f));
        self
    }

    /// Sets the connection initialization options.
    pub fn with_options(mut self, options: ConnectionInitOptions) -> Self {
        self.options = options;
        self
    }

    /// Starts connecting in the background using the default [`ClientConfig`].
    pub async fn connect(self, url: &str) -> Result<WebSocket, WebSocketClientError> {
        self.connect_with_config(url, ClientConfig::new()).await
    }

    /// Starts connecting in the background with the given configuration.
    /// URL and configuration errors are returned here. Connection failures use `on_error`.
    pub async fn connect_with_config(
        self,
        url: &str,
        config: ClientConfig,
    ) -> Result<WebSocket, WebSocketClientError> {
        base_client::parse_url(url).map_err(|e| WebSocketClientError::InvalidUrl(e.to_string()))?;
        config.validate()?;
        base_client::validate_headers(&self.options.custom_headers)
            .map_err(|err| WebSocketClientError::InvalidConfig(err.to_string()))?;
        let (commands, receiver) = mpsc::unbounded_channel();
        let (stop, stopped) = oneshot::channel();
        let driver = Driver {
            config,
            options: self.options,
            callbacks: self.callbacks,
            commands: receiver,
            stopped,
            first_send: None,
        };
        Ok(WebSocket {
            task_handle: tokio::spawn(driver.run(url.to_owned())),
            senders: Mutex::new(Some(Senders {
                commands,
                _stop: stop,
            })),
        })
    }
}

struct Driver {
    config: ClientConfig,
    options: ConnectionInitOptions,
    callbacks: CallbackSet,
    commands: mpsc::UnboundedReceiver<ClientCommand>,
    stopped: oneshot::Receiver<()>,
    // Later commands stay in the channel until this message can be sent.
    first_send: Option<Frame<'static>>,
}

impl Driver {
    async fn apply_command(&mut self, command: ClientCommand) -> Option<Frame<'static>> {
        match command {
            ClientCommand::Send(frame) => return Some(frame),
            ClientCommand::UpdateConfig(config) => match config.validate() {
                Ok(()) => self.config = config,
                Err(error) => self.callbacks.call_on_error(error.to_string()).await,
            },
            ClientCommand::UpdateOptions(options) => {
                match base_client::validate_headers(&options.custom_headers) {
                    Ok(()) => self.options = *options,
                    Err(error) => self.callbacks.call_on_error(error.to_string()).await,
                }
            }
            ClientCommand::UpdateCallback(update) => match update {
                CallbackUpdate::Open(f) => self.callbacks.on_open = Some(f),
                CallbackUpdate::Close(f) => self.callbacks.on_close = Some(f),
                CallbackUpdate::Error(f) => self.callbacks.on_error = Some(f),
                CallbackUpdate::Message(f) => self.callbacks.on_message = Some(f),
                CallbackUpdate::Binary(f) => self.callbacks.on_binary = Some(f),
            },
        }
        None
    }

    async fn apply_queued_updates(&mut self) {
        for _ in 0..self.commands.len() {
            if self.first_send.is_some() {
                break;
            }
            let Ok(command) = self.commands.try_recv() else {
                break;
            };
            self.first_send = self.apply_command(command).await;
        }
    }

    // Even with a send waiting for a connection, close can interrupt this wait.
    async fn while_disconnected<T>(&mut self, future: impl Future<Output = T>) -> Option<T> {
        let mut future = std::pin::pin!(future);
        loop {
            tokio::select! {
                biased;
                _ = &mut self.stopped => return None,
                result = &mut future => return Some(result),
                command = self.commands.recv(), if self.first_send.is_none() => {
                    self.first_send = self.apply_command(command?).await;
                }
            }
        }
    }

    async fn run(mut self, url: String) {
        while !self.commands.is_closed() {
            // Pick up option changes before taking this attempt's snapshot.
            self.apply_queued_updates().await;
            let options = self.options.clone();
            let connecting = try_connect(&url, &options, self.config.connect_timeout);
            let Some(result) = self.while_disconnected(connecting).await else {
                return;
            };
            // Include handlers registered while this attempt was in progress.
            tokio::task::yield_now().await;
            self.apply_queued_updates().await;
            if self.commands.is_closed() {
                return;
            }
            match result {
                Err(error) => {
                    self.callbacks.call_on_error(error.to_string()).await;
                    let delay = time::sleep(self.config.reconnect_delay);
                    if self.while_disconnected(delay).await.is_none() {
                        return;
                    }
                }
                Ok(socket) => {
                    self.callbacks.call_on_open().await;
                    let event = match self.session(socket, &options).await {
                        Ok(event) => event,
                        Err(error) => {
                            self.callbacks.call_on_error(error.to_string()).await;
                            CloseEvent::abnormal()
                        }
                    };
                    self.callbacks.call_on_close(event).await;
                }
            }
        }
    }

    async fn session(
        &mut self,
        socket: base_client::Socket,
        options: &ConnectionInitOptions,
    ) -> Result<CloseEvent, WebSocketError> {
        let (reader, mut writer) = socket.split(tokio::io::split);
        // Keep a partial frame read alive when a command or heartbeat wins select.
        let frames = stream::try_unfold(
            FragmentCollectorRead::new(reader),
            |mut reader| async move {
                let frame: Frame<'static> = reader
                    .read_frame(&mut |_| std::future::ready(Ok::<_, WebSocketError>(())))
                    .await?;
                Ok(Some((frame, reader)))
            },
        );
        let mut frames = std::pin::pin!(frames);

        if let Some(first) = self.first_send.take() {
            send_frame(&mut writer, first).await?;
        }
        let mut ping = time::interval(self.config.ping_interval);
        loop {
            tokio::select! {
                incoming = frames.try_next() => {
                    let frame = incoming?.ok_or(WebSocketError::ConnectionClosed)?;
                    base_client::check_message_size(&frame, options.max_message_size)?;
                    match frame.opcode {
                        OpCode::Close => {
                            return self.peer_close(&mut writer, frame, options.auto_close).await;
                        }
                        OpCode::Text => {
                            let text = String::from_utf8(frame.payload.into()).map_err(|_| WebSocketError::InvalidUTF8)?;
                            self.callbacks.call_on_message(text).await;
                        }
                        OpCode::Binary => self.callbacks.call_on_binary(frame.payload.into()).await,
                        OpCode::Ping => send_frame(&mut writer, Frame::pong(frame.payload)).await?,
                        _ => {}
                    }
                }
                command = self.commands.recv() => {
                    let Some(command) = command else {
                        return close_session(&mut writer, &mut frames, self.config.close_timeout).await;
                    };
                    let previous_ping = self.config.ping_interval;
                    if let Some(frame) = self.apply_command(command).await {
                        send_frame(&mut writer, frame).await?;
                    }
                    if previous_ping != self.config.ping_interval {
                        ping = time::interval(self.config.ping_interval);
                    }
                }
                _ = ping.tick() => send_frame(&mut writer, Frame::new(true, OpCode::Ping, None, (&b""[..]).into())).await?,
            }
        }
    }

    async fn peer_close(
        &mut self,
        writer: &mut Writer,
        frame: Frame<'_>,
        auto_close: bool,
    ) -> Result<CloseEvent, WebSocketError> {
        let mut event = match CloseEvent::from_peer(&frame.payload, false) {
            Ok(event) => event,
            Err(error) => {
                if auto_close {
                    let code = if matches!(error, WebSocketError::InvalidUTF8) {
                        1007
                    } else {
                        1002
                    };
                    let _ = send_frame(writer, Frame::close(code, b"")).await;
                }
                return Err(error);
            }
        };
        if auto_close {
            match send_frame(writer, Frame::close_raw(frame.payload)).await {
                Ok(()) => event.was_clean = true,
                Err(error) => self.callbacks.call_on_error(error.to_string()).await,
            }
        }
        Ok(event)
    }
}

impl CloseEvent {
    fn abnormal() -> Self {
        Self {
            code: 1006,
            reason: String::new(),
            was_clean: false,
        }
    }

    fn from_peer(payload: &[u8], was_clean: bool) -> Result<Self, WebSocketError> {
        let (code, reason) = match payload {
            [] => (1005, ""),
            [high, low, rest @ ..] if payload.len() <= 125 => {
                let code = u16::from_be_bytes([*high, *low]);
                // 1014 is the registered Bad Gateway close code.
                if code != 1014 && !CloseCode::from(code).is_allowed() {
                    return Err(WebSocketError::InvalidCloseCode);
                }
                let reason = std::str::from_utf8(rest).map_err(|_| WebSocketError::InvalidUTF8)?;
                (code, reason)
            }
            _ => return Err(WebSocketError::InvalidCloseFrame),
        };
        Ok(Self {
            code,
            reason: reason.to_owned(),
            was_clean,
        })
    }
}

type Writer = WebSocketWrite<tokio::io::WriteHalf<base_client::Stream>>;

async fn send_frame(writer: &mut Writer, frame: Frame<'_>) -> Result<(), WebSocketError> {
    writer.write_frame(frame).await?;
    writer.flush().await
}

async fn close_session(
    writer: &mut Writer,
    frames: &mut (impl Stream<Item = Result<Frame<'static>, WebSocketError>> + Unpin),
    timeout: Duration,
) -> Result<CloseEvent, WebSocketError> {
    let handshake = async {
        send_frame(writer, Frame::close(1000, b"")).await?;
        while let Some(frame) = frames.try_next().await? {
            if frame.opcode == OpCode::Close {
                return CloseEvent::from_peer(&frame.payload, true);
            }
        }
        Err(WebSocketError::ConnectionClosed)
    };
    time::timeout(timeout, handshake)
        .await
        .unwrap_or_else(|_| Ok(CloseEvent::abnormal()))
}

async fn try_connect(
    url: &str,
    options: &ConnectionInitOptions,
    connect_timeout: Duration,
) -> Result<base_client::Socket, WebSocketClientError> {
    let mut offline = base_client::Offline::new();
    offline
        .set_writev(options.vectored)
        .set_writev_threshold(options.writev_threshold)
        .set_auto_close(false)
        .set_auto_pong(false)
        .set_max_message_size(options.max_message_size)
        .set_auto_apply_mask(options.auto_apply_mask)
        .set_proxy(options.proxy.clone());
    offline.set_headers(options.custom_headers.clone());

    time::timeout(connect_timeout, offline.connect_socket(url))
        .await
        .map_err(|_| WebSocketClientError::ConnectionTimeout(connect_timeout))?
        .map_err(|e| WebSocketClientError::ConnectionError(e.to_string()))
}
