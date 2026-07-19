//! Stats' config section (`[logging]` in the TOML file). The `level` knob is
//! read by the root wiring for tracing setup; the log toggles are stats' own.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    pub level: String,
    pub log_actions: bool,
    pub log_requests: bool,
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
