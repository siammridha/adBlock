//! API handlers for server (listener) and proxy egress settings.

use std::sync::Arc;

use hyper::StatusCode;
use serde_json::{json, Value};

use crate::dns::control::DnsRuntime;
use crate::proxy::control::ProxyRuntime;
use crate::proxy::egress::{EgressOverrides, EgressPolicy};
use crate::stats::SharedState;

use super::respond::{command, json_ok, json_status};
use super::{AdminCommand, AdminResponse};

impl AdminCommand for EgressOverrides {
    fn parse(body: &[u8]) -> std::result::Result<Self, String> {
        parse_proxy_config(body)
    }
}

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

pub(super) fn edit_proxy_config(
    state: &SharedState,
    egress: &EgressPolicy,
    body: &[u8],
) -> AdminResponse {
    let upd = match command::<EgressOverrides>(body) {
        Ok(upd) => upd,
        Err(resp) => return resp,
    };
    let settings = egress.apply(&upd);
    state.log_event(
        crate::stats::EventKind::Info,
        format!(
            "proxy egress: resolver-only={} ech={} disable-ipv6={}",
            settings.resolver_only, settings.use_ech, settings.disable_ipv6
        ),
    );
    json_ok(serde_json::to_value(settings).unwrap_or_default())
}

fn parse_proxy_config(body: &[u8]) -> std::result::Result<EgressOverrides, String> {
    let v: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    if !v.is_object() {
        return Err("expected a JSON object".into());
    }
    let flag = |key: &str| -> Option<bool> { v.get(key).and_then(Value::as_bool) };
    Ok(EgressOverrides {
        resolver_only: flag("resolver_only"),
        use_ech: flag("use_ech"),
        disable_ipv6: flag("disable_ipv6"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_config_parses_present_flags_only() {
        let upd = parse_proxy_config(br#"{"resolver_only": true}"#).unwrap();
        assert_eq!(upd.resolver_only, Some(true));
        assert_eq!(upd.use_ech, None);
        assert_eq!(upd.disable_ipv6, None);
        let upd = parse_proxy_config(br#"{"use_ech": "yes"}"#).unwrap();
        assert_eq!(upd.use_ech, None);
        assert!(parse_proxy_config(b"[]").is_err());
    }
}
