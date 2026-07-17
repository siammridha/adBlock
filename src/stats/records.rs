//! Record types for proxied requests and DNS queries shown in the UI.

use std::sync::Arc;

use super::Event;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestKind {
    #[default]
    Forwarded,
    Blocked,
    Tunnel,
    Failed,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct RequestRecord {
    pub seq: u64,
    pub ts_ms: u64,
    pub method: String,
    pub status: u16,
    pub kind: RequestKind,
    #[serde(rename = "type")]
    pub req_type: String,
    pub url: String,
    pub req_body: String,
    pub resp_body: String,
    pub req_headers: String,
    pub resp_headers: String,
    pub blocked_by: String,
    pub scriptlets: String,
    pub ech: bool,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DnsOutcome {
    Resolved,
    Cached,
    Blocked,
    Rewritten,
    Error,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct DnsRecord {
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
