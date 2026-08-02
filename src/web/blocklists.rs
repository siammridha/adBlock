//! API handlers for filter lists and scriptlets.

use std::sync::Arc;

use hyper::StatusCode;
use serde_json::{json, Value};

use crate::adblock::api::{BlocklistCommand, CosmeticQuery, RuleTest};
use crate::adblock::api::{ScriptletUpdater, UBO_TARBALL_PAGE};
use crate::adblock::api::{AdBlocker, ListCuration, ListEntry};
use crate::adblock::api::Result;
use crate::adblock::api::{event_list_change, event_scriptlets, RefreshError};
use crate::stats::api::{EventKind, SharedState};

use super::respond::{command, json_cors, json_ok, json_status, parse_query, percent_decode};
use super::{Admin, AdminResponse};

pub(super) async fn update_scriptlets(
    state: &SharedState,
    updater: &ScriptletUpdater,
    curation: &Arc<ListCuration>,
) -> AdminResponse {
    match updater.refresh(curation).await {
        Ok(count) => {
            event_scriptlets(state, count, "updated from uBO master");
            json_ok(json!({ "ok": true, "loaded": count }))
        }
        Err(e) => {
            state.log_event(EventKind::Error, format!("scriptlet library update: {e}"));
            json_status(StatusCode::BAD_GATEWAY, json!({ "error": e }))
        }
    }
}

pub(super) fn scriptlets_json(curation: &ListCuration) -> Value {
    let scriptlets = curation.scriptlets();
    let lib = scriptlets.library();
    let injectable = lib.iter().filter(|s| s.injectable).count();
    let library: Vec<Value> = lib
        .into_iter()
        .map(|s| {
            json!({ "name": s.name, "aliases": s.aliases,
                    "injectable": s.injectable, "bytes": s.bytes })
        })
        .collect();
    let updated_ms = std::fs::metadata(scriptlets.path())
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64);
    json!({
        "enabled": scriptlets.enabled(),
        "loaded": library.len(),
        "injectable": injectable,
        "source_url": UBO_TARBALL_PAGE,
        "updated_ms": updated_ms,
        "library": library,
    })
}

pub(super) fn scriptlet_source_json(curation: &ListCuration, query: &str) -> Value {
    let name = parse_query(query, "name")
        .map(percent_decode)
        .unwrap_or_default();
    match curation.scriptlets().source(&name) {
        Some(source) => json!({ "name": name, "source": source }),
        None => json!({ "error": format!("no scriptlet named '{name}'") }),
    }
}

pub(super) fn blocklists_json(curation: &ListCuration) -> Value {
    let lists: Vec<Value> = curation
        .lists()
        .into_iter()
        .map(|l| {
            let (network, cosmetic, exception) = l.categories();
            json!({ "name": l.name, "source": l.source, "rules": l.rules,
                    "enabled": l.enabled,
                    "network": network, "cosmetic": cosmetic, "exception": exception })
        })
        .collect();
    json!({ "lists": lists })
}

pub(super) fn blocklist_text_json(curation: &ListCuration, query: &str) -> Value {
    let name = parse_query(query, "name")
        .map(percent_decode)
        .unwrap_or_default();
    match curation.lists().into_iter().find(|l| l.name == name) {
        Some(l) => json!({
            "name": l.name,
            "source": l.source,
            "rules": l.rules,
            "text": l.text(),
        }),
        None => json!({ "error": format!("no list named '{name}'") }),
    }
}

