//! API handlers for the MITM exclusion list.

use std::sync::Arc;

use hyper::StatusCode;
use serde_json::{json, Value};

use crate::proxy::exclusions::ExclusionStore;
use crate::stats::SharedState;

use super::respond::{command, json_ok, json_status};
use super::{AdminCommand, AdminResponse};

pub(super) fn exclusions_json(exclusions: &ExclusionStore) -> Value {
    json!({ "domains": exclusions.list() })
}

#[derive(Debug, PartialEq)]
pub(crate) enum ExclusionCommand {
    Add { domain: String },
    Delete { domain: String },
}

impl AdminCommand for ExclusionCommand {
    fn parse(body: &[u8]) -> std::result::Result<Self, String> {
        let v: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        let Some(domain) = v.get("domain").and_then(Value::as_str).map(str::trim) else {
            return Err("expected 'domain'".into());
        };
        let domain = domain.to_string();
        Ok(if v.get("delete").and_then(Value::as_bool) == Some(true) {
            Self::Delete { domain }
        } else {
            Self::Add { domain }
        })
    }
}

pub(super) fn edit_exclusions(
    state: &Arc<SharedState>,
    exclusions: &Arc<ExclusionStore>,
    body: &[u8],
) -> AdminResponse {
    let cmd = match command::<ExclusionCommand>(body) {
        Ok(cmd) => cmd,
        Err(resp) => return resp,
    };

    let outcome = match &cmd {
        ExclusionCommand::Delete { domain } => exclusions.remove(domain).map(|removed| {
            if removed {
                state.log_event(
                    crate::stats::EventKind::Info,
                    format!("excluded domain removed: {domain}"),
                );
            }
        }),
        ExclusionCommand::Add { domain } => exclusions.add(domain).map(|_| {
            state.log_event(
                crate::stats::EventKind::Info,
                format!("excluded domain added: {domain} (bypasses MITM)"),
            );
        }),
    };

    match outcome {
        Ok(()) => json_ok(exclusions_json(exclusions)),
        Err(e) => json_status(StatusCode::BAD_REQUEST, json!({"error": e.to_string()})),
    }
}
