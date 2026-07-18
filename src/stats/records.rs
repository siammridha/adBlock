//! Record types for proxied requests and DNS queries shown in the UI.

use std::sync::Arc;

use super::Event;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestKind {
    #[default]
    Forwarded,
    Blocked,
    Tunnel,
    Failed,
}

// The captured artifacts (headers/bodies/scriptlets) are heavy and only fetched
// on demand, so they are stripped from the persisted list line via
// `skip_serializing_if` and live instead in the detail sidecar
// (`RequestDetail`). `#[serde(default)]` lets old log lines — and the lean list
// lines that omit the captures — deserialize cleanly.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RequestRecord {
    pub seq: u64,
    pub ts_ms: u64,
    pub method: String,
    pub status: u16,
    pub kind: RequestKind,
    #[serde(rename = "type")]
    pub req_type: String,
    pub url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub req_body: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub resp_body: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub req_headers: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub resp_headers: String,
    pub blocked_by: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub scriptlets: String,
    pub ech: bool,
}

/// The heavy, fetched-on-demand half of a request record: the captured headers,
/// bodies, and scriptlet names. Written to the detail sidecar when the exchange
/// finishes and returned by the `/api/request` detail endpoint.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RequestDetail {
    pub seq: u64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub req_headers: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub resp_headers: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub req_body: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub resp_body: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub scriptlets: String,
}

impl RequestDetail {
    /// Pull the captured slots off a finished record. Returns `None` when
    /// nothing was captured, so empty details never touch the sidecar.
    pub(super) fn from_record(r: &RequestRecord) -> Option<Self> {
        if r.req_headers.is_empty()
            && r.resp_headers.is_empty()
            && r.req_body.is_empty()
            && r.resp_body.is_empty()
            && r.scriptlets.is_empty()
        {
            return None;
        }
        Some(Self {
            seq: r.seq,
            req_headers: r.req_headers.clone(),
            resp_headers: r.resp_headers.clone(),
            req_body: r.req_body.clone(),
            resp_body: r.resp_body.clone(),
            scriptlets: r.scriptlets.clone(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureSlot {
    ReqBody,
    RespBody,
    ReqHeaders,
    RespHeaders,
    Scriptlets,
}

impl CaptureSlot {
    pub fn as_str(&self) -> &'static str {
        match self {
            CaptureSlot::ReqBody => "req_body",
            CaptureSlot::RespBody => "resp_body",
            CaptureSlot::ReqHeaders => "req_headers",
            CaptureSlot::RespHeaders => "resp_headers",
            CaptureSlot::Scriptlets => "scriptlets",
        }
    }

    pub fn apply(self, record: &mut RequestRecord, text: String) {
        *match self {
            CaptureSlot::ReqBody => &mut record.req_body,
            CaptureSlot::RespBody => &mut record.resp_body,
            CaptureSlot::ReqHeaders => &mut record.req_headers,
            CaptureSlot::RespHeaders => &mut record.resp_headers,
            CaptureSlot::Scriptlets => &mut record.scriptlets,
        } = text;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DnsOutcome {
    Resolved,
    Cached,
    Blocked,
    Rewritten,
    Error,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct DnsRecord {
    // Monotonic id assigned when the query is recorded; the page cursor orders
    // and de-duplicates rows. Defaults to 0 for log lines written before the
    // field existed.
    #[serde(default)]
    pub seq: u64,
    pub ts_ms: u64,
    pub domain: String,
    pub qtype: String,
    pub outcome: DnsOutcome,
    pub rcode: String,
    pub answers: String,
    pub upstream: String,
    pub ech: bool,
    pub blocked_by: String,
    pub elapsed_ms: u64,
    pub proxy: bool,
}

#[derive(Clone, Debug)]
pub enum UiMsg {
    Request(Arc<RequestRecord>),
    Attach {
        seq: u64,
        slot: CaptureSlot,
        text: Arc<str>,
    },
    Dns(Arc<DnsRecord>),
    Event(Arc<Event>),
}
