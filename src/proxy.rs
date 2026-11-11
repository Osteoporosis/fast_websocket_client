use std::error::Error;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};

use base64::Engine;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::{TlsConnector, client::TlsStream};
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
    /// HTTPS proxies use TLS with certificate and hostname verification before
    /// sending the CONNECT request or proxy credentials.
    /// Returns an error for an invalid URL or unsupported proxy scheme.
    pub fn http(mut self, uri: &str) -> Result<Self, Box<dyn Error>> {
        let parsed = Url::parse(uri)?;
        validate_http_proxy(&parsed)?;
        self.kind = Some(ProxyKind::Http(parsed));
        Ok(self)
    }

    /// Set a **SOCKS5** proxy address (`host:port`).
    pub fn socks5(mut self, addr: &str) -> Self {
        self.kind = Some(ProxyKind::Socks5(addr.to_string()));
        self
    }

    /// Provide username/password credentials for either proxy type.
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
    /// Open a stream through the proxy to `target_host:target_port`.
    ///
    /// Returns a connected stream ready for the destination's TLS/WebSocket handshake.
    ///
    /// # Errors
    ///
    /// * Underlying network I/O errors  
    /// * Authentication failures  
    /// * Unsupported proxy scheme, invalid/oversized response, or non-2xx status
    pub(crate) async fn tunnel(
        &self,
        target_host: &str,
        target_port: u16,
        tls_connector: &(impl Fn() -> Result<TlsConnector, Box<dyn Error + Send + Sync>> + Sync),
    ) -> Result<ProxyStream, Box<dyn Error + Send + Sync>> {
        match self {
            Proxy::Http { uri, auth } => {
                // Proxy is public, so also validate configurations not made by the builder.
                validate_http_proxy(uri)?;
                let proxy_host = match uri.host().ok_or("invalid proxy host")? {
                    url::Host::Domain(host) => host.to_owned(),
                    url::Host::Ipv4(ip) => ip.to_string(),
                    url::Host::Ipv6(ip) => ip.to_string(),
                };
                let proxy_port = uri.port_or_known_default().ok_or("proxy port missing")?;
                let stream = TcpStream::connect((proxy_host.as_str(), proxy_port)).await?;
                let mut stream = if uri.scheme() == "https" {
                    let server_name = rustls_pki_types::ServerName::try_from(proxy_host)?;
                    let stream = tls_connector()?.connect(server_name, stream).await?;
                    ProxyStream::Tls(Box::new(stream))
                } else {
                    ProxyStream::Plain(stream)
                };

                let target = match target_host.parse::<IpAddr>() {
                    Ok(ip) => SocketAddr::new(ip, target_port).to_string(),
                    Err(_) => format!("{target_host}:{target_port}"),
                };
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
                stream.flush().await?;

                read_connect_response(&mut stream).await?;

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
                Ok(ProxyStream::Plain(stream.into_inner()))
            }
        }
    }
}

fn validate_http_proxy(uri: &Url) -> io::Result<()> {
    if !matches!(uri.scheme(), "http" | "https") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported proxy scheme: expected an http:// or https:// CONNECT proxy",
        ));
    }
    if uri.host().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid proxy host",
        ));
    }
    Ok(())
}

async fn read_connect_response(
    stream: &mut (impl AsyncRead + Unpin),
) -> Result<(), Box<dyn Error + Send + Sync>> {
    const MAX_HEADER_BYTES: usize = 16 * 1024;
    let mut response = Vec::with_capacity(256);
    while !response.ends_with(b"\r\n\r\n") {
        if response.len() == MAX_HEADER_BYTES {
            return Err("proxy CONNECT response headers exceed 16384 bytes".into());
        }
        // Consume exactly the headers; any following bytes belong to the tunnel.
        response.push(stream.read_u8().await?);
    }
    let mut headers =
        vec![httparse::EMPTY_HEADER; response.iter().filter(|&&b| b == b'\n').count()];
    let mut parsed = httparse::Response::new(&mut headers);
    let status = parsed
        .parse(&response)
        .map_err(|error| format!("invalid proxy CONNECT response: {error}"))?;
    if status.is_partial() {
        return Err("incomplete proxy CONNECT response".into());
    }
    let code = parsed.code.ok_or("missing proxy CONNECT status")?;
    if !(200..300).contains(&code) {
        return Err(format!("proxy CONNECT failed with HTTP {code}").into());
    }
    Ok(())
}

// Keep ordinary TCP paths unboxed; retain the outer TLS session throughout the tunnel.
pub(crate) enum ProxyStream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for ProxyStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for ProxyStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write_vectored(cx, bufs),
            Self::Tls(stream) => Pin::new(stream).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            Self::Plain(stream) => stream.is_write_vectored(),
            Self::Tls(stream) => stream.is_write_vectored(),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio::time::{Duration, timeout};

    #[tokio::test]
    async fn connect_response_preserves_trailing_tunnel_bytes() {
        timeout(Duration::from_secs(3), async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut peer, _) = listener.accept().await.unwrap();
                peer.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n\x00\xfftunnel")
                    .await
                    .unwrap();
            });

            let mut stream = TcpStream::connect(address).await.unwrap();
            read_connect_response(&mut stream).await.unwrap();
            let mut trailing = [0; 8];
            stream.read_exact(&mut trailing).await.unwrap();
            assert_eq!(&trailing, b"\x00\xfftunnel");
            server.await.unwrap();
        })
        .await
        .unwrap();
    }
}
