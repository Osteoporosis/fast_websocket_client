use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use thiserror::Error;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time;

use crate::HeaderMap;
use crate::OpCode;
use crate::base_client;

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

/// Represents various error types that can occur in the WebSocket client.
#[derive(Error, Debug)]
pub enum WebSocketClientError {
    /// Raised when the WebSocket connection fails.
    #[error("WebSocket connection failed: {0}")]
    ConnectionError(String),

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

/// Holds callback functions that are invoked on specific WebSocket events.
#[derive(Default)]
struct CallbackSet {
    /// Called when the connection is successfully established.
    on_open: Option<VoidCallback>,
    /// Called when the connection is closed.
    on_close: Option<VoidCallback>,
    /// Called when an error occurs.
    on_error: Option<Callback<String>>,
    /// Called when a text message is received.
    on_message: Option<Callback<String>>,
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
    pub async fn call_on_error(&mut self, message: String) {
        if let Some(cb) = &mut self.on_error {
            cb.call_mut(message).await;
        }
    }
    pub async fn call_on_close(&mut self) {
        if let Some(cb) = &mut self.on_close {
            cb.call_mut(()).await;
        }
    }
}

/// Represents updates to callback functions.
enum CallbackUpdate {
    /// Set the callback to be invoked on connection open.
    Open(VoidCallback),
    /// Set the callback to be invoked on connection close.
    Close(VoidCallback),
    /// Set the callback to be invoked on error.
    Error(Callback<String>),
    /// Set the callback to be invoked on message receive.
    Message(Callback<String>),
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
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            ping_interval: Duration::from_secs(30),
            reconnect_delay: Duration::from_secs(10),
        }
    }
}

impl ClientConfig {
    /// Creates a new default configuration.
    ///
    /// - `ping_interval`: 30 seconds  
    /// - `reconnect_delay`: 10 seconds
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the ping interval duration.
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
}

/// Options used to initialize a WebSocket connection.
///
/// # Defaults
/// - `vectored`: `true`  
/// - `max_message_size`: `64 MiB`  
/// - `auto_close`: `true`  
/// - `writev_threshold`: `1024`  
/// - `auto_apply_mask`: `true`
/// - `custom_headers`: empty  
#[derive(Clone)]
pub struct ConnectionInitOptions {
    vectored: bool,
    max_message_size: usize,
    auto_close: bool,
    writev_threshold: usize,
    auto_apply_mask: bool,
    custom_headers: HeaderMap,
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
}

// Represents commands sent to the WebSocket client runtime.
enum ClientCommand {
    /// Close the connection.
    Close,
    /// Update client configuration.
    UpdateConfig(ClientConfig),
    /// Update connection initialization options.
    UpdateOptions(ConnectionInitOptions),
    /// Update callback handlers.
    UpdateCallback(CallbackUpdate),
    /// Send a text message.
    SendMessage(String),
}

/// Represents a WebSocket client instance.
pub struct WebSocket {
    task_handle: JoinHandle<()>,
    command_tx: mpsc::UnboundedSender<ClientCommand>,
    shutdown_notifier: oneshot::Receiver<()>,
}

impl WebSocket {
    /// Connects to the given URL using the default configuration and returns a new [`WebSocket`] instance.
    ///
    /// This is a convenience method that delegates to [`WebSocketBuilder`].
    ///
    /// # Arguments
    ///
    /// * `url` - A string slice representing the WebSocket server URL.
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketClientError`] if the connection attempt fails.
    ///
    /// # Example
    ///
    /// ```
    /// let socket = WebSocket::connect("wss://example.com/socket").await?;
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
    /// Returns [`WebSocketClientError`] if the connection attempt fails.
    ///
    /// # Example
    ///
    /// ```
    /// let socket = WebSocket::new("wss://example.com/socket").await?;
    /// ```
    pub async fn new(url: &str) -> Result<Self, WebSocketClientError> {
        Self::connect(url).await
    }

    /// Updates the client configuration at runtime.
    pub async fn update_config(&self, config: ClientConfig) -> &Self {
        let _ = self.command_tx.send(ClientCommand::UpdateConfig(config));
        self
    }

