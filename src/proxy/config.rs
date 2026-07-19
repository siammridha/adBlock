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
