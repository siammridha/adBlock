//! API handlers for server (listener) and proxy egress settings.

use std::sync::Arc;

use hyper::StatusCode;
use serde_json::{json, Value};

use crate::dns::api::DnsRuntime;
use crate::proxy::api::ProxyRuntime;
use crate::proxy::api::{EgressOverrides, EgressPolicy, InjectionOverrides, InjectionPolicy};
use crate::stats::api::SharedState;

use super::respond::{command, json_ok, json_status};
use super::AdminResponse;

/// The dashboard reads one combined server status; each module reports its
/// own half and this merges them for display.
pub(super) async fn server_status_json(
    proxy: &Arc<ProxyRuntime>,
    dns: &Arc<DnsRuntime>,
) -> Value {
    let p = proxy.status().await;
    let d = dns.status().await;
    json!({
        "proxy_enabled": p.enabled,
        "proxy_listen": p.listen,
        "proxy_running": p.running,
        "proxy_controllable": p.controllable,
        "dns_enabled": d.enabled,
        "dns_listen": d.listen,
        "dns_running": d.running,
    })
}

/// Hand the raw update to both settings interfaces; each module picks out and
/// validates its own keys and starts/stops/rebinds its own service.
pub(super) async fn edit_server_config(
    proxy: &Arc<ProxyRuntime>,
    dns: &Arc<DnsRuntime>,
    body: &[u8],
) -> AdminResponse {
    if let Err(e) = proxy.apply_raw(body).await {
        return json_status(StatusCode::BAD_REQUEST, json!({"error": e}));
    }
    if let Err(e) = dns.apply_raw(body).await {
        return json_status(StatusCode::BAD_REQUEST, json!({"error": e}));
    }
    json_ok(server_status_json(proxy, dns).await)
}

/// The proxy's two settings groups in one object for the settings tab: egress
/// policy and what the proxy injects into pages. Each comes from the proxy's own
/// interface; this only merges them for display.
pub(super) fn proxy_settings_json(egress: &EgressPolicy, injection: &InjectionPolicy) -> Value {
    let mut out = serde_json::to_value(egress.settings()).unwrap_or_default();
    let inj = serde_json::to_value(injection.settings()).unwrap_or_default();
    if let (Some(out), Some(inj)) = (out.as_object_mut(), inj.as_object()) {
        out.extend(inj.clone());
    }
    out
}

/// Hand the raw update to both proxy settings interfaces; each picks out and
/// validates its own keys, so a panel can send only the flags it owns.
pub(super) fn edit_proxy_config(
    state: &SharedState,
    egress: &EgressPolicy,
    injection: &InjectionPolicy,
    body: &[u8],
) -> AdminResponse {
    let upd = match command(EgressOverrides::parse(body)) {
        Ok(upd) => upd,
        Err(resp) => return resp,
    };
    let inj_upd = match command(InjectionOverrides::parse(body)) {
        Ok(upd) => upd,
        Err(resp) => return resp,
    };
    let settings = egress.apply(&upd);
    let inj = injection.apply(&inj_upd);
    state.log_event(
        crate::stats::api::EventKind::Info,
        format!(
            "proxy egress: resolver-only={} disable-ipv6={}; \
             page injection: cosmetic={} scriptlets={} runtime={}",
            settings.resolver_only,
            settings.disable_ipv6,
            inj.cosmetic,
            inj.scriptlets,
            inj.runtime
        ),
    );
    json_ok(proxy_settings_json(egress, injection))
}
