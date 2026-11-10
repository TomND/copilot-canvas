//! TLS for the satellite's link to the hub (`wss://`, plan VM-23): rustls
//! with the ring provider on every OS, server certificates checked by the
//! platform (`rustls-platform-verifier`: the system's trust store and its
//! revocation checks), with a pack's own CA added when it names one.

use std::sync::Arc;

use anyhow::{Context, Result};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;

/// The certificates of a PEM file (a pack's CA); none is an error.
pub fn pem_certificates(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>> {
    let certificates = CertificateDer::pem_slice_iter(pem).collect::<Result<Vec<_>, _>>().map_err(|e| anyhow::anyhow!("not a PEM certificate file: {e}"))?;
    anyhow::ensure!(!certificates.is_empty(), "the file has no PEM certificate");
    Ok(certificates)
}

/// A client configuration that trusts the platform's roots and `extra`
/// (a pack's CA), offering `http/1.1` (a WebSocket upgrade).
pub fn client_config(extra: Vec<CertificateDer<'static>>) -> Result<Arc<rustls::ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = if extra.is_empty() { rustls_platform_verifier::Verifier::new(provider.clone()) } else { rustls_platform_verifier::Verifier::new_with_extra_roots(extra, provider.clone()) }.context("cannot use the platform's certificate verifier")?;
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS versions")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_config_trusts_the_platform_and_refuses_what_is_not_a_certificate() {
        let config = client_config(Vec::new()).unwrap();
        assert_eq!(config.alpn_protocols, [b"http/1.1".to_vec()]);
        assert!(pem_certificates(b"").is_err());
        assert!(pem_certificates(b"-----BEGIN CERTIFICATE-----\nnot base64\n-----END CERTIFICATE-----\n").is_err());
        assert!(pem_certificates(b"-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n").is_err(), "a key is not a CA");
    }
}
