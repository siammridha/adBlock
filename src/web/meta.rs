//! API handlers for the error log and the CA certificate download.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Response, StatusCode};
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

pub(super) fn ca_cert(state: &SharedState) -> AdminResponse {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/x-pem-file")
        .header("content-disposition", "attachment; filename=\"proxy-ca.pem\"")
        .body(Full::new(Bytes::from(state.info.ca_pem.clone())).boxed())
        .unwrap()
}