pub(super) fn check_rule(adblock: &AdBlocker, body: &[u8]) -> AdminResponse {
    let t = match command(RuleTest::parse(body)) {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    let d = adblock.check(&t.url, &t.source, &t.req_type);
    json_ok(json!({
        "url": t.url,
        "request_type": t.req_type,
        "source": t.source,
        "outcome": if d.blocked { "blocked" } else { "allowed" },
        "blocked": d.blocked,
        "filter": d.attribution.rule,
        "list": d.attribution.list,
    }))
}

/// Adblock's own switches, as it reports them.
pub(super) fn adblock_settings_json(adblock: &AdBlocker) -> Value {
    serde_json::to_value(adblock.decisions()).unwrap_or_default()
}

/// Hand the raw update to Adblock; it validates its own keys and answers with
/// the new settings or why it said no.
pub(super) fn edit_adblock_config(
    state: &SharedState,
    adblock: &AdBlocker,
    body: &[u8],
) -> AdminResponse {
    let s = match command(adblock.set_decisions(body)) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    state.log_event(
        EventKind::Info,
        format!(
            "adblock: redirect={} removeparam={} csp={}; \
             page injection: cosmetic={} scriptlets={} runtime={}",
            s.redirect, s.removeparam, s.csp, s.cosmetic, s.scriptlets, s.runtime
        ),
    );
    json_ok(adblock_settings_json(adblock))
}

/// Answer a filtered page asking about class and id names it grew after it was
/// served. The page sends raw bytes; Adblock decides what is valid and what the
/// answer is, and this only renders it.
pub(super) fn cosmetic_for_page(adblock: &AdBlocker, body: &[u8]) -> AdminResponse {
    let q = match CosmeticQuery::parse(body) {
        Ok(q) => q,
        Err(e) => return json_cors(StatusCode::BAD_REQUEST, json!({ "error": e })),
    };
    let css = adblock.cosmetic_css_for_names(&q.url, &q.classes, &q.ids);
    json_cors(StatusCode::OK, json!({ "css": css }))
}

impl Admin {
    pub(super) async fn add_blocklist(&self, body: &[u8]) -> AdminResponse {
        let cmd = match command(BlocklistCommand::parse(body)) {
            Ok(cmd) => cmd,
            Err(resp) => return resp,
        };

        match cmd {
            BlocklistCommand::Delete { name } => {
                let curation2 = self.curation.clone();
                let n2 = name.clone();
                let result = tokio::task::spawn_blocking(move || curation2.remove_list(&n2)).await;
                match result {
                    Ok(Ok(true)) => {
                        self.state.log_event(
                            crate::stats::api::EventKind::Info,
                            format!("blocklist removed: {name}"),
                        );
                        json_ok(blocklists_json(&self.curation))
                    }
                    Ok(Ok(false)) => json_status(
                        StatusCode::NOT_FOUND,
                        json!({"error": format!("no list named '{name}'")}),
                    ),
                    Ok(Err(e)) => {
                        json_status(StatusCode::BAD_REQUEST, json!({"error": e.to_string()}))
                    }
                    Err(e) => json_status(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        json!({"error": e.to_string()}),
                    ),
                }
            }

            BlocklistCommand::SetEnabled { name, enabled } => {
                let curation2 = self.curation.clone();
                let n2 = name.clone();
                let result = tokio::task::spawn_blocking(move || {
                    curation2.set_list_enabled(&n2, enabled)
                })
                .await;
                match result {
                    Ok(Ok(true)) => {
                        let what = if enabled { "enabled" } else { "disabled" };
                        self.state.log_event(
                            EventKind::Info,
                            format!("blocklist {what}: {name} (engine rebuilt)"),
                        );
                        json_ok(blocklists_json(&self.curation))
                    }
                    Ok(Ok(false)) => json_status(
                        StatusCode::NOT_FOUND,
                        json!({"error": format!("no list named '{name}'")}),
                    ),
                    Ok(Err(e)) => {
                        json_status(StatusCode::BAD_REQUEST, json!({"error": e.to_string()}))
                    }
                    Err(e) => json_status(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        json!({"error": e.to_string()}),
                    ),
                }
            }

            BlocklistCommand::AddUrl { url: given } => {
                match self.fetcher.install_from_url(&self.state, &given, "added").await {
                    Ok(_) => json_ok(blocklists_json(&self.curation)),
                    Err(e @ RefreshError::Fetch { .. }) => {
                        json_status(StatusCode::BAD_GATEWAY, json!({"error": e.to_string()}))
                    }
                    Err(e @ RefreshError::Install(_)) => {
                        json_status(StatusCode::BAD_REQUEST, json!({"error": e.to_string()}))
                    }
                    Err(e @ RefreshError::Internal(_)) => json_status(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        json!({"error": e.to_string()}),
                    ),
                }
            }

            BlocklistCommand::ApplyRules { name, rules, update } => {
                let curation2 = self.curation.clone();
                let result = tokio::task::spawn_blocking(move || {
                    curation2.apply_rules(name.as_deref(), &rules, update)
                })
                .await;
                list_change_response(&self.state, &self.curation, result)
            }
        }
    }
}

fn list_change_response(
    state: &Arc<SharedState>,
    curation: &Arc<ListCuration>,
    result: std::result::Result<Result<ListEntry>, tokio::task::JoinError>,
) -> AdminResponse {
    match result {
        Ok(Ok(entry)) => {
            event_list_change(state, &entry, "added");
            json_ok(blocklists_json(curation))
        }
        Ok(Err(e)) => json_status(StatusCode::BAD_REQUEST, json!({"error": e.to_string()})),
        Err(e) => json_status(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})),
    }
}
