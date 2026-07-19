//! Adblock's config section (`[adblock]` in the TOML file).

use serde::Deserialize;
use std::path::PathBuf;

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
