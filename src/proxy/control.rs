//! Settings-driven lifecycle for the proxy listener. Applying a settings
//! update is what starts, stops, or rebinds the listener; callers only submit
//! raw input and render the result.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::support::error::Result;
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

/// Persisted overrides for the proxy listener (its own settings file).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxyServerOverrides {
    pub enabled: Option<bool>,
    pub listen: Option<String>,
}

impl ProxyServerOverrides {
    /// Parse a raw settings update. The update shares one JSON body with the
    /// DNS server settings, so only the `proxy_*` keys belong to this module.
    pub fn parse(body: &[u8]) -> std::result::Result<Self, String> {
        let v: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        if !v.is_object() {
            return Err("expected a JSON object".into());
        }
        Ok(Self {
            enabled: v.get("proxy_enabled").and_then(Value::as_bool),
            listen: v
                .get("proxy_listen")
                .and_then(Value::as_str)
                .map(|s| s.trim().to_string()),
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ProxyServerStatus {
    pub enabled: bool,
    pub listen: String,
    pub running: bool,
    pub controllable: bool,
}

struct Inner {
    control: Option<Arc<dyn ProxyControl>>,
    enabled: bool,
    listen: SocketAddr,
    task: Option<JoinHandle<()>>,
    overrides: ProxyServerOverrides,
}

/// The proxy listener's runtime: owns the enabled/listen settings, their
/// persistence, and the running accept task.
pub struct ProxyRuntime {
    state: Arc<SharedState>,
    store: OverrideStore<ProxyServerOverrides>,
    inner: Mutex<Inner>,
}

impl ProxyRuntime {
    /// `legacy_store_path` points at the old combined `server-settings.json`;
    /// it is read once when the proxy's own settings file does not exist yet.
    pub fn new(
        state: Arc<SharedState>,
        store_path: PathBuf,
        legacy_store_path: Option<PathBuf>,
        control: Option<Arc<dyn ProxyControl>>,
        cfg_listen: &str,
        cfg_enabled: bool,
    ) -> Result<Arc<Self>> {
        let store: OverrideStore<ProxyServerOverrides> = OverrideStore::new(store_path.clone());
        let overrides = if store_path.exists() {
            store.load()
        } else {
            legacy_overrides(legacy_store_path.as_deref(), "proxy_enabled", "proxy_listen")
        };

        let listen_s = overrides
            .listen
            .clone()
            .unwrap_or_else(|| cfg_listen.to_string());
        let listen = listen_s.parse().map_err(|e| {
            crate::support::error::Error::Config(format!("invalid proxy listen '{listen_s}': {e}"))
        })?;
        let enabled = overrides.enabled.unwrap_or(cfg_enabled);

        Ok(Arc::new(Self {
            state,
            store,
            inner: Mutex::new(Inner { control, enabled, listen, task: None, overrides }),
        }))
    }

    pub async fn start_initial(self: &Arc<Self>) -> Result<()> {
        let mut inner = self.inner.lock().await;
        if inner.enabled {
            if let Some(control) = inner.control.clone() {
                inner.task = Some(control.bind_and_serve(inner.listen).await?);
            }
        }
        Ok(())
    }

    pub async fn status(&self) -> ProxyServerStatus {
        status_of(&*self.inner.lock().await)
    }

    /// Parse, validate, apply, and persist a raw settings update. Enabling,
    /// disabling, or changing the listen address takes effect immediately.
    pub async fn apply_raw(
        self: &Arc<Self>,
        body: &[u8],
    ) -> std::result::Result<ProxyServerStatus, String> {
        let upd = ProxyServerOverrides::parse(body)?;
        let new_listen = match &upd.listen {
            Some(s) => Some(
                s.parse::<SocketAddr>()
                    .map_err(|e| format!("invalid proxy listen '{s}': {e}"))?,
            ),
            None => None,
        };

        let mut inner = self.inner.lock().await;
        let want_enabled = upd.enabled.unwrap_or(inner.enabled);
        let want_listen = new_listen.unwrap_or(inner.listen);
        if want_enabled != inner.enabled || want_listen != inner.listen {
            self.apply(&mut inner, want_enabled, want_listen).await?;
        }
        Ok(status_of(&inner))
    }

    async fn apply(
        &self,
        inner: &mut Inner,
        enabled: bool,
        listen: SocketAddr,
    ) -> std::result::Result<(), String> {
        let Some(control) = inner.control.clone() else {
            return Err("proxy control is unavailable".into());
        };
        if let Some(task) = inner.task.take() {
            task.abort();
            let _ = task.await;
        }
        let was = inner.enabled.then_some(inner.listen);
        let task = if enabled {
            match control.bind_and_serve(listen).await {
                Ok(task) => Some(task),
                Err(e) => {
                    inner.enabled = false;
                    self.state
                        .log_event(EventKind::Error, format!("proxy bind {listen}: {e}"));
                    return Err(e.to_string());
                }
            }
        } else {
            None
        };
        inner.task = task;
        inner.enabled = enabled;
        inner.listen = listen;
        inner.overrides.enabled = Some(enabled);
        inner.overrides.listen = Some(listen.to_string());
        if let Err(e) = self.store.save(&inner.overrides) {
            tracing::warn!(error = %e, "persisting proxy server settings");
            self.state
                .log_event(EventKind::Error, format!("saving proxy server settings: {e}"));
        }
        transition_note(&self.state, "proxy", was, enabled.then_some(listen));
        Ok(())
    }
}

fn status_of(inner: &Inner) -> ProxyServerStatus {
    ProxyServerStatus {
        enabled: inner.enabled,
        listen: inner.listen.to_string(),
        running: inner.task.is_some(),
        controllable: inner.control.is_some(),
    }
}

pub(crate) fn transition_note(
    state: &SharedState,
    kind: &str,
    was: Option<SocketAddr>,
    now: Option<SocketAddr>,
) {
    let msg = match (was, now) {
        (None, Some(a)) => format!("{kind} enabled on {a}"),
        (Some(_), None) => format!("{kind} disabled"),
        (Some(p), Some(n)) if p != n => format!("{kind} re-bound to {n}"),
        _ => return,
    };
    tracing::info!("{msg}");
    state.log_event(EventKind::Info, msg);
}

/// Read this module's keys out of the old combined `server-settings.json`.
fn legacy_overrides(
    path: Option<&std::path::Path>,
    enabled_key: &str,
    listen_key: &str,
) -> ProxyServerOverrides {
    let Some(path) = path else {
        return ProxyServerOverrides::default();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return ProxyServerOverrides::default();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return ProxyServerOverrides::default();
    };
    ProxyServerOverrides {
        enabled: v.get(enabled_key).and_then(Value::as_bool),
        listen: v
            .get(listen_key)
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::config::LoggingConfig;
    use crate::support::error::Error;
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
        let dir = std::env::temp_dir().join(format!("proxy-control-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn state() -> Arc<SharedState> {
        Arc::new(SharedState::new(
            StaticInfo {
                version: "test".into(),
                listen: String::new(),
                admin_listen: String::new(),
                ca_pem: String::new(),
                started: std::time::Instant::now(),
            },
            &LoggingConfig { level: "info".into(), log_actions: true, log_requests: true },
        ))
    }

    fn runtime_in(dir: &Path, cfg_enabled: bool) -> Arc<ProxyRuntime> {
        ProxyRuntime::new(
            state(),
            dir.join("proxy-server.json"),
            Some(dir.join("server-settings.json")),
            Some(Arc::new(TcpBinder)),
            "127.0.0.1:0",
            cfg_enabled,
        )
        .unwrap()
    }

    fn free_addr() -> SocketAddr {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap()
    }

    fn body(json: &str) -> Vec<u8> {
        json.as_bytes().to_vec()
    }

    #[tokio::test]
    async fn enable_rebind_disable_realize_then_persist() {
        let dir = temp_dir("apply");
        let rt = runtime_in(&dir, false);

        let addr_a = free_addr();
        let status = rt
            .apply_raw(&body(&format!(
                r#"{{"proxy_enabled": true, "proxy_listen": "{addr_a}"}}"#
            )))
            .await
            .unwrap();
        assert!(status.running && status.enabled);
        assert_eq!(status.listen, addr_a.to_string());
        assert!(std::net::TcpStream::connect(addr_a).is_ok());

        let addr_b = free_addr();
        let status = rt
            .apply_raw(&body(&format!(r#"{{"proxy_listen": "{addr_b}"}}"#)))
            .await
            .unwrap();
        assert!(status.running);
        assert_eq!(status.listen, addr_b.to_string());
        assert!(std::net::TcpStream::connect(addr_b).is_ok());
        assert!(std::net::TcpStream::connect(addr_a).is_err(), "old port must be freed");

        let status = rt.apply_raw(&body(r#"{"proxy_enabled": false}"#)).await.unwrap();
        assert!(!status.running && !status.enabled);
        assert!(std::net::TcpStream::connect(addr_b).is_err());
        let reloaded = runtime_in(&dir, true);
        let status = reloaded.status().await;
        assert!(!status.enabled, "persisted disable must beat config.toml");
        assert_eq!(status.listen, addr_b.to_string());
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn bind_failure_leaves_service_down_and_previous_config_on_disk() {
        let dir = temp_dir("rollback");
        let rt = runtime_in(&dir, false);

        let good = free_addr();
        rt.apply_raw(&body(&format!(
            r#"{{"proxy_enabled": true, "proxy_listen": "{good}"}}"#
        )))
        .await
        .unwrap();

        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let err = rt
            .apply_raw(&body(&format!(
                r#"{{"proxy_listen": "{}"}}"#,
                occupied.local_addr().unwrap()
            )))
            .await
            .unwrap_err();
        assert!(err.contains("binding proxy"), "err: {err}");

        let status = rt.status().await;
        assert!(!status.enabled && !status.running);
        assert!(std::net::TcpStream::connect(good).is_err());

        let reloaded = runtime_in(&dir, false);
        let status = reloaded.status().await;
        assert!(status.enabled, "previous enable must survive on disk");
        assert_eq!(status.listen, good.to_string());
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn overlapping_rebind_reuses_the_port() {
        let dir = temp_dir("overlap");
        let rt = runtime_in(&dir, false);
        let addr = free_addr();
        rt.apply_raw(&body(&format!(
            r#"{{"proxy_enabled": true, "proxy_listen": "{addr}"}}"#
        )))
        .await
        .unwrap();
        let wide = format!("0.0.0.0:{}", addr.port());
        let status = rt
            .apply_raw(&body(&format!(r#"{{"proxy_listen": "{wide}"}}"#)))
            .await
            .unwrap();
        assert!(status.running);
        assert_eq!(status.listen, wide);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn legacy_server_settings_are_read_once_when_own_file_is_missing() {
        let dir = temp_dir("legacy");
        let listen = free_addr().to_string();
        std::fs::write(
            dir.join("server-settings.json"),
            format!(
                r#"{{"proxy_enabled": false, "proxy_listen": "{listen}", "dns_enabled": true}}"#
            ),
        )
        .unwrap();

        let rt = runtime_in(&dir, true);
        let status = rt.status().await;
        assert!(!status.enabled, "legacy proxy_enabled must be honored");
        assert_eq!(status.listen, listen);

        // Applying any change writes the module's own file; the legacy file is
        // no longer consulted afterwards.
        rt.apply_raw(&body(r#"{"proxy_enabled": false}"#)).await.unwrap();
        let addr = free_addr();
        rt.apply_raw(&body(&format!(
            r#"{{"proxy_enabled": true, "proxy_listen": "{addr}"}}"#
        )))
        .await
        .unwrap();
        assert!(dir.join("proxy-server.json").exists());
        let reloaded = runtime_in(&dir, false);
        assert!(reloaded.status().await.enabled);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn update_parse_takes_only_proxy_keys() {
        let upd = ProxyServerOverrides::parse(
            br#"{"proxy_enabled": true, "proxy_listen": " 0.0.0.0:1 ", "dns_enabled": false}"#,
        )
        .unwrap();
        assert_eq!(upd.enabled, Some(true));
        assert_eq!(upd.listen, Some("0.0.0.0:1".into()));
        assert!(ProxyServerOverrides::parse(b"not json").is_err());
        assert!(ProxyServerOverrides::parse(b"[]").is_err());
    }
}
