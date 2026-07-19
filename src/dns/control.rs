//! Settings-driven lifecycle for the DNS listener. Applying a settings update
//! starts, stops, or rebinds the UDP/TCP servers. The resolver itself is
//! always available to in-process callers, listener up or not.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use super::{DnsHandles, DnsService};
use crate::dns::error::{Error, Result};
use crate::dns::persist::OverrideStore;
use crate::stats::{EventKind, SharedState};

/// Persisted overrides for the DNS listener (its own settings file).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DnsServerOverrides {
    pub enabled: Option<bool>,
    pub listen: Option<String>,
}

impl DnsServerOverrides {
    /// Parse a raw settings update. The update shares one JSON body with the
    /// proxy server settings, so only the `dns_*` keys belong to this module.
    pub fn parse(body: &[u8]) -> std::result::Result<Self, String> {
        let v: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        if !v.is_object() {
            return Err("expected a JSON object".into());
        }
        Ok(Self {
            enabled: v.get("dns_enabled").and_then(Value::as_bool),
            listen: v
                .get("dns_listen")
                .and_then(Value::as_str)
                .map(|s| s.trim().to_string()),
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct DnsServerStatus {
    pub enabled: bool,
    pub listen: String,
    pub running: bool,
}

struct Inner {
    enabled: bool,
    listen: SocketAddr,
    handles: Option<DnsHandles>,
    overrides: DnsServerOverrides,
}

/// The DNS listener's runtime: owns the enabled/listen settings, their
/// persistence, and the running listener tasks. Holds the long-lived
/// [`DnsService`], which keeps resolving for in-process callers even while
/// the listener is off.
pub struct DnsRuntime {
    state: Arc<SharedState>,
    store: OverrideStore<DnsServerOverrides>,
    service: Arc<DnsService>,
    inner: Mutex<Inner>,
}

impl DnsRuntime {
    /// `legacy_store_path` points at the old combined `server-settings.json`;
    /// it is read once when the DNS module's own settings file does not exist.
    pub fn new(
        state: Arc<SharedState>,
        store_path: PathBuf,
        legacy_store_path: Option<PathBuf>,
        service: Arc<DnsService>,
        cfg_listen: &str,
        cfg_enabled: bool,
    ) -> Result<Arc<Self>> {
        let store: OverrideStore<DnsServerOverrides> = OverrideStore::new(store_path.clone());
        let overrides = if store_path.exists() {
            store.load()
        } else {
            legacy_overrides(legacy_store_path.as_deref())
        };

        let listen_s = overrides
            .listen
            .clone()
            .unwrap_or_else(|| cfg_listen.to_string());
        let listen = listen_s
            .parse()
            .map_err(|e| Error::Config(format!("invalid dns listen '{listen_s}': {e}")))?;
        let enabled = overrides.enabled.unwrap_or(cfg_enabled);

        Ok(Arc::new(Self {
            state,
            store,
            service,
            inner: Mutex::new(Inner { enabled, listen, handles: None, overrides }),
        }))
    }

    /// The always-available resolver. This handle stays valid whether or not
    /// the listener is running.
    pub fn service(&self) -> Arc<DnsService> {
        self.service.clone()
    }

    pub async fn start_initial(self: &Arc<Self>) -> Result<()> {
        let mut inner = self.inner.lock().await;
        if inner.enabled {
            match self.service.start(inner.listen).await {
                Ok(h) => inner.handles = Some(h),
                Err(e) => {
                    tracing::error!(error = %e, "dns server");
                    self.state
                        .log_event(EventKind::Error, format!("dns server: {e}"));
                }
            }
        }
        Ok(())
    }

    pub async fn status(&self) -> DnsServerStatus {
        status_of(&*self.inner.lock().await)
    }

    /// Parse, validate, apply, and persist a raw settings update. Disabling
    /// stops only the listener; the resolver keeps serving in-process callers.
    pub async fn apply_raw(
        self: &Arc<Self>,
        body: &[u8],
    ) -> std::result::Result<DnsServerStatus, String> {
        let upd = DnsServerOverrides::parse(body)?;
        let new_listen = match &upd.listen {
            Some(s) => Some(
                s.parse::<SocketAddr>()
                    .map_err(|e| format!("invalid dns listen '{s}': {e}"))?,
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
        let was = inner.enabled.then_some(inner.listen);
        if let Some(h) = inner.handles.take() {
            h.shutdown().await;
        }
        let handles = if enabled {
            match self.service.start(listen).await {
                Ok(h) => Some(h),
                Err(e) => {
                    inner.enabled = false;
                    self.state
                        .log_event(EventKind::Error, format!("dns bind {listen}: {e}"));
                    return Err(e.to_string());
                }
            }
        } else {
            None
        };
        inner.handles = handles;
        inner.enabled = enabled;
        inner.listen = listen;
        inner.overrides.enabled = Some(enabled);
        inner.overrides.listen = Some(listen.to_string());
        if let Err(e) = self.store.save(&inner.overrides) {
            tracing::warn!(error = %e, "persisting dns server settings");
            self.state
                .log_event(EventKind::Error, format!("saving dns server settings: {e}"));
        }
        transition_note(&self.state, was, enabled.then_some(listen));
        Ok(())
    }
}

fn status_of(inner: &Inner) -> DnsServerStatus {
    DnsServerStatus {
        enabled: inner.enabled,
        listen: inner.listen.to_string(),
        running: inner.handles.is_some(),
    }
}

fn transition_note(state: &SharedState, was: Option<SocketAddr>, now: Option<SocketAddr>) {
    let msg = match (was, now) {
        (None, Some(a)) => format!("dns enabled on {a}"),
        (Some(_), None) => "dns disabled".to_string(),
        (Some(p), Some(n)) if p != n => format!("dns re-bound to {n}"),
        _ => return,
    };
    tracing::info!("{msg}");
    state.log_event(EventKind::Info, msg);
}

/// Read this module's keys out of the old combined `server-settings.json`.
fn legacy_overrides(path: Option<&std::path::Path>) -> DnsServerOverrides {
    let Some(path) = path else {
        return DnsServerOverrides::default();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return DnsServerOverrides::default();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return DnsServerOverrides::default();
    };
    DnsServerOverrides {
        enabled: v.get("dns_enabled").and_then(Value::as_bool),
        listen: v
            .get("dns_listen")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adblock::MemoryListStore;
    use crate::adblock::AdblockConfig;
    use crate::dns::DnsConfig;
    use crate::stats::LoggingConfig;
    use crate::stats::StaticInfo;
    use std::path::Path;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dns-control-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn runtime_with(dir: &Path, cfg_enabled: bool, rules: &[&str]) -> Arc<DnsRuntime> {
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
                started: std::time::Instant::now(),
            },
            &LoggingConfig { level: "info".into(), log_actions: true, log_requests: true, ..Default::default() },
        ));
        let dns_cfg = DnsConfig::default();
        let service = DnsService::new(&dns_cfg, dir, adblock, state.clone()).unwrap();
        DnsRuntime::new(
            state,
            dir.join("dns-server.json"),
            Some(dir.join("server-settings.json")),
            service,
            &dns_cfg.listen,
            cfg_enabled,
        )
        .unwrap()
    }

    fn free_addr() -> SocketAddr {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap()
    }

    #[tokio::test]
    async fn disable_stops_listener_but_resolver_keeps_answering() {
        use hickory_proto::op::{Message, Query};
        use hickory_proto::rr::rdata::A;
        use hickory_proto::rr::{Name, RData, RecordType};
        use std::str::FromStr;

        let dir = temp_dir("dns-off");
        let rt = runtime_with(&dir, false, &["||ads.example.com^"]);

        let addr = free_addr();
        let status = rt
            .apply_raw(
                format!(r#"{{"dns_enabled": true, "dns_listen": "{addr}"}}"#).as_bytes(),
            )
            .await
            .unwrap();
        assert!(status.running && status.enabled);

        let before = rt.service();
        let status = rt.apply_raw(br#"{"dns_enabled": false}"#).await.unwrap();
        assert!(!status.running && !status.enabled);

        // Same service instance: the proxy's egress handle stays valid and the
        // resolver keeps answering in-process queries with the listener down.
        let after = rt.service();
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

    #[tokio::test]
    async fn settings_persist_in_own_file_and_legacy_file_seeds_them() {
        let dir = temp_dir("persist");
        let listen = free_addr().to_string();
        std::fs::write(
            dir.join("server-settings.json"),
            format!(r#"{{"dns_enabled": false, "dns_listen": "{listen}"}}"#),
        )
        .unwrap();

        let rt = runtime_with(&dir, true, &[]);
        let status = rt.status().await;
        assert!(!status.enabled, "legacy dns_enabled must be honored");
        assert_eq!(status.listen, listen);

        let addr = free_addr();
        rt.apply_raw(
            format!(r#"{{"dns_enabled": true, "dns_listen": "{addr}"}}"#).as_bytes(),
        )
        .await
        .unwrap();
        assert!(dir.join("dns-server.json").exists());

        let rt2 = runtime_with(&dir, false, &[]);
        let status = rt2.status().await;
        assert!(status.enabled, "own file wins once written");
        assert_eq!(status.listen, addr.to_string());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn update_parse_takes_only_dns_keys() {
        let upd = DnsServerOverrides::parse(
            br#"{"dns_enabled": false, "dns_listen": " 0.0.0.0:53 ", "proxy_enabled": true}"#,
        )
        .unwrap();
        assert_eq!(upd.enabled, Some(false));
        assert_eq!(upd.listen, Some("0.0.0.0:53".into()));
        assert!(DnsServerOverrides::parse(b"not json").is_err());
    }
}
