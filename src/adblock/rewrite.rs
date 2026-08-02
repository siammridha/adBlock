//! Rewriting a page on its way through: splicing in cosmetic CSS and scriptlet
//! JS, and reading back the class and id names a page carries.
//!
//! This lives in Adblock because every edit here is a filter rule being
//! applied. The caller hands over the bytes it received and forwards the bytes
//! it gets back; it never edits a page itself.

const COSMETIC_RUNTIME: &str = include_str!("cosmetic_runtime.js");

/// Pages bigger than this are forwarded untouched. Splicing costs a full copy
/// of the page, and a page carrying ads is never this big.
pub(crate) const MAX_EDIT_BYTES: usize = 4 * 1024 * 1024;

/// The response headers an inline script of ours cannot run under.
pub(crate) const CSP_HEADERS: [&str; 2] = [
    "content-security-policy",
    "content-security-policy-report-only",
];

/// The live-DOM cosmetic script, pointed at the admin server's cosmetic
/// endpoint. `None` when there is no admin server to ask, in which case pages
/// keep the one-shot scan and nothing else.
///
/// ponytail: the endpoint is built from the configured admin address, so a
/// browser on another machine cannot reach it — a wildcard bind becomes
/// loopback here. Serve the endpoint through the proxy itself if remote
/// clients ever need it.
pub(crate) fn cosmetic_runtime(admin_listen: &str) -> Option<String> {
    let addr = admin_listen.trim();
    if addr.is_empty() {
        return None;
    }
    let port = addr.rsplit(':').next().filter(|p| !p.is_empty())?;
    let host = match addr.rsplit_once(':').map(|(h, _)| h.trim_matches(['[', ']'])) {
        Some("0.0.0.0") | Some("::") | Some("") | None => "127.0.0.1",
        Some(h) if h.contains(':') => return Some(endpoint(&format!("[{h}]"), port)),
        Some(h) => h,
    };
    Some(endpoint(host, port))
}

fn endpoint(host: &str, port: &str) -> String {
    COSMETIC_RUNTIME.replace(
        "__COSMETIC_ENDPOINT__",
        &format!("http://{host}:{port}/api/cosmetic"),
    )
}

/// Whether a response is one we can rewrite at all: a successful, uncompressed
/// HTML page. Anything else is forwarded as it arrived.
pub(crate) fn response_is_editable(status: u16, headers: &hyper::HeaderMap) -> bool {
    let is_html = headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("text/html");
    (200..300).contains(&status) && is_html && !headers.contains_key(hyper::header::CONTENT_ENCODING)
}

/// Splice the CSS and scriptlets into a served page.
///
/// This works on raw bytes, never on a `&str`. The insertion points are all
/// ASCII tags that look identical in every encoding a browser accepts, so a
/// windows-1252 or Shift_JIS page — or one with a single bad byte — still gets
/// filtered, and everything outside the splice is copied through untouched.
pub(crate) fn inject_into_html(html: &[u8], css: &str, script: &str) -> Option<Vec<u8>> {
    if css.is_empty() && script.is_empty() {
        return None;
    }

    let style = (!css.is_empty()).then(|| format!("<style type=\"text/css\">{css}</style>"));
    // `document.currentScript.remove()` takes our own <script> tag back out of
    // the DOM once it has run, so anti-adblock code cannot find it in
    // `document.scripts` and read the scriptlet source back. It sits after the
    // catch, so a scriptlet that throws still leaves the tag cleaned up.
    let script_tag = (!script.is_empty()).then(|| {
        format!("<script>try{{\n{script}\n}}catch(e){{}}document.currentScript.remove()</script>")
    });

    let mut out: Vec<u8> = Vec::with_capacity(html.len() + 256);
    let mut cursor = 0usize;

    if let Some(tag) = script_tag {
        let after_head =
            find_ascii(html, b"<head", 0).and_then(|h| find_ascii(html, b">", h).map(|g| g + 1));
        match after_head {
            Some(p) => {
                out.extend_from_slice(&html[..p]);
                out.extend_from_slice(tag.as_bytes());
                cursor = p;
            }
            None => out.extend_from_slice(tag.as_bytes()),
        }
    }

    if let Some(style) = style {
        let pos = find_ascii(html, b"</head>", cursor)
            .or_else(|| find_ascii(html, b"</body>", cursor));
        match pos {
            Some(p) => {
                out.extend_from_slice(&html[cursor..p]);
                out.extend_from_slice(style.as_bytes());
                cursor = p;
            }
            None => out.extend_from_slice(style.as_bytes()),
        }
    }

    out.extend_from_slice(&html[cursor..]);
    Some(out)
}

