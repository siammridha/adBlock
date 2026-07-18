//! MITM certificate authority: mints per-host leaf certs signed by the
//! root CA.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use tokio::sync::Mutex;

use crate::support::error::{Error, Result};

pub struct CertAuthority {
    ca_cert: Certificate,
    ca_key: KeyPair,
    ca_der: CertificateDer<'static>,
    ca_pem: String,
    cache: Mutex<HashMap<String, Arc<ServerConfig>>>,
}

impl CertAuthority {
    pub fn load(cert_path: &Path, key_path: &Path) -> Result<Self> {
        if !cert_path.exists() || !key_path.exists() {
            return Err(Error::Tls(format!(
                "CA cert/key not found ({} / {}) — provide a signing CA before starting",
                cert_path.display(),
                key_path.display()
            )));
        }
        let cert_pem = std::fs::read_to_string(cert_path)?;
        let key_pem = std::fs::read_to_string(key_path)?;
        let key =
            KeyPair::from_pem(&key_pem).map_err(|e| Error::Tls(format!("ca key parse: {e}")))?;
        let params = CertificateParams::from_ca_cert_pem(&cert_pem)
            .map_err(|e| Error::Tls(format!("ca cert parse: {e}")))?;
        let ca_cert = params
            .self_signed(&key)
            .map_err(|e| Error::Tls(format!("ca rebuild: {e}")))?;
        let ca_der = pem_cert_to_der(&cert_pem)?;
        Ok(Self {
            ca_cert,
            ca_key: key,
            ca_der,
            ca_pem: cert_pem,
            cache: Mutex::new(HashMap::new()),
        })
    }

    pub fn root_pem(&self) -> &str {
        &self.ca_pem
    }

    pub async fn server_config_for(&self, host: &str) -> Result<Arc<ServerConfig>> {
        if let Some(cfg) = self.cache.lock().await.get(host) {
            return Ok(cfg.clone());
        }
        let cfg = Arc::new(self.mint(host)?);
        self.cache.lock().await.insert(host.to_string(), cfg.clone());
        Ok(cfg)
    }

    fn mint(&self, host: &str) -> Result<ServerConfig> {
        let leaf_key =
            KeyPair::generate().map_err(|e| Error::Tls(format!("leaf keygen: {e}")))?;
        let mut params = CertificateParams::new(vec![host.to_string()])
            .map_err(|e| Error::Tls(format!("leaf params: {e}")))?;
        params.distinguished_name.push(DnType::CommonName, host);
        let leaf = params
            .signed_by(&leaf_key, &self.ca_cert, &self.ca_key)
            .map_err(|e| Error::Tls(format!("leaf sign: {e}")))?;

        let chain = vec![leaf.der().clone(), self.ca_der.clone()];
        let key_der =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));

        crate::net::http_client::ensure_crypto_provider();
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(chain, key_der)
            .map_err(|e| Error::Tls(format!("server config: {e}")))
    }
}

#[cfg(test)]
impl CertAuthority {
    pub(crate) fn generate() -> Result<Self> {
        let key = KeyPair::generate().map_err(|e| Error::Tls(format!("ca keygen: {e}")))?;
        let mut params =
            CertificateParams::new(vec![]).map_err(|e| Error::Tls(format!("ca params: {e}")))?;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "proxy Test Root CA");
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca_cert = params
            .self_signed(&key)
            .map_err(|e| Error::Tls(format!("ca self-sign: {e}")))?;
        let ca_der = ca_cert.der().clone();
        let ca_pem = ca_cert.pem();
        Ok(Self {
            ca_cert,
            ca_key: key,
            ca_der,
            ca_pem,
            cache: Mutex::new(HashMap::new()),
        })
    }
}

/// Generate a fresh self-signed root CA, returning its `(cert_pem, key_pem)`.
/// The cert can be handed to clients to install as trusted; the key is what
/// lets this proxy sign per-host leaves, so it must be stored securely.
pub fn generate_root_ca(common_name: &str) -> Result<(String, String)> {
    let key = KeyPair::generate().map_err(|e| Error::Tls(format!("ca keygen: {e}")))?;
    let mut params =
        CertificateParams::new(vec![]).map_err(|e| Error::Tls(format!("ca params: {e}")))?;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name.push(DnType::CommonName, common_name);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca_cert = params
        .self_signed(&key)
        .map_err(|e| Error::Tls(format!("ca self-sign: {e}")))?;
    Ok((ca_cert.pem(), key.serialize_pem()))
}

fn pem_cert_to_der(pem: &str) -> Result<CertificateDer<'static>> {
    let mut cursor = std::io::Cursor::new(pem.as_bytes());
    let cert = rustls_pemfile::certs(&mut cursor)
        .next()
        .ok_or_else(|| Error::Tls("no certificate in CA PEM".into()))?
        .map_err(|e| Error::Tls(format!("pem parse: {e}")))?;
    Ok(cert)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::ServerName;
    use rustls::{ClientConfig, RootCertStore};
    use tokio::io::AsyncWriteExt;
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    fn make_root() -> (Certificate, KeyPair, CertificateDer<'static>) {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec![]).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "Test Step-CA Root");
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let cert = params.self_signed(&key).unwrap();
        let der = cert.der().clone();
        (cert, key, der)
    }

    fn make_intermediate(root: &Certificate, root_key: &KeyPair) -> (String, String) {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec![]).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params
            .distinguished_name
            .push(DnType::CommonName, "proxy Signing CA");
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let cert = params.signed_by(&key, root, root_key).unwrap();
        (cert.pem(), key.serialize_pem())
    }

    fn scratch_dir() -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("proxy-ca-test-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn leaf_from_loaded_intermediate_chains_to_root_only() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let (root, root_key, root_der) = make_root();
        let (int_pem, int_key_pem) = make_intermediate(&root, &root_key);

        let dir = scratch_dir();
        let cert_path = dir.join("int-cert.pem");
        let key_path = dir.join("int-key.pem");
        std::fs::write(&cert_path, &int_pem).unwrap();
        std::fs::write(&key_path, &int_key_pem).unwrap();

        let ca = CertAuthority::load(&cert_path, &key_path).unwrap();
        let server_cfg = ca.server_config_for("example.com").await.unwrap();

        let mut roots = RootCertStore::empty();
        roots.add(root_der).unwrap();
        let client_cfg = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();

        let acceptor = TlsAcceptor::from(server_cfg);
        let connector = TlsConnector::from(std::sync::Arc::new(client_cfg));
        let (client_io, server_io) = tokio::io::duplex(16 * 1024);

        let server = tokio::spawn(async move {
            acceptor.accept(server_io).await.map(|_| ())
        });

        let name = ServerName::try_from("example.com").unwrap();
        let mut client = connector
            .connect(name, client_io)
            .await
            .expect("leaf -> intermediate must validate against a root-only trust store");
        client.write_all(b"ping").await.unwrap();
        client.flush().await.unwrap();

        server.await.unwrap().expect("server handshake");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn absent_files_are_an_error() {
        let dir = scratch_dir();
        let cert_path = dir.join("ca-cert.pem");
        let key_path = dir.join("ca-key.pem");

        assert!(
            CertAuthority::load(&cert_path, &key_path).is_err(),
            "missing CA files must error, not self-generate"
        );
        assert!(!cert_path.exists(), "must not write a CA to disk");

        std::fs::remove_dir_all(&dir).ok();
    }
}
