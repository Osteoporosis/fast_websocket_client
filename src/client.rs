use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use thiserror::Error;

use tokio::sync::mpsc;
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

/// Error type returned by high-level [`WebSocket`] API.
///
/// All variants are convertible to string via `Display`.
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
    /// The peer's close code, or 1005 if omitted, or 1006 if no Close frame arrived.
    pub code: u16,
    /// The peer's close reason, or an empty string if none was received.
    pub reason: String,
    /// Whether both peers exchanged Close frames successfully.
    pub was_clean: bool,
}

/// Holds callback functions that are invoked on specific WebSocket events.
#[derive(Default)]
struct CallbackSet {
    /// Called when the connection is successfully established.
    on_open: Option<VoidCallback>,
    /// Called when the connection is closed.
    on_close: Option<Callback<CloseEvent>>,
    /// Called when an error occurs.
    on_error: Option<Callback<String>>,
    /// Called when a text message is received.
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

/// Represents updates to callback functions.
enum CallbackUpdate {
    /// Set the callback to be invoked on connection open.
    Open(VoidCallback),
    /// Set the callback to be invoked on connection close.
    Close(Callback<CloseEvent>),
    /// Set the callback to be invoked on error.
    Error(Callback<String>),
    /// Set the callback to be invoked on message receive.
    Message(Callback<String>),
    Binary(Callback<Vec<u8>>),
}

/// Configuration options for the WebSocket client runtime behavior.
pub struct ClientConfig {
    /// Interval at which ping frames are sent.
    ///
    /// **Default**: 30 seconds
    ping_interval: Duration,
    /// Delay before attempting to reconnect after a disconnection.
    ///
    /// **Default**: 10 seconds
    reconnect_delay: Duration,
    /// Maximum time to wait when establishing a connection.
    ///
    /// **Default**: 10 seconds
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
        Ok(())
    }

    /// Maximum time to finish a locally requested close handshake (default: 5 seconds).
    pub fn with_close_timeout(mut self, timeout: Duration) -> Self {
        self.close_timeout = timeout;
        self
    }

    /// Creates a new default configuration.
    ///
    /// - `ping_interval`: 30 seconds  
    /// - `reconnect_delay`: 10 seconds
    /// - `connect_timeout`: 10 seconds
    /// - `close_timeout`: 5 seconds
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the ping interval duration.
    /// The interval must be nonzero.
    ///
    /// **Default**: 30 seconds
    pub fn with_ping_interval(mut self, interval: Duration) -> Self {
        self.ping_interval = interval;
        self
    }

    /// Sets the reconnect delay duration.
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

/// Options applied _per connection attempt_.
///
/// # Defaults
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

    /// Sets the maximum allowed message size in bytes.
    ///
    /// **Default**: `64 MiB`
    pub fn max_message_size(mut self, value: usize) -> Self {
        self.max_message_size = value;
        self
    }

    /// Enables or disables automatic connection closing on close frames.
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

