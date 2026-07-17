//! Helpers for building admin API responses and parsing request bodies
//! and query strings.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Response, StatusCode};
use serde_json::{json, Value};

use super::{AdminCommand, AdminResponse};

#[allow(clippy::result_large_err)]
pub(super) fn command<C: AdminCommand>(body: &[u8]) -> std::result::Result<C, AdminResponse> {
    C::parse(body).map_err(|e| json_status(StatusCode::BAD_REQUEST, json!({ "error": e })))
}

pub(super) fn json_ok(v: Value) -> AdminResponse {
    json_status(StatusCode::OK, v)
}

pub(super) fn json_status(status: StatusCode, v: Value) -> AdminResponse {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("cache-control", "no-store")
        .body(Full::new(Bytes::from(v.to_string())).boxed())
        .unwrap()
}

pub(super) fn html(body: impl Into<Bytes>) -> AdminResponse {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/html; charset=utf-8")
        .header("cache-control", "no-store")
        .body(Full::new(body.into()).boxed())
        .unwrap()
}

pub(super) fn text_status(status: StatusCode, msg: &str) -> AdminResponse {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(Full::new(Bytes::from(msg.to_string())).boxed())
        .unwrap()
}

pub(super) fn parse_query<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then_some(v)
    })
}

pub(super) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                    continue;
                }
                out.push(b'%');
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_handles_spaces_and_escapes() {
        assert_eq!(percent_decode("EasyList"), "EasyList");
        assert_eq!(percent_decode("my+list"), "my list");
        assert_eq!(percent_decode("my%20list"), "my list");
        assert_eq!(percent_decode("uBO%20(ads)"), "uBO (ads)");
        assert_eq!(percent_decode("100%done"), "100%done");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
        assert_eq!(percent_decode("trailing%4"), "trailing%4");
        assert_eq!(percent_decode("%"), "%");
    }

    #[test]
    fn parse_query_finds_the_named_key_only() {
        assert_eq!(parse_query("name=easylist&x=1", "name"), Some("easylist"));
        assert_eq!(parse_query("a=1&name=v", "name"), Some("v"));
        assert_eq!(parse_query("other=1", "name"), None);
        assert_eq!(parse_query("", "name"), None);
    }
}
