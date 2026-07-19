//! Managed CA store: keeps zero or more signing CAs on disk (one folder each
//! under `data/certs/<name>/`) and remembers which one is active. The active
//! selection is applied at startup — switching CAs takes effect on the next
//! restart, not live, because the running proxy binds its CA once.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::proxy::ca::{generate_root_ca, CertAuthority};
use crate::proxy::error::{Error, Result};
use crate::proxy::persist::OverrideStore;

const CERT_FILE: &str = "cert.pem";
const KEY_FILE: &str = "key.pem";
/// Reserved name that means "use the CA from config.toml" (no stored override).
pub const CONFIG_NAME: &str = "config";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct ActiveSelection {
    /// Folder name of the active stored CA. `None` means the config-file CA.
    active: Option<String>,
}

/// One row for the certificates UI.
#[derive(Clone, Debug, Serialize)]
pub struct CertSummary {
    pub name: String,
    pub source: &'static str,
    pub active: bool,
    pub subject: String,
    pub not_after: String,
    pub expired: bool,
    pub readable: bool,
}

pub struct CertStore {
    certs_dir: PathBuf,
    active: OverrideStore<ActiveSelection>,
    config_cert: PathBuf,
    config_key: PathBuf,
}

impl CertStore {
    /// `active_store` is the JSON file that records the selection;
    /// `config_cert`/`config_key` are the fallback CA from config.toml.
    pub fn load(
        certs_dir: PathBuf,
        active_store: PathBuf,
        config_cert: PathBuf,
        config_key: PathBuf,
    ) -> Self {
        Self {
            certs_dir,
            active: OverrideStore::new(active_store),
            config_cert,
            config_key,
        }
    }

    /// The cert/key paths the proxy should load at startup: the active stored CA
    /// if one is selected and present, otherwise the config-file CA.
    pub fn active_paths(&self) -> (PathBuf, PathBuf) {
        if let Some(name) = self.active.load().active {
            let dir = self.certs_dir.join(&name);
            let cert = dir.join(CERT_FILE);
            let key = dir.join(KEY_FILE);
            if cert.exists() && key.exists() {
                return (cert, key);
            }
            tracing::warn!(%name, "active CA missing on disk; falling back to the config CA");
        }
        (self.config_cert.clone(), self.config_key.clone())
    }

    /// All selectable CAs: the config-file CA first, then each stored one.
    pub fn list(&self) -> Vec<CertSummary> {
        let active = self.active.load().active;
        let mut out = vec![summarize(
            CONFIG_NAME,
            "config",
            active.is_none(),
            &self.config_cert,
        )];
        let mut names = self.stored_names();
        names.sort();
        for name in names {
            let cert = self.certs_dir.join(&name).join(CERT_FILE);
            let is_active = active.as_deref() == Some(name.as_str());
            out.push(summarize_owned(name, "stored", is_active, &cert));
        }
        out
    }

    /// Store a pasted/uploaded CA. Both PEM blobs are required — a signing CA is
    /// useless without its private key. The pair is validated by loading it as a
    /// real `CertAuthority` before it is kept.
    pub fn add_pem(&self, name: &str, cert_pem: &str, key_pem: &str) -> Result<()> {
        let name = clean_name(name)?;
        if cert_pem.trim().is_empty() || key_pem.trim().is_empty() {
            return Err(Error::Tls("both a certificate and a private key are required".into()));
        }
        self.write_pair(&name, cert_pem, key_pem)
    }

    /// Generate a fresh self-signed root CA and store it under `name`.
    /// Returns the new certificate PEM so the UI can offer it for download.
    pub fn generate(&self, name: &str, common_name: &str) -> Result<String> {
        let name = clean_name(name)?;
        let cn = if common_name.trim().is_empty() {
            format!("{name} Root CA")
        } else {
            common_name.trim().to_string()
        };
        let (cert_pem, key_pem) = generate_root_ca(&cn)?;
        self.write_pair(&name, &cert_pem, &key_pem)?;
        Ok(cert_pem)
    }

