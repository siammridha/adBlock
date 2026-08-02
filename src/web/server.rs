//! API handlers for server (listener) and proxy egress settings.

use std::sync::Arc;

use hyper::StatusCode;
use serde_json::{json, Value};

use crate::dns::api::DnsRuntime;
use crate::proxy::api::ProxyRuntime;
use crate::proxy::api::{EgressOverrides, EgressPolicy};
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

/// The proxy's egress policy, as the proxy's own interface reports it.
pub(super) fn proxy_settings_json(egress: &EgressPolicy) -> Value {
    serde_json::to_value(egress.settings()).unwrap_or_default()
}

/// Hand the raw update to the proxy's egress interface; it picks out and
/// validates its own keys.
pub(super) fn edit_proxy_config(
    state: &SharedState,
    egress: &EgressPolicy,
    body: &[u8],
) -> AdminResponse {
    let upd = match command(EgressOverrides::parse(body)) {
        Ok(upd) => upd,
        Err(resp) => return resp,
    };
    let settings = egress.apply(&upd);
    state.log_event(
        crate::stats::api::EventKind::Info,
        format!(
            "proxy egress: resolver-only={} disable-ipv6={}",
            settings.resolver_only, settings.disable_ipv6
        ),
    );
    json_ok(proxy_settings_json(egress))
}
