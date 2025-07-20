use tokio_rustls::rustls;

/// Returns a pre-configured [`tokio_rustls::TlsConnector`] with
/// *webpki* root certificates preloaded.
///
/// # Panics
///
/// This function is **infallible** in the current implementation,
/// but is marked `-> TlsConnector` instead of `Result` to
/// emphasise that it constructs a default value without I/O.
pub(crate) fn get_tls_connector() -> tokio_rustls::TlsConnector {
    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    tokio_rustls::TlsConnector::from(std::sync::Arc::new(config))
}
