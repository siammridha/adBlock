//! Rewriting a page on its way through: splicing in cosmetic CSS and scriptlet
//! JS, and reading back the class and id names a page carries.
//!
//! This lives in Adblock because every edit here is a filter rule being
//! applied. The caller hands over the bytes it received and forwards the bytes
//! it gets back; it never edits a page itself.

const COSMETIC_RUNTIME: &str = include_str!("cosmetic_runtime.js");
const BLUR_RUNTIME: &str = include_str!("blur_runtime.js");

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

/// The picture-blurring script, carrying the settings it was built with. Built
/// per page rather than once, because the settings can change between pages and
/// this is a few string replacements on a few kilobytes.
pub(crate) fn blur_runtime(on: &super::settings::DecisionSettings) -> String {
    BLUR_RUNTIME
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
        .replace("__BLUR_RESIZE__", if on.blur_resize { "true" } else { "false" })
        .replace("__BLUR_IMG_SIZE__", &on.blur_img_size.to_string())
        .replace("__BLUR_VIDEO_SIZE__", &on.blur_video_size.to_string())
        .replace("__BLUR_SKIP_SMALL__", if on.blur_skip_small { "true" } else { "false" })
        .replace("__BLUR_MIN_SIZE__", &on.blur_min_size.to_string())
        .replace("__BLUR_MODEL__", on.blur_model.id())
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

    fn blur_settings(
        amount: u8,
        strictness: u8,
        videos: bool,
        regions: bool,
        resize: bool,
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
            blur_resize: resize,
            blur_amount: amount,
            blur_strictness: strictness,
            blur_img_size: 400,
            blur_video_size: 427,
            blur_skip_small: true,
            blur_min_size: 32,
            blur_model: super::super::settings::BlurModel::Human,
        }
    }

    #[test]
    fn the_blur_runtime_carries_its_settings() {
        let js = blur_runtime(&blur_settings(40, 75, false, true, true));
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
        assert!(js.contains("var RESIZE = true;"));
        assert!(js.contains("var IMG_MAX = 400;"));
        assert!(js.contains("var VID_MAX = 427;"));
        assert!(js.contains("data-ab-blur"), "the marks stylesheet goes in with them");
        // The bars a face has to clear are HaramBlur's, slid by the strictness
        // number rather than replaced by it, and a face read as a child is left
        // alone whatever the switches say. Losing either is silent in a browser.
        assert!(js.contains("var MALE_MIN = 0.3 + SHIFT;"), "{js}");
        assert!(js.contains("var FEMALE_MIN = 0.25 + SHIFT;"), "{js}");
        assert!(js.contains("var MIN_AGE = 20;"), "{js}");
        assert!(js.contains("if (f.age !== null && f.age < MIN_AGE) return false;"), "{js}");
        // The chosen model reaches the runtime by name, and the name has to be
        // one the table answers to.
        assert!(js.contains(r#"var model = modelById("human")"#), "{js}");
        assert!(js.contains(r#"id: "human""#), "{js}");
        assert!(!js.contains("__BLUR_"), "every placeholder must be replaced");
        let js = blur_runtime(&blur_settings(10, 10, true, false, false));
        assert!(js.contains("var VIDEOS = true;"));
        assert!(js.contains("var REGIONS = false;"));
        assert!(js.contains("var RESIZE = false;"));
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
        let js = blur_runtime(&blur_settings(20, 50, true, true, true));

        // Every model is a string that only ever gets parsed inside a Worker, so
        // they need building and parsing here rather than reading. The model
        // table is one unbroken run of the file, from the first worker source to
        // the line that picks one, so it is lifted out whole and run with the
        // values it interpolates stubbed. `new Function` then parses each source
        // the table produced — a syntax error in any one of them fails here
        // instead of on a page.
        let mut block = String::new();
        for line in js.lines().skip_while(|l| !l.contains("var TJS = ")) {
            if line.contains("var model = modelById(") {
                break;
            }
            block.push_str(line);
            block.push('\n');
        }
        assert!(block.contains("function humanSrc("), "the worker sources were not found: {block}");
        assert!(block.contains("var MODELS = ["), "the model table was not found: {block}");
        let worker = format!(
            "var HUMAN = 'human', HUMAN_MODELS = 'models/';\n\
             var MAX_FACES = 20;\n\
             {block}\n\
             if (MODELS.length < 2) throw new Error('a table with one model is not a table');\n\
             MODELS.forEach(function (m) {{ new Function(m.src); }});"
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
        let js = blur_runtime(&blur_settings(20, 50, true, true, true));
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
