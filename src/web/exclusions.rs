//! API handlers for the MITM exclusion list.

use std::sync::Arc;

use hyper::StatusCode;
use serde_json::{json, Value};

use crate::proxy::api::{ExclusionCommand, ExclusionStore};
use crate::stats::api::SharedState;

use super::respond::{command, json_ok, json_status};
use super::AdminResponse;

pub(super) fn exclusions_json(exclusions: &ExclusionStore) -> Value {
    json!({ "domains": exclusions.list() })
}

pub(super) fn edit_exclusions(
    state: &Arc<SharedState>,
    exclusions: &Arc<ExclusionStore>,
    body: &[u8],
) -> AdminResponse {
    let cmd = match command(ExclusionCommand::parse(body)) {
        Ok(cmd) => cmd,
        Err(resp) => return resp,
    };

    let outcome = match &cmd {
        ExclusionCommand::Delete { domain } => exclusions.remove(domain).map(|removed| {
            if removed {
                state.log_event(
                    crate::stats::api::EventKind::Info,
                    format!("excluded domain removed: {domain}"),
                );
            }
        }),
        ExclusionCommand::Add { domain } => exclusions.add(domain).map(|()| {
            state.log_event(
                crate::stats::api::EventKind::Info,
                format!("excluded domain added: {domain} (bypasses MITM)"),
            );
        }),
        ExclusionCommand::SetEnabled { domain, enabled } => {
            exclusions.set_enabled(domain, *enabled).map(|found| {
                if found {
                    let what = if *enabled { "enabled" } else { "disabled" };
                    state.log_event(
                        crate::stats::api::EventKind::Info,
                        format!("excluded domain {what}: {domain}"),
                    );
                }
            })
        }
    };

    match outcome {
        Ok(()) => json_ok(exclusions_json(exclusions)),
        Err(e) => json_status(StatusCode::BAD_REQUEST, json!({"error": e.to_string()})),
    }
}
