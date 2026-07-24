//! Proxy's config sections (`[server]`, `[tls]`, and `[performance]`) and their
//! validation.

use serde::Deserialize;
use std::path::PathBuf;

use super::error::{Error, Result};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
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
            ca_cert: PathBuf::from("data/certs/ca-cert.pem"),
            ca_key: PathBuf::from("data/certs/ca-key.pem"),
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

/// Proxy's base configuration, grouping the sections Proxy owns, loaded from its
/// base-config file under the data dir.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ProxyBaseConfig {
    pub server: ServerConfig,
    pub tls: TlsConfig,
    pub performance: PerformanceConfig,
}

impl ProxyBaseConfig {
    /// Load Proxy's base config from Proxy's own file
    /// (`<data_dir>/settings/proxy-base.toml`), else built-in defaults.
    /// `data_dir` is root-supplied wiring and always wins for the on-disk data
    /// root. `admin_listen` is validated by the root, not here.
    pub fn load(data_dir: &std::path::Path) -> Result<Self> {
        let own = data_dir.join("settings").join("proxy-base.toml");
        let mut cfg = if own.exists() {
            Self::from_toml_file(&own)?
        } else {
            Self::default()
        };
        cfg.server.data_dir = data_dir.to_path_buf();
        cfg.server.validate()?;
        Ok(cfg)
    }

    fn from_toml_file(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("reading {}: {e}", path.display())))?;
        toml::from_str(&text).map_err(|e| Error::Config(format!("parsing {}: {e}", path.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_reads_own_sections_and_ignores_others() {
        let dir = std::env::temp_dir().join("proxy-base-cfg-own");
        let _ = std::fs::remove_dir_all(&dir);
        let settings = dir.join("settings");
        std::fs::create_dir_all(&settings).unwrap();
        std::fs::write(
            settings.join("proxy-base.toml"),
            r#"
[server]
enabled = false
listen = "127.0.0.1:9090"
admin_listen = "127.0.0.1:9091"

[tls]
ca_cert = "my-ca.pem"
ca_key = "my-key.pem"

[performance]
max_inspect_bytes = 123
upstream_timeout_ms = 456

[adblock]
enabled = true
some_unrelated_field = "ignored"
"#,
        )
        .unwrap();

        let cfg = ProxyBaseConfig::load(&dir).unwrap();
        assert!(!cfg.server.enabled);
        assert_eq!(cfg.server.listen, "127.0.0.1:9090");
        assert_eq!(cfg.server.admin_listen, "127.0.0.1:9091");
        assert_eq!(cfg.tls.ca_cert, PathBuf::from("my-ca.pem"));
        assert_eq!(cfg.tls.ca_key, PathBuf::from("my-key.pem"));
        assert_eq!(cfg.performance.max_inspect_bytes, 123);
        assert_eq!(cfg.performance.upstream_timeout_ms, 456);
        // data_dir is root-supplied wiring, not taken from the file.
        assert_eq!(cfg.server.data_dir, dir);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_falls_back_to_defaults() {
        let dir = std::env::temp_dir().join("proxy-base-cfg-missing-xyz");
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = ProxyBaseConfig::load(&dir).unwrap();
        let defaults = ServerConfig::default();
        assert_eq!(cfg.server.listen, defaults.listen);
        assert_eq!(cfg.server.enabled, defaults.enabled);
        // data_dir still reflects the passed-in root.
        assert_eq!(cfg.server.data_dir, dir);
    }
}
