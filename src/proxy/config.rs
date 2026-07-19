//! Proxy's config sections (`[server]`, `[tls]`, and `[performance]` in the
//! TOML file) and their validation.

use serde::Deserialize;
use std::path::PathBuf;

use super::error::{Error, Result};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub enabled: bool,
    pub listen: String,
    /// Where the admin web UI listens. Stored in this section for config-file
    /// compatibility; the root wiring validates and hands it to the web app.
    pub admin_listen: String,
    /// Root of the proxy's on-disk data tree. Proxy keeps its certificates under
    /// `certs/` and its settings files under `settings/`. Defaults to `data`, the
    /// same root the other modules default to, so the existing layout is shared.
    pub data_dir: PathBuf,
}

impl ServerConfig {
    /// Proxy validates its own listen address; `admin_listen` belongs to the
    /// web app and is checked by the root wiring.
    pub fn validate(&self) -> Result<()> {
        self.listen
            .parse::<std::net::SocketAddr>()
            .map_err(|e| Error::Config(format!("invalid listen '{}': {e}", self.listen)))?;
        Ok(())
    }

    /// Managed CA cert/key pairs, one subfolder per CA.
    pub fn certs_dir(&self) -> PathBuf {
        self.data_dir.join("certs")
    }

    fn settings_dir(&self) -> PathBuf {
        self.data_dir.join("settings")
    }

    /// Records which managed CA is active.
    pub fn active_ca_path(&self) -> PathBuf {
        self.settings_dir().join("active-ca.json")
    }

    /// The MITM exclusion list.
    pub fn exclusions_path(&self) -> PathBuf {
        self.settings_dir().join("excluded-domains.conf")
    }

    /// Persisted egress (resolver-only/ECH/IPv6) settings.
    pub fn egress_settings_path(&self) -> PathBuf {
        self.settings_dir().join("proxy-settings.json")
    }

    /// The proxy listener's own settings (enabled/listen overrides).
    pub fn server_settings_path(&self) -> PathBuf {
        self.settings_dir().join("proxy-server.json")
    }

    /// The pre-split combined settings file, read once to seed the per-service
    /// files when they are missing.
    pub fn legacy_server_settings_path(&self) -> PathBuf {
        self.settings_dir().join("server-settings.json")
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TlsConfig {
    pub ca_cert: PathBuf,
    pub ca_key: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PerformanceConfig {
    pub max_inspect_bytes: usize,
    pub upstream_timeout_ms: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            listen: "127.0.0.1:8080".into(),
            admin_listen: "127.0.0.1:8081".into(),
            data_dir: PathBuf::from("data"),
        }
    }
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            ca_cert: PathBuf::from("ca-cert.pem"),
            ca_key: PathBuf::from("ca-key.pem"),
        }
    }
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            max_inspect_bytes: 4 * 1024 * 1024,
            upstream_timeout_ms: 15_000,
        }
    }
}
