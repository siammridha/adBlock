//! Which rules Adblock is allowed to act on, and where that choice is kept.
//!
//! One master switch: with it off, Adblock matches nothing and every request
//! passes, which is what the dashboard's "Ad blocking" toggle turns. Then three
//! switches for what a block decision may carry: serving a `$redirect`
//! stand-in body in place of a blocked resource, reporting the `$removeparam`
//! cleaned URL, and adding the `$csp` directives to a page. Three more for what
//! Adblock puts into a page it rewrites: cosmetic CSS, uBO scriptlets, and the
//! live-DOM runtime.
//!
//! All seven belong to Adblock rather than the caller. The caller hands over a
//! request or a response and takes back what Adblock made of it; it never asks
//! for a rule to be applied, so it has nothing to switch off.
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
    pub enabled: Option<bool>,
    pub redirect: Option<bool>,
    pub removeparam: Option<bool>,
    pub csp: Option<bool>,
    pub cosmetic: Option<bool>,
    pub scriptlets: Option<bool>,
    pub runtime: Option<bool>,
}

impl DecisionOverrides {
    pub fn parse(body: &[u8]) -> std::result::Result<Self, String> {
        let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        if !v.is_object() {
            return Err("expected a JSON object".into());
        }
        let flag = |key: &str| -> Option<bool> { v.get(key).and_then(serde_json::Value::as_bool) };
        Ok(Self {
            enabled: flag("enabled"),
            redirect: flag("redirect"),
            removeparam: flag("removeparam"),
            csp: flag("csp"),
            cosmetic: flag("cosmetic"),
            scriptlets: flag("scriptlets"),
            runtime: flag("runtime"),
        })
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct DecisionSettings {
    /// Whether Adblock does anything at all. Off, every request passes.
    pub enabled: bool,
    pub redirect: bool,
    pub removeparam: bool,
    pub csp: bool,
    pub cosmetic: bool,
    pub scriptlets: bool,
    pub runtime: bool,
}

impl DecisionSettings {
    /// Whether anything at all would go into a page. Nothing on means Adblock
    /// has no reason to read a response body.
    pub(crate) fn injects(&self) -> bool {
        self.cosmetic || self.scriptlets || self.runtime
    }
}

pub struct DecisionPolicy {
    enabled: AtomicBool,
    redirect: AtomicBool,
    removeparam: AtomicBool,
    csp: AtomicBool,
    cosmetic: AtomicBool,
    scriptlets: AtomicBool,
    runtime: AtomicBool,
    path: PathBuf,
}

impl DecisionPolicy {
    /// Load the switches, all on by default. An empty path means an in-memory
    /// policy that is never written.
    pub fn load(path: PathBuf) -> Self {
        let saved: DecisionOverrides = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let policy = Self {
            enabled: AtomicBool::new(saved.enabled.unwrap_or(true)),
            redirect: AtomicBool::new(saved.redirect.unwrap_or(true)),
            removeparam: AtomicBool::new(saved.removeparam.unwrap_or(true)),
            csp: AtomicBool::new(saved.csp.unwrap_or(true)),
            cosmetic: AtomicBool::new(saved.cosmetic.unwrap_or(true)),
            scriptlets: AtomicBool::new(saved.scriptlets.unwrap_or(true)),
            runtime: AtomicBool::new(saved.runtime.unwrap_or(true)),
            path,
        };
        // Seed the file a fresh install does not have yet, so every switch is
        // there to edit by hand. Failing to seed it is not worth a warning: the
        // defaults still apply and saving from the UI will report the problem.
        if !policy.path.as_os_str().is_empty() && !policy.path.exists() {
            if let Err(e) = policy.persist() {
                tracing::debug!(error = %e, "seeding adblock settings file");
            }
        }
        policy
    }

    /// Every switch on, with no settings file behind them.
    pub fn all_on() -> Self {
        Self::load(PathBuf::new())
    }

    pub fn settings(&self) -> DecisionSettings {
        DecisionSettings {
            enabled: self.enabled.load(Ordering::Relaxed),
            redirect: self.redirect.load(Ordering::Relaxed),
            removeparam: self.removeparam.load(Ordering::Relaxed),
            csp: self.csp.load(Ordering::Relaxed),
            cosmetic: self.cosmetic.load(Ordering::Relaxed),
            scriptlets: self.scriptlets.load(Ordering::Relaxed),
            runtime: self.runtime.load(Ordering::Relaxed),
        }
    }

    /// Apply an update and persist it. Takes effect on the next request — the
    /// switches are read per decision and per page, nothing is rebuilt.
    pub fn apply(&self, upd: &DecisionOverrides) -> DecisionSettings {
        for (flag, cell) in [
            (upd.enabled, &self.enabled),
            (upd.redirect, &self.redirect),
            (upd.removeparam, &self.removeparam),
            (upd.csp, &self.csp),
            (upd.cosmetic, &self.cosmetic),
            (upd.scriptlets, &self.scriptlets),
            (upd.runtime, &self.runtime),
        ] {
            if let Some(v) = flag {
                cell.store(v, Ordering::Relaxed);
            }
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
                enabled: Some(snap.enabled),
                redirect: Some(snap.redirect),
                removeparam: Some(snap.removeparam),
                csp: Some(snap.csp),
                cosmetic: Some(snap.cosmetic),
                scriptlets: Some(snap.scriptlets),
                runtime: Some(snap.runtime),
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
    fn the_master_switch_turns_off_and_survives_a_reload() {
        let path = std::env::temp_dir()
            .join(format!("adblock-enabled-test-{}", std::process::id()))
            .join("adblock.json");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());

        let policy = DecisionPolicy::load(path.clone());
        assert!(policy.settings().enabled, "blocking is on by default");

        let off = DecisionOverrides { enabled: Some(false), ..Default::default() };
        assert!(!policy.apply(&off).enabled);
        assert!(policy.settings().redirect, "the other switches are left alone");
        assert!(!DecisionPolicy::load(path.clone()).settings().enabled, "and it is remembered");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn parse_picks_up_only_present_flags() {
        let o = DecisionOverrides::parse(br#"{"redirect":false,"cosmetic":false}"#).unwrap();
        assert_eq!(o.enabled, None, "the master switch is left alone too");
        assert_eq!(o.redirect, Some(false));
        assert_eq!(o.cosmetic, Some(false));
        assert_eq!(o.removeparam, None, "an absent key leaves that switch alone");
        assert_eq!(o.csp, None);
        assert_eq!(o.scriptlets, None);
        assert_eq!(o.runtime, None);
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
        assert_eq!(
            (s.redirect, s.removeparam, s.csp, s.cosmetic, s.scriptlets, s.runtime),
            (true, true, true, true, true, true),
            "defaults are on"
        );
        assert!(path.exists(), "a fresh install gets a settings file to edit");

        let snap = policy.apply(&DecisionOverrides {
            redirect: Some(false),
            csp: Some(false),
            runtime: Some(false),
            ..Default::default()
        });
        assert_eq!(
            (snap.redirect, snap.removeparam, snap.csp, snap.cosmetic, snap.runtime),
            (false, true, false, true, false),
            "untouched switch stays"
        );
        assert!(snap.injects(), "cosmetic and scriptlets are still on");

        let reloaded = DecisionPolicy::load(path.clone()).settings();
        assert_eq!(
            (reloaded.redirect, reloaded.removeparam, reloaded.csp, reloaded.runtime),
            (false, true, false, false)
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn nothing_to_inject_when_every_page_switch_is_off() {
        let policy = DecisionPolicy::all_on();
        assert!(policy.settings().injects());
        let snap = policy.apply(&DecisionOverrides {
            cosmetic: Some(false),
            scriptlets: Some(false),
            runtime: Some(false),
            ..Default::default()
        });
        assert!(!snap.injects(), "no page edits left to make");
    }
}
