use std::error::Error;
use std::sync::Arc;
use tokio_rustls::rustls::{self, crypto::CryptoProvider};

/// Returns a pre-configured [`tokio_rustls::TlsConnector`] with
/// *webpki* root certificates preloaded.
///
/// Honors the application's installed crypto provider. Without one, exactly
/// one of this crate's `aws_lc_rs` or `ring` features must be enabled.
pub(crate) fn get_tls_connector() -> Result<tokio_rustls::TlsConnector, Box<dyn Error + Send + Sync>>
{
    let provider = CryptoProvider::get_default().cloned().or_else(default_provider)
        .ok_or("TLS needs an installed CryptoProvider or exactly one of the aws_lc_rs and ring features")?;
    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(root_store)
        .with_no_client_auth();

    Ok(tokio_rustls::TlsConnector::from(Arc::new(config)))
}

fn default_provider() -> Option<Arc<CryptoProvider>> {
    #[cfg(all(feature = "aws_lc_rs", not(feature = "ring")))]
    {
        Some(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
    }
    #[cfg(all(feature = "ring", not(feature = "aws_lc_rs")))]
    {
        Some(Arc::new(rustls::crypto::ring::default_provider()))
    }
    #[cfg(any(
        all(feature = "aws_lc_rs", feature = "ring"),
        not(any(feature = "aws_lc_rs", feature = "ring"))
    ))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_selection_is_fallible_without_global_mutation() {
        let before = CryptoProvider::get_default().cloned();
        let result = get_tls_connector();
        let exactly_one = cfg!(feature = "aws_lc_rs") ^ cfg!(feature = "ring");
        if before.is_none() && !exactly_one {
            let error = result
                .err()
                .expect("missing/ambiguous provider must be an error");
            assert!(
                error
                    .to_string()
                    .contains("TLS needs an installed CryptoProvider")
            );
        } else {
            assert!(result.is_ok());
        }
        match (before.as_ref(), CryptoProvider::get_default()) {
            (None, None) => {}
            (Some(before), Some(after)) => assert!(Arc::ptr_eq(before, after)),
            _ => panic!("TLS setup changed the process-wide crypto provider"),
        }
    }
}
