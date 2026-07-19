//! API handlers for the error log.

use serde_json::json;

use crate::stats::SharedState;

use super::respond::json_ok;
use super::AdminResponse;

pub(super) fn error_log(state: &SharedState) -> AdminResponse {
    json_ok(json!({ "errors": state.error_log() }))
}

pub(super) fn clear_errors(state: &SharedState) -> AdminResponse {
    let cleared = state.clear_error_log();
    json_ok(json!({ "ok": true, "cleared": cleared }))
}