    /// Updates the connection initialization options.
    pub async fn update_options(&self, opts: ConnectionInitOptions) -> &Self {
        let _ = self.command_tx.send(ClientCommand::UpdateOptions(opts));
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
        F: FnMut(()) -> Fut + Send + 'static,
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

    /// Sends a command to close the connection.
    pub async fn close(&self) -> &Self {
        let _ = self.command_tx.send(ClientCommand::Close);
        self
    }

    /// Awaits shutdown of the WebSocket task.
    pub async fn await_shutdown(self) {
        let _ = self.shutdown_notifier.await;
        let _ = self.task_handle.await;
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
}

/// Builder for creating and connecting a WebSocket client with callbacks and options.
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
        F: FnMut(()) -> Fut + Send + 'static,
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

    /// Sets the connection initialization options.
    pub fn with_options(mut self, options: ConnectionInitOptions) -> Self {
        self.options = options;
        self
    }

    /// Connects to the given URL using default client configuration.
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketClientError`] if the connection could not be established.
    pub async fn connect(self, url: &str) -> Result<WebSocket, WebSocketClientError> {
        self.connect_with_config(url, ClientConfig::new()).await
    }

    /// Connects to the given URL using a specific client configuration.
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketClientError`] if the connection could not be established.
    pub async fn connect_with_config(
        self,
        url: &str,
        config: ClientConfig,
    ) -> Result<WebSocket, WebSocketClientError> {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let url = url.to_owned();

        let task_handle = tokio::spawn(async move {
            run(&url, config, self.options, self.callbacks, command_rx).await;
            let _ = shutdown_tx.send(());
        });

        Ok(WebSocket {
            command_tx,
            task_handle,
            shutdown_notifier: shutdown_rx,
        })
    }
}

async fn run(
    url: &str,
    mut config: ClientConfig,
    mut options: ConnectionInitOptions,
    mut callbacks: CallbackSet,
    mut command_rx: mpsc::UnboundedReceiver<ClientCommand>,
) {
    let mut shutdown = false;

    while !shutdown {
        match try_connect(url, &options).await {
            Ok(mut client) => {
                callbacks.call_on_open().await;
                let mut ping_timer = time::interval(config.ping_interval);

                loop {
                    tokio::select! {
                        _ = ping_timer.tick() => {
                            let _ = client.send_ping("").await;
                        }

                        Some(cmd) = command_rx.recv() => {
                            match cmd {
                                ClientCommand::Close => {
                                    let _ = client.send_close("").await;
                                    callbacks.call_on_close().await;
                                    shutdown = true;
                                    break;
                                },
                                ClientCommand::UpdateConfig(cfg) => {
                                    config = cfg;
                                    ping_timer = time::interval(config.ping_interval);
                                },
                                ClientCommand::UpdateOptions(opts) => {
                                    options = opts;
                                },
                                ClientCommand::UpdateCallback(cb) => match cb {
                                    CallbackUpdate::Open(f) => callbacks.on_open = Some(f),
                                    CallbackUpdate::Close(f) => callbacks.on_close = Some(f),
                                    CallbackUpdate::Error(f) => callbacks.on_error = Some(f),
                                    CallbackUpdate::Message(f) => callbacks.on_message = Some(f),
                                },
                                ClientCommand::SendMessage(message) => {
                                    if let Err(e) = client.send_string(&message).await {
                                        callbacks.call_on_error(e.to_string()).await;
                                    }
                                },
                            }
                        }

                        result = client.receive_frame() => {
                            match result {
                                Ok(frame) => match frame.opcode {
                                    OpCode::Close => {
                                        callbacks.call_on_close().await;
                                        break;
                                    },
                                    OpCode::Text => {
                                        if let Ok(text) = std::str::from_utf8(&frame.payload) {
                                            callbacks.call_on_message(text.to_string()).await;
                                        }
                                    },
                                    _ => {},
                                },
                                Err(e) => {
                                    callbacks.call_on_error(e.to_string()).await;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                callbacks.call_on_error(e.to_string()).await;
                time::sleep(config.reconnect_delay).await;
            }
        }
    }
}

async fn try_connect(
    url: &str,
    options: &ConnectionInitOptions,
) -> Result<base_client::Online, WebSocketClientError> {
    let mut offline = base_client::Offline::new();
    offline
        .set_writev(options.vectored)
        .set_writev_threshold(options.writev_threshold)
        .set_auto_close(options.auto_close)
        .set_max_message_size(options.max_message_size)
        .set_auto_apply_mask(options.auto_apply_mask);

    for (k, v) in options.custom_headers.iter() {
        offline.add_header(k.clone(), v.clone());
    }

    offline
        .connect(url)
        .await
        .map_err(|e| WebSocketClientError::ConnectionError(e.to_string()))
}
