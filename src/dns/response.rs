//! Helpers for building DNS response messages, plus ECH parameter detection
//! and stripping.

use hickory_proto::op::{Edns, Message, MessageType, OpCode, ResponseCode};
use hickory_proto::rr::rdata::svcb::SvcParamKey;
use hickory_proto::rr::{RData, Record};

pub(super) fn record_has_ech(r: &Record) -> bool {
    let params = match &r.data {
        RData::HTTPS(https) => &https.0.svc_params,
        RData::SVCB(svcb) => &svcb.svc_params,
        _ => return false,
    };
    params.iter().any(|(k, _)| *k == SvcParamKey::EchConfigList)
}

pub(super) fn base_response(request: &Message) -> Message {
    let mut resp = Message::response(request.metadata.id, OpCode::Query);
    resp.metadata.recursion_desired = request.metadata.recursion_desired;
    resp.metadata.recursion_available = true;
    resp.add_queries(request.queries.iter().cloned());
    if request.edns.is_some() {
        let mut edns = Edns::new();
        edns.set_max_payload(1232);
        resp.edns = Some(edns);
    }
    resp
}

pub(super) fn finish_response(mut resp: Message, request: &Message) -> Message {
    resp.metadata.id = request.metadata.id;
    resp.metadata.message_type = MessageType::Response;
    resp.metadata.op_code = OpCode::Query;
    resp.metadata.recursion_desired = request.metadata.recursion_desired;
    resp.metadata.recursion_available = true;
    resp.metadata.authoritative = false;
    resp.queries = request.queries.clone();
    resp.signature = None;
    resp.edns = request.edns.is_some().then(|| {
        let mut edns = Edns::new();
        edns.set_max_payload(1232);
        edns
    });
    resp
}

pub(super) fn error_response(request: &Message, code: ResponseCode) -> Message {
    let mut resp = base_response(request);
    resp.metadata.response_code = code;
    resp
}

pub(super) fn strip_ech_params(msg: &mut Message) -> usize {
    let mut stripped = 0;
    for r in msg.answers.iter_mut().chain(msg.additionals.iter_mut()) {
        let params = match &mut r.data {
            RData::HTTPS(https) => &mut https.0.svc_params,
            RData::SVCB(svcb) => &mut svcb.svc_params,
            _ => continue,
        };
        let before = params.len();
        params.retain(|(key, _)| *key != SvcParamKey::EchConfigList);
        stripped += before - params.len();
    }
    stripped
}

pub(super) fn rcode_str(code: ResponseCode) -> &'static str {
    match code {
        ResponseCode::NoError => "NOERROR",
        ResponseCode::FormErr => "FORMERR",
        ResponseCode::ServFail => "SERVFAIL",
        ResponseCode::NXDomain => "NXDOMAIN",
        ResponseCode::NotImp => "NOTIMP",
        ResponseCode::Refused => "REFUSED",
        _ => "OTHER",
    }
}

pub(super) fn summarize_answers(resp: &Message) -> String {
    let mut parts: Vec<String> = resp
        .answers
        .iter()
        .take(3)
        .map(|r| match &r.data {
            RData::A(a) => a.0.to_string(),
            RData::AAAA(aaaa) => aaaa.0.to_string(),
            RData::CNAME(c) => format!("CNAME {}", c.0),
            RData::PTR(p) => p.0.to_utf8(),
            RData::HTTPS(_) => format!("HTTPS{}", if record_has_ech(r) { " +ech" } else { "" }),
            other => other.record_type().to_string(),
        })
        .collect();
    let extra = resp.answers.len().saturating_sub(3);
    if extra > 0 {
        parts.push(format!("+{extra} more"));
    }
    parts.join(", ")
}
