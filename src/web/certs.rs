//! API handlers for the certificates tab: list the managed CAs, add or generate
//! one, choose which is active, and download a CA's certificate.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Response, StatusCode};
use serde_json::{json, Value};

use crate::proxy::api::{CertCommand, CertStore};
use crate::stats::api::{EventKind, SharedState};

use super::respond::{json_ok, json_status, parse_query, percent_decode};
use super::AdminResponse;

pub(super) fn certs_json(store: &CertStore) -> Value {
    json!({ "certs": store.list() })
}

pub(super) fn edit_certs(store: &CertStore, state: &SharedState, body: &[u8]) -> AdminResponse {
    let cmd = match CertCommand::parse(body) {
        Ok(c) => c,
        Err(e) => return json_status(StatusCode::BAD_REQUEST, json!({ "error": e })),
    };
    let action = cmd.action();
    let name = cmd.name().to_string();

    let result: std::result::Result<Value, String> = match &cmd {
        CertCommand::Add { name, cert, key } => store
            .add_pem(name, cert, key)
            .map(|_| json!({ "ok": true, "added": name }))
            .map_err(|e| e.to_string()),
        CertCommand::Generate { name, common_name } => store
            .generate(name, common_name)
            .map(|pem| json!({ "ok": true, "added": name, "cert": pem }))
            .map_err(|e| e.to_string()),
        CertCommand::Activate { name } => store
            .activate(name)
            // The running proxy binds its CA once, so a switch needs a restart.
            .map(|_| json!({ "ok": true, "active": name, "restart_needed": true }))
            .map_err(|e| e.to_string()),
        CertCommand::Delete { name } => store
            .delete(name)
            .map(|_| json!({ "ok": true, "deleted": name }))
            .map_err(|e| e.to_string()),
    };

    match result {
        Ok(v) => {
            state.log_event(EventKind::Info, format!("certificate {action} {name}"));
            json_ok(v)
        }
        Err(e) => json_status(StatusCode::BAD_REQUEST, json!({ "error": e })),
    }
}

/// `GET /api/cert?name=<name>` — download a CA's certificate as a PEM file.
/// With no name, downloads the active CA.
pub(super) fn cert_download(store: &CertStore, query: &str) -> AdminResponse {
    let name = parse_query(query, "name").map(percent_decode);
    let (pem, filename) = match name.as_deref() {
        None | Some("") => (store.active_cert_pem(), "proxy-ca.pem".to_string()),
        Some(n) => (store.cert_pem(n), format!("{n}.pem")),
    };
    match pem {
        Ok(pem) => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/x-pem-file")
            .header("content-disposition", format!("attachment; filename=\"{filename}\""))
            .body(Full::new(Bytes::from(pem)).boxed())
            .unwrap(),
        Err(e) => json_status(StatusCode::NOT_FOUND, json!({ "error": e.to_string() })),
    }
}
