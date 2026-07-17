//! Outbound connection policy: resolves hosts through the built-in DNS and
//! controls ECH, IPv6, and resolver-only mode.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::dns::DnsService;
use crate::support::persist::OverrideStore;

pub type DnsSlot = Arc<RwLock<Option<Arc<DnsService>>>>;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EgressOverrides {
    pub resolver_only: Option<bool>,
    pub use_ech: Option<bool>,
    pub disable_ipv6: Option<bool>,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct EgressSettings {
    pub resolver_only: bool,
    pub use_ech: bool,
    pub disable_ipv6: bool,
}

pub struct EgressPolicy {
    dns: DnsSlot,
    resolver_only: AtomicBool,
    use_ech: AtomicBool,
    disable_ipv6: AtomicBool,
    store: OverrideStore<EgressOverrides>,
}

impl EgressPolicy {
    pub fn load(store_path: PathBuf, dns: DnsSlot) -> Arc<Self> {
        let store: OverrideStore<EgressOverrides> = OverrideStore::new(store_path);
        let o = store.load();
        Arc::new(Self {
            dns,
            resolver_only: AtomicBool::new(o.resolver_only.unwrap_or(true)),
            use_ech: AtomicBool::new(o.use_ech.unwrap_or(true)),
            disable_ipv6: AtomicBool::new(o.disable_ipv6.unwrap_or(true)),
            store,
        })
    }

    pub fn resolver_only(&self) -> bool {
        self.resolver_only.load(Ordering::Relaxed)
    }

    pub fn use_ech(&self) -> bool {
        self.use_ech.load(Ordering::Relaxed)
    }

    pub fn disable_ipv6(&self) -> bool {
        self.disable_ipv6.load(Ordering::Relaxed)
    }

    pub fn settings(&self) -> EgressSettings {
        EgressSettings {
            resolver_only: self.resolver_only(),
            use_ech: self.use_ech(),
            disable_ipv6: self.disable_ipv6(),
        }
    }

    pub fn apply(&self, upd: &EgressOverrides) -> EgressSettings {
        if let Some(v) = upd.resolver_only {
            self.resolver_only.store(v, Ordering::Relaxed);
        }
        if let Some(v) = upd.use_ech {
            self.use_ech.store(v, Ordering::Relaxed);
        }
        if let Some(v) = upd.disable_ipv6 {
            self.disable_ipv6.store(v, Ordering::Relaxed);
        }
        let snap = self.settings();
        let persisted = EgressOverrides {
            resolver_only: Some(snap.resolver_only),
            use_ech: Some(snap.use_ech),
            disable_ipv6: Some(snap.disable_ipv6),
        };
        if let Err(e) = self.store.save(&persisted) {
            tracing::warn!(error = %e, "persisting proxy egress settings");
        }
        snap
    }

    pub async fn resolve(&self, host: &str) -> std::io::Result<Vec<IpAddr>> {
        let dns = self.dns.read().expect("egress dns lock").clone();
        let Some(dns) = dns else {
            return Err(std::io::Error::other(
                "resolver-only egress is on but the built-in DNS resolver is unavailable",
            ));
        };
        dns.resolve(host, !self.disable_ipv6()).await
    }

    pub async fn ech_config_list(&self, host: &str) -> Option<Vec<u8>> {
        if !self.use_ech() {
            return None;
        }
        let dns = self.dns.read().expect("egress dns lock").clone()?;
        dns.ech_config_list(host).await
    }
}
