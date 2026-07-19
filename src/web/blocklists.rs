//! API handlers for filter lists and scriptlets.

use std::sync::Arc;

use hyper::StatusCode;
use serde_json::{json, Value};

use crate::adblock::updater::{ScriptletUpdater, UBO_TARBALL_PAGE};
use crate::adblock::{AdBlocker, ListCuration, ListEntry, RulesUpdate};
use crate::adblock::error::Result;
use crate::adblock::maintenance::{event_list_change, event_scriptlets, RefreshError};
use crate::stats::{EventKind, SharedState};

use super::respond::{command, json_ok, json_status, parse_query, percent_decode};
use super::{Admin, AdminCommand, AdminResponse};

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
    let v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return json_status(StatusCode::BAD_REQUEST, json!({"error": e.to_string()})),
    };
    let url = match v.get("url").and_then(Value::as_str) {
        Some(u) if !u.trim().is_empty() => crate::adblock::normalize_test_url(u),
        _ => return json_status(StatusCode::BAD_REQUEST, json!({"error": "missing 'url'"})),
    };
    let req_type = v.get("type").and_then(Value::as_str).unwrap_or("other");
    let source = v.get("source").and_then(Value::as_str).unwrap_or("");

    let d = adblock.check(&url, source, req_type);
    json_ok(json!({
        "url": url,
        "request_type": req_type,
        "source": source,
        "outcome": if d.blocked { "blocked" } else { "allowed" },
        "blocked": d.blocked,
        "filter": d.attribution.rule,
        "list": d.attribution.list,
    }))
}

#[derive(Debug, PartialEq)]
pub(crate) enum BlocklistCommand {
    Delete { name: String },
    AddUrl { url: String },
    ApplyRules {
        name: Option<String>,
        rules: String,
        update: RulesUpdate,
    },
}

impl AdminCommand for BlocklistCommand {
    fn parse(body: &[u8]) -> std::result::Result<Self, String> {
        let v: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        if v.get("delete").and_then(Value::as_bool) == Some(true) {
            let name = v
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.trim().is_empty())
                .ok_or("delete needs 'name'")?
                .to_string();
            return Ok(BlocklistCommand::Delete { name });
        }
        if let Some(url) = v.get("url").and_then(Value::as_str) {
            return Ok(BlocklistCommand::AddUrl {
                url: url.trim().to_string(),
            });
        }
        let Some(rules) = v.get("rules").and_then(Value::as_str) else {
            return Err("expected {\"url\": …} or {\"rules\": …}".into());
        };
        Ok(BlocklistCommand::ApplyRules {
            name: v.get("name").and_then(Value::as_str).map(str::to_string),
            rules: rules.to_string(),
            update: if v.get("replace").and_then(Value::as_bool) == Some(true) {
                RulesUpdate::Replace
            } else {
                RulesUpdate::Append
            },
        })
    }
}

impl Admin {
    pub(super) async fn add_blocklist(&self, body: &[u8]) -> AdminResponse {
        let cmd = match command::<BlocklistCommand>(body) {
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
                            crate::stats::EventKind::Info,
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
