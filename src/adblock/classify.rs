//! Naming a request the way the browser would: its resource type, the page it
//! came from, and whether it may be a beacon. Filter rules match on all three
//! (`$script`, `$image`, `$domain=`, `$1p`/`$3p`, `$ping`), so reading them off
//! the wire is Adblock's job and not the caller's.

use hyper::Request;

/// The `$type` option this request answers to.
pub(super) fn request_type<B>(req: &Request<B>) -> String {
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
    // `text/html` first, and it has to stay first. Both Chrome and Firefox list
    // `image/avif,image/webp` in the `Accept` of a top-level navigation, so
    // testing for an image before HTML calls every page an image — and a page
    // read as an image is never handed the cosmetic rules, scriptlets or `$csp`
    // it should get. An image request never asks for `text/html`, so this order
    // costs the image case nothing.
    if accept.contains("text/html") {
        "document".to_string()
    } else if accept.contains("image/") {
        "image".to_string()
    } else if !matches!(req.method(), &hyper::Method::GET | &hyper::Method::HEAD) {
        "fetch".to_string()
    } else {
        "other".to_string()
    }
}

/// The page this request belongs to — what `$domain=`, `$1p`/`$3p` and `$csp`
/// are asked about.
pub(super) fn source_url<B>(req: &Request<B>, url: &str, req_type: &str) -> String {
    let referer = req
        .headers()
        .get(hyper::header::REFERER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !referer.is_empty() {
        return referer.to_string();
    }
    // Typing a URL in and following a link off-site both arrive without a
    // referer, and a page loaded that way is its own source. Left empty the
    // engine reads the page as third-party to nothing, so none of those rules
    // apply to the one request that matters most. Only for a top-level page: an
    // iframe without a referer really does have a parent we cannot see, and
    // guessing itself there would call a third-party frame first-party.
    match req_type {
        "document" => url.to_string(),
        _ => String::new(),
    }
}

/// A fire-and-forget cross-origin POST: what `navigator.sendBeacon()` produces.
/// `fetch(url, {mode: "no-cors", method: "POST"})` produces the same bytes, and
/// nothing in the request tells them apart — uBO only knows because the browser
/// hands it a resource type. Requiring `sec-fetch-mode` to actually say
/// `no-cors` keeps ordinary same-origin and CORS traffic out of this.
pub(super) fn is_beacon_shaped<B>(req: &Request<B>) -> bool {
    req.method() == hyper::Method::POST
        && header_is(req, hyper::header::HeaderName::from_static("sec-fetch-mode"), "no-cors")
        && header_is(req, hyper::header::HeaderName::from_static("sec-fetch-dest"), "empty")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn req(headers: &[(&str, &str)]) -> Request<()> {
        let mut b = Request::builder().uri("http://e.com/x");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(()).unwrap()
    }

    fn post(headers: &[(&str, &str)]) -> Request<()> {
        let mut b = Request::builder().method(hyper::Method::POST).uri("http://e.com/x");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(()).unwrap()
    }

    /// Browsers only send `sec-fetch-dest` to a trustworthy origin, so on a
    /// plain-http page the `Accept` fallback is all there is — and a real
    /// navigation `Accept` names images too.
    #[test]
    fn a_real_navigation_accept_is_a_document_not_an_image() {
        for accept in [
            // Chrome
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8",
            // Firefox
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
        ] {
            assert_eq!(request_type(&req(&[("accept", accept)])), "document", "{accept}");
        }
        // and an <img> still reads as one
        assert_eq!(
            request_type(&req(&[("accept", "image/avif,image/webp,*/*;q=0.8")])),
            "image"
        );
    }

    #[test]
    fn fetch_named_from_sec_fetch_and_from_a_write_method() {
        assert_eq!(request_type(&req(&[("sec-fetch-dest", "empty")])), "fetch");
        assert_eq!(request_type(&post(&[("accept", "*/*")])), "fetch");
        assert_eq!(
            request_type(&post(&[("accept", "text/html,application/xhtml+xml")])),
            "document",
            "a form post is still a page"
        );
        assert_eq!(request_type(&req(&[("accept", "*/*")])), "other");
    }

    #[test]
    fn a_frame_is_named_a_subdocument() {
        assert_eq!(request_type(&req(&[("sec-fetch-dest", "iframe")])), "subdocument");
    }

    #[test]
    fn sec_fetch_dest_wins_over_accept() {
        assert_eq!(
            request_type(&req(&[("sec-fetch-dest", "script"), ("accept", "text/html")])),
            "script"
        );
    }

    #[test]
    fn websockets_and_pings_are_named_by_their_own_headers() {
        let ws = req(&[("connection", "Upgrade"), ("upgrade", "websocket")]);
        assert_eq!(request_type(&ws), "websocket");

        // Hyperlink auditing is a POST that would otherwise pass for a fetch.
        let ping = post(&[("sec-fetch-dest", "empty"), ("content-type", "text/ping")]);
        assert_eq!(request_type(&ping), "ping");

        assert_eq!(request_type(&req(&[("content-type", "application/json")])), "other");
    }

    #[test]
    fn a_top_level_page_without_a_referer_is_its_own_source() {
        let nav = req(&[("sec-fetch-dest", "document")]);
        assert_eq!(source_url(&nav, "http://e.com/page", "document"), "http://e.com/page");

        let referred = req(&[("referer", "http://other.test/")]);
        assert_eq!(
            source_url(&referred, "http://e.com/page", "document"),
            "http://other.test/",
            "a referer we were given always wins"
        );

        let frame = req(&[("sec-fetch-dest", "iframe")]);
        assert_eq!(
            source_url(&frame, "http://e.com/f", "subdocument"),
            "",
            "a frame has a parent we cannot see; calling it its own would make it first-party"
        );
    }

    #[test]
    fn only_the_exact_beacon_shape_is_flagged() {
        assert!(is_beacon_shaped(&post(&[
            ("sec-fetch-mode", "no-cors"),
            ("sec-fetch-dest", "empty")
        ])));
        assert!(
            !is_beacon_shaped(&post(&[("sec-fetch-mode", "cors"), ("sec-fetch-dest", "empty")])),
            "cors is not a beacon"
        );
        assert!(
            !is_beacon_shaped(&req(&[("sec-fetch-mode", "no-cors"), ("sec-fetch-dest", "empty")])),
            "a GET is not a beacon"
        );
        assert!(
            !is_beacon_shaped(&post(&[
                ("sec-fetch-mode", "no-cors"),
                ("sec-fetch-dest", "image")
            ])),
            "typed, so not ambiguous"
        );
        assert!(!is_beacon_shaped(&post(&[])), "no headers, no guessing");
    }
}