/// Case-insensitive search for an ASCII needle in raw page bytes, starting at
/// `from`.
fn find_ascii(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))
        .map(|p| from + p)
}

pub(crate) fn html_classes_and_ids(html: &str) -> (Vec<String>, Vec<String>) {
    let lower = html.to_ascii_lowercase();
    let mut classes = std::collections::HashSet::new();
    let mut ids = std::collections::HashSet::new();
    for (attr, out, split) in [
        ("class", &mut classes, true),
        ("id", &mut ids, false),
    ] {
        let mut from = 0;
        while let Some(p) = lower[from..].find(attr) {
            let start = from + p;
            from = start + attr.len();
            if !lower[..start].ends_with(|c: char| c.is_ascii_whitespace()) {
                continue;
            }
            let rest = &html[start + attr.len()..];
            let Some(rest) = rest.strip_prefix('=') else { continue };
            let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') else {
                continue;
            };
            let value = &rest[1..];
            let Some(end) = value.find(quote) else { continue };
            let value = &value[..end];
            if split {
                out.extend(value.split_ascii_whitespace().map(str::to_string));
            } else if !value.is_empty() {
                out.insert(value.to_string());
            }
        }
    }
    (classes.into_iter().collect(), ids.into_iter().collect())
}

/// Rebuild a request URI from the cleaned absolute URL a `$removeparam` rule
/// produced, keeping the form the client used: absolute-form for a plain proxy
/// request, origin-form inside a MITM'd connection. Returns `None` when the
/// cleaned URL cannot be parsed, so the request forwards unchanged.
pub(crate) fn rewrite_uri(original: &hyper::Uri, clean: &str) -> Option<hyper::Uri> {
    let clean: hyper::Uri = clean.parse().ok()?;
    if original.authority().is_some() {
        Some(clean)
    } else {
        clean.path_and_query()?.as_str().parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_class_and_id_collection_is_pragmatic() {
        let (mut classes, mut ids) = html_classes_and_ids(
            r#"<html><body>
                <div class="adbox banner_ads" id="Top">
                <DIV CLASS='textads'>
                <span data-id="not-an-id"班id="nope">
                <p id="">empty</p>
                <a class="adbox">dup</a>
            </body></html>"#,
        );
        classes.sort();
        ids.sort();
        assert_eq!(classes, vec!["adbox", "banner_ads", "textads"], "split, dedup, case kept");
        assert_eq!(ids, vec!["Top"], "data-id and empty ids are not ids");
    }

    #[test]
    fn inject_style_before_head_close() {
        let out =
            inject_into_html(b"<html><head></head><body>x</body></html>", ".ad{}", "").unwrap();
        let s = String::from_utf8(out).unwrap();
        assert_eq!(
            s,
            "<html><head><style type=\"text/css\">.ad{}</style></head><body>x</body></html>"
        );
    }

    #[test]
    fn inject_style_falls_back_to_body_then_front() {
        let s = String::from_utf8(inject_into_html(b"<body>x</body>", "c", "").unwrap()).unwrap();
        assert!(s.starts_with("<body>x<style"));
        let s = String::from_utf8(inject_into_html(b"plain", "c", "").unwrap()).unwrap();
        assert!(s.starts_with("<style"));
        assert!(s.ends_with("plain"));
    }

    #[test]
    fn inject_script_runs_early_and_before_style() {
        let out = inject_into_html(
            b"<html><head><script>page()</script></head><body>x</body></html>",
            ".ad{}",
            "hook()",
        )
        .unwrap();
        let s = String::from_utf8(out).unwrap();
        let ours = s.find("hook()").unwrap();
        let page = s.find("page()").unwrap();
        let style = s.find("<style").unwrap();
        assert!(ours < page, "scriptlet must precede page script");
        assert!(ours < style, "scriptlet must precede style");
        assert!(s.contains("try{\nhook()\n}catch(e){}"), "wrapped: {s}");
    }

    #[test]
    fn inject_nothing_returns_none() {
        assert!(inject_into_html(b"<html></html>", "", "").is_none());
    }

    #[test]
    fn a_page_that_is_not_utf8_still_gets_filtered() {
        // 0xff/0xfe are not valid UTF-8. Splicing on bytes means the page is
        // still injected and the bad bytes survive the round trip untouched.
        let mut page = b"<html><head></head><body>".to_vec();
        page.extend_from_slice(&[0xff, 0xfe]);
        page.extend_from_slice(b"</body></html>");
        let out = inject_into_html(&page, ".ad{}", "hook()").unwrap();
        assert!(out.windows(6).any(|w| w == b"hook()"), "scriptlets must still be injected");
        assert!(out.windows(6).any(|w| w == b"<style"), "css must still be injected");
        assert!(out.windows(2).any(|w| w == [0xff, 0xfe]), "the original bytes must survive");
    }

    #[test]
    fn the_runtime_points_at_the_admin_endpoint() {
        assert!(cosmetic_runtime("").is_none(), "no admin server, no runtime");
        let js = cosmetic_runtime("127.0.0.1:8081").unwrap();
        assert!(js.contains("\"http://127.0.0.1:8081/api/cosmetic\""), "{js}");
        assert!(!js.contains("__COSMETIC_ENDPOINT__"), "placeholder must be replaced");
        assert!(
            cosmetic_runtime("0.0.0.0:8081").unwrap().contains("http://127.0.0.1:8081/"),
            "a wildcard bind is not reachable from a page; use loopback"
        );
        assert!(cosmetic_runtime("[::1]:8081").unwrap().contains("http://[::1]:8081/"));
    }

    /// The runtime ships as text and only ever runs in a browser, so a syntax
    /// error in it would reach every page before anyone noticed. Node parses it
    /// here as a stand-in; skipped where node is not installed.
    #[test]
    fn the_injected_javascript_parses() {
        if std::process::Command::new("node").arg("-v").output().is_err() {
            eprintln!("node not installed; skipping the runtime syntax check");
            return;
        }
        let path = std::env::temp_dir().join(format!("adblock-js-check-{}.js", std::process::id()));
        std::fs::write(&path, cosmetic_runtime("127.0.0.1:8081").unwrap()).unwrap();
        let out = std::process::Command::new("node").arg("--check").arg(&path).output().unwrap();
        assert!(
            out.status.success(),
            "the cosmetic runtime does not parse: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn the_injected_script_tag_removes_itself() {
        let out = inject_into_html(b"<html><head></head></html>", "", "hook()").unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("}catch(e){}document.currentScript.remove()</script>"), "{s}");
    }

    fn headers(pairs: &[(&str, &str)]) -> hyper::HeaderMap {
        let mut h = hyper::HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                hyper::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn editable_only_for_successful_uncompressed_html() {
        assert!(response_is_editable(200, &headers(&[("content-type", "text/html; charset=utf-8")])));
        assert!(!response_is_editable(
            200,
            &headers(&[("content-type", "text/html"), ("content-encoding", "gzip")])
        ));
        assert!(!response_is_editable(200, &headers(&[("content-type", "application/json")])));
        assert!(!response_is_editable(404, &headers(&[("content-type", "text/html")])));
    }

    #[test]
    fn removeparam_rewrite_keeps_the_form_the_client_used() {
        let absolute: hyper::Uri = "http://e.com/x?a=1&utm_source=ad".parse().unwrap();
        assert_eq!(
            rewrite_uri(&absolute, "http://e.com/x?a=1").unwrap(),
            "http://e.com/x?a=1",
            "an absolute-form request stays absolute"
        );

        let origin: hyper::Uri = "/x?a=1&utm_source=ad".parse().unwrap();
        assert_eq!(
            rewrite_uri(&origin, "https://e.com/x?a=1").unwrap(),
            "/x?a=1",
            "inside a MITM'd connection only the path and query go on the wire"
        );

        assert!(rewrite_uri(&origin, "not a url").is_none(), "unparseable leaves the URI alone");
    }
}