    /// Set the active CA. `CONFIG_NAME` clears the override (use config.toml).
    /// The switch takes effect on the next restart.
    pub fn activate(&self, name: &str) -> Result<()> {
        let selection = if name == CONFIG_NAME {
            ActiveSelection { active: None }
        } else {
            let name = clean_name(name)?;
            let dir = self.certs_dir.join(&name);
            if !dir.join(CERT_FILE).exists() || !dir.join(KEY_FILE).exists() {
                return Err(Error::Tls(format!("no stored CA named '{name}'")));
            }
            ActiveSelection { active: Some(name) }
        };
        self.active.save(&selection).map_err(Error::Tls)
    }

    /// Remove a stored CA. Clears the active selection if it pointed here.
    pub fn delete(&self, name: &str) -> Result<()> {
        let name = clean_name(name)?;
        let dir = self.certs_dir.join(&name);
        if !dir.exists() {
            return Err(Error::Tls(format!("no stored CA named '{name}'")));
        }
        std::fs::remove_dir_all(&dir)
            .map_err(|e| Error::Tls(format!("removing {}: {e}", dir.display())))?;
        if self.active.load().active.as_deref() == Some(name.as_str()) {
            self.active.save(&ActiveSelection { active: None }).map_err(Error::Tls)?;
        }
        Ok(())
    }

    /// The active CA's certificate PEM (for download), whichever is selected.
    pub fn active_cert_pem(&self) -> Result<String> {
        let (cert, _) = self.active_paths();
        std::fs::read_to_string(&cert)
            .map_err(|e| Error::Tls(format!("reading {}: {e}", cert.display())))
    }

    /// A stored CA's certificate PEM (for download). `CONFIG_NAME` returns the
    /// config-file CA's cert.
    pub fn cert_pem(&self, name: &str) -> Result<String> {
        let path = if name == CONFIG_NAME {
            self.config_cert.clone()
        } else {
            self.certs_dir.join(clean_name(name)?).join(CERT_FILE)
        };
        std::fs::read_to_string(&path)
            .map_err(|e| Error::Tls(format!("reading {}: {e}", path.display())))
    }

    fn stored_names(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.certs_dir) else {
            return Vec::new();
        };
        entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().join(CERT_FILE).exists())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect()
    }

    fn write_pair(&self, name: &str, cert_pem: &str, key_pem: &str) -> Result<()> {
        let dir = self.certs_dir.join(name);
        if dir.exists() {
            return Err(Error::Tls(format!("a CA named '{name}' already exists")));
        }
        std::fs::create_dir_all(&dir)
            .map_err(|e| Error::Tls(format!("creating {}: {e}", dir.display())))?;
        let cert_path = dir.join(CERT_FILE);
        let key_path = dir.join(KEY_FILE);
        let write = || -> std::io::Result<()> {
            std::fs::write(&cert_path, cert_pem)?;
            std::fs::write(&key_path, key_pem)?;
            Ok(())
        };
        if let Err(e) = write() {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(Error::Tls(format!("writing CA files: {e}")));
        }
        // Prove the pair actually works as a signing CA before we keep it.
        if let Err(e) = CertAuthority::load(&cert_path, &key_path) {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
        Ok(())
    }
}

/// Names become folder names, so keep them to a safe, predictable set.
fn clean_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::Tls("a name is required".into()));
    }
    if name == CONFIG_NAME {
        return Err(Error::Tls(format!("'{CONFIG_NAME}' is reserved")));
    }
    if name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        || name.starts_with('.')
    {
        return Err(Error::Tls(
            "name may use letters, digits, dash, underscore, and dot only".into(),
        ));
    }
    Ok(name.to_string())
}

fn summarize_owned(name: String, source: &'static str, active: bool, cert: &Path) -> CertSummary {
    let mut s = summarize(&name, source, active, cert);
    s.name = name;
    s
}

