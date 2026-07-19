//! DNS's own TLS client setup for DoT/DoH upstreams. Each module owns its
//! outbound networking, so this store is private to the DNS module.

use std::sync::Arc;

pub(super) fn client_config() -> Arc<rustls::ClientConfig> {
    use std::sync::OnceLock;
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            if rustls::crypto::CryptoProvider::get_default().is_none() {
                let _ = rustls::crypto::ring::default_provider().install_default();
            }
            let mut roots = rustls::RootCertStore::empty();
            let native = rustls_native_certs::load_native_certs();
            for e in &native.errors {
                tracing::warn!(error = %e, "reading a native root cert");
            }
            for cert in native.certs {
                let _ = roots.add(cert);
            }
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}
