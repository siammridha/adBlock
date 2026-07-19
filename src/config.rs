//! Root config wiring: parses the TOML file and hands each module its own
//! section. Every section's schema and validation belong to the owning
//! module; this file only assembles them.

use serde::Deserialize;

use crate::adblock::AdblockConfig;
use crate::dns::DnsConfig;
use crate::error::{Error, Result};
use crate::proxy::config::{PerformanceConfig, ServerConfig, TlsConfig};
use crate::stats::LoggingConfig;

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

impl Config {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("reading {}: {e}", path.display())))?;
        let cfg: Config =
            toml::from_str(&text).map_err(|e| Error::Config(format!("parsing config: {e}")))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Each module validates its own section; the root only checks the one
    /// wiring-owned knob (`admin_listen`, handed to the web app).
    pub fn validate(&self) -> Result<()> {
        self.server.validate()?;
        if !self.server.admin_listen.is_empty() {
            self.server.admin_listen.parse::<std::net::SocketAddr>().map_err(|e| {
                Error::Config(format!(
                    "invalid admin_listen '{}': {e}",
                    self.server.admin_listen
                ))
            })?;
        }
        self.dns.validate()?;
        Ok(())
    }
}
