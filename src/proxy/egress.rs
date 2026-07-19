//! Outbound connection policy: resolves hosts through the built-in DNS and
//! controls ECH, IPv6, and resolver-only mode.

use std::net::IpAddr;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lru::LruCache;
use serde::{Deserialize, Serialize};

use crate::dns::DnsService;
use crate::support::persist::OverrideStore;

/// How long a resolved host stays in the egress-side cache. This sits in front
/// of the DNS answer cache purely to skip the resolve pipeline (filter match,
/// message clone, query logging) for the burst of connections a page opens to
/// the same host. Kept short so DNS TTL and filter changes are picked up soon;
/// [`EgressPolicy::apply`] clears it outright when settings change.
const RESOLVE_CACHE_TTL: Duration = Duration::from_secs(10);
const RESOLVE_CACHE_CAP: usize = 1024;

/// Tiny per-host cache with a fixed TTL, backed by an LRU so it stays bounded.
struct TtlCache<T> {
    map: Mutex<LruCache<String, (T, Instant)>>,
}

impl<T: Clone> TtlCache<T> {
    fn new(cap: usize) -> Self {
        let cap = NonZeroUsize::new(cap).expect("egress cache capacity is non-zero");
        Self { map: Mutex::new(LruCache::new(cap)) }
    }

    fn get(&self, host: &str) -> Option<T> {
        let mut map = self.map.lock().expect("egress cache lock");
        match map.get(host) {
            Some((v, expires)) if *expires > Instant::now() => Some(v.clone()),
            Some(_) => {
                map.pop(host);
                None
            }
            None => None,
        }
    }

    fn put(&self, host: String, value: T) {
        let mut map = self.map.lock().expect("egress cache lock");
        map.put(host, (value, Instant::now() + RESOLVE_CACHE_TTL));
    }

    fn clear(&self) {
        self.map.lock().expect("egress cache lock").clear();
    }
}

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
    dns: Arc<DnsService>,
    resolver_only: AtomicBool,
    use_ech: AtomicBool,
    disable_ipv6: AtomicBool,
    store: OverrideStore<EgressOverrides>,
    addr_cache: TtlCache<Vec<IpAddr>>,
    ech_cache: TtlCache<Option<Vec<u8>>>,
}

impl EgressPolicy {
    pub fn load(store_path: PathBuf, dns: Arc<DnsService>) -> Arc<Self> {
        let store: OverrideStore<EgressOverrides> = OverrideStore::new(store_path);
        let o = store.load();
        Arc::new(Self {
            dns,
            resolver_only: AtomicBool::new(o.resolver_only.unwrap_or(true)),
            use_ech: AtomicBool::new(o.use_ech.unwrap_or(true)),
            disable_ipv6: AtomicBool::new(o.disable_ipv6.unwrap_or(true)),
            store,
            addr_cache: TtlCache::new(RESOLVE_CACHE_CAP),
            ech_cache: TtlCache::new(RESOLVE_CACHE_CAP),
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
        // Settings like disable_ipv6 and use_ech change what a resolve returns,
        // so drop anything cached under the old settings.
        self.addr_cache.clear();
        self.ech_cache.clear();
        snap
    }

    pub async fn resolve(&self, host: &str) -> std::io::Result<Vec<IpAddr>> {
        if let Some(addrs) = self.addr_cache.get(host) {
            return Ok(addrs);
        }
        let addrs = self.dns.resolve(host, !self.disable_ipv6()).await?;
        self.addr_cache.put(host.to_string(), addrs.clone());
        Ok(addrs)
    }

    pub async fn ech_config_list(&self, host: &str) -> Option<Vec<u8>> {
        if !self.use_ech() {
            return None;
        }
        if let Some(list) = self.ech_cache.get(host) {
            return list;
        }
        let list = self.dns.ech_config_list(host).await;
        self.ech_cache.put(host.to_string(), list.clone());
        list
    }
}
