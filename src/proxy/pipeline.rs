//! Pure decision logic for a request: where it goes, whether it is blocked,
//! and whether the response gets inspected or injected.

use hyper::{HeaderMap, Request, StatusCode};

use super::html::{html_classes_and_ids, inject_into_html};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub(crate) struct RequestPlan {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub url: String,
    pub method: String,
    pub source: String,
    pub req_type: String,
    pub injection_target: bool,
}

pub(crate) fn plan_request<B>(
    req: &Request<B>,
    secure: bool,
    adblock_enabled: bool,
) -> std::result::Result<RequestPlan, BoxError> {
    let t = target_of(req, secure)?;
    let url = t.url();
    let source = req
        .headers()
        .get(hyper::header::REFERER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let req_type = guess_request_type(req);
    let injection_target = adblock_enabled && is_injectable_type(&req_type);
    Ok(RequestPlan {
        scheme: t.scheme,
        host: t.host,
        port: t.port,
        url,
        method: req.method().to_string(),
        source,
        req_type,
        injection_target,
    })
}

pub(crate) fn response_wants_inspection(
    injection_target: bool,
    status: StatusCode,
    headers: &HeaderMap,
) -> bool {
    injection_target && response_is_injectable(status, headers)
}

pub(crate) struct InspectPlan {
    strip_csp: bool,
    max_inject_bytes: usize,
}

impl InspectPlan {
    fn within_cap(&self, len: usize) -> bool {
        len <= self.max_inject_bytes
    }

    pub fn apply(
        &self,
        parts: &mut hyper::http::response::Parts,
        body: &[u8],
        script: &str,
        cosmetic: impl FnOnce(&[String], &[String]) -> String,
    ) -> Option<Vec<u8>> {
        if !self.within_cap(body.len()) {
            return None;
        }
        let html = std::str::from_utf8(body).ok()?;
        let (classes, ids) = html_classes_and_ids(html);
        let css = cosmetic(&classes, &ids);
        let out = inject_into_html(body, &css, script)?;
        if self.strip_csp {
            for h in CSP_HEADERS {
                parts.headers.remove(h);
            }
        }
        parts.headers.remove(hyper::header::CONTENT_LENGTH);
        Some(out)
    }
}

pub(crate) fn plan_inspection(scriptlets_enabled: bool, max_inspect_bytes: usize) -> InspectPlan {
    InspectPlan {
        strip_csp: scriptlets_enabled,
        max_inject_bytes: max_inspect_bytes.max(4 * 1024 * 1024),
    }
}

pub(crate) fn response_is_injectable(status: StatusCode, headers: &HeaderMap) -> bool {
    let is_html = headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("text/html");
    status.is_success() && is_html && !headers.contains_key(hyper::header::CONTENT_ENCODING)
}

fn target_of<B>(
    req: &Request<B>,
    secure: bool,
) -> std::result::Result<crate::proxy::target::HttpTarget, BoxError> {
    let uri = req.uri();
    let path = uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .to_string();
    if let Some(authority) = uri.authority() {
        let scheme = uri.scheme_str().unwrap_or("http").to_string();
        let host = authority.host().to_string();
        let port = authority
            .port_u16()
            .unwrap_or(crate::proxy::target::default_port(&scheme));
        Ok(crate::proxy::target::HttpTarget { scheme, host, port, path })
    } else {
        let host_header = req
            .headers()
            .get(hyper::header::HOST)
            .and_then(|v| v.to_str().ok())
            .ok_or("request without Host header")?;
        let scheme = if secure { "https" } else { "http" }.to_string();
        let (host, port) =
            crate::proxy::target::split_host_port(host_header, crate::proxy::target::default_port(&scheme));
        Ok(crate::proxy::target::HttpTarget { scheme, host, port, path })
    }
}

fn guess_request_type<B>(req: &Request<B>) -> String {
    if let Some(dest) = req
        .headers()
        .get("sec-fetch-dest")
        .and_then(|v| v.to_str().ok())
    {
        return match dest {
            "document" => "document",
            "iframe" | "frame" => "subdocument",
            "image" => "image",
            "script" => "script",
            "style" => "stylesheet",
            "video" | "audio" => "media",
            "font" => "font",
            "object" | "embed" => "object",
            "empty" => "fetch",
            _ => "other",
        }
        .to_string();
    }
    if req
        .headers()
        .get("x-requested-with")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("XMLHttpRequest"))
    {
        return "fetch".to_string();
    }
    let accept = req
        .headers()
        .get(hyper::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if accept.contains("image/") {
        "image".to_string()
    } else if accept.contains("text/html") {
        "document".to_string()
    } else if !matches!(req.method(), &hyper::Method::GET | &hyper::Method::HEAD) {
        "fetch".to_string()
    } else {
        "other".to_string()
    }
}

pub(crate) fn is_injectable_type(req_type: &str) -> bool {
    matches!(req_type, "document" | "subdocument")
}

pub(crate) const CSP_HEADERS: [&str; 2] = [
    "content-security-policy",
    "content-security-policy-report-only",
];

pub(crate) struct ConnectPlan {
    pub host: String,
    pub url: String,
    pub verdict: ConnectVerdict,
}

pub(crate) enum ConnectVerdict {
    Deny { blocked_by: String },
    BlindTunnel { excluded_by: String },
    Mitm,
}

impl ConnectPlan {
    pub fn record_label(&self) -> &'static str {
        match self.verdict {
            ConnectVerdict::BlindTunnel { .. } => "tunnel-blind",
            _ => "tunnel-mitm",
        }
    }

    pub fn record_tag(&self) -> String {
        match &self.verdict {
            ConnectVerdict::BlindTunnel { excluded_by } => format!("excluded: {excluded_by}"),
            _ => String::new(),
        }
    }
}

