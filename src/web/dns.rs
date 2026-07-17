//! API handlers for DNS status, rewrites, and DNS settings.

use std::sync::Arc;

use hyper::StatusCode;
use serde_json::{json, Value};

use crate::adblock::AdBlocker;
use crate::dns::{DnsService, DnsStatus};
use crate::stats::SharedState;

use super::respond::{command, json_ok, json_status};
use super::{AdminCommand, AdminResponse};

#[derive(serde::Serialize)]
struct DnsResponse {
    enabled: bool,
    #[serde(flatten)]
    status: DnsStatus,
    metrics: Value,
}

pub(super) fn dns_json(state: &SharedState, dns: Option<&DnsService>) -> Value {
    let Some(dns) = dns else {
        return json!({ "enabled": false });
    };
    let resp = DnsResponse { enabled: true, status: dns.status(), metrics: state.metrics.dns_view() };
    serde_json::to_value(resp).unwrap_or_else(|_| json!({ "enabled": false }))
}

pub(super) fn check_dns_rule(adblock: &AdBlocker, body: &[u8]) -> AdminResponse {
    let v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return json_status(StatusCode::BAD_REQUEST, json!({"error": e.to_string()})),
    };
    let domain = v
        .get("domain")
        .and_then(Value::as_str)
        .map(|d| d.trim().trim_end_matches('.').to_ascii_lowercase())
        .unwrap_or_default();
    if domain.is_empty() {
        return json_status(StatusCode::BAD_REQUEST, json!({"error": "missing 'domain'"}));
    }
    let d = adblock.check_dns(&domain);
    json_ok(json!({
        "domain": domain,
        "outcome": if d.blocked { "blocked" } else { "allowed" },
        "blocked": d.blocked,
        "filter": d.attribution.rule,
        "list": d.attribution.list,
    }))
}

pub(super) fn rewrites_json(dns: &DnsService) -> Value {
    let rewrites: Vec<Value> = dns
        .rewrites()
        .list()
        .into_iter()
        .map(|r| json!({ "domain": r.domain, "answer": r.answer.to_string() }))
        .collect();
    json!({ "rewrites": rewrites })
}

#[derive(Debug, PartialEq)]
pub(crate) enum RewriteCommand {
    Add { domain: String, answer: String },
    Delete { domain: String, answer: String },
}

impl AdminCommand for RewriteCommand {
    fn parse(body: &[u8]) -> std::result::Result<Self, String> {
        let v: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        let (Some(domain), Some(answer)) = (
            v.get("domain").and_then(Value::as_str),
            v.get("answer").and_then(Value::as_str),
        ) else {
            return Err("expected 'domain' and 'answer'".into());
        };
        let (domain, answer) = (domain.to_string(), answer.to_string());
        Ok(if v.get("delete").and_then(Value::as_bool) == Some(true) {
            Self::Delete { domain, answer }
        } else {
            Self::Add { domain, answer }
        })
    }
}

pub(super) fn edit_rewrites(
    state: &Arc<SharedState>,
    dns: &Arc<DnsService>,
    body: &[u8],
) -> AdminResponse {
    let cmd = match command::<RewriteCommand>(body) {
        Ok(cmd) => cmd,
        Err(resp) => return resp,
    };

    let outcome = match &cmd {
        RewriteCommand::Delete { domain, answer } => {
            dns.rewrites().remove(domain, answer).map(|removed| {
                if removed {
                    state.log_event(
                        crate::stats::EventKind::Info,
                        format!("dns rewrite removed: {domain} → {answer}"),
                    );
                }
            })
        }
        RewriteCommand::Add { domain, answer } => {
            dns.rewrites().add(domain, answer).map(|()| {
                state.log_event(
                    crate::stats::EventKind::Info,
                    format!("dns rewrite added: {domain} → {answer}"),
                );
            })
        }
    };
    match outcome {
        Ok(()) => json_ok(rewrites_json(dns)),
        Err(e) => json_status(StatusCode::BAD_REQUEST, json!({"error": e.to_string()})),
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum DnsConfigCommand {
    Reset,
    Apply(crate::dns::DnsOverrides),
}

impl AdminCommand for DnsConfigCommand {
    fn parse(body: &[u8]) -> std::result::Result<Self, String> {
        let v: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        if v.get("reset").and_then(Value::as_bool) == Some(true) {
            return Ok(Self::Reset);
        }
        let str_list = |key: &str| -> Option<Vec<String>> {
            v.get(key).and_then(Value::as_array).map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
        };
        let upstream_mode = match v.get("upstream_mode") {
            None | Some(Value::Null) => None,
            Some(m) => Some(serde_json::from_value(m.clone()).map_err(|_| {
                "upstream_mode must be \"failover\", \"load-balance\", or \"parallel\"".to_string()
            })?),
        };
        Ok(Self::Apply(crate::dns::DnsOverrides {
            upstreams: str_list("upstreams"),
            upstream_mode,
            bootstrap: str_list("bootstrap"),
            cache_size: v.get("cache_size").and_then(Value::as_u64).map(|n| n as usize),
            min_ttl_secs: v.get("min_ttl_secs").and_then(Value::as_u64).map(|n| n as u32),
            max_ttl_secs: v.get("max_ttl_secs").and_then(Value::as_u64).map(|n| n as u32),
            ech_probe_domain: v
                .get("ech_probe_domain")
                .and_then(Value::as_str)
                .map(|s| s.trim().to_string()),
            log_ipv6: v.get("log_ipv6").and_then(Value::as_bool),
        }))
    }
}

pub(super) fn edit_dns_config(
    state: &Arc<SharedState>,
    dns: &Arc<DnsService>,
    body: &[u8],
) -> AdminResponse {
    let cmd = match command::<DnsConfigCommand>(body) {
        Ok(cmd) => cmd,
        Err(resp) => return resp,
    };

    let outcome = match cmd {
        DnsConfigCommand::Reset => dns
            .reset_settings()
            .map(|()| "dns settings reset to config.toml".to_string()),
        DnsConfigCommand::Apply(upd) => {
            dns.apply_settings(upd).map(|()| "dns settings updated".to_string())
        }
    };

    match outcome {
        Ok(msg) => {
            state.log_event(crate::stats::EventKind::Info, msg);
            json_ok(dns_json(state, Some(dns)))
        }
        Err(e) => json_status(StatusCode::BAD_REQUEST, json!({"error": e})),
    }
}
