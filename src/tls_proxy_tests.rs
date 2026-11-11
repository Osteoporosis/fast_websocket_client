#[path = "../tests/common/mod.rs"]
mod common;

use std::error::Error;
use std::sync::Arc;

use base64::Engine;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};
use tokio_rustls::{TlsAcceptor, TlsConnector, rustls};

use crate::OpCode;
use crate::base_client::{Offline, Online};
use crate::proxy::ProxyBuilder;

const DEADLINE: Duration = Duration::from_secs(5);

// These certificates and private keys are test-only fixtures, valid 2020–2100.
// Their CA is trusted only by these tests; normal certificate verification stays enabled.
const CA_CERT: &[u8] = include_bytes!("../tests/fixtures/tls/ca-cert.pem");
const PROXY_CERT: &[u8] = include_bytes!("../tests/fixtures/tls/proxy-cert.pem");
const PROXY_KEY: &[u8] = include_bytes!("../tests/fixtures/tls/proxy-key.pem");
const DESTINATION_CERT: &[u8] = include_bytes!("../tests/fixtures/tls/destination-cert.pem");
const DESTINATION_KEY: &[u8] = include_bytes!("../tests/fixtures/tls/destination-key.pem");

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    // Select explicitly, including when both crate features are enabled. Do not
    // install a process-wide provider or race other TLS tests.
    #[cfg(feature = "aws_lc_rs")]
    {
        Arc::new(rustls::crypto::aws_lc_rs::default_provider())
    }
    #[cfg(all(not(feature = "aws_lc_rs"), feature = "ring"))]
    {
        Arc::new(rustls::crypto::ring::default_provider())
    }
}

fn client_connector(trust_test_ca: bool) -> TlsConnector {
    let mut roots = rustls::RootCertStore::empty();
    if trust_test_ca {
        roots
            .add(CertificateDer::from_pem_slice(CA_CERT).unwrap())
            .unwrap();
    }
    let config = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}

fn server_acceptor(cert: &[u8], key: &[u8]) -> TlsAcceptor {
    let config = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from_pem_slice(cert).unwrap()],
            PrivateKeyDer::from_pem_slice(key).unwrap(),
        )
        .unwrap();
    TlsAcceptor::from(Arc::new(config))
}

async fn assert_starts_with_tls(stream: &TcpStream) {
    let mut first = [0];
    assert_eq!(stream.peek(&mut first).await.unwrap(), 1);
    assert_eq!(first[0], 0x16, "CONNECT must not precede the TLS handshake");
}

async fn websocket_echo<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S, expected: &[u8]) {
    let request = common::handshake(stream).await;
    assert!(request.starts_with("GET /socket?x=%2F HTTP/1.1\r\n"));
    assert!(
        !request
            .to_ascii_lowercase()
            .contains("proxy-authorization:")
    );
    stream.flush().await.unwrap();
    let (opcode, payload) = common::read_frame(stream).await.unwrap();
    assert_eq!(opcode, 2);
    assert_eq!(payload.as_slice(), expected);
    common::write_frame(stream, true, 2, &payload).await;
    stream.flush().await.unwrap();
    let (opcode, payload) = common::read_frame(stream).await.unwrap();
    assert_eq!(opcode, 8);
    common::write_frame(stream, true, 8, &payload).await;
    stream.flush().await.unwrap();
}

async fn roundtrip(client: &mut Online, payload: &[u8]) {
    client.send_binary(payload).await.unwrap();
    let frame = client.receive_frame().await.unwrap();
    assert_eq!(frame.opcode, OpCode::Binary);
    assert_eq!(&frame.payload[..], payload);
    client.send_close("done").await.unwrap();
    assert_eq!(client.receive_frame().await.unwrap().opcode, OpCode::Close);
}