fn summarize(name: &str, source: &'static str, active: bool, cert: &Path) -> CertSummary {
    match read_cert_info(cert) {
        Some((subject, not_after, expired)) => CertSummary {
            name: name.to_string(),
            source,
            active,
            subject,
            not_after,
            expired,
            readable: true,
        },
        None => CertSummary {
            name: name.to_string(),
            source,
            active,
            subject: "<not found or unreadable>".into(),
            not_after: String::new(),
            expired: false,
            readable: false,
        },
    }
}

/// Pull subject and expiry out of a cert PEM for display. Returns `None` if the
/// file is missing or unparseable.
fn read_cert_info(path: &Path) -> Option<(String, String, bool)> {
    let pem = std::fs::read_to_string(path).ok()?;
    let mut cursor = Cursor::new(pem.as_bytes());
    let der = rustls_pemfile::certs(&mut cursor).next()?.ok()?;
    let (_, cert) = x509_parser::parse_x509_certificate(&der).ok()?;
    let subject = cert.subject().to_string();
    let not_after = cert.validity().not_after;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let expired = not_after.timestamp() < now;
    Some((subject, not_after.to_string(), expired))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> (PathBuf, PathBuf) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let nanos = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("proxy-certstore-{nanos}"));
        std::fs::create_dir_all(root.join("certs")).unwrap();
        (root.join("certs"), root.join("active-ca.json"))
    }

    fn store(certs: &Path, active: &Path) -> CertStore {
        CertStore::load(
            certs.to_path_buf(),
            active.to_path_buf(),
            certs.join("config-cert.pem"),
            certs.join("config-key.pem"),
        )
    }

    #[test]
    fn generate_list_activate_and_fallback() {
        let (certs, active) = scratch();
        let s = store(&certs, &active);

        // No selection → active paths are the config fallback.
        let (cert, key) = s.active_paths();
        assert!(cert.ends_with("config-cert.pem") && key.ends_with("config-key.pem"));

        // Generate a root CA; it shows up and the config row is still active.
        let pem = s.generate("mine", "Test Root").unwrap();
        assert!(pem.contains("BEGIN CERTIFICATE"));
        let list = s.list();
        assert_eq!(list[0].name, CONFIG_NAME);
        assert!(list[0].active, "config CA is active until one is selected");
        assert!(list.iter().any(|c| c.name == "mine" && c.readable && !c.active));

        // Activate it → active paths point at the stored pair.
        s.activate("mine").unwrap();
        let (cert, _) = s.active_paths();
        assert_eq!(cert, certs.join("mine").join(CERT_FILE));
        assert!(s.list().iter().find(|c| c.name == "mine").unwrap().active);

        std::fs::remove_dir_all(certs.parent().unwrap()).ok();
    }

    #[test]
    fn add_validates_the_pair_and_rejects_junk() {
        let (certs, active) = scratch();
        let s = store(&certs, &active);

        // Round-trip a real CA through the paste path by reading back what we
        // generated for a different name.
        s.generate("src", "Paste Source").unwrap();
        let cert_pem = std::fs::read_to_string(certs.join("src").join(CERT_FILE)).unwrap();
        let key_pem = std::fs::read_to_string(certs.join("src").join(KEY_FILE)).unwrap();
        s.add_pem("pasted", &cert_pem, &key_pem).unwrap();
        assert!(s.list().iter().any(|c| c.name == "pasted" && c.readable));

        // Garbage, a reserved name, and a duplicate are all refused.
        assert!(s.add_pem("bad", "not a cert", "not a key").is_err());
        assert!(s.add_pem(CONFIG_NAME, &cert_pem, &key_pem).is_err());
        assert!(s.add_pem("pasted", &cert_pem, &key_pem).is_err());
        assert!(!certs.join("bad").exists(), "a rejected pair leaves nothing behind");

        // Delete clears the active selection when it pointed at the removed CA.
        s.activate("pasted").unwrap();
        s.delete("pasted").unwrap();
        assert!(s.active_paths().0.ends_with("config-cert.pem"));

        std::fs::remove_dir_all(certs.parent().unwrap()).ok();
    }
}
