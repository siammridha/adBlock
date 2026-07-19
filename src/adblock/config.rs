//! Adblock's config section (`[adblock]` in the TOML file).

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AdblockConfig {
    pub enabled: bool,
    pub custom_rules: Vec<String>,
    /// Root of the adblock module's on-disk data tree. Adblock keeps downloaded
    /// lists under `blocklists/` and the scriptlet bundle under `scriptlets/`.
    /// Defaults to `data`, the shared default root the other modules use too.
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
    /// Scriptlet resource bundle.
    pub fn scriptlets_dir(&self) -> PathBuf {
        self.data_dir.join("scriptlets")
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
