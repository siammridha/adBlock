//! Stats' logging config, its loader, and validation. The `level` knob is read
//! by the root wiring for tracing setup; the log toggles are stats' own.

use std::path::PathBuf;

use super::error::Result;

#[derive(Debug, Clone)]
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
    /// Build Stats's config from built-in defaults. `data_dir` is root-supplied
    /// wiring and always wins for the on-disk data root.
    pub fn load(data_dir: &std::path::Path) -> Result<Self> {
        let cfg = Self {
            data_dir: data_dir.to_path_buf(),
            ..Default::default()
        };
        cfg.validate()?;
        Ok(cfg)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_uses_defaults_with_root_supplied_data_dir() {
        let dir = std::path::Path::new("some/data/root");
        let cfg = LoggingConfig::load(dir).unwrap();
        let defaults = LoggingConfig::default();
        assert_eq!(cfg.level, defaults.level);
        assert_eq!(cfg.log_actions, defaults.log_actions);
        assert_eq!(cfg.log_requests, defaults.log_requests);
        // data_dir reflects the passed-in root, not the default.
        assert_eq!(cfg.data_dir, dir);
    }
}
