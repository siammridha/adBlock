//! Runtime control of the proxy and DNS listeners: start, stop, and
//! reconfigure them without restarting the process.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::dns::{DnsHandles, DnsService};
use crate::support::error::{Error, Result};
use crate::support::persist::OverrideStore;
use crate::proxy::Proxy;
use crate::stats::{EventKind, SharedState};

type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

pub trait ProxyControl: Send + Sync {
    fn bind_and_serve(&self, listen: SocketAddr) -> BoxFuture<'_, Result<JoinHandle<()>>>;
}

impl ProxyControl for Proxy {
    fn bind_and_serve(&self, listen: SocketAddr) -> BoxFuture<'_, Result<JoinHandle<()>>> {
        let proxy = self.clone();
        Box::pin(async move {
            let listener = Proxy::bind(listen).await?;
            Ok(tokio::spawn(proxy.accept_loop(listener)))
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerOverrides {
    pub proxy_enabled: Option<bool>,
    pub proxy_listen: Option<String>,
    pub dns_enabled: Option<bool>,
    pub dns_listen: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ServerStatus {
    pub proxy_enabled: bool,
    pub proxy_listen: String,
    pub proxy_running: bool,
    pub proxy_controllable: bool,
    pub dns_enabled: bool,
    pub dns_listen: String,
    pub dns_running: bool,
}

struct Inner {
    proxy: Option<Arc<dyn ProxyControl>>,
    proxy_enabled: bool,
    proxy_listen: SocketAddr,
    proxy_task: Option<JoinHandle<()>>,

    dns_enabled: bool,
    dns_listen: SocketAddr,
    dns_handles: Option<DnsHandles>,

    overrides: ServerOverrides,
}

pub struct Runtime {
    state: Arc<SharedState>,
    store: OverrideStore<ServerOverrides>,
    // The DNS service outlives its listeners: disabling DNS only stops the
    // UDP/TCP servers, in-process resolution (the proxy's egress) keeps going.
    dns: Arc<DnsService>,
    inner: Mutex<Inner>,
}

impl Runtime {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        state: Arc<SharedState>,
        store_path: PathBuf,
        proxy: Option<Arc<dyn ProxyControl>>,
        proxy_cfg_listen: &str,
        proxy_cfg_enabled: bool,
        dns: Arc<DnsService>,
        dns_cfg_listen: &str,
        dns_cfg_enabled: bool,
    ) -> Result<Arc<Self>> {
        let store: OverrideStore<ServerOverrides> = OverrideStore::new(store_path);
        let overrides = store.load();

        let proxy_listen_s = overrides
            .proxy_listen
            .clone()
            .unwrap_or_else(|| proxy_cfg_listen.to_string());
        let proxy_listen = proxy_listen_s.parse().map_err(|e| {
            Error::Config(format!("invalid proxy listen '{proxy_listen_s}': {e}"))
        })?;
        let proxy_enabled = overrides.proxy_enabled.unwrap_or(proxy_cfg_enabled);

        let dns_listen_s = overrides
            .dns_listen
            .clone()
            .unwrap_or_else(|| dns_cfg_listen.to_string());
        let dns_listen = dns_listen_s
            .parse()
            .map_err(|e| Error::Config(format!("invalid dns listen '{dns_listen_s}': {e}")))?;
        let dns_enabled = overrides.dns_enabled.unwrap_or(dns_cfg_enabled);

        Ok(Arc::new(Self {
            state,
            store,
            dns,
            inner: Mutex::new(Inner {
                proxy,
                proxy_enabled,
                proxy_listen,
                proxy_task: None,
                dns_enabled,
                dns_listen,
                dns_handles: None,
                overrides,
            }),
        }))
    }

    pub async fn start_initial(self: &Arc<Self>) -> Result<()> {
        let mut inner = self.inner.lock().await;
        if inner.proxy_enabled {
            if let Some(proxy) = inner.proxy.clone() {
                inner.proxy_task = Some(proxy.bind_and_serve(inner.proxy_listen).await?);
            }
        }
        if inner.dns_enabled {
            match self.dns.start(inner.dns_listen).await {
                Ok(h) => inner.dns_handles = Some(h),
                Err(e) => {
                    tracing::error!(error = %e, "dns server");
                    self.state
                        .log_event(EventKind::Error, format!("dns server: {e}"));
                }
            }
        }
        Ok(())
    }

    pub fn dns(&self) -> Arc<DnsService> {
        self.dns.clone()
    }

    pub async fn status(&self) -> ServerStatus {
        status_of(&*self.inner.lock().await)
    }

    pub async fn apply(
        self: &Arc<Self>,
        upd: ServerOverrides,
    ) -> std::result::Result<ServerStatus, String> {
        let new_proxy_listen = parse_opt_addr("proxy", &upd.proxy_listen)?;
        let new_dns_listen = parse_opt_addr("dns", &upd.dns_listen)?;

        let mut inner = self.inner.lock().await;

        let want_proxy_enabled = upd.proxy_enabled.unwrap_or(inner.proxy_enabled);
        let want_proxy_listen = new_proxy_listen.unwrap_or(inner.proxy_listen);
        if want_proxy_enabled != inner.proxy_enabled || want_proxy_listen != inner.proxy_listen {
            self.apply_proxy(&mut inner, want_proxy_enabled, want_proxy_listen)
                .await?;
        }

        let want_dns_enabled = upd.dns_enabled.unwrap_or(inner.dns_enabled);
        let want_dns_listen = new_dns_listen.unwrap_or(inner.dns_listen);
        if want_dns_enabled != inner.dns_enabled || want_dns_listen != inner.dns_listen {
            self.apply_dns(&mut inner, want_dns_enabled, want_dns_listen).await?;
        }

        Ok(status_of(&inner))
    }

    async fn apply_proxy(
        &self,
        inner: &mut Inner,
        enabled: bool,
        listen: SocketAddr,
    ) -> std::result::Result<(), String> {
        let Some(proxy) = inner.proxy.clone() else {
            return Err("proxy control is unavailable".into());
        };
        if let Some(task) = inner.proxy_task.take() {
            task.abort();
            let _ = task.await;
        }
        let was = inner.proxy_enabled.then_some(inner.proxy_listen);
        let task = if enabled {
            match proxy.bind_and_serve(listen).await {
                Ok(task) => Some(task),
                Err(e) => {
                    inner.proxy_enabled = false;
                    self.state
                        .log_event(EventKind::Error, format!("proxy bind {listen}: {e}"));
                    return Err(e.to_string());
                }
            }
        } else {
            None
        };
        inner.proxy_task = task;
        inner.proxy_enabled = enabled;
        inner.proxy_listen = listen;
        inner.overrides.proxy_enabled = Some(enabled);
        inner.overrides.proxy_listen = Some(listen.to_string());
        self.persist(inner);
        self.transition_note("proxy", was, enabled.then_some(listen));
        Ok(())
    }

    async fn apply_dns(
        &self,
        inner: &mut Inner,
        enabled: bool,
        listen: SocketAddr,
    ) -> std::result::Result<(), String> {
        let was = inner.dns_enabled.then_some(inner.dns_listen);
        if let Some(h) = inner.dns_handles.take() {
            h.shutdown().await;
        }
        let handles = if enabled {
            match self.dns.start(listen).await {
                Ok(h) => Some(h),
                Err(e) => {
                    inner.dns_enabled = false;
                    self.state
                        .log_event(EventKind::Error, format!("dns bind {listen}: {e}"));
                    return Err(e.to_string());
                }
            }
        } else {
            None
        };
        inner.dns_handles = handles;
        inner.dns_enabled = enabled;
        inner.dns_listen = listen;
        inner.overrides.dns_enabled = Some(enabled);
        inner.overrides.dns_listen = Some(listen.to_string());
        self.persist(inner);
        self.transition_note("dns", was, enabled.then_some(listen));
        Ok(())
    }

    fn transition_note(&self, kind: &str, was: Option<SocketAddr>, now: Option<SocketAddr>) {
        let msg = match (was, now) {
            (None, Some(a)) => format!("{kind} enabled on {a}"),
            (Some(_), None) => format!("{kind} disabled"),
            (Some(p), Some(n)) if p != n => format!("{kind} re-bound to {n}"),
            _ => return,
        };
        tracing::info!("{msg}");
        self.state.log_event(EventKind::Info, msg);
    }

    fn persist(&self, inner: &Inner) {
        if let Err(e) = self.store.save(&inner.overrides) {
            tracing::warn!(error = %e, "persisting server settings");
            self.state
                .log_event(EventKind::Error, format!("saving server settings: {e}"));
        }
    }
}

fn status_of(inner: &Inner) -> ServerStatus {
    ServerStatus {
        proxy_enabled: inner.proxy_enabled,
        proxy_listen: inner.proxy_listen.to_string(),
        proxy_running: inner.proxy_task.is_some(),
        proxy_controllable: inner.proxy.is_some(),
        dns_enabled: inner.dns_enabled,
        dns_listen: inner.dns_listen.to_string(),
        dns_running: inner.dns_handles.is_some(),
    }
}

fn parse_opt_addr(
    which: &str,
    s: &Option<String>,
) -> std::result::Result<Option<SocketAddr>, String> {
    match s {
        Some(s) => s
            .trim()
            .parse::<SocketAddr>()
            .map(Some)
            .map_err(|e| format!("invalid {which} listen '{}': {e}", s.trim())),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adblock::MemoryListStore;
    use crate::support::config::{AdblockConfig, DnsConfig, LoggingConfig};
    use crate::stats::StaticInfo;
    use std::path::Path;

    struct TcpBinder;

    impl ProxyControl for TcpBinder {
        fn bind_and_serve(
            &self,
            listen: SocketAddr,
        ) -> BoxFuture<'_, Result<JoinHandle<()>>> {
            Box::pin(async move {
                let listener = tokio::net::TcpListener::bind(listen)
                    .await
                    .map_err(|e| Error::Config(format!("binding proxy {listen}: {e}")))?;
                Ok(tokio::spawn(async move {
                    loop {
                        let _ = listener.accept().await;
                    }
                }))
            })
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("proxy-runtime-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn runtime_in(dir: &Path, cfg_enabled: bool) -> Arc<Runtime> {
        runtime_with(dir, cfg_enabled, &[])
    }

    fn runtime_with(dir: &Path, cfg_enabled: bool, rules: &[&str]) -> Arc<Runtime> {
        let cfg = AdblockConfig {
            enabled: true,
            custom_rules: rules.iter().map(|s| s.to_string()).collect(),
            data_dir: PathBuf::from("/nonexistent-for-tests"),
            auto_update_hours: 0,
            inject_scriptlets: false,
            scriptlet_resources: PathBuf::new(),
        };
        let (adblock, _curation) =
            crate::adblock::with_store(&cfg, Arc::new(MemoryListStore::new())).unwrap();
        let state = Arc::new(SharedState::new(
            StaticInfo {
                version: "test".into(),
                listen: String::new(),
                admin_listen: String::new(),
                ca_pem: String::new(),
                started: std::time::Instant::now(),
            },
            &LoggingConfig { level: "info".into(), log_actions: true, log_requests: true },
        ));
        let dns_cfg = DnsConfig::default();
        let dns =
            DnsService::new(&dns_cfg, dir, adblock, state.clone()).unwrap();
        Runtime::new(
            state,
            dir.join("server-settings.json"),
            Some(Arc::new(TcpBinder)),
            "127.0.0.1:0",
            cfg_enabled,
            dns,
            &dns_cfg.listen,
            false,
        )
        .unwrap()
    }

    fn free_addr() -> SocketAddr {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap()
    }

    fn update(f: impl FnOnce(&mut ServerOverrides)) -> ServerOverrides {
        let mut upd = ServerOverrides::default();
        f(&mut upd);
        upd
    }

    #[tokio::test]
    async fn proxy_enable_rebind_disable_realize_then_persist() {
        let dir = temp_dir("apply");
        let rt = runtime_in(&dir, false);

        let addr_a = free_addr();
        let status = rt
            .apply(update(|u| {
                u.proxy_enabled = Some(true);
                u.proxy_listen = Some(addr_a.to_string());
            }))
            .await
            .unwrap();
        assert!(status.proxy_running && status.proxy_enabled);
        assert_eq!(status.proxy_listen, addr_a.to_string());
        assert!(std::net::TcpStream::connect(addr_a).is_ok());

        let addr_b = free_addr();
        let status = rt
            .apply(update(|u| u.proxy_listen = Some(addr_b.to_string())))
            .await
            .unwrap();
        assert!(status.proxy_running);
        assert_eq!(status.proxy_listen, addr_b.to_string());
        assert!(std::net::TcpStream::connect(addr_b).is_ok());
        assert!(std::net::TcpStream::connect(addr_a).is_err(), "old port must be freed");

        let status =
            rt.apply(update(|u| u.proxy_enabled = Some(false))).await.unwrap();
        assert!(!status.proxy_running && !status.proxy_enabled);
        assert!(std::net::TcpStream::connect(addr_b).is_err());
        let reloaded = runtime_in(&dir, true);
        let status = reloaded.status().await;
        assert!(!status.proxy_enabled, "persisted disable must beat config.toml");
        assert_eq!(status.proxy_listen, addr_b.to_string());
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn proxy_bind_failure_leaves_service_down_and_previous_config_on_disk() {
        let dir = temp_dir("rollback");
        let rt = runtime_in(&dir, false);

        let good = free_addr();
        rt.apply(update(|u| {
            u.proxy_enabled = Some(true);
            u.proxy_listen = Some(good.to_string());
        }))
        .await
        .unwrap();

        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let err = rt
            .apply(update(|u| u.proxy_listen = Some(occupied.local_addr().unwrap().to_string())))
            .await
            .unwrap_err();
        assert!(err.contains("binding proxy"), "err: {err}");

        let status = rt.status().await;
        assert!(!status.proxy_enabled && !status.proxy_running);
        assert!(std::net::TcpStream::connect(good).is_err());

        let reloaded = runtime_in(&dir, false);
        let status = reloaded.status().await;
        assert!(status.proxy_enabled, "previous enable must survive on disk");
        assert_eq!(status.proxy_listen, good.to_string());
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn overlapping_rebind_reuses_the_port() {
        let dir = temp_dir("overlap");
        let rt = runtime_in(&dir, false);
        let addr = free_addr();
        rt.apply(update(|u| {
            u.proxy_enabled = Some(true);
            u.proxy_listen = Some(addr.to_string());
        }))
        .await
        .unwrap();
        let wide = format!("0.0.0.0:{}", addr.port());
        let status =
            rt.apply(update(|u| u.proxy_listen = Some(wide.clone()))).await.unwrap();
        assert!(status.proxy_running);
        assert_eq!(status.proxy_listen, wide);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn dns_disable_stops_listener_but_resolver_keeps_answering() {
        use hickory_proto::op::{Message, Query};
        use hickory_proto::rr::rdata::A;
        use hickory_proto::rr::{Name, RData, RecordType};
        use std::str::FromStr;

        let dir = temp_dir("dns-off");
        let rt = runtime_with(&dir, false, &["||ads.example.com^"]);

        let addr = free_addr();
        let status = rt
            .apply(update(|u| {
                u.dns_enabled = Some(true);
                u.dns_listen = Some(addr.to_string());
            }))
            .await
            .unwrap();
        assert!(status.dns_running && status.dns_enabled);

        let before = rt.dns();
        let status = rt.apply(update(|u| u.dns_enabled = Some(false))).await.unwrap();
        assert!(!status.dns_running && !status.dns_enabled);

        // Same service instance: the proxy's egress handle stays valid and the
        // resolver keeps answering in-process queries with the listener down.
        let after = rt.dns();
        assert!(Arc::ptr_eq(&before, &after));
        let mut msg = Message::query();
        msg.metadata.id = 7;
        msg.metadata.recursion_desired = true;
        msg.add_query(Query::query(
            Name::from_str("ads.example.com.").unwrap(),
            RecordType::A,
        ));
        let resp = after.handle_proxy(&msg).await;
        assert_eq!(
            resp.answers[0].data,
            RData::A(A(std::net::Ipv4Addr::UNSPECIFIED)),
            "blocked domain must still get the null-IP answer while the listener is off"
        );
        std::fs::remove_dir_all(dir).ok();
    }
}