    /// Sets the proxy configuration to use when establishing the connection.
    ///
    /// This method accepts a fully constructed [`Proxy`] instance, usually created
    /// using [`ProxyBuilder`](crate::proxy::ProxyBuilder). Supports both HTTP and SOCKS5 proxies.
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

/// Messages sent from the public handle (`WebSocket`) to the
/// background task running in [`run`].
enum ClientCommand {
    /// Close the connection.
    Close,
    /// Update client configuration.
    UpdateConfig(ClientConfig),
    /// Update connection initialization options.
    UpdateOptions(Box<ConnectionInitOptions>),
    /// Update callback handlers.
    UpdateCallback(CallbackUpdate),
    /// Send a text message.
    SendMessage(String),
    SendBinary(Vec<u8>),
}

/// Represents a WebSocket client instance.
pub struct WebSocket {
    task_handle: JoinHandle<()>,
    command_tx: mpsc::UnboundedSender<ClientCommand>,
}

impl WebSocket {
    /// Starts connecting in the background and returns a new [`WebSocket`] handle.
    /// Connection results are delivered through callbacks; this does not wait for a handshake.
    ///
    /// This is a convenience method that delegates to [`WebSocketBuilder`].
    ///
    /// # Arguments
    ///
    /// * `url` - A string slice representing the WebSocket server URL.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid URL or configuration. Network errors use `on_error`.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use fast_websocket_client::WebSocket;
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), fast_websocket_client::WebSocketClientError> {
    ///     let socket = WebSocket::connect("wss://echo.websocket.org").await?;
    ///     let _ = socket;
    ///     Ok(())
    /// }
    /// ```
    pub async fn connect(url: &str) -> Result<Self, WebSocketClientError> {
        WebSocketBuilder::new().connect(url).await
    }

    /// Alias for [`WebSocket::connect`]. Initializes a new [`WebSocket`] connection.
    ///
    /// # Arguments
    ///
    /// * `url` - A string slice representing the WebSocket server URL.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid URL or configuration. Network errors use `on_error`.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use fast_websocket_client::WebSocket;
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), fast_websocket_client::WebSocketClientError> {
    ///     let socket = WebSocket::new("wss://echo.websocket.org").await?;
    ///     let _ = socket;
    ///     Ok(())
    /// }
    /// ```
    pub async fn new(url: &str) -> Result<Self, WebSocketClientError> {
        Self::connect(url).await
    }

    /// Updates the client configuration at runtime.
    /// Invalid configurations are reported through `on_error` and leave the
    /// previous configuration in place. An ongoing connection attempt, retry
    /// wait, or close handshake keeps the timeout it started with.
    pub async fn update_config(&self, config: ClientConfig) -> &Self {
        let _ = self.command_tx.send(ClientCommand::UpdateConfig(config));
        self
    }

    /// Updates the connection initialization options.
    pub async fn update_options(&self, opts: ConnectionInitOptions) -> &Self {
        let _ = self
            .command_tx
            .send(ClientCommand::UpdateOptions(Box::new(opts)));
        self
    }

    /// Updates the callback handlers.
    async fn update_callback(&self, cb: CallbackUpdate) -> &Self {
        let _ = self.command_tx.send(ClientCommand::UpdateCallback(cb));
        self
    }

    /// Update the `on_open` callback.
    pub async fn on_open<F, Fut>(&self, f: F) -> &Self
    where
        F: FnMut(()) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.update_callback(CallbackUpdate::Open(Box::new(f)))
            .await;
        self
    }