pub(crate) fn plan_connect(
    authority: &str,
    check: impl FnOnce(&str) -> crate::adblock::BlockDecision,
    exclusion: impl FnOnce(&str) -> Option<String>,
) -> ConnectPlan {
    let host = authority.split(':').next().unwrap_or("").to_string();
    let url = format!("https://{host}/");
    let decision = check(&url);
    let verdict = if decision.blocked {
        ConnectVerdict::Deny { blocked_by: decision.attribution.display() }
    } else if let Some(excluded_by) = exclusion(&host) {
        ConnectVerdict::BlindTunnel { excluded_by }
    } else {
        ConnectVerdict::Mitm
    };
    ConnectPlan { host, url, verdict }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(uri: &str, headers: &[(&str, &str)]) -> Request<()> {
        let mut b = Request::builder().uri(uri);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(()).unwrap()
    }

    #[test]
    fn plan_absolute_form_request() {
        let r = req("http://example.com/ads.js", &[("accept", "*/*")]);
        let p = plan_request(&r, false, true).unwrap();
        assert_eq!(p.url, "http://example.com/ads.js", "default port stays out of the URL");
        assert_eq!(p.host, "example.com");
        assert_eq!(p.port, 80);
        assert_eq!(p.req_type, "other");
        assert!(!p.injection_target);
    }

    #[test]
    fn plan_origin_form_recovers_host_header() {
        let r = req("/page", &[("host", "example.com:8443"), ("accept", "text/html")]);
        let p = plan_request(&r, true, true).unwrap();
        assert_eq!(p.scheme, "https");
        assert_eq!(p.url, "https://example.com:8443/page");
        assert_eq!(p.req_type, "document");
        assert!(p.injection_target, "HTML navigation should request identity encoding");
    }

    #[test]
    fn plan_origin_form_without_host_fails() {
        let r = req("/page", &[]);
        assert!(plan_request(&r, false, true).is_err());
    }

    #[test]
    fn no_identity_rewrite_when_adblock_disabled() {
        let r = req("/page", &[("host", "example.com"), ("accept", "text/html")]);
        let p = plan_request(&r, true, false).unwrap();
        assert!(!p.injection_target);
    }

    #[test]
    fn fetch_classified_from_sec_fetch_and_from_write_method() {
        let r = req("http://e.com/api", &[("sec-fetch-dest", "empty")]);
        assert_eq!(plan_request(&r, false, true).unwrap().req_type, "fetch");

        let r = Request::builder()
            .method(hyper::Method::POST)
            .uri("http://e.com/api")
            .header("accept", "*/*")
            .body(())
            .unwrap();
        let p = plan_request(&r, false, true).unwrap();
        assert_eq!(p.req_type, "fetch");
        assert!(!p.injection_target, "fetch is not an injection target");

        let r = Request::builder()
            .method(hyper::Method::POST)
            .uri("http://e.com/login")
            .header("accept", "text/html,application/xhtml+xml")
            .body(())
            .unwrap();
        assert_eq!(plan_request(&r, false, true).unwrap().req_type, "document");
    }

    #[test]
    fn identity_encoding_requested_for_frames_too() {
        let r = req("http://e.com/frame", &[("sec-fetch-dest", "iframe")]);
        let p = plan_request(&r, false, true).unwrap();
        assert_eq!(p.req_type, "subdocument");
        assert!(p.injection_target);
    }

    #[test]
    fn only_documents_and_frames_are_injectable() {
        assert!(is_injectable_type("document"));
        assert!(is_injectable_type("subdocument"));
        assert!(!is_injectable_type("fetch"));
        assert!(!is_injectable_type("other"));
        assert!(!is_injectable_type("script"));
    }

    #[test]
    fn sec_fetch_dest_wins_over_accept() {
        let r = req(
            "http://e.com/x",
            &[("sec-fetch-dest", "script"), ("accept", "text/html")],
        );
        let p = plan_request(&r, false, true).unwrap();
        assert_eq!(p.req_type, "script");
    }

    fn resp_headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                hyper::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn injectable_only_for_successful_uncompressed_html() {
        let html = resp_headers(&[("content-type", "text/html; charset=utf-8")]);
        assert!(response_is_injectable(StatusCode::OK, &html));

        let gz = resp_headers(&[("content-type", "text/html"), ("content-encoding", "gzip")]);
        assert!(!response_is_injectable(StatusCode::OK, &gz));

        let json = resp_headers(&[("content-type", "application/json")]);
        assert!(!response_is_injectable(StatusCode::OK, &json));

        let html2 = resp_headers(&[("content-type", "text/html")]);
        assert!(!response_is_injectable(StatusCode::NOT_FOUND, &html2));
    }

    #[test]
    fn inspection_gate_consumes_the_request_time_verdict() {
        let html = resp_headers(&[("content-type", "text/html")]);
        assert!(response_wants_inspection(true, StatusCode::OK, &html));
        assert!(!response_wants_inspection(false, StatusCode::OK, &html));
        let gz = resp_headers(&[("content-type", "text/html"), ("content-encoding", "gzip")]);
        assert!(!response_wants_inspection(true, StatusCode::OK, &gz));
    }

    #[test]
    fn one_verdict_drives_both_the_request_rewrite_and_the_response_gate() {
        let html = resp_headers(&[("content-type", "text/html")]);

        let nav = req("http://e.com/", &[("sec-fetch-dest", "document")]);
        let p = plan_request(&nav, false, true).unwrap();
        assert!(p.injection_target);
        assert!(response_wants_inspection(p.injection_target, StatusCode::OK, &html));

        let xhr = req("http://e.com/api", &[("sec-fetch-dest", "empty")]);
        let p = plan_request(&xhr, false, true).unwrap();
        assert!(!p.injection_target);
        assert!(!response_wants_inspection(p.injection_target, StatusCode::OK, &html));
    }

    fn parts_with(headers: &[(&str, &str)]) -> hyper::http::response::Parts {
        let mut b = hyper::Response::builder().status(StatusCode::OK);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(()).unwrap().into_parts().0
    }

    #[test]
    fn nothing_to_inject_passes_body_and_headers_through() {
        let mut parts = parts_with(&[("content-length", "26")]);
        let plan = plan_inspection(true, 0);
        assert!(plan
            .apply(&mut parts, b"<html><head></head></html>", "", |_, _| String::new())
            .is_none());
        assert!(parts.headers.contains_key(hyper::header::CONTENT_LENGTH));
    }

    #[test]
    fn apply_feeds_the_pages_classes_and_ids_to_the_cosmetic_source() {
        let mut parts = parts_with(&[("content-length", "60")]);
        let out = plan_inspection(false, 0)
            .apply(
                &mut parts,
                b"<html><head></head><body><div class=\"adsbox\" id=\"top\">x</div></body></html>",
                "",
                |classes, ids| {
                    assert_eq!(classes, ["adsbox"]);
                    assert_eq!(ids, ["top"]);
                    ".adsbox{display:none !important}\n".into()
                },
            )
            .unwrap();
        let html = String::from_utf8(out).unwrap();
        assert!(html.contains(".adsbox{display:none !important}"), "html: {html}");
        assert!(!parts.headers.contains_key(hyper::header::CONTENT_LENGTH));
    }

    #[test]
    fn csp_stripped_only_for_scriptlet_injection() {
        let body = b"<html><head></head></html>";
        let mut parts = parts_with(&[("content-security-policy", "script-src 'self'")]);
        assert!(plan_inspection(false, 0)
            .apply(&mut parts, body, "", |_, _| ".ad{}".into())
            .is_some());
        assert!(parts.headers.contains_key("content-security-policy"), "CSS-only keeps the CSP");
        let mut parts = parts_with(&[
            ("content-security-policy", "script-src 'self'"),
            ("content-security-policy-report-only", "script-src 'self'"),
        ]);
        let out = plan_inspection(true, 0)
            .apply(&mut parts, body, "hook()", |_, _| String::new())
            .unwrap();
        assert!(String::from_utf8(out).unwrap().contains("hook()"));
        for h in CSP_HEADERS {
            assert!(!parts.headers.contains_key(h), "{h} must be stripped");
        }
        let mut parts = parts_with(&[("content-security-policy", "script-src 'self'")]);
        assert!(plan_inspection(true, 0)
            .apply(&mut parts, body, "", |_, _| String::new())
            .is_none());
        assert!(parts.headers.contains_key("content-security-policy"));
    }

    #[test]
    fn inject_cap_never_drops_below_4mib() {
        let body: &[u8] = b"<html><head></head></html>";
        for configured in [0, 1, 1024] {
            let plan = plan_inspection(false, configured);
            let mut parts = parts_with(&[]);
            assert!(
                plan.apply(&mut parts, body, "", |_, _| ".ad{}".into()).is_some(),
                "cap {configured} must not block a small page"
            );
        }
        let mut big = b"<html><head></head><body>".to_vec();
        big.resize(4 * 1024 * 1024 + 1, b'x');
        let mut parts = parts_with(&[]);
        assert!(plan_inspection(false, 0)
            .apply(&mut parts, &big, "", |_, _| panic!("oversized body must skip the cosmetic source"))
            .is_none());
        let mut parts = parts_with(&[]);
        assert!(plan_inspection(false, 8 * 1024 * 1024)
            .apply(&mut parts, &big, "", |_, _| ".ad{}".into())
            .is_some());
    }

    fn pass_decision() -> crate::adblock::BlockDecision {
        crate::adblock::BlockDecision {
            blocked: false,
            attribution: crate::adblock::BlockAttribution { rule: None, list: None },
        }
    }

    fn block_by(rule: &str) -> crate::adblock::BlockDecision {
        crate::adblock::BlockDecision {
            blocked: true,
            attribution: crate::adblock::BlockAttribution {
                rule: Some(rule.to_string()),
                list: None,
            },
        }
    }

    #[test]
    fn connect_to_a_plain_host_is_mitm_with_an_untagged_record() {
        let plan = plan_connect(
            "example.com:443",
            |url| {
                assert_eq!(url, "https://example.com/", "filters probe the synthetic URL");
                pass_decision()
            },
            |host| {
                assert_eq!(host, "example.com", "exclusions match on the bare host");
                None
            },
        );
        assert!(matches!(plan.verdict, ConnectVerdict::Mitm));
        assert_eq!(plan.host, "example.com");
        assert_eq!(plan.url, "https://example.com/");
        assert_eq!(plan.record_label(), "tunnel-mitm");
        assert_eq!(plan.record_tag(), "");
    }

    #[test]
    fn connect_to_a_blocked_host_is_denied_with_attribution() {
        let plan = plan_connect("ads.example:443", |_| block_by("||ads.example^"), |_| None);
        match plan.verdict {
            ConnectVerdict::Deny { blocked_by } => assert_eq!(blocked_by, "||ads.example^"),
            _ => panic!("blocked host must be denied"),
        }
    }

    #[test]
    fn connect_to_an_excluded_host_tunnels_blind_and_names_the_rule() {
        let plan = plan_connect(
            "push.apple.com:443",
            |_| pass_decision(),
            |_| Some("apple.com".to_string()),
        );
        assert!(matches!(&plan.verdict, ConnectVerdict::BlindTunnel { excluded_by } if excluded_by == "apple.com"));
        assert_eq!(plan.record_label(), "tunnel-blind");
        assert_eq!(plan.record_tag(), "excluded: apple.com");
    }

    #[test]
    fn connect_block_wins_over_exclusion_and_skips_the_match() {
        let plan = plan_connect(
            "ads.example:443",
            |_| block_by("||ads.example^"),
            |_| panic!("exclusions must not be consulted for a blocked host"),
        );
        assert!(matches!(plan.verdict, ConnectVerdict::Deny { .. }));
    }
}
