//! Adblock's config section and its loader.

use std::path::PathBuf;

use super::error::Result;

#[derive(Debug, Clone)]
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
    /// Adblock's own settings file — the switches that can be changed while the
    /// process runs. No other module reads or writes it.
    pub fn settings_path(&self) -> PathBuf {
        self.data_dir.join("settings").join("adblock.json")
    }

    /// Validate Adblock's own settings. Adblock has no invalid states today:
    /// every field has a usable default and none can be set to a value that
    /// would break the module, so there is currently nothing to reject. This
    /// stays as the single validation seam the loader calls, so future
    /// checkable settings have a home.
    pub fn validate(&self) -> Result<()> {
        Ok(())
    }

    /// Build Adblock's config from built-in defaults. `data_dir` is root-supplied
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_uses_defaults_with_root_supplied_data_dir() {
        let dir = std::path::Path::new("some/data/root");
        let cfg = AdblockConfig::load(dir).unwrap();
        let defaults = AdblockConfig::default();
        assert_eq!(cfg.enabled, defaults.enabled);
        assert_eq!(cfg.auto_update_hours, defaults.auto_update_hours);
        assert_eq!(cfg.inject_scriptlets, defaults.inject_scriptlets);
        // data_dir reflects the passed-in root, not the default.
        assert_eq!(cfg.data_dir, dir);
    }
}
