//! Stats' config section (`[logging]` in the TOML file). The `level` knob is
//! read by the root wiring for tracing setup; the log toggles are stats' own.

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    pub level: String,
    pub log_actions: bool,
    pub log_requests: bool,
    /// Root of the stats module's on-disk data tree; rotating logs live under
    /// `logs/` and the stats settings file under `settings/`. Defaults to `data`,
    /// the shared default root.
    pub data_dir: PathBuf,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            log_actions: true,
            log_requests: true,
            data_dir: PathBuf::from("data"),
        }
    }
}
