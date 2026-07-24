//! Stats' config section (`[logging]`), its loader, and validation. The `level`
//! knob is read by the root wiring for tracing setup; the log toggles are
//! stats' own.

use serde::Deserialize;
use std::path::PathBuf;

use super::error::{Error, Result};

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

impl LoggingConfig {
    /// Load Stats's base config from Stats's own file
    /// (`<data_dir>/settings/stats-base.toml`), else built-in defaults.
    /// `data_dir` is root-supplied wiring and always wins for the on-disk data
    /// root.
    pub fn load(data_dir: &std::path::Path) -> Result<Self> {
        let own = data_dir.join("settings").join("stats-base.toml");
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
        // The base-config file nests settings under `[logging]`; unwrap it.
        let file: BaseFile = toml::from_str(&text)
            .map_err(|e| Error::Config(format!("parsing {}: {e}", path.display())))?;
        Ok(file.logging)
    }

    /// Stats validates its own settings. There are no invalid states today
    /// (level falls back to a default filter in the root's tracing setup, and
    /// the toggles are booleans), so this is the single seam for future checks.
    pub fn validate(&self) -> Result<()> {
        Ok(())
    }
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

/// Wrapper matching the `[logging]` table. Stats's config is a single
/// top-level table, so parsing through this reads its base-config file.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct BaseFile {
    logging: LoggingConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_reads_logging_table_and_ignores_others() {
        let dir = std::env::temp_dir().join("stats-base-cfg-own");
        let _ = std::fs::remove_dir_all(&dir);
        let settings = dir.join("settings");
        std::fs::create_dir_all(&settings).unwrap();
        std::fs::write(
            settings.join("stats-base.toml"),
            r#"
[logging]
level = "debug"
log_requests = false

[server]
listen = "127.0.0.1:9090"
some_unrelated_field = "ignored"
"#,
        )
        .unwrap();

        let cfg = LoggingConfig::load(&dir).unwrap();
        assert_eq!(cfg.level, "debug");
        assert!(!cfg.log_requests);
        // Untouched knob keeps its default.
        assert!(cfg.log_actions);
        // data_dir is root-supplied wiring, not taken from the file.
        assert_eq!(cfg.data_dir, dir);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_falls_back_to_defaults() {
        let dir = std::env::temp_dir().join("stats-base-cfg-missing-xyz");
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = LoggingConfig::load(&dir).unwrap();
        let defaults = LoggingConfig::default();
        assert_eq!(cfg.level, defaults.level);
        assert_eq!(cfg.log_actions, defaults.log_actions);
        assert_eq!(cfg.log_requests, defaults.log_requests);
        // data_dir still reflects the passed-in root.
        assert_eq!(cfg.data_dir, dir);
    }
}
