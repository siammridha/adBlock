//! Config file schema: every TOML section and its defaults.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::support::error::{Error, Result};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub tls: TlsConfig,
    pub adblock: AdblockConfig,
    pub dns: DnsConfig,
    pub logging: LoggingConfig,
    pub performance: PerformanceConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub enabled: bool,
    pub listen: String,
    pub admin_listen: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TlsConfig {
    pub ca_cert: PathBuf,
    pub ca_key: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AdblockConfig {
    pub enabled: bool,
    pub custom_rules: Vec<String>,
    /// Root of the on-disk data tree. Everything the proxy persists lives under
    /// here in one of four subfolders: `blocklists/`, `settings/`, `logs/`, and
    /// `scriptlets/` (plus `certs/` for managed CAs).
    pub data_dir: PathBuf,
    pub auto_update_hours: u64,
    pub inject_scriptlets: bool,
    pub scriptlet_resources: PathBuf,
}

impl AdblockConfig {
    /// Downloaded blocklist files.
    pub fn blocklists_dir(&self) -> PathBuf {
        self.data_dir.join("blocklists")
    }
    /// Persisted settings/overrides (JSON and `.conf` files).
    pub fn settings_dir(&self) -> PathBuf {
        self.data_dir.join("settings")
    }
    /// Rotating request/query/error logs.
    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }
    /// Scriptlet resource bundle.
    pub fn scriptlets_dir(&self) -> PathBuf {
        self.data_dir.join("scriptlets")
    }
    /// Managed CA cert/key pairs, one subfolder per CA.
    pub fn certs_dir(&self) -> PathBuf {
        self.data_dir.join("certs")
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DnsConfig {
    pub enabled: bool,
    pub listen: String,
    pub upstreams: Vec<String>,
    pub upstream_mode: UpstreamMode,
    pub bootstrap: Vec<String>,
    pub cache_size: usize,
    pub min_ttl_secs: u32,
    pub max_ttl_secs: u32,
    pub blocking_mode: BlockingMode,
    pub blocked_ttl_secs: u32,
    pub strip_ech: bool,
    pub ech_probe_domain: String,
    pub log_ipv6: bool,
    pub upstream_timeout_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BlockingMode {
    NullIp,
    Nxdomain,
    Refused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpstreamMode {
    Failover,
    LoadBalance,
    Parallel,
}

impl UpstreamMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Failover => "failover",
            Self::LoadBalance => "load-balance",
            Self::Parallel => "parallel",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    pub level: String,
    pub log_actions: bool,
    pub log_requests: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PerformanceConfig {
    pub max_inspect_bytes: usize,
    pub upstream_timeout_ms: u64,
}

impl Config {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("reading {}: {e}", path.display())))?;
        let cfg: Config =
            toml::from_str(&text).map_err(|e| Error::Config(format!("parsing config: {e}")))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        self.server
            .listen
            .parse::<std::net::SocketAddr>()
            .map_err(|e| Error::Config(format!("invalid listen '{}': {e}", self.server.listen)))?;
        if !self.server.admin_listen.is_empty() {
            self.server.admin_listen.parse::<std::net::SocketAddr>().map_err(|e| {
                Error::Config(format!(
                    "invalid admin_listen '{}': {e}",
                    self.server.admin_listen
                ))
            })?;
        }
        if self.dns.enabled {
            self.dns
                .listen
                .parse::<std::net::SocketAddr>()
                .map_err(|e| {
                    Error::Config(format!("invalid dns.listen '{}': {e}", self.dns.listen))
                })?;
            if self.dns.upstreams.is_empty() {
                return Err(Error::Config("dns.upstreams must not be empty".into()));
            }
        }
        Ok(())
    }
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

impl Default for AdblockConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            custom_rules: Vec::new(),
            data_dir: PathBuf::from("data"),
            auto_update_hours: 24,
            inject_scriptlets: true,
            scriptlet_resources: PathBuf::from("data/scriptlets/scriptlets.json"),
        }
    }
}

impl Default for DnsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            listen: "127.0.0.1:5353".into(),
            upstreams: vec![
                "https://dns.cloudflare.com/dns-query".into(),
                "tls://1.1.1.1".into(),
                "https://dns.google/dns-query".into(),
            ],
            upstream_mode: UpstreamMode::Failover,
            bootstrap: vec!["1.1.1.1:53".into(), "8.8.8.8:53".into()],
            cache_size: 4096,
            min_ttl_secs: 0,
            max_ttl_secs: 86_400,
            blocking_mode: BlockingMode::NullIp,
            blocked_ttl_secs: 10,
            strip_ech: false,
            ech_probe_domain: "crypto.cloudflare.com".into(),
            log_ipv6: false,
            upstream_timeout_ms: 5_000,
        }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            log_actions: true,
            log_requests: true,
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
