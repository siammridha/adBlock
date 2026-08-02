//! Pure routing logic for a request: where it goes, and what to call it when
//! asking Adblock about it. Nothing here changes a request or a response —
//! that is Adblock's, and the proxy only forwards what Adblock hands back.

use hyper::Request;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub(crate) struct RequestPlan {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub url: String,
    pub method: String,
    pub source: String,
    pub req_type: String,
    /// This request has the exact shape of a `navigator.sendBeacon()` call,
    /// which on the wire is indistinguishable from a no-cors `fetch()`. The
    /// caller should ask a second time as a `ping` before letting it through.
    pub maybe_beacon: bool,
}

pub(crate) fn plan_request<B>(
    req: &Request<B>,
    secure: bool,
) -> std::result::Result<RequestPlan, BoxError> {
    let t = target_of(req, secure)?;
    let url = t.url();
    let req_type = guess_request_type(req);
    let mut source = req
        .headers()
        .get(hyper::header::REFERER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    // Typing a URL in and following a link off-site both arrive without a
    // referer, and a page loaded that way is its own source — that is what
    // `$domain=`, `$1p`/`$3p` and `$csp` are asked about. Left empty the engine
    // reads the page as third-party to nothing, so none of those rules apply to
    // the one request that matters most. Only for a top-level page: an iframe
    // without a referer really does have a parent we cannot see, and guessing
    // itself there would call a third-party frame first-party.
    if source.is_empty() && req_type == "document" {
        source = url.clone();
    }
    let maybe_beacon = req_type != "ping" && is_beacon_shaped(req);
    Ok(RequestPlan {
        scheme: t.scheme,
        host: t.host,
        port: t.port,
        url,
        method: req.method().to_string(),
        source,
        req_type,
        maybe_beacon,
    })
}

/// A fire-and-forget cross-origin POST: what `navigator.sendBeacon()` produces.
/// `fetch(url, {mode: "no-cors", method: "POST"})` produces the same bytes, and
/// nothing in the request tells them apart — uBO only knows because the browser
/// hands it a resource type. Requiring `sec-fetch-mode` to actually say
/// `no-cors` keeps ordinary same-origin and CORS traffic out of this.
fn is_beacon_shaped<B>(req: &Request<B>) -> bool {
    req.method() == hyper::Method::POST
        && header_is(req, hyper::header::HeaderName::from_static("sec-fetch-mode"), "no-cors")
        && header_is(req, hyper::header::HeaderName::from_static("sec-fetch-dest"), "empty")
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
    // Two types name themselves in a header and are gone by the time
    // `sec-fetch-dest` is read: a WebSocket upgrade carries no `sec-fetch-dest`
    // at all, and hyperlink auditing (`<a ping>`) carries `empty`, which would
    // otherwise pass for a `fetch`.
    if header_is(req, hyper::header::UPGRADE, "websocket") {
        return "websocket".to_string();
    }
    if header_is(req, hyper::header::CONTENT_TYPE, "text/ping") {
        return "ping".to_string();
    }
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

/// Whether a header is present and names `want`, ignoring case and any
/// parameters after a `;`.
fn header_is<B>(req: &Request<B>, name: hyper::header::HeaderName, want: &str) -> bool {
    req.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case(want))
}

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

/// `check` answers with the rule that blocked the host, or `None` to let it
/// through.
pub(crate) fn plan_connect(
    authority: &str,
    check: impl FnOnce(&str) -> Option<String>,
    exclusion: impl FnOnce(&str) -> Option<String>,
) -> ConnectPlan {
    let host = authority.split(':').next().unwrap_or("").to_string();
    let url = format!("https://{host}/");
    let verdict = if let Some(blocked_by) = check(&url) {
        ConnectVerdict::Deny { blocked_by }
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
        let p = plan_request(&r, false).unwrap();
        assert_eq!(p.url, "http://example.com/ads.js", "default port stays out of the URL");
        assert_eq!(p.host, "example.com");
        assert_eq!(p.port, 80);
        assert_eq!(p.req_type, "other");
    }

    #[test]
    fn plan_origin_form_recovers_host_header() {
        let r = req("/page", &[("host", "example.com:8443"), ("accept", "text/html")]);
        let p = plan_request(&r, true).unwrap();
        assert_eq!(p.scheme, "https");
        assert_eq!(p.url, "https://example.com:8443/page");
        assert_eq!(p.req_type, "document");
    }

    #[test]
    fn plan_origin_form_without_host_fails() {
        let r = req("/page", &[]);
        assert!(plan_request(&r, false).is_err());
    }

    #[test]
    fn fetch_classified_from_sec_fetch_and_from_write_method() {
        let r = req("http://e.com/api", &[("sec-fetch-dest", "empty")]);
        assert_eq!(plan_request(&r, false).unwrap().req_type, "fetch");

        let r = Request::builder()
            .method(hyper::Method::POST)
            .uri("http://e.com/api")
            .header("accept", "*/*")
            .body(())
            .unwrap();
        let p = plan_request(&r, false).unwrap();
        assert_eq!(p.req_type, "fetch");

        let r = Request::builder()
            .method(hyper::Method::POST)
            .uri("http://e.com/login")
            .header("accept", "text/html,application/xhtml+xml")
            .body(())
            .unwrap();
        assert_eq!(plan_request(&r, false).unwrap().req_type, "document");
    }

    #[test]
    fn a_frame_is_named_a_subdocument() {
        let r = req("http://e.com/frame", &[("sec-fetch-dest", "iframe")]);
        assert_eq!(plan_request(&r, false).unwrap().req_type, "subdocument");
    }

    #[test]
    fn a_top_level_page_without_a_referer_is_its_own_source() {
        let nav = req("http://e.com/page", &[("sec-fetch-dest", "document")]);
        assert_eq!(plan_request(&nav, false).unwrap().source, "http://e.com/page");

        let referred = req(
            "http://e.com/page",
            &[("sec-fetch-dest", "document"), ("referer", "http://other.test/")],
        );
        assert_eq!(
            plan_request(&referred, false).unwrap().source,
            "http://other.test/",
            "a referer we were given always wins"
        );

        let frame = req("http://e.com/f", &[("sec-fetch-dest", "iframe")]);
        assert_eq!(
            plan_request(&frame, false).unwrap().source,
            "",
            "a frame has a parent we cannot see; calling it its own would make it first-party"
        );
    }

    #[test]
    fn websockets_and_pings_are_named_by_their_own_headers() {
        let ws = req(
            "http://e.com/socket",
            &[("connection", "Upgrade"), ("upgrade", "websocket")],
        );
        assert_eq!(plan_request(&ws, false).unwrap().req_type, "websocket");

        // Hyperlink auditing is a POST that would otherwise pass for a fetch.
        let ping = Request::builder()
            .method(hyper::Method::POST)
            .uri("http://e.com/track")
            .header("sec-fetch-dest", "empty")
            .header("content-type", "text/ping")
            .body(())
            .unwrap();
        assert_eq!(plan_request(&ping, false).unwrap().req_type, "ping");

        let plain = req("http://e.com/api", &[("content-type", "application/json")]);
        assert_eq!(plan_request(&plain, false).unwrap().req_type, "other");
    }

    #[test]
    fn a_beacon_shaped_post_is_flagged_for_a_second_look() {
        let beacon = |extra: &[(&str, &str)]| {
            let mut b = Request::builder()
                .method(hyper::Method::POST)
                .uri("http://e.com/collect")
                .header("sec-fetch-mode", "no-cors")
                .header("sec-fetch-dest", "empty");
            for (k, v) in extra {
                b = b.header(*k, *v);
            }
            b.body(()).unwrap()
        };
        let p = plan_request(&beacon(&[]), false).unwrap();
        assert_eq!(p.req_type, "fetch", "it really is a fetch on the wire");
        assert!(p.maybe_beacon, "and it may equally be a sendBeacon call");

        // Everything that is not that exact shape must not get the second look.
        let cors = Request::builder()
            .method(hyper::Method::POST)
            .uri("http://e.com/api")
            .header("sec-fetch-mode", "cors")
            .header("sec-fetch-dest", "empty")
            .body(())
            .unwrap();
        assert!(!plan_request(&cors, false).unwrap().maybe_beacon, "cors is not a beacon");

        let get = req("http://e.com/x", &[("sec-fetch-mode", "no-cors"), ("sec-fetch-dest", "empty")]);
        assert!(!plan_request(&get, false).unwrap().maybe_beacon, "a GET is not a beacon");

        let img = Request::builder()
            .method(hyper::Method::POST)
            .uri("http://e.com/x.png")
            .header("sec-fetch-mode", "no-cors")
            .header("sec-fetch-dest", "image")
            .body(())
            .unwrap();
        assert!(!plan_request(&img, false).unwrap().maybe_beacon, "typed, so not ambiguous");

        let bare = req("http://e.com/x", &[]);
        assert!(!plan_request(&bare, false).unwrap().maybe_beacon, "no headers, no guessing");

        // A real `<a ping>` is already named; it must not be asked about twice.
        let ping = beacon(&[("content-type", "text/ping")]);
        let p = plan_request(&ping, false).unwrap();
        assert_eq!(p.req_type, "ping");
        assert!(!p.maybe_beacon);
    }

    #[test]
    fn sec_fetch_dest_wins_over_accept() {
        let r = req(
            "http://e.com/x",
            &[("sec-fetch-dest", "script"), ("accept", "text/html")],
        );
        let p = plan_request(&r, false).unwrap();
        assert_eq!(p.req_type, "script");
    }

    #[test]
    fn connect_to_a_plain_host_is_mitm_with_an_untagged_record() {
        let plan = plan_connect(
            "example.com:443",
            |url| {
                assert_eq!(url, "https://example.com/", "filters probe the synthetic URL");
                None
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
        let plan = plan_connect("ads.example:443", |_| Some("||ads.example^".to_string()), |_| None);
        match plan.verdict {
            ConnectVerdict::Deny { blocked_by } => assert_eq!(blocked_by, "||ads.example^"),
            _ => panic!("blocked host must be denied"),
        }
    }

    #[test]
    fn connect_to_an_excluded_host_tunnels_blind_and_names_the_rule() {
        let plan = plan_connect(
            "push.apple.com:443",
            |_| None,
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
            |_| Some("||ads.example^".to_string()),
            |_| panic!("exclusions must not be consulted for a blocked host"),
        );
        assert!(matches!(plan.verdict, ConnectVerdict::Deny { .. }));
    }
}
