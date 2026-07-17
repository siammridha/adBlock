//! API handlers for server (listener) and proxy egress settings.

use std::sync::Arc;

use hyper::StatusCode;
use serde_json::{json, Value};

use crate::net::egress::{EgressOverrides, EgressPolicy};
use crate::web::runtime::{Runtime, ServerOverrides};
use crate::stats::SharedState;

use super::respond::{command, json_ok, json_status};
use super::{AdminCommand, AdminResponse};

impl AdminCommand for ServerOverrides {
    fn parse(body: &[u8]) -> std::result::Result<Self, String> {
        parse_server_config(body)
    }
}

impl AdminCommand for EgressOverrides {
    fn parse(body: &[u8]) -> std::result::Result<Self, String> {
        parse_proxy_config(body)
    }
}

pub(super) async fn edit_server_config(runtime: &Arc<Runtime>, body: &[u8]) -> AdminResponse {
    let upd = match command::<ServerOverrides>(body) {
        Ok(upd) => upd,
        Err(resp) => return resp,
    };
    match runtime.apply(upd).await {
        Ok(status) => json_ok(serde_json::to_value(status).unwrap_or_default()),
        Err(e) => json_status(StatusCode::BAD_REQUEST, json!({"error": e})),
    }
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

fn parse_server_config(body: &[u8]) -> std::result::Result<ServerOverrides, String> {
    let v: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    if !v.is_object() {
        return Err("expected a JSON object".into());
    }
    let listen = |key: &str| -> Option<String> {
        v.get(key).and_then(Value::as_str).map(|s| s.trim().to_string())
    };
    let flag = |key: &str| -> Option<bool> { v.get(key).and_then(Value::as_bool) };
    Ok(ServerOverrides {
        proxy_enabled: flag("proxy_enabled"),
        proxy_listen: listen("proxy_listen"),
        dns_enabled: flag("dns_enabled"),
        dns_listen: listen("dns_listen"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_config_parses_present_fields_only() {
        let upd =
            parse_server_config(br#"{"dns_enabled": false, "proxy_listen": " 0.0.0.0:8080 "}"#)
                .unwrap();
        assert_eq!(upd.dns_enabled, Some(false));
        assert_eq!(upd.proxy_listen, Some("0.0.0.0:8080".to_string()));
        assert_eq!(upd.proxy_enabled, None);
        assert_eq!(upd.dns_listen, None);
        assert!(parse_server_config(b"not json").is_err());
        assert!(parse_server_config(b"[]").is_err());
    }

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
