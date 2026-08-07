//! Rewriting a page on its way through: splicing in cosmetic CSS and scriptlet
//! JS, and reading back the class and id names a page carries.
//!
//! This lives in Adblock because every edit here is a filter rule being
//! applied. The caller hands over the bytes it received and forwards the bytes
//! it gets back; it never edits a page itself.

const COSMETIC_RUNTIME: &str = include_str!("injected/cosmetic_runtime.js");
const BLUR_RUNTIME: &str = include_str!("injected/blur_runtime.js");
const DEBUG_BLUR_RUNTIME: &str = include_str!("injected/debug_blur_runtime.js");

/// A short tag of the blur JS that changes whenever either source file changes.
/// Shown in the debug panel so a page can be checked for running the latest
/// build: same tag means the same bytes were compiled in and served.
static BLUR_VERSION: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    BLUR_RUNTIME.hash(&mut h);
    DEBUG_BLUR_RUNTIME.hash(&mut h);
    format!("{:08x}", h.finish() as u32)
});

/// Pages bigger than this are forwarded untouched. Splicing costs a full copy
/// of the page, and a page carrying ads is never this big.
pub(crate) const MAX_EDIT_BYTES: usize = 4 * 1024 * 1024;

/// The response headers an inline script of ours cannot run under.
pub(crate) const CSP_HEADERS: [&str; 2] = [
    "content-security-policy",
    "content-security-policy-report-only",
];

/// The live-DOM cosmetic script. Nothing in it varies from page to page — it
/// asks the page's own origin — so it is built once.
pub(crate) static COSMETIC_RUNTIME_JS: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| {
        COSMETIC_RUNTIME.replace("__ROUTE_PREFIX__", super::routes::PREFIX)
    });

/// The picture-blurring script, carrying the settings it was built with. Built
/// per page rather than once, because the settings can change between pages and
/// this is a few string replacements on a few kilobytes.
///
/// With `blur_marks` off the runtime goes out on its own and starts itself. With
/// it on, what goes out is the debugging build: the runtime spliced inside
/// `debug_blur_runtime.js`, which puts up the outlines, the boxes and the corner
/// panel, and holds the runtime back until that panel's own switch is ticked. A
/// page that is not being debugged is never sent a byte of the panel.
pub(crate) fn blur_runtime(on: &super::settings::DecisionSettings) -> String {
    let js = match on.blur_marks {
        true => DEBUG_BLUR_RUNTIME.replace("__BLUR_RUNTIME__", BLUR_RUNTIME),
        false => BLUR_RUNTIME.to_string(),
    };
    js.replace("__ROUTE_PREFIX__", super::routes::PREFIX)
        .replace("__BLUR_AMOUNT__", &on.blur_amount.to_string())
        .replace("__BLUR_STRICTNESS__", &on.blur_strictness.to_string())
        .replace("__BLUR_MEN__", if on.blur_men { "true" } else { "false" })
        .replace("__BLUR_WOMEN__", if on.blur_women { "true" } else { "false" })
        .replace("__BLUR_IMAGES__", if on.blur_images { "true" } else { "false" })
        .replace("__BLUR_VIDEOS__", if on.blur_videos { "true" } else { "false" })
        .replace("__BLUR_REGIONS__", if on.blur_regions { "true" } else { "false" })
        .replace("__BLUR_GRAY__", if on.blur_gray { "true" } else { "false" })
        .replace("__BLUR_ON_LOAD__", if on.blur_on_load { "true" } else { "false" })
        .replace("__BLUR_HOVER_IMAGES__", if on.blur_hover_images { "true" } else { "false" })
        .replace("__BLUR_HOVER_VIDEOS__", if on.blur_hover_videos { "true" } else { "false" })
        .replace("__BLUR_MARKS__", if on.blur_marks { "true" } else { "false" })
        .replace("__BLUR_VERSION__", &BLUR_VERSION)
}

