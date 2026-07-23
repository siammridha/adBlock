//! API handlers for DNS status, rewrites, and DNS settings.

use std::sync::Arc;

use hyper::StatusCode;
use serde_json::{json, Value};

use crate::adblock::api::DnsRuleTest;
use crate::adblock::api::AdBlocker;
use crate::dns::api::{DnsConfigCommand, RewriteCommand};
use crate::dns::api::{DnsService, DnsStatus};
use crate::stats::api::SharedState;

use super::respond::{command, json_ok, json_status};
use super::AdminResponse;

#[derive(serde::Serialize)]
struct DnsResponse {
    enabled: bool,
    #[serde(flatten)]
    status: DnsStatus,
    metrics: Value,
}

pub(super) fn dns_json(state: &SharedState, dns: &DnsService) -> Value {
    // `enabled` refers to the resolver, which is always on; the listener's
    // on/off state lives in the server status.
    let resp = DnsResponse { enabled: true, status: dns.status(), metrics: state.metrics.dns_view() };
    serde_json::to_value(resp).unwrap_or_else(|_| json!({ "enabled": false }))
}

pub(super) fn check_dns_rule(adblock: &AdBlocker, body: &[u8]) -> AdminResponse {
    let t = match command(DnsRuleTest::parse(body)) {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    let d = adblock.check_dns(&t.domain);
    json_ok(json!({
        "domain": t.domain,
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

pub(super) fn edit_rewrites(
    state: &Arc<SharedState>,
    dns: &Arc<DnsService>,
    body: &[u8],
) -> AdminResponse {
    let cmd = match command(RewriteCommand::parse(body)) {
        Ok(cmd) => cmd,
        Err(resp) => return resp,
    };

    let outcome = match &cmd {
        RewriteCommand::Delete { domain, answer } => {
            dns.rewrites().remove(domain, answer).map(|removed| {
                if removed {
                    state.log_event(
                        crate::stats::api::EventKind::Info,
                        format!("dns rewrite removed: {domain} → {answer}"),
                    );
                }
            })
        }
        RewriteCommand::Add { domain, answer } => {
            dns.rewrites().add(domain, answer).map(|()| {
                state.log_event(
                    crate::stats::api::EventKind::Info,
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

pub(super) fn edit_dns_config(
    state: &Arc<SharedState>,
    dns: &Arc<DnsService>,
    body: &[u8],
) -> AdminResponse {
    let cmd = match command(DnsConfigCommand::parse(body)) {
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
            state.log_event(crate::stats::api::EventKind::Info, msg);
            json_ok(dns_json(state, dns))
        }
        Err(e) => json_status(StatusCode::BAD_REQUEST, json!({"error": e})),
    }
}