async fn assert_https_proxy_roundtrip(nested_tls: bool) {
    timeout(DEADLINE, async {
        let (listener, _) = common::listener().await;
        let proxy = ProxyBuilder::new()
            .http(&format!(
                "https://localhost:{}",
                listener.local_addr().unwrap().port()
            ))
            .unwrap()
            .auth("test-user", "test-pass")
            .build()
            .unwrap();
        let proxy_acceptor = server_acceptor(PROXY_CERT, PROXY_KEY);
        let destination_acceptor = server_acceptor(DESTINATION_CERT, DESTINATION_KEY);
        let payload = if nested_tls {
            vec![0xa5; 2048]
        } else {
            vec![0, 255, 128]
        };
        let expected = payload.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            stream.set_nodelay(true).unwrap();
            assert_starts_with_tls(&stream).await;
            let mut proxy_stream = proxy_acceptor.accept(stream).await.unwrap();
            assert_eq!(proxy_stream.get_ref().1.server_name(), Some("localhost"));
            let request = common::read_headers(&mut proxy_stream).await;
            let port = if nested_tls { 443 } else { 80 };
            assert!(
                request.starts_with(&format!("CONNECT destination.invalid:{port} HTTP/1.1\r\n"))
            );
            let credentials =
                base64::engine::general_purpose::STANDARD.encode("test-user:test-pass");
            assert!(request.contains(&format!("Proxy-Authorization: Basic {credentials}\r\n")));
            proxy_stream
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
                .unwrap();
            proxy_stream.flush().await.unwrap();
            if nested_tls {
                // The local server acts as the tunnel's destination too, so the
                // second TLS handshake runs inside the established proxy TLS.
                let mut destination_stream =
                    destination_acceptor.accept(proxy_stream).await.unwrap();
                assert_eq!(
                    destination_stream.get_ref().1.server_name(),
                    Some("destination.invalid")
                );
                websocket_echo(&mut destination_stream, &expected).await;
            } else {
                websocket_echo(&mut proxy_stream, &expected).await;
            }
        });
        let connector = client_connector(true);
        let mut offline = Offline::new();
        offline.set_proxy(Some(proxy));
        let scheme = if nested_tls { "wss" } else { "ws" };
        let mut client = offline
            .connect_with_tls_connector(
                &format!("{scheme}://destination.invalid/socket?x=%2F"),
                || Ok(connector.clone()),
            )
            .await
            .unwrap();
        roundtrip(&mut client, &payload).await;
        server.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn https_proxy_authenticates_inside_tls_and_carries_ws_frames() {
    assert_https_proxy_roundtrip(false).await;
}

#[tokio::test]
async fn https_proxy_preserves_outer_tls_during_destination_wss() {
    assert_https_proxy_roundtrip(true).await;
}

#[derive(Clone, Copy, Debug)]
enum CertificateFailure {
    UntrustedIssuer,
    WrongHostname,
}

fn assert_certificate_error(
    error: &(dyn Error + Send + Sync + 'static),
    expected: CertificateFailure,
) {
    let io_error = error
        .downcast_ref::<std::io::Error>()
        .expect("expected a TLS I/O error");
    let tls_error = io_error
        .get_ref()
        .and_then(|error| error.downcast_ref::<rustls::Error>())
        .expect("expected a rustls certificate error");
    let matches = matches!(
        (expected, tls_error),
        (
            CertificateFailure::UntrustedIssuer,
            rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer),
        ) | (
            CertificateFailure::WrongHostname,
            rustls::Error::InvalidCertificate(
                rustls::CertificateError::NotValidForName
                    | rustls::CertificateError::NotValidForNameContext { .. },
            ),
        )
    );
    assert!(matches, "expected {expected:?}, received {tls_error:?}");
}

async fn assert_proxy_certificate_rejected(failure: CertificateFailure) {
    timeout(DEADLINE, async {
        let (listener, _) = common::listener().await;
        let (hostname, trust_ca) = match failure {
            CertificateFailure::UntrustedIssuer => ("localhost", false),
            CertificateFailure::WrongHostname => ("127.0.0.1", true),
        };
        let proxy = ProxyBuilder::new()
            .http(&format!(
                "https://{hostname}:{}",
                listener.local_addr().unwrap().port()
            ))
            .unwrap()
            .auth("test-user", "test-pass")
            .build()
            .unwrap();
        let acceptor = server_acceptor(PROXY_CERT, PROXY_KEY);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            assert_starts_with_tls(&stream).await;
            assert!(
                acceptor.accept(stream).await.is_err(),
                "certificate rejection must end TLS before any CONNECT or credentials"
            );
        });
        let connector = client_connector(trust_ca);
        let mut offline = Offline::new();
        offline.set_proxy(Some(proxy));
        let error = offline
            .connect_with_tls_connector("ws://destination.invalid/", || Ok(connector.clone()))
            .await
            .err()
            .expect("invalid proxy certificate was accepted");
        assert_certificate_error(error.as_ref(), failure);
        server.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn untrusted_https_proxy_is_rejected_before_connect_or_credentials() {
    assert_proxy_certificate_rejected(CertificateFailure::UntrustedIssuer).await;
}

#[tokio::test]
async fn wrong_https_proxy_hostname_is_rejected_before_connect_or_credentials() {
    assert_proxy_certificate_rejected(CertificateFailure::WrongHostname).await;
}

#[tokio::test]
async fn https_proxy_does_not_bypass_destination_certificate_verification() {
    timeout(DEADLINE, async {
        let (listener, _) = common::listener().await;
        let proxy = ProxyBuilder::new()
            .http(&format!(
                "https://localhost:{}",
                listener.local_addr().unwrap().port()
            ))
            .unwrap()
            .build()
            .unwrap();
        let proxy_acceptor = server_acceptor(PROXY_CERT, PROXY_KEY);
        // This certificate is trusted but valid only for localhost, not the
        // destination.invalid name requested inside the tunnel.
        let destination_acceptor = server_acceptor(PROXY_CERT, PROXY_KEY);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut proxy_stream = proxy_acceptor.accept(stream).await.unwrap();
            assert_eq!(proxy_stream.get_ref().1.server_name(), Some("localhost"));
            let request = common::read_headers(&mut proxy_stream).await;
            assert!(request.starts_with("CONNECT destination.invalid:443 HTTP/1.1\r\n"));
            proxy_stream
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
                .unwrap();
            proxy_stream.flush().await.unwrap();
            assert!(
                destination_acceptor.accept(proxy_stream).await.is_err(),
                "destination TLS must fail before any WebSocket GET"
            );
        });
        let connector = client_connector(true);
        let mut offline = Offline::new();
        offline.set_proxy(Some(proxy));
        let error = offline
            .connect_with_tls_connector("wss://destination.invalid/", || Ok(connector.clone()))
            .await
            .err()
            .expect("the destination certificate must be checked inside proxy TLS");
        assert_certificate_error(error.as_ref(), CertificateFailure::WrongHostname);
        server.await.unwrap();
    })
    .await
    .unwrap();
}
