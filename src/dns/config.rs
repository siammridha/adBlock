//! DNS's config section (`[dns]` in the TOML file) and its validation.

use serde::{Deserialize, Serialize};

use super::error::{Error, Result};

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

impl DnsConfig {
    /// DNS validates its own section; callers hand it over raw.
    pub fn validate(&self) -> Result<()> {
        if self.enabled {
            self.listen.parse::<std::net::SocketAddr>().map_err(|e| {
                Error::Config(format!("invalid dns.listen '{}': {e}", self.listen))
            })?;
            if self.upstreams.is_empty() {
                return Err(Error::Config("dns.upstreams must not be empty".into()));
            }
        }
        Ok(())
    }
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
