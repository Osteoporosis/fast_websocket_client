use std::error::Error;

use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_socks::tcp::Socks5Stream;
use url::Url;

/// Builder pattern for constructing a [`Proxy`] instance.
///
/// Example:
/// ```
/// use fast_websocket_client::proxy::ProxyBuilder;
///
/// let proxy = ProxyBuilder::new()
///     .http("http://proxy.example.com:8080").unwrap()
///     .auth("user", "pass")
///     .build().unwrap();
/// ```
#[derive(Debug, Default)]
pub struct ProxyBuilder {
    kind: Option<ProxyKind>,
    auth: Option<ProxyAuth>,
}

#[derive(Debug)]
enum ProxyKind {
    Http(Url),
    Socks5(String),
}

impl ProxyBuilder {
    /// Create a new, empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set an **HTTP CONNECT** proxy URI (`http://…` or `https://…`).
    ///
    /// Returns the builder on success, or a URL-parse error on failure.
    pub fn http(mut self, uri: &str) -> Result<Self, Box<dyn Error>> {
        let parsed = Url::parse(uri)?;
        self.kind = Some(ProxyKind::Http(parsed));
        Ok(self)
    }

    /// Set a **SOCKS5** proxy address (`host:port`).
    pub fn socks5(mut self, addr: &str) -> Self {
        self.kind = Some(ProxyKind::Socks5(addr.to_string()));
        self
    }

    /// Provide Basic-Auth credentials for either proxy type.
    pub fn auth(mut self, username: &str, password: &str) -> Self {
        self.auth = Some(ProxyAuth {
            username: username.to_string(),
            password: password.to_string(),
        });
        self
    }

    /// Consume the builder and return a concrete [`Proxy`].
    ///
    /// # Errors
    /// * Returns an error if neither `http` nor `socks5` was specified.
    pub fn build(self) -> Result<Proxy, Box<dyn Error>> {
        match self.kind {
            Some(ProxyKind::Http(uri)) => Ok(Proxy::Http {
                uri,
                auth: self.auth,
            }),
            Some(ProxyKind::Socks5(addr)) => Ok(Proxy::Socks5 {
                addr,
                auth: self.auth,
            }),
            None => Err("Proxy type not specified".into()),
        }
    }
}

/// Username/password pair for proxy authentication (Basic or SOCKS5).
#[derive(Clone, Debug)]
pub struct ProxyAuth {
    username: String,
    password: String,
}

/// Concrete proxy configuration returned by [`ProxyBuilder`].
///
/// * `Http` – plain or TLS tunnel via HTTP CONNECT  
/// * `Socks5` – TCP tunnel via SOCKS5 (optionally username/password)
#[derive(Clone, Debug)]
pub enum Proxy {
    Http {
        uri: Url,
        auth: Option<ProxyAuth>,
    },
    Socks5 {
        addr: String,
        auth: Option<ProxyAuth>,
    },
}

impl Proxy {
    /// Open a TCP tunnel through the proxy to `target_host:target_port`.
    ///
    /// Returns a connected [`tokio::net::TcpStream`] ready for
    /// TLS/WebSocket hand-shake.
    ///
    /// # Errors
    ///
    /// * Underlying network I/O errors  
    /// * Authentication failures  
    /// * Non-`200 OK` response from HTTP proxy
    pub(crate) async fn tunnel(
        &self,
        target_host: &str,
        target_port: u16,
    ) -> Result<TcpStream, Box<dyn Error + Send + Sync>> {
        match self {
            Proxy::Http { uri, auth } => {
                let proxy_addr = format!(
                    "{}:{}",
                    uri.host_str().ok_or("invalid proxy host")?,
                    uri.port_or_known_default().ok_or("proxy port missing")?
                );

                let mut stream = TcpStream::connect(&proxy_addr).await?;

                let target = format!("{target_host}:{target_port}");
                let mut req = format!(
                    "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Connection: Keep-Alive\r\n"
                );

                if let Some(auth) = auth {
                    let token = base64::engine::general_purpose::STANDARD
                        .encode(format!("{}:{}", auth.username, auth.password));
                    req.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
                }

                req.push_str("\r\n");
                stream.write_all(req.as_bytes()).await?;

                let mut buf = [0u8; 1024 * 16];
                let n = stream.read(&mut buf).await?;
                let resp = std::str::from_utf8(&buf[..n])?;

                if !resp.starts_with("HTTP/1.1 200") && !resp.starts_with("HTTP/1.0 200") {
                    return Err(format!("proxy CONNECT failed: {resp}").into());
                }

                Ok(stream)
            }

            Proxy::Socks5 { addr, auth } => {
                let address = addr.as_str();
                let target = (target_host, target_port);
                let stream = if let Some(auth) = auth {
                    Socks5Stream::connect_with_password(
                        address,
                        target,
                        &auth.username,
                        &auth.password,
                    )
                    .await?
                } else {
                    Socks5Stream::connect(address, target).await?
                };
                Ok(stream.into_inner())
            }
        }
    }
}
