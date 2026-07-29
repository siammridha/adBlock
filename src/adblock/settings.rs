//! What a block decision is allowed to carry, and where that choice is kept.
//!
//! Two switches: serving a `$redirect` stand-in body in place of a blocked
//! resource, and reporting the `$removeparam` cleaned URL. Both belong to
//! Adblock rather than the caller — the caller asks one thing ("blocked?") and
//! gets these back inside the answer without asking for them, so Adblock is the
//! module that decides whether to offer them.
//!
//! Persisted to Adblock's own settings file. Nothing else writes that file, so
//! saving rewrites it whole.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

/// A settings update. Callers hand over raw bytes and Adblock decides what is
/// valid; only present, well-typed flags are picked up.
#[derive(Debug, Default, Clone, Copy, Deserialize, Serialize)]
pub struct DecisionOverrides {
    pub redirect: Option<bool>,
    pub removeparam: Option<bool>,
}

impl DecisionOverrides {
    pub fn parse(body: &[u8]) -> std::result::Result<Self, String> {
        let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        if !v.is_object() {
            return Err("expected a JSON object".into());
        }
        let flag = |key: &str| -> Option<bool> { v.get(key).and_then(serde_json::Value::as_bool) };
        Ok(Self { redirect: flag("redirect"), removeparam: flag("removeparam") })
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct DecisionSettings {
    pub redirect: bool,
    pub removeparam: bool,
}

pub struct DecisionPolicy {
    redirect: AtomicBool,
    removeparam: AtomicBool,
    path: PathBuf,
}

impl DecisionPolicy {
    /// Load the switches, both on by default. An empty path means an in-memory
    /// policy that is never written.
    pub fn load(path: PathBuf) -> Self {
        let saved: DecisionOverrides = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let policy = Self {
            redirect: AtomicBool::new(saved.redirect.unwrap_or(true)),
            removeparam: AtomicBool::new(saved.removeparam.unwrap_or(true)),
            path,
        };
        // Seed the file a fresh install does not have yet, so both switches are
        // there to edit by hand. Failing to seed it is not worth a warning: the
        // defaults still apply and saving from the UI will report the problem.
        if !policy.path.as_os_str().is_empty() && !policy.path.exists() {
            if let Err(e) = policy.persist() {
                tracing::debug!(error = %e, "seeding adblock settings file");
            }
        }
        policy
    }

    /// Both switches on, with no settings file behind them.
    pub fn all_on() -> Self {
        Self::load(PathBuf::new())
    }

    pub fn settings(&self) -> DecisionSettings {
        DecisionSettings {
            redirect: self.redirect.load(Ordering::Relaxed),
            removeparam: self.removeparam.load(Ordering::Relaxed),
        }
    }

    /// Apply an update and persist it. Takes effect on the next request — both
    /// switches are read per decision, nothing is rebuilt.
    pub fn apply(&self, upd: &DecisionOverrides) -> DecisionSettings {
        if let Some(v) = upd.redirect {
            self.redirect.store(v, Ordering::Relaxed);
        }
        if let Some(v) = upd.removeparam {
            self.removeparam.store(v, Ordering::Relaxed);
        }
        if let Err(e) = self.persist() {
            tracing::warn!(error = %e, "persisting adblock settings");
        }
        self.settings()
    }

    fn persist(&self) -> std::result::Result<(), String> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        let snap = self.settings();
        write(
            &self.path,
            &DecisionOverrides {
                redirect: Some(snap.redirect),
                removeparam: Some(snap.removeparam),
            },
        )
    }
}

fn write(path: &Path, saved: &DecisionOverrides) -> std::result::Result<(), String> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    let body = serde_json::to_string_pretty(saved).map_err(|e| e.to_string())?;
    std::fs::write(path, body).map_err(|e| format!("writing {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_picks_up_only_present_flags() {
        let o = DecisionOverrides::parse(br#"{"redirect":false}"#).unwrap();
        assert_eq!(o.redirect, Some(false));
        assert_eq!(o.removeparam, None, "an absent key leaves that switch alone");
        assert!(DecisionOverrides::parse(b"[]").is_err(), "not an object");
        assert!(DecisionOverrides::parse(b"nonsense").is_err());
    }

    #[test]
    fn apply_persists_and_reloads() {
        let path = std::env::temp_dir()
            .join(format!("adblock-settings-test-{}", std::process::id()))
            .join("adblock.json");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());

        let policy = DecisionPolicy::load(path.clone());
        let s = policy.settings();
        assert_eq!((s.redirect, s.removeparam), (true, true), "defaults are on");
        assert!(path.exists(), "a fresh install gets a settings file to edit");

        let snap = policy.apply(&DecisionOverrides { redirect: Some(false), removeparam: None });
        assert_eq!((snap.redirect, snap.removeparam), (false, true), "untouched switch stays");

        let reloaded = DecisionPolicy::load(path.clone()).settings();
        assert_eq!((reloaded.redirect, reloaded.removeparam), (false, true));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
