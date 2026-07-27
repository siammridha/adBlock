//! What the proxy injects into the HTML pages it forwards: cosmetic CSS and
//! uBO scriptlets. The rules come from Adblock; whether they are injected is a
//! proxy setting, kept here and persisted to the proxy's settings file (which it
//! shares with the egress policy, each writing only its own keys).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::persist::OverrideStore;

/// A settings update. Callers hand over raw bytes and the proxy decides what is
/// valid; only present, well-typed flags are picked up.
#[derive(Debug, Default, Clone, Copy, Deserialize, Serialize)]
pub struct InjectionOverrides {
    pub cosmetic: Option<bool>,
    pub scriptlets: Option<bool>,
}

impl InjectionOverrides {
    pub fn parse(body: &[u8]) -> std::result::Result<Self, String> {
        let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        if !v.is_object() {
            return Err("expected a JSON object".into());
        }
        let flag = |key: &str| -> Option<bool> { v.get(key).and_then(serde_json::Value::as_bool) };
        Ok(Self { cosmetic: flag("cosmetic"), scriptlets: flag("scriptlets") })
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct InjectionSettings {
    pub cosmetic: bool,
    pub scriptlets: bool,
}

pub struct InjectionPolicy {
    cosmetic: AtomicBool,
    scriptlets: AtomicBool,
    store: OverrideStore<InjectionOverrides>,
}

impl InjectionPolicy {
    pub fn load(store_path: PathBuf) -> Arc<Self> {
        let store: OverrideStore<InjectionOverrides> = OverrideStore::new(store_path);
        // Fill in both switches, on, for a file that does not have them yet.
        store.ensure(&InjectionOverrides { cosmetic: Some(true), scriptlets: Some(true) });
        let o = store.load();
        Arc::new(Self {
            cosmetic: AtomicBool::new(o.cosmetic.unwrap_or(true)),
            scriptlets: AtomicBool::new(o.scriptlets.unwrap_or(true)),
            store,
        })
    }

    /// Both switches on, for callers that build a proxy without a settings file.
    pub fn all_on() -> Arc<Self> {
        Self::load(PathBuf::new())
    }

    pub fn cosmetic(&self) -> bool {
        self.cosmetic.load(Ordering::Relaxed)
    }

    pub fn scriptlets(&self) -> bool {
        self.scriptlets.load(Ordering::Relaxed)
    }

    pub fn settings(&self) -> InjectionSettings {
        InjectionSettings { cosmetic: self.cosmetic(), scriptlets: self.scriptlets() }
    }

    /// Apply an update and persist it. Takes effect on the next page — both
    /// switches are read per response, nothing is rebuilt.
    pub fn apply(&self, upd: &InjectionOverrides) -> InjectionSettings {
        if let Some(v) = upd.cosmetic {
            self.cosmetic.store(v, Ordering::Relaxed);
        }
        if let Some(v) = upd.scriptlets {
            self.scriptlets.store(v, Ordering::Relaxed);
        }
        let snap = self.settings();
        let persisted =
            InjectionOverrides { cosmetic: Some(snap.cosmetic), scriptlets: Some(snap.scriptlets) };
        if let Err(e) = self.store.save(&persisted) {
            tracing::warn!(error = %e, "persisting proxy injection settings");
        }
        snap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_picks_up_only_present_flags() {
        let o = InjectionOverrides::parse(br#"{"cosmetic":false}"#).unwrap();
        assert_eq!(o.cosmetic, Some(false));
        assert_eq!(o.scriptlets, None, "an absent key leaves that switch alone");
        assert!(InjectionOverrides::parse(b"[]").is_err(), "not an object");
        assert!(InjectionOverrides::parse(b"nonsense").is_err());
    }

    #[test]
    fn apply_persists_and_reloads() {
        let path = std::env::temp_dir()
            .join(format!("proxy-injection-test-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let policy = InjectionPolicy::load(path.clone());
        assert_eq!((policy.cosmetic(), policy.scriptlets()), (true, true), "defaults are on");

        let snap = policy.apply(&InjectionOverrides { cosmetic: Some(false), scriptlets: None });
        assert_eq!((snap.cosmetic, snap.scriptlets), (false, true), "untouched switch stays");

        let reloaded = InjectionPolicy::load(path.clone());
        assert_eq!((reloaded.cosmetic(), reloaded.scriptlets()), (false, true));
        let _ = std::fs::remove_file(&path);
    }
}