    /// Update the `on_close` callback.
    pub async fn on_close<F, Fut>(&self, f: F) -> &Self
    where
        F: FnMut(CloseEvent) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.update_callback(CallbackUpdate::Close(Box::new(f)))
            .await;
        self
    }

    /// Update the `on_error` callback.
    pub async fn on_error<F, Fut>(&self, f: F) -> &Self
    where
        F: FnMut(String) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.update_callback(CallbackUpdate::Error(Box::new(f)))
            .await;
        self
    }

    /// Update the `on_message` callback.
    pub async fn on_message<F, Fut>(&self, f: F) -> &Self
    where
        F: FnMut(String) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.update_callback(CallbackUpdate::Message(Box::new(f)))
            .await;
        self
    }

    /// Update the binary-message callback.
    pub async fn on_binary<F, Fut>(&self, f: F) -> &Self
    where
        F: FnMut(Vec<u8>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.update_callback(CallbackUpdate::Binary(Box::new(f)))
            .await;
        self
    }

    /// Stops reconnecting and closes the current connection, if any.
    pub async fn close(&self) -> &Self {
        let _ = self.command_tx.send(ClientCommand::Close);
        self
    }

    /// Awaits shutdown of the WebSocket task and reports a task panic or cancellation.
    pub async fn await_shutdown(self) -> Result<(), WebSocketClientError> {
        self.task_handle
            .await
            .map_err(|e| WebSocketClientError::TaskError(e.to_string()))
    }

    /// Sends a text message over the connection.
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketClientError::SendError`] if the message could not be sent.
    pub async fn send(&self, message: &str) -> Result<(), WebSocketClientError> {
        self.command_tx
            .send(ClientCommand::SendMessage(message.to_string()))
            .map_err(|e| WebSocketClientError::SendError(e.to_string()))
    }

    /// Sends a binary message over the connection.
    pub async fn send_binary(&self, message: &[u8]) -> Result<(), WebSocketClientError> {
        self.command_tx
            .send(ClientCommand::SendBinary(message.to_vec()))
            .map_err(|e| WebSocketClientError::SendError(e.to_string()))
    }
}

/// Builder for [`WebSocket`] that lets you register callbacks
/// **before** the first connection attempt.
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

    /// Starts a background connection attempt using default client configuration.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid URL or configuration. Network errors use `on_error`.
    pub async fn connect(self, url: &str) -> Result<WebSocket, WebSocketClientError> {
        self.connect_with_config(url, ClientConfig::new()).await
    }

    /// Starts a background connection attempt using a specific client configuration.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid URL or configuration. Network errors use `on_error`.
    pub async fn connect_with_config(
        self,
        url: &str,
        config: ClientConfig,
    ) -> Result<WebSocket, WebSocketClientError> {
        base_client::parse_url(url).map_err(|e| WebSocketClientError::InvalidUrl(e.to_string()))?;
        config.validate()?;
        let (command_tx, command_rx) = mpsc::unbounded_channel();

        let url = url.to_owned();

        let task_handle = tokio::spawn(async move {
            run(&url, config, self.options, self.callbacks, command_rx).await;
        });

        Ok(WebSocket {
            command_tx,
            task_handle,
        })
    }
}

async fn apply_update(
    command: ClientCommand,
    config: &mut ClientConfig,
    options: &mut ConnectionInitOptions,
    callbacks: &mut CallbackSet,
) -> Option<ClientCommand> {
    match command {
        ClientCommand::UpdateConfig(value) => match value.validate() {
            Ok(()) => *config = value,
            Err(error) => callbacks.call_on_error(error.to_string()).await,
        },
        ClientCommand::UpdateOptions(value) => *options = *value,
        ClientCommand::UpdateCallback(update) => match update {
            CallbackUpdate::Open(f) => callbacks.on_open = Some(f),
            CallbackUpdate::Close(f) => callbacks.on_close = Some(f),
            CallbackUpdate::Error(f) => callbacks.on_error = Some(f),
            CallbackUpdate::Message(f) => callbacks.on_message = Some(f),
            CallbackUpdate::Binary(f) => callbacks.on_binary = Some(f),
        },
        other => return Some(other),
    }
    None
}

// Connecting and retry waits accept updates and cancellation. Sends stay FIFO
// until a connection is available; on_open never reorders them.
async fn wait_with_commands<T>(
    future: impl Future<Output = T>,
    config: &mut ClientConfig,
    options: &mut ConnectionInitOptions,
    callbacks: &mut CallbackSet,
    rx: &mut mpsc::UnboundedReceiver<ClientCommand>,
    pending: &mut VecDeque<ClientCommand>,
) -> Option<T> {
    let mut future = std::pin::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => return Some(result),
            command = rx.recv() => {
                let command = command?;
                match apply_update(command, config, options, callbacks).await {
                    Some(ClientCommand::Close) => return None,
                    Some(message) => pending.push_back(message),
                    None => {},
                }
            }
        }
    }
}

async fn next_command(
    pending: &mut VecDeque<ClientCommand>,
    rx: &mut mpsc::UnboundedReceiver<ClientCommand>,
) -> Option<ClientCommand> {
    match pending.pop_front() {
        Some(command) => Some(command),
        None => rx.recv().await,
    }
}

async fn run(
    url: &str,
    mut config: ClientConfig,
    mut options: ConnectionInitOptions,
    mut callbacks: CallbackSet,
    mut command_rx: mpsc::UnboundedReceiver<ClientCommand>,
) {
    let mut pending = VecDeque::new();
    loop {
        let attempt_options = options.clone();
        let connecting = try_connect(url, &attempt_options, config.connect_timeout);
        let Some(result) = wait_with_commands(
            connecting,
            &mut config,
            &mut options,
            &mut callbacks,
            &mut command_rx,
            &mut pending,
        )
        .await
        else {
            return;
        };
        match result {
            Err(error) => callbacks.call_on_error(error.to_string()).await,
            Ok(mut client) => {
                // Apply registrations already queued before the first send.
                loop {
                    match command_rx.try_recv() {
                        Ok(command) => {
                            if let Some(message) =
                                apply_update(command, &mut config, &mut options, &mut callbacks)
                                    .await
                            {
                                pending.push_back(message);
                                break;
                            }
                        }
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            pending.push_back(ClientCommand::Close);
                            break;
                        }
                    }
                }
                callbacks.call_on_open().await;
                let mut ping_timer = time::interval(config.ping_interval);
                let mut closing = None;
                let mut shutdown = false;
                let mut close_event = CloseEvent {
                    code: 1006,
                    reason: String::new(),
                    was_clean: false,
                };

                'connection: loop {
                    let result = {
                        // A command may win select without discarding this read.
                        let mut receive = std::pin::pin!(client.reader.read());
                        loop {
                            tokio::select! {
                                result = &mut receive => break result,
                                _ = time::sleep_until(closing.unwrap_or_else(time::Instant::now)), if closing.is_some() => {
                                    break 'connection;
                                }
                                _ = ping_timer.tick(), if !shutdown => {
                                    if let Err(error) = client.writer.write(fastwebsockets::Frame::new(true, OpCode::Ping, None, (&b""[..]).into())).await {
                                        callbacks.call_on_error(error.to_string()).await;
                                        break 'connection;
                                    }
                                }
                                command = next_command(&mut pending, &mut command_rx), if !shutdown => {
                                    let previous_ping = config.ping_interval;
                                    let command = match command {
                                        Some(command) => apply_update(command, &mut config, &mut options, &mut callbacks).await,
                                        None => Some(ClientCommand::Close),
                                    };
                                    if previous_ping != config.ping_interval { ping_timer = time::interval(config.ping_interval); }
                                    let frame = match command {
                                        Some(ClientCommand::Close) => {
                                            shutdown = true;
                                            let deadline = time::Instant::now() + config.close_timeout;
                                            closing = Some(deadline);
                                            let result = time::timeout_at(deadline, client.writer.write(fastwebsockets::Frame::close(1000, b""))).await;
                                            match result {
                                                Ok(Ok(())) => {},
                                                Ok(Err(error)) => { callbacks.call_on_error(error.to_string()).await; break 'connection; }
                                                Err(_) => break 'connection,
                                            }
                                            continue;
                                        }
                                        Some(ClientCommand::SendMessage(message)) => fastwebsockets::Frame::text(message.into_bytes().into()),
                                        Some(ClientCommand::SendBinary(message)) => fastwebsockets::Frame::binary(message.into()),
                                        None => continue,
                                        _ => unreachable!(),
                                    };
                                    if let Err(error) = client.writer.write(frame).await {
                                        callbacks.call_on_error(error.to_string()).await;
                                        break 'connection;
                                    }
                                }
                            }
                        }
                    };
                    match result {
                        Ok(frame) => {
                            if frame.opcode == OpCode::Close {
                                if frame.payload.is_empty() {
                                    close_event.code = 1005;
                                } else {
                                    close_event.code =
                                        u16::from_be_bytes([frame.payload[0], frame.payload[1]]);
                                    close_event.reason = std::str::from_utf8(&frame.payload[2..])
                                        .expect("Close payload was validated by the reader")
                                        .to_owned();
                                }
                            }
                            if let Err(error) = client.writer.respond(&frame).await {
                                callbacks.call_on_error(error.to_string()).await;
                                break;
                            }
                            match frame.opcode {
                                OpCode::Close => {
                                    close_event.was_clean = client.writer.is_closed();
                                    break;
                                }
                                OpCode::Text if !shutdown => {
                                    match String::from_utf8(frame.payload.into()) {
                                        Ok(text) => callbacks.call_on_message(text).await,
                                        Err(error) => {
                                            callbacks.call_on_error(error.to_string()).await;
                                            break;
                                        }
                                    }
                                }
                                OpCode::Binary if !shutdown => {
                                    callbacks.call_on_binary(frame.payload.into()).await
                                }
                                _ => {}
                            }
                        }
                        Err(error) => {
                            callbacks.call_on_error(error.to_string()).await;
                            break;
                        }
                    }
                }
                drop(client);
                callbacks.call_on_close(close_event).await;
                if shutdown {
                    return;
                }
            }
        }
        let delay = time::sleep(config.reconnect_delay);
        if wait_with_commands(
            delay,
            &mut config,
            &mut options,
            &mut callbacks,
            &mut command_rx,
            &mut pending,
        )
        .await
        .is_none()
        {
            return;
        }
    }
}

async fn try_connect(
    url: &str,
    options: &ConnectionInitOptions,
    connect_timeout: Duration,
) -> Result<base_client::Online, WebSocketClientError> {
    let mut offline = base_client::Offline::new();
    offline
        .set_writev(options.vectored)
        .set_writev_threshold(options.writev_threshold)
        .set_auto_close(options.auto_close)
        .set_max_message_size(options.max_message_size)
        .set_auto_apply_mask(options.auto_apply_mask)
        .set_proxy(options.proxy.clone());

    for (k, v) in options.custom_headers.iter() {
        offline.add_header(k.clone(), v.clone());
    }

    time::timeout(connect_timeout, offline.connect(url))
        .await
        .map_err(|_| WebSocketClientError::ConnectionTimeout(connect_timeout))?
        .map_err(|e| WebSocketClientError::ConnectionError(e.to_string()))
}
