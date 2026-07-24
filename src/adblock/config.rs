//! Adblock's config section (`[adblock]`) and its loader.

use serde::Deserialize;
use std::path::PathBuf;

use super::error::{Error, Result};

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

    /// Validate Adblock's own settings. Adblock has no invalid states today:
    /// every field has a usable default and none can be set to a value that
    /// would break the module, so there is currently nothing to reject. This
    /// stays as the single validation seam the loader calls, so future
    /// checkable settings have a home.
    pub fn validate(&self) -> Result<()> {
        Ok(())
    }

    /// Load Adblock's base config from Adblock's own file
    /// (`<data_dir>/settings/adblock-base.toml`), else built-in defaults.
    /// `data_dir` is root-supplied wiring and always wins for the on-disk data
    /// root.
    pub fn load(data_dir: &std::path::Path) -> Result<Self> {
        let own = data_dir.join("settings").join("adblock-base.toml");
        let mut cfg = if own.exists() {
            Self::from_toml_file(&own)?
        } else {
            Self::default()
        };
        cfg.data_dir = data_dir.to_path_buf();
        cfg.validate()?;
        Ok(cfg)
    }

    fn from_toml_file(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("reading {}: {e}", path.display())))?;
        // The base-config file nests settings under `[adblock]`; unwrap it.
        let file: BaseFile = toml::from_str(&text)
            .map_err(|e| Error::Config(format!("parsing {}: {e}", path.display())))?;
        Ok(file.adblock)
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

/// Wrapper matching the `[adblock]` table. Adblock's config is a single
/// top-level table, so parsing through this reads its base-config file.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct BaseFile {
    adblock: AdblockConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_reads_adblock_table_and_ignores_others() {
        let dir = std::env::temp_dir().join("adblock-base-cfg-own");
        let _ = std::fs::remove_dir_all(&dir);
        let settings = dir.join("settings");
        std::fs::create_dir_all(&settings).unwrap();
        std::fs::write(
            settings.join("adblock-base.toml"),
            r#"
[adblock]
enabled = false
auto_update_hours = 6

[server]
listen = "127.0.0.1:9090"
some_unrelated_field = "ignored"
"#,
        )
        .unwrap();

        let cfg = AdblockConfig::load(&dir).unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.auto_update_hours, 6);
        // data_dir is root-supplied wiring, not taken from the file.
        assert_eq!(cfg.data_dir, dir);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_falls_back_to_defaults() {
        let dir = std::env::temp_dir().join("adblock-base-cfg-missing-xyz");
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = AdblockConfig::load(&dir).unwrap();
        let defaults = AdblockConfig::default();
        assert_eq!(cfg.enabled, defaults.enabled);
        assert_eq!(cfg.auto_update_hours, defaults.auto_update_hours);
        assert_eq!(cfg.inject_scriptlets, defaults.inject_scriptlets);
        // data_dir still reflects the passed-in root.
        assert_eq!(cfg.data_dir, dir);
    }
}
