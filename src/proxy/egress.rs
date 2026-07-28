//! Outbound connection policy: resolves hosts through the built-in DNS, hands
//! out their ECH configs, and controls IPv6 and resolver-only mode.

use std::net::IpAddr;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lru::LruCache;
use serde::{Deserialize, Serialize};

use crate::dns::api::DnsService;
use crate::proxy::persist::OverrideStore;

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
    pub disable_ipv6: Option<bool>,
}

impl EgressOverrides {
    /// Parse a raw settings update. Callers (the web app) hand bytes here and
    /// render the result; the proxy decides what is valid. Only present,
    /// well-typed flags are picked up.
    pub fn parse(body: &[u8]) -> std::result::Result<Self, String> {
        let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        if !v.is_object() {
            return Err("expected a JSON object".into());
        }
        let flag = |key: &str| -> Option<bool> { v.get(key).and_then(serde_json::Value::as_bool) };
        Ok(EgressOverrides {
            resolver_only: flag("resolver_only"),
            disable_ipv6: flag("disable_ipv6"),
        })
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct EgressSettings {
    pub resolver_only: bool,
    pub disable_ipv6: bool,
}

pub struct EgressPolicy {
    dns: Arc<DnsService>,
    resolver_only: AtomicBool,
    disable_ipv6: AtomicBool,
    store: OverrideStore<EgressOverrides>,
    addr_cache: TtlCache<Vec<IpAddr>>,
    ech_cache: TtlCache<Option<Vec<u8>>>,
}

impl EgressPolicy {
    pub fn load(store_path: PathBuf, dns: Arc<DnsService>) -> Arc<Self> {
        let store: OverrideStore<EgressOverrides> = OverrideStore::new(store_path);
        // On first run, write the full default egress settings; an existing file
        // is used as-is.
        store.ensure(&EgressOverrides {
            resolver_only: Some(true),
            disable_ipv6: Some(true),
        });
        let o = store.load();
        Arc::new(Self {
            dns,
            resolver_only: AtomicBool::new(o.resolver_only.unwrap_or(true)),
            disable_ipv6: AtomicBool::new(o.disable_ipv6.unwrap_or(true)),
            store,
            addr_cache: TtlCache::new(RESOLVE_CACHE_CAP),
            ech_cache: TtlCache::new(RESOLVE_CACHE_CAP),
        })
    }

    pub fn resolver_only(&self) -> bool {
        self.resolver_only.load(Ordering::Relaxed)
    }

    pub fn disable_ipv6(&self) -> bool {
        self.disable_ipv6.load(Ordering::Relaxed)
    }

    pub fn settings(&self) -> EgressSettings {
        EgressSettings {
            resolver_only: self.resolver_only(),
            disable_ipv6: self.disable_ipv6(),
        }
    }

    pub fn apply(&self, upd: &EgressOverrides) -> EgressSettings {
        if let Some(v) = upd.resolver_only {
            self.resolver_only.store(v, Ordering::Relaxed);
        }
        if let Some(v) = upd.disable_ipv6 {
            self.disable_ipv6.store(v, Ordering::Relaxed);
        }
        let snap = self.settings();
        let persisted = EgressOverrides {
            resolver_only: Some(snap.resolver_only),
            disable_ipv6: Some(snap.disable_ipv6),
        };
        if let Err(e) = self.store.save(&persisted) {
            tracing::warn!(error = %e, "persisting proxy egress settings");
        }
        // A setting like disable_ipv6 changes what a resolve returns, so drop
        // anything cached under the old settings.
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
        if let Some(list) = self.ech_cache.get(host) {
            return list;
        }
        let list = self.dns.ech_config_list(host).await;
        self.ech_cache.put(host.to_string(), list.clone());
        list
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    use super::*;
    use crate::adblock::api::{with_store, AdblockConfig, MemoryListStore};
    use crate::dns::api::{DnsConfig, DnsService};
    use crate::stats::api::{LoggingConfig, SharedState, StaticInfo};

    #[test]
    fn proxy_config_parses_present_flags_only() {
        let upd = EgressOverrides::parse(br#"{"resolver_only": true}"#).unwrap();
        assert_eq!(upd.resolver_only, Some(true));
        assert_eq!(upd.disable_ipv6, None);
        let upd = EgressOverrides::parse(br#"{"disable_ipv6": "yes"}"#).unwrap();
        assert_eq!(upd.disable_ipv6, None);
        assert!(EgressOverrides::parse(b"[]").is_err());
    }

    // Build a DNS service reachable only through DNS's public API, backed by a
    // rewrite so it answers without any upstream. We keep the shared state so
    // the test can read the DNS query counter directly.
    fn dns_with_rewrite() -> (Arc<DnsService>, Arc<SharedState>) {
        let adblock_cfg = AdblockConfig {
            enabled: true,
            custom_rules: vec![],
            data_dir: PathBuf::from("/nonexistent-for-tests"),
            auto_update_hours: 0,
            inject_scriptlets: false,
            scriptlet_resources: PathBuf::new(),
        };
        let (adblock, _) = with_store(&adblock_cfg, Arc::new(MemoryListStore::new())).unwrap();
        let state = Arc::new(SharedState::new(
            StaticInfo {
                version: "test".into(),
                listen: String::new(),
                admin_listen: String::new(),
                started: Instant::now(),
            },
            &LoggingConfig {
                level: "info".into(),
                log_actions: true,
                log_requests: true,
                ..Default::default()
            },
        ));
        let cfg = DnsConfig {
            upstreams: vec!["udp://127.0.0.1:1".into()],
            upstream_timeout_ms: 200,
            ..DnsConfig::default()
        };
        let dir = std::env::temp_dir().join("proxy-egress-cache-test");
        let _ = std::fs::remove_dir_all(&dir);
        let dns = DnsService::new(&cfg, &dir, adblock, state.clone()).unwrap();
        dns.rewrites().add("*.lab.example", "10.9.8.7").unwrap();
        (dns, state)
    }

    #[tokio::test]
    async fn resolve_serves_repeats_from_the_egress_cache_until_settings_change() {
        let (dns, state) = dns_with_rewrite();
        let store = std::env::temp_dir().join("proxy-egress-cache-test.json");
        let _ = std::fs::remove_file(&store);
        let egress = EgressPolicy::load(store, dns);

        // First resolve goes through the DNS pipeline.
        let addrs = egress.resolve("app.lab.example").await.unwrap();
        assert_eq!(addrs, vec![IpAddr::V4(std::net::Ipv4Addr::new(10, 9, 8, 7))]);
        let after_first = state.metrics.dns_queries_total.load(Ordering::Relaxed);
        assert!(after_first >= 1);

        // A repeat is served from the egress cache without touching DNS.
        egress.resolve("app.lab.example").await.unwrap();
        assert_eq!(state.metrics.dns_queries_total.load(Ordering::Relaxed), after_first);

        // Applying settings clears the cache, so the next resolve hits DNS again.
        egress.apply(&EgressOverrides::default());
        egress.resolve("app.lab.example").await.unwrap();
        assert!(state.metrics.dns_queries_total.load(Ordering::Relaxed) > after_first);
    }
}
