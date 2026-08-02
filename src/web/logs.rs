//! API handlers for the persisted request and DNS query logs.
//!
//! The list endpoints page backwards from the newest record (cursor = the
//! smallest `seq` already shown), returning lean records without the heavy
//! captured bodies. The detail endpoint fetches one request's captured
//! headers/bodies/scriptlets on demand.

use serde_json::json;

use crate::stats::api::{BodyDecode, SharedState};

use super::respond::{json_ok, json_status, octet_download, parse_query};
use super::AdminResponse;

const DEFAULT_PAGE: usize = 100;
const MAX_PAGE: usize = 500;

fn cursor(query: &str) -> Option<u64> {
    parse_query(query, "before").and_then(|s| s.parse().ok())
}

fn page_size(query: &str) -> usize {
    parse_query(query, "limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PAGE)
        .clamp(1, MAX_PAGE)
}

pub(super) fn requests_page(state: &SharedState, query: &str) -> AdminResponse {
    let limit = page_size(query);
    let records = state.request_page(cursor(query), limit);
    // `done` tells the client to stop paging: a short page means we hit the
    // oldest retained record.
    json_ok(json!({ "records": records, "done": records.len() < limit }))
}

pub(super) fn queries_page(state: &SharedState, query: &str) -> AdminResponse {
    let limit = page_size(query);
    let records = state.query_page(cursor(query), limit);
    json_ok(json!({ "records": records, "done": records.len() < limit }))
}

pub(super) fn request_detail(state: &SharedState, query: &str) -> AdminResponse {
    let Some(seq) = parse_query(query, "seq").and_then(|s| s.parse::<u64>().ok()) else {
        return json_status(
            hyper::StatusCode::BAD_REQUEST,
            json!({ "error": "missing seq" }),
        );
    };
    json_ok(serde_json::to_value(state.request_detail(seq)).unwrap_or_default())
}

/// Decode one captured body on demand. `slot` is `req` or `resp`. Returns the
/// decompressed text, or an explanatory note when there's nothing to decode.
pub(super) fn request_body_decode(state: &SharedState, query: &str) -> AdminResponse {
    let Some(seq) = parse_query(query, "seq").and_then(|s| s.parse::<u64>().ok()) else {
        return json_status(
            hyper::StatusCode::BAD_REQUEST,
            json!({ "error": "missing seq" }),
        );
    };
    let slot = parse_query(query, "slot").unwrap_or_default();
    match state.decode_captured_body(seq, slot) {
        BodyDecode::Text(text) => json_ok(json!({ "text": text })),
        // A binary body comes back as real bytes: hand them to the client as a
        // plain download. The web app renders them (download / hex); it does not
        // decode. Text stays JSON, so the client tells the two apart by
        // content-type.
        BodyDecode::Binary(bytes) => octet_download(bytes),
        BodyDecode::NoData => json_status(
            hyper::StatusCode::NOT_FOUND,
            json!({ "error": "no compressed body captured for this request" }),
        ),
        BodyDecode::UnknownSlot => json_status(
            hyper::StatusCode::BAD_REQUEST,
            json!({ "error": "slot must be req or resp" }),
        ),
    }
}
