//! Proxy's config sections (server, TLS, and performance) and their validation.

use std::path::PathBuf;

use super::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub enabled: bool,
    pub listen: String,
    /// Where the admin web UI listens. The root wiring validates it and hands it
    /// to the web app.
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

    /// The proxy's persisted settings: the egress policy (resolver-only,
    /// IPv6).
    pub fn settings_path(&self) -> PathBuf {
        self.settings_dir().join("proxy-settings.json")
    }

    /// The proxy listener's own settings (enabled/listen overrides).
    pub fn server_settings_path(&self) -> PathBuf {
        self.settings_dir().join("proxy-server.json")
    }
}

#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub ca_cert: PathBuf,
    pub ca_key: PathBuf,
}

#[derive(Debug, Clone)]
pub struct PerformanceConfig {
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
            ca_cert: PathBuf::from("data/certs/ca-cert.pem"),
            ca_key: PathBuf::from("data/certs/ca-key.pem"),
        }
    }
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self { upstream_timeout_ms: 15_000 }
    }
}

/// Proxy's base configuration, grouping the sections Proxy owns.
#[derive(Debug, Clone, Default)]
pub struct ProxyBaseConfig {
    pub server: ServerConfig,
    pub tls: TlsConfig,
    pub performance: PerformanceConfig,
}

impl ProxyBaseConfig {
    /// Build Proxy's base config from built-in defaults. `data_dir` is
    /// root-supplied wiring and always wins for the on-disk data root.
    /// `admin_listen` is validated by the root, not here.
    pub fn load(data_dir: &std::path::Path) -> Result<Self> {
        let cfg = Self {
            server: ServerConfig {
                data_dir: data_dir.to_path_buf(),
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.server.validate()?;
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_uses_defaults_with_root_supplied_data_dir() {
        let dir = std::path::Path::new("some/data/root");
        let cfg = ProxyBaseConfig::load(dir).unwrap();
        let defaults = ServerConfig::default();
        assert_eq!(cfg.server.listen, defaults.listen);
        assert_eq!(cfg.server.enabled, defaults.enabled);
        // data_dir reflects the passed-in root, not the default.
        assert_eq!(cfg.server.data_dir, dir);
    }
}