/// CSS that blurs media the instant it paints, before the runtime has reached
/// it. Injected into the page's `<head>` when blur-on-load is on, so a picture
/// never flashes unblurred while the model is still downloading. The runtime
/// lifts it per element by adding `abx-blur-processed` once the model returns a
/// verdict; a picture the model could not read never gains the class, so it
/// stays covered.
///
/// Covers CSS backgrounds too: the ones the runtime has marked `data-ab-hold`,
/// and — until the runtime's first sweep has been over the page — anything with
/// an inline `background-image: url(`. The runtime deletes that last selector
/// from this stylesheet once its own marks are in place.
///
/// Empty when neither images nor videos are being looked at — the caller only
/// asks for this when blur-on-load is set, but the two media switches still
/// decide which selectors are worth writing.
pub(crate) fn blur_preload_css(on: &super::settings::DecisionSettings) -> String {
    let filter = format!(
        "blur({}px){}",
        on.blur_amount,
        if on.blur_gray { " grayscale(100%)" } else { "" }
    );
    let mut selectors = Vec::new();
    if on.blur_images {
        selectors.push("img:not(.abx-blur-processed)");
    }
    if on.blur_videos {
        selectors.push("video:not(.abx-blur-processed)");
    }
    if on.blur_images {
        // A CSS background has no tag to match. The runtime marks the ones it
        // finds with `data-ab-hold`, but only once it has run, so until then any
        // element carrying an inline background url is covered on sight. The
        // runtime drops that second selector out of this stylesheet as soon as
        // its first sweep has looked at every element — see BLANKET there.
        selectors.push("[data-ab-hold]:not(.abx-blur-processed)");
        selectors.push(r#"[style*="background-image: url("]"#);
    }
    if selectors.is_empty() {
        return String::new();
    }
    format!("{}{{filter:{filter}!important}}", selectors.join(","))
}

/// Let a page read a picture's pixels back.
///
/// A cross-origin image or video is tainted: the blur runtime can display it
/// but cannot look at it, so nothing can be detected. Saying the response may
/// be read lifts that. Only ever done while the blur is on, only for pictures,
/// and never over a permission the server already granted — its own header is
/// left exactly as it sent it, so a site relying on a narrower one keeps
/// working. The read this allows is credential-free, so it exposes nothing a
/// page could not have fetched for itself.
pub(crate) fn allow_pixel_read(headers: &mut hyper::HeaderMap) {
    const ALLOW_ORIGIN: &str = "access-control-allow-origin";
    if headers.contains_key(ALLOW_ORIGIN) {
        return;
    }
    let is_picture = headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.starts_with("image/") || ct.starts_with("video/"))
        .unwrap_or(false);
    if !is_picture {
        return;
    }
    headers.insert(
        hyper::header::HeaderName::from_static(ALLOW_ORIGIN),
        hyper::header::HeaderValue::from_static("*"),
    );
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

    // Marked so the blur runtime can find this stylesheet again and take its
    // pre-sweep background rule back out once it no longer needs it.
    let style =
        (!css.is_empty()).then(|| format!("<style type=\"text/css\" data-ab-css>{css}</style>"));
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
            "<html><head><style type=\"text/css\" data-ab-css>.ad{}</style></head><body>x</body></html>"
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

    /// The page asks its own address, so the answer reaches it whatever machine
    /// it is on and whether or not the page is HTTPS.
    #[test]
    fn the_runtime_asks_the_pages_own_origin() {
        let js = COSMETIC_RUNTIME_JS.as_str();
        assert!(js.contains("location.origin + \"/__abx/cosmetic\""), "{js}");
        assert!(!js.contains("__ROUTE_PREFIX__"), "placeholder must be replaced");
        assert!(!js.contains("127.0.0.1"), "and nothing may point at the admin server");
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
        std::fs::write(&path, COSMETIC_RUNTIME_JS.as_str()).unwrap();
        let out = std::process::Command::new("node").arg("--check").arg(&path).output().unwrap();
        assert!(
            out.status.success(),
            "the cosmetic runtime does not parse: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::fs::remove_file(&path).ok();
    }

    fn blur_settings(
        amount: u8,
        strictness: u8,
        videos: bool,
        regions: bool,
    ) -> super::super::settings::DecisionSettings {
        super::super::settings::DecisionSettings {
            enabled: true,
            redirect: true,
            removeparam: true,
            csp: true,
            cosmetic: true,
            scriptlets: true,
            runtime: true,
            blur: true,
            blur_men: true,
            blur_women: true,
            blur_images: true,
            blur_videos: videos,
            blur_regions: regions,
            blur_gray: true,
            blur_on_load: true,
            blur_hover_images: true,
            blur_hover_videos: false,
            blur_marks: true,
            blur_amount: amount,
            blur_strictness: strictness,
        }
    }

    #[test]
    fn the_blur_runtime_carries_its_settings() {
        let js = blur_runtime(&blur_settings(40, 75, false, true));
        assert!(js.contains("var AMOUNT = 40;"), "{js}");
        assert!(js.contains("var STRICTNESS = 75;"), "strictness goes in as written");
        assert!(js.contains("var MEN = true;"));
        assert!(js.contains("var WOMEN = true;"));
        assert!(js.contains("var IMAGES = true;"));
        assert!(js.contains("var VIDEOS = false;"));
        assert!(js.contains("var REGIONS = true;"));
        // The switches that only change what a blur looks like still have to
        // reach the page: they are read once, when the stylesheet is built.
        assert!(js.contains("var GRAY = true;"));
        assert!(js.contains("var ON_LOAD = true;"));
        assert!(js.contains("var HOVER_IMAGES = true;"));
        assert!(js.contains("var HOVER_VIDEOS = false;"));
        assert!(js.contains("var MARKS = true;"));
        assert!(js.contains("data-ab-blur"), "the marks stylesheet goes in with them");
        assert!(js.contains("function report("), "and the panel itself");
        assert!(
            js.contains("function CONTROL(") && js.contains("CONTROL({"),
            "with the runtime inside it, handing itself over rather than starting"
        );
        // The weights have no CDN, so the page fetches them from its own address
        // and Adblock answers. Getting this wrong is a detector that never loads.
        assert!(
            js.contains(r#"var MODEL_BASE = location.origin + "/__abx/blur-model/";"#),
            "{js}"
        );
        // Girl rides on the women switch and Person is never hidden. Losing
        // either half is silent in a browser: one leaves girls showing with
        // women blurred, the other covers bodies the model could not read.
        assert!(
            js.contains(r#"(f.gender === "man" && MEN) ||"#)
                && js.contains(r#"((f.gender === "woman" || f.gender === "girl") && WOMEN)"#)
                && !js.contains(r#"f.gender === "person""#),
            "{js}"
        );
        assert!(!js.contains("__BLUR_"), "every placeholder must be replaced");
        assert!(!js.contains("__ROUTE_PREFIX__"), "the route prefix too");
        let js = blur_runtime(&blur_settings(10, 10, true, false));
        assert!(js.contains("var VIDEOS = true;"));
        assert!(js.contains("var REGIONS = false;"));
    }

    #[test]
    fn the_preload_css_blurs_the_media_switches_ask_for() {
        // Images only, colour drained out.
        let css = blur_preload_css(&blur_settings(40, 40, false, false));
        assert_eq!(
            css,
            concat!(
                r#"img:not(.abx-blur-processed),[data-ab-hold]:not(.abx-blur-processed),"#,
                r#"[style*="background-image: url("]"#,
                "{filter:blur(40px) grayscale(100%)!important}"
            )
        );
        // Both kinds share one rule, and the runtime lifts either off the same
        // processed class it adds on a verdict.
        let mut on = blur_settings(20, 40, true, false);
        on.blur_gray = false;
        assert_eq!(
            blur_preload_css(&on),
            concat!(
                r#"img:not(.abx-blur-processed),video:not(.abx-blur-processed),"#,
                r#"[data-ab-hold]:not(.abx-blur-processed),[style*="background-image: url("]"#,
                "{filter:blur(20px)!important}"
            )
        );
        // Videos only: no background selectors, those are the images switch.
        on.blur_images = false;
        assert_eq!(
            blur_preload_css(&on),
            "video:not(.abx-blur-processed){filter:blur(20px)!important}"
        );
        // Nothing to look at, nothing to write.
        on.blur_images = false;
        on.blur_videos = false;
        assert!(blur_preload_css(&on).is_empty());
    }

    /// The panel is a debugging aid that is off by default. A page that did not
    /// ask for it is not sent a byte of it, and the runtime it would have been
    /// wrapped in starts itself instead.
    #[test]
    fn the_panel_only_goes_out_when_it_is_switched_on() {
        let mut off = blur_settings(40, 40, true, true);
        off.blur_marks = false;
        let plain = blur_runtime(&off);
        assert!(plain.contains("var MARKS = false;"));
        assert!(!plain.contains("data-ab-blur"), "no marks stylesheet");
        assert!(!plain.contains("abx-blur-hud"), "no panel");
        assert!(!plain.contains("function report("), "and nothing that draws either");
        assert!(!plain.contains("__BLUR_"), "every placeholder is replaced");
        // Nobody is going to ask it to start, so it asks itself.
        assert!(
            plain.contains("if (typeof CONTROL === \"function\") {"),
            "the runtime still checks for a panel"
        );

        let on = blur_runtime(&blur_settings(40, 40, true, true));
        assert!(on.len() > plain.len(), "so the switched-off page is the smaller one");
    }

    /// The strictness number is the only bar this pipeline has, and 40 has to
    /// land on HaramBlur's own 0.35 or the two are not being compared on the
    /// same terms. Worked out in the page, so it is worked out here too.
    #[test]
    fn the_default_strictness_is_haramblurs_own_score_bar() {
        let bar = |strictness: f64| f64::max(0.05, (0.35 * (100.0 - strictness)) / 60.0);
        assert!((bar(40.0) - 0.35).abs() < 1e-9, "40 is HaramBlur's 0.35");
        assert!(bar(100.0) < bar(10.0), "higher strictness is a lower bar");
        assert_eq!(bar(100.0), 0.05, "and it never reaches zero");
    }

    /// Same reason as the cosmetic runtime: it only ever runs in a browser, so
    /// a syntax error would reach every page unnoticed. The detector source is
    /// a string inside it, so that gets parsed too.
    #[test]
    fn the_blur_runtime_and_its_worker_parse() {
        if std::process::Command::new("node").arg("-v").output().is_err() {
            eprintln!("node not installed; skipping the blur runtime syntax check");
            return;
        }
        let js = blur_runtime(&blur_settings(20, 50, true, true));

        // The worker is a string that only ever gets parsed inside a Worker, so
        // it needs building and parsing here rather than reading. The lines that
        // build it are one unbroken run of the file, so they are lifted out
        // whole and run; `new Function` then parses what they produced, and a
        // syntax error fails here instead of on a page.
        let mut block = String::new();
        for line in js.lines().skip_while(|l| !l.contains("var WORKER_SRC = [")) {
            block.push_str(line);
            block.push('\n');
            if line.trim() == "].join(\"\\n\");" {
                break;
            }
        }
        assert!(block.contains("nonMaxSuppressionAsync"), "the worker was not found: {block}");
        let worker = format!(
            "var TFJS = 'tf.js', MODEL_BASE = 'model/';\n\
             var MODEL_SIZE = 640, CLASSES = ['woman'], MAX_DETECTED = 70, IOU = 0.7;\n\
             var SCORE = 0.35;\n\
             {block}\n\
             new Function(WORKER_SRC);"
        );

        let checks = [("blur runtime", js.clone()), ("blur worker", worker)];
        for (name, source) in checks {
            let path = std::env::temp_dir()
                .join(format!("adblock-{}-{}.js", name.replace(' ', "-"), std::process::id()));
            std::fs::write(&path, &source).unwrap();
            // The runtime is only parsed; the worker script is run, because
            // running it is what parses the string it builds.
            let mut cmd = std::process::Command::new("node");
            if name == "blur runtime" {
                cmd.arg("--check");
            }
            let out = cmd.arg(&path).output().unwrap();
            assert!(
                out.status.success(),
                "the {name} does not parse: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            std::fs::remove_file(&path).ok();
        }
    }

    /// A patch is placed as a fraction of the frame, so the layer has to be the
    /// frame and not the element around it. A video is letterboxed inside its
    /// element by default, and getting this wrong slides every patch sideways,
    /// which no Rust test would see. The function is lifted out and run.
    #[test]
    fn a_letterboxed_frame_is_measured_where_it_is_drawn() {
        if std::process::Command::new("node").arg("-v").output().is_err() {
            eprintln!("node not installed; skipping the frame placement check");
            return;
        }
        let js = blur_runtime(&blur_settings(20, 50, true, true));
        let mut block = String::new();
        let mut at_last = false;
        for line in js.lines().skip_while(|l| !l.contains("function drawn(")) {
            block.push_str(line);
            block.push('\n');
            at_last |= line.contains("function frac(");
            if at_last && line == "  }" {
                break;
            }
        }
        assert!(block.contains("objectFit"), "the placement functions were not found: {block}");

        let script = format!(
            "var fit = 'contain';\n\
             var edge = '0px';\n\
             var place = '50% 50%';\n\
             var window = {{ getComputedStyle: function () {{\n\
               return {{ objectFit: fit, objectPosition: place,\n\
                 borderLeftWidth: edge, borderRightWidth: edge,\n\
                 borderTopWidth: edge, borderBottomWidth: edge, paddingLeft: edge,\n\
                 paddingRight: edge, paddingTop: edge, paddingBottom: edge }};\n\
             }} }};\n\
             {block}\n\
             var assert = require('assert');\n\
             function elem(w, h) {{\n\
               return {{\n\
                 videoWidth: w, videoHeight: h,\n\
                 getBoundingClientRect: function () {{\n\
                   return {{ left: 0, top: 0, width: 400, height: 300 }};\n\
                 }},\n\
               }};\n\
             }}\n\
             // A 16:9 frame in a 4:3 element: bars top and bottom, and the frame\n\
             // is pushed down by half the leftover height.\n\
             var el = elem(1920, 1080);\n\
             var d = drawn(el);\n\
             assert.deepStrictEqual(d.frame, {{ left: 0, top: 37.5, width: 400, height: 225 }});\n\
             // The other way round: a tall frame in a wide element sits in from\n\
             // the left, which is the offset a layer sized to the element misses.\n\
             assert.deepStrictEqual(drawn(elem(1080, 1920)).frame,\n\
               {{ left: 115.625, top: 0, width: 168.75, height: 300 }});\n\
             // A page that moves the frame in its box moves the patches with it:\n\
             // hard down the bottom edge, and a plain length from the top one.\n\
             place = '50% 100%';\n\
             assert.strictEqual(drawn(el).frame.top, 75, 'against the bottom edge');\n\
             place = '50% 12px';\n\
             assert.strictEqual(drawn(el).frame.top, 12, 'a length is from the near edge');\n\
             place = '50% 50%';\n\
             // A border and padding hold the frame in by their own widths, on top\n\
             // of that: 10px of each takes 20 off every side.\n\
             edge = '10px';\n\
             assert.deepStrictEqual(drawn(el).frame, {{ left: 20, top: 48.75, width: 360, height: 202.5 }});\n\
             edge = '0px';\n\
             fit = 'fill';\n\
             assert.deepStrictEqual(drawn(el).frame, d.box, 'a filled frame is the whole box');\n\
             fit = 'cover';\n\
             d = drawn(el);\n\
             assert.strictEqual(d.frame.height, 300, 'a covered frame fills the short side');\n\
             assert.ok(d.frame.left < 0 && d.frame.width > 400, 'and hangs over the long one');\n\
             // A covered frame runs past the box, so a patch is trimmed to the\n\
             // slice of the frame that is on the picture and no wider.\n\
             var vx = seen(d.box.left, d.box.left + d.box.width, d.frame.left, d.frame.width);\n\
             assert.ok(vx[0] > 0 && vx[1] < 1, 'the sides of a covered frame are off the picture');\n\
             assert.ok(Math.abs(vx[0] + vx[1] - 1) < 1e-9, 'centred, so the trim is even');\n\
             assert.deepStrictEqual(\n\
               seen(d.box.top, d.box.top + d.box.height, d.frame.top, d.frame.height), [0, 1],\n\
               'the side it fills is all of it');\n\
             fit = 'contain';\n\
             d = drawn(elem(0, 0));\n\
             assert.deepStrictEqual(d.frame, d.box, 'no frame size, no opinion');\n\
             d = drawn(el);\n\
             assert.deepStrictEqual(\n\
               seen(d.box.left, d.box.left + d.box.width, d.frame.left, d.frame.width), [0, 1],\n\
               'a letterboxed frame is all on the picture, so nothing is trimmed');\n"
        );
        let path = std::env::temp_dir().join(format!("adblock-drawn-{}.js", std::process::id()));
        std::fs::write(&path, &script).unwrap();
        let out = std::process::Command::new("node").arg(&path).output().unwrap();
        assert!(
            out.status.success(),
            "frame placement is wrong: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::fs::remove_file(&path).ok();
    }

    /// HaramBlur shrinks a frame before the model is handed it, and getting the
    /// cap wrong is invisible: the worker scales whatever it gets to 640 anyway,
    /// so a frame carried over whole still comes back with the right answer and
    /// only costs. The function is lifted out and run.
    #[test]
    fn a_frame_is_shrunk_to_its_cap_before_the_model_sees_it() {
        if std::process::Command::new("node").arg("-v").output().is_err() {
            eprintln!("node not installed; skipping the frame shrink check");
            return;
        }
        let js = blur_runtime(&blur_settings(20, 50, true, true));
        let mut block = String::new();
        for line in js.lines().skip_while(|l| !l.contains("function fit(")) {
            block.push_str(line);
            block.push('\n');
            if line == "  }" {
                break;
            }
        }
        assert!(block.contains("resizeQuality"), "the shrink was not found: {block}");

        let script = format!(
            "{block}\n\
             var assert = require('assert');\n\
             // HaramBlur's two caps for this model: 640 for a picture, 720 for a\n\
             // video frame.\n\
             var IMAGE = 640, VIDEO = 720;\n\
             function shrunk(w, h) {{ return {{ resizeWidth: w, resizeHeight: h,\n\
               resizeQuality: 'pixelated' }}; }}\n\
             // Already inside the cap, so nothing is asked for at all: a resize\n\
             // asked for is a resize done, even onto the size it started at.\n\
             assert.strictEqual(fit(474, 316, IMAGE), undefined, 'under the cap');\n\
             assert.strictEqual(fit(640, 640, IMAGE), undefined, 'exactly the cap');\n\
             assert.strictEqual(fit(0, 0, VIDEO), undefined, 'no metadata yet');\n\
             // The cap is on the longest side, so 720p goes down on both paths.\n\
             assert.deepStrictEqual(fit(1280, 720, IMAGE), shrunk(640, 360));\n\
             assert.deepStrictEqual(fit(1280, 720, VIDEO), shrunk(720, 405));\n\
             assert.deepStrictEqual(fit(3840, 2160, VIDEO), shrunk(720, 405), '4K');\n\
             assert.deepStrictEqual(fit(600, 1800, IMAGE), shrunk(213, 640), 'portrait');\n\
             // The shape has to survive it: a frame squashed here reaches the\n\
             // detector with people no longer shaped like people.\n\
             [[1280, 720, IMAGE], [3840, 2160, VIDEO], [600, 1800, IMAGE],\n\
              [1001, 337, VIDEO]].forEach(function (c) {{\n\
               var o = fit(c[0], c[1], c[2]);\n\
               assert.ok(Math.abs(c[0] / c[1] - o.resizeWidth / o.resizeHeight) < 0.01,\n\
                 'shape kept for ' + c[0] + 'x' + c[1]);\n\
               assert.strictEqual(Math.max(o.resizeWidth, o.resizeHeight), c[2],\n\
                 'longest side lands on the cap for ' + c[0] + 'x' + c[1]);\n\
             }});\n"
        );
        let path = std::env::temp_dir().join(format!("adblock-fit-{}.js", std::process::id()));
        std::fs::write(&path, &script).unwrap();
        let out = std::process::Command::new("node").arg(&path).output().unwrap();
        assert!(
            out.status.success(),
            "the frame shrink is wrong: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::fs::remove_file(&path).ok();
    }

    /// Two overlapping patches blur the strip they share twice, which draws a
    /// bright seam down the middle of the thing being hidden, so HaramBlur
    /// swallows one box into the other first. Nothing in Rust can see a seam.
    /// The functions are lifted out and run.
    #[test]
    fn people_standing_together_are_covered_by_one_patch() {
        if std::process::Command::new("node").arg("-v").output().is_err() {
            eprintln!("node not installed; skipping the patch merge check");
            return;
        }
        let js = blur_runtime(&blur_settings(20, 50, true, true));
        let mut block = String::new();
        let mut at_last = false;
        for line in js.lines().skip_while(|l| !l.contains("function merge(")) {
            block.push_str(line);
            block.push('\n');
            at_last |= line.contains("function overlap(");
            if at_last && line == "  }" {
                break;
            }
        }
        assert!(block.contains("Math.min"), "the merge was not found: {block}");

        let script = format!(
            "{block}\n\
             var assert = require('assert');\n\
             function box(x, y, w, h) {{ return {{ x: x, y: y, w: w, h: h }}; }}\n\
             // Apart: two people, two patches.\n\
             var apart = [box(0, 0, .2, .5), box(.5, 0, .2, .5)];\n\
             assert.deepStrictEqual(merge(apart), apart);\n\
             // Touching edge to edge is not overlapping — there is no shared\n\
             // strip to blur twice, so they stay two.\n\
             assert.strictEqual(merge([box(0, 0, .5, .5), box(.5, 0, .5, .5)]).length, 2);\n\
             // Overlapping: one patch, the rectangle around both.\n\
             assert.deepStrictEqual(merge([box(0, 0, .4, .6), box(.3, .2, .4, .6)]),\n\
               [box(0, 0, .7, .8)]);\n\
             // A third that reaches the merged pair joins it too.\n\
             assert.deepStrictEqual(\n\
               merge([box(0, 0, .3, .3), box(.2, .2, .3, .3), box(.4, .4, .3, .3)]),\n\
               [box(0, 0, .7, .7)]);\n\
             // One swallowed whole is still covered.\n\
             assert.deepStrictEqual(merge([box(0, 0, 1, 1), box(.4, .4, .1, .1)]),\n\
               [box(0, 0, 1, 1)]);\n\
             assert.deepStrictEqual(merge([]), []);\n\
             // Whatever it merges, every person has to end up under something.\n\
             [apart, [box(.1, .1, .5, .5), box(.3, .0, .4, .9)],\n\
              [box(0, 0, .3, .3), box(.2, .2, .3, .3), box(.9, .9, .1, .1)]\n\
             ].forEach(function (people) {{\n\
               var out = merge(people);\n\
               people.forEach(function (p) {{\n\
                 assert.ok(out.some(function (o) {{\n\
                   return o.x <= p.x && o.y <= p.y &&\n\
                     o.x + o.w >= p.x + p.w && o.y + o.h >= p.y + p.h;\n\
                 }}), 'nobody is left uncovered');\n\
               }});\n\
             }});\n"
        );
        let path = std::env::temp_dir().join(format!("adblock-merge-{}.js", std::process::id()));
        std::fs::write(&path, &script).unwrap();
        let out = std::process::Command::new("node").arg(&path).output().unwrap();
        assert!(
            out.status.success(),
            "the patch merge is wrong: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn pixels_are_only_opened_up_for_pictures_the_server_did_not_speak_for() {
        let allow = |pairs: &[(&str, &str)]| {
            let mut h = headers(pairs);
            allow_pixel_read(&mut h);
            h.get("access-control-allow-origin").map(|v| v.to_str().unwrap().to_string())
        };
        assert_eq!(allow(&[("content-type", "image/jpeg")]).as_deref(), Some("*"));
        assert_eq!(allow(&[("content-type", "video/mp4")]).as_deref(), Some("*"));
        assert_eq!(allow(&[("content-type", "text/html")]), None, "only pictures");
        assert_eq!(allow(&[]), None, "no content type, no opinion");
        assert_eq!(
            allow(&[("content-type", "image/png"), ("access-control-allow-origin", "https://a.example")])
                .as_deref(),
            Some("https://a.example"),
            "the server's own permission is never widened"
        );
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
