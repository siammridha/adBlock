//! DNS's config section (`[dns]`), its loader, and validation.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct DnsConfig {
    pub enabled: bool,
    pub listen: String,
    pub upstreams: Vec<String>,
    pub upstream_mode: UpstreamMode,
    pub bootstrap: Vec<String>,
    pub cache_size: usize,
    pub override_min_ttl_secs: u32,
    pub override_max_ttl_secs: u32,
    pub blocking_mode: BlockingMode,
    pub blocked_ttl_secs: u32,
    pub strip_ech: bool,
    pub ech_probe_domain: String,
    pub ech_probe_mins: u32,
    pub log_ipv6: bool,
    pub upstream_timeout_ms: u64,
    /// Root of the DNS module's on-disk data tree; its rewrite and settings files
    /// live under `settings/`. Defaults to `data`, the shared default root.
    pub data_dir: PathBuf,
}

impl DnsConfig {
    /// Where DNS keeps its persisted files (rewrites, settings, listener state).
    pub fn settings_dir(&self) -> PathBuf {
        self.data_dir.join("settings")
    }

    /// The DNS listener's own settings (enabled/listen overrides).
    pub fn server_settings_path(&self) -> PathBuf {
        self.settings_dir().join("dns-server.json")
    }

    /// DNS validates its own section; callers hand it over raw.
    pub fn validate(&self) -> Result<()> {
        if self.enabled {
            self.listen.parse::<std::net::SocketAddr>().map_err(|e| {
                Error::Config(format!("invalid dns.listen '{}': {e}", self.listen))
            })?;
        }
        Ok(())
    }

    /// Build DNS's config from built-in defaults. `data_dir` is root-supplied
    /// wiring and always wins for the on-disk data root.
    pub fn load(data_dir: &std::path::Path) -> Result<Self> {
        let cfg = Self {
            data_dir: data_dir.to_path_buf(),
            ..Default::default()
        };
        cfg.validate()?;
        Ok(cfg)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

impl Default for DnsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // Listen on all interfaces, port 53, so a mapped container port (or a
            // client on the LAN) can reach the resolver. Live UI changes persist to
            // data/settings/dns-server.json and layer over this.
            listen: "0.0.0.0:53".into(),
            // Empty by default; the operator configures upstreams before DNS can
            // forward. With none set the resolver still serves cache, rewrites,
            // and block answers.
            upstreams: Vec::new(),
            upstream_mode: UpstreamMode::Failover,
            bootstrap: vec!["1.1.1.1".into(), "8.8.8.8".into()],
            cache_size: 4096,
            override_min_ttl_secs: 0,
            override_max_ttl_secs: 0,
            blocking_mode: BlockingMode::NullIp,
            blocked_ttl_secs: 10,
            strip_ech: false,
            ech_probe_domain: "crypto.cloudflare.com".into(),
            ech_probe_mins: 60,
            log_ipv6: false,
            upstream_timeout_ms: 5_000,
            data_dir: PathBuf::from("data"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_uses_defaults_with_root_supplied_data_dir() {
        let dir = std::path::Path::new("some/data/root");
        let cfg = DnsConfig::load(dir).unwrap();
        let defaults = DnsConfig::default();
        assert_eq!(cfg.enabled, defaults.enabled);
        assert_eq!(cfg.cache_size, defaults.cache_size);
        assert_eq!(cfg.listen, defaults.listen);
        // data_dir reflects the passed-in root, not the default.
        assert_eq!(cfg.data_dir, dir);
    }
}
