//! Rule-type tester: a page that reports which adblock rule types are actually
//! being enforced, whoever is enforcing them.
//!
//! The module is self-contained. It owns its page, its filter list, its test
//! assets, its own on/off switch, and the record of which of those assets
//! reached the server. It calls nothing and nothing calls into it except the
//! web app, which mounts [`Tester::route`] and renders whatever comes back.
//!
//! Every verdict is reached in the browser, so the page works the same with
//! this project's proxy, with a browser extension, or with no blocker at all.
//! The server's only job is to say which requests it received: an asset that
//! never arrived was stopped somewhere.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Response, StatusCode};
use serde_json::json;

pub mod api;

type Res = Response<BoxBody<Bytes, Infallible>>;

const PAGE: &str = include_str!("page.html");
const RULES: &str = include_str!("rules.txt");
const FRAME: &str = include_str!("frame.html");
const OTHERS: &str = include_str!("others.html");

/// 1x1 transparent GIF — a real image, so an `<img>` probe that is not blocked
/// fires `load` rather than `error`.
const GIF: &[u8] = &[
    0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xff, 0xff, 0xff, 0x21, 0xf9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00, 0x3b,
];

/// One request the server received during a run.
struct Hit {
    method: String,
    url: String,
}

/// What arrived, per run id. A run is a page load, so this only ever holds the
/// handful of runs open at once.
///
/// ponytail: whole-map reset once it grows past a few runs. Per-run expiry if
/// this ever serves more than a couple of testers at a time.
fn hits() -> &'static Mutex<HashMap<String, Vec<Hit>>> {
    static HITS: OnceLock<Mutex<HashMap<String, Vec<Hit>>>> = OnceLock::new();
    HITS.get_or_init(|| Mutex::new(HashMap::new()))
}

const MAX_RUNS: usize = 8;

fn record(run: &str, method: &Method, url: String) {
    let mut map = hits().lock().unwrap();
    if map.len() > MAX_RUNS && !map.contains_key(run) {
        map.clear();
    }
    map.entry(run.to_string()).or_default().push(Hit {
        method: method.to_string(),
        url,
    });
}

/// The tester: its on/off switch, and the file that remembers it.
pub struct Tester {
    enabled: AtomicBool,
    path: PathBuf,
}

impl Tester {
    /// Read the switch from the tester's own settings file under the data root.
    /// On when there is no file yet.
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join("settings").join("tester.json");
        let saved = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v.get("enabled").and_then(serde_json::Value::as_bool));
        let tester = Self {
            enabled: AtomicBool::new(saved.unwrap_or(true)),
            path,
        };
        // Seed the file a fresh install does not have yet, so the switch is
        // there to edit by hand. A failure here is not worth a warning: the
        // default still applies and saving from the UI reports the problem.
        if !tester.path.exists() {
            if let Err(e) = tester.persist() {
                tracing::debug!(error = %e, "seeding tester settings file");
            }
        }
        tester
    }

    /// On, with no settings file behind it.
    pub fn memory() -> Self {
        Self {
            enabled: AtomicBool::new(true),
            path: PathBuf::new(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Turn the tester on or off and persist the answer. The caller hands over
    /// raw bytes; deciding what is valid is the tester's job.
    pub fn set_enabled(&self, body: &[u8]) -> std::result::Result<bool, String> {
        let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        let on = v
            .get("enabled")
            .and_then(serde_json::Value::as_bool)
            .ok_or("expected a JSON object with an \"enabled\" boolean")?;
        self.enabled.store(on, Ordering::Relaxed);
        self.persist()?;
        Ok(on)
    }

    fn persist(&self) -> std::result::Result<(), String> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        }
        let body = serde_json::to_string_pretty(&json!({ "enabled": self.enabled() }))
            .map_err(|e| e.to_string())?;
        std::fs::write(&self.path, body)
            .map_err(|e| format!("writing {}: {e}", self.path.display()))
    }

    /// Handle anything under `/test`. Returns `None` for every other path, so
    /// the caller falls through to its own routing. Switched off, the tester
    /// serves nothing, so every `/test` address falls through too.
    pub fn route(&self, method: &Method, path: &str, query: &str, host: &str) -> Option<Res> {
        let rest = path.strip_prefix("/test")?;
        if !self.enabled() {
            return None;
        }
        Some(match rest {
            "" | "/" => html(PAGE.to_string()),
            "/rules.txt" => text(
                "text/plain; charset=utf-8",
                RULES
                    .replace("{{H}}", hostname(host))
                    .replace("{{TP}}", third_party_host(hostname(host))),
            ),
            "/hits" => hits_json(query),
            _ => match rest.strip_prefix("/a/") {
                Some(tail) => asset(method, tail, query),
                None => not_found(),
            },
        })
    }
}

/// The `Host` header without its port. Filter syntax names hosts, not ports.
fn hostname(host: &str) -> &str {
    host.rsplit_once(':').map_or(host, |(h, _)| h)
}

/// The host the page uses for anything that has to look like a different
/// registrable domain. Mirrors the same choice in `page.html`: between the two
/// spellings of loopback that domain is free, and on a real name there is no
/// second spelling, so a fixed name is used and a DNS rewrite points it back
/// here.
fn third_party_host(host: &str) -> &str {
    match host {
        "localhost" => "127.0.0.1",
        "127.0.0.1" => "localhost",
        _ => "thirdparty.test",
    }
}

fn hits_json(query: &str) -> Res {
    let run = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("run="))
        .unwrap_or("");
    let map = hits().lock().unwrap();
    let list: Vec<_> = map
        .get(run)
        .map(|v| {
            v.iter()
                .map(|h| json!({ "method": h.method, "url": h.url }))
                .collect()
        })
        .unwrap_or_default();
    json_res(json!({ "run": run, "hits": list }))
}

/// A test asset: record that it arrived, then serve something of the right
/// type. `tail` is `<run>/<name>`.
fn asset(method: &Method, tail: &str, query: &str) -> Res {
    let (run, name) = match tail.split_once('/') {
        Some(pair) => pair,
        None => return not_found(),
    };
    let url = if query.is_empty() {
        format!("/test/a/{tail}")
    } else {
        format!("/test/a/{tail}?{query}")
    };
    record(run, method, url);

    // :others() wrecks any document it shares with other fixtures, so it has a
    // page of its own rather than the shared frame.
    if name == "others.html" {
        return html(OTHERS.to_string());
    }

    // The ##^responseheader() probe. The frame reads this cookie back out of
    // document.cookie; if the rule ran, the header never arrived. A run id that
    // will not fit in a header value simply gets no cookie, and the row reads
    // "enforced" wrongly rather than the server panicking on crafted input.
    if name == "respheader.html" {
        let mut res = html(FRAME.replace("{{RUN}}", run).replace("{{NAME}}", stem(name)));
        if let Ok(v) = format!("t{run}=1; Path=/test").parse() {
            res.headers_mut().insert(hyper::header::SET_COOKIE, v);
        }
        return res;
    }

    let ext = name.rsplit_once('.').map_or("", |(_, e)| e);
    match ext {
        "html" => html(FRAME.replace("{{RUN}}", run).replace("{{NAME}}", stem(name))),
        "js" => text("application/javascript", "/* tester stand-in */\n".into()),
        "css" => text("text/css", "/* tester stand-in */\n".into()),
        "json" => text(
            "application/json",
            "{\"ok\":true,\"marker\":\"NEEDLE\"}".into(),
        ),
        "vtt" => text("text/vtt", "WEBVTT\n".into()),
        "txt" => text("text/plain", "tester\n".into()),
        "gif" => bytes("image/gif", GIF.to_vec()),
        "woff2" => bytes("font/woff2", Vec::new()),
        "mp4" => bytes("video/mp4", Vec::new()),
        "swf" => bytes("application/x-shockwave-flash", Vec::new()),
        "ws" => text("text/plain", "not a websocket server\n".into()),
        _ => bytes("application/octet-stream", Vec::new()),
    }
}

/// `csp.html` -> `csp`, the name the frame posts back to the page.
fn stem(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(s, _)| s)
}

// ------------------------------------------------------------------ responses
//
// Every asset carries `x-test-marker` (the `$header=` probe reads it) and
// `access-control-allow-origin` (the third-party probes are cross-origin).

fn bytes(content_type: &str, body: Vec<u8>) -> Res {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", content_type)
        .header("cache-control", "no-store")
        .header("x-test-marker", "1")
        .header("access-control-allow-origin", "*")
        .body(Full::new(Bytes::from(body)).boxed())
        .unwrap()
}

fn text(content_type: &str, body: String) -> Res {
    bytes(content_type, body.into_bytes())
}

fn html(body: String) -> Res {
    text("text/html; charset=utf-8", body)
}

fn json_res(v: serde_json::Value) -> Res {
    text("application/json", v.to_string())
}

fn not_found() -> Res {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header("content-type", "text/plain")
        .body(Full::new(Bytes::from("not found")).boxed())
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn body_of(res: Res) -> String {
        String::from_utf8_lossy(&res.into_body().collect().await.unwrap().to_bytes()).into_owned()
    }

    #[test]
    fn only_test_paths_are_claimed() {
        assert!(Tester::memory().route(&Method::GET, "/", "", "localhost").is_none());
        assert!(Tester::memory().route(&Method::GET, "/api/stats", "", "localhost").is_none());
        assert!(Tester::memory().route(&Method::GET, "/test", "", "localhost").is_some());
    }

    #[tokio::test]
    async fn the_list_is_written_for_the_host_that_asked() {
        let res = Tester::memory().route(&Method::GET, "/test/rules.txt", "", "127.0.0.1:8080").unwrap();
        let body = body_of(res).await;
        assert!(body.contains("127.0.0.1##.t-hide"), "host substituted without its port");
        assert!(!body.contains("{{H}}"));
        // The :if() rule has to land on the third-party host, not this one, or a
        // blocker that throws while compiling it takes the page's other
        // procedural rules down with it.
        assert!(body.contains("localhost#?#.t-if:if("), "{{TP}} substituted");
        assert!(!body.contains("{{TP}}"));
    }

    /// The `##^responseheader()` probe. Only this one asset carries a cookie,
    /// and it is named after the run so an earlier run cannot pass the row.
    #[tokio::test]
    async fn only_the_respheader_frame_sets_a_cookie() {
        let res = Tester::memory().route(&Method::GET, "/test/a/run9/respheader.html", "", "localhost").unwrap();
        assert_eq!(
            res.headers().get(hyper::header::SET_COOKIE).unwrap(),
            "trun9=1; Path=/test"
        );

        let other = Tester::memory().route(&Method::GET, "/test/a/run9/csp.html", "", "localhost").unwrap();
        assert!(other.headers().get(hyper::header::SET_COOKIE).is_none());
    }

    /// A run id is whatever the browser put in the path, so it reaches a header
    /// value. One that cannot go in a header costs the cookie, not the process.
    #[tokio::test]
    async fn a_run_id_that_cannot_be_a_header_value_is_dropped() {
        let res = Tester::memory().route(&Method::GET, "/test/a/a\rb/respheader.html", "", "localhost").unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().get(hyper::header::SET_COOKIE).is_none());
    }

    /// The uBO column is a lookup keyed by row name. Most names are literals
    /// sitting next to their entry, but the "No setup needed" rows build theirs
    /// from `BAIT` and a list of selectors, so those are the ones that can drift
    /// apart unnoticed and read "?" in the column.
    #[test]
    fn every_generated_row_name_has_a_ubo_entry() {
        for name in ["doubleclick", "googlesyndication", "google-analytics", "adnxs",
                     "scorecardresearch", "criteo", "taboola", "outbrain"] {
            assert!(PAGE.contains(&format!("{{ name: \"{name}\"")), "{name} left BAIT");
            assert!(PAGE.contains(&format!("\"{name}\": ")), "{name} has no uBO entry");
        }
        for sel in ["#ad-slot", "#ad_banner", "#banner-ad", ".sponsored-link"] {
            assert!(PAGE.contains(&format!("\"generic hide {sel}\": ")), "{sel} has no uBO entry");
        }
    }

    /// Switched off the tester answers nothing at all, and the answer survives
    /// a restart because it goes to the tester's own settings file.
    #[test]
    fn the_switch_stops_every_test_path_and_is_remembered() {
        let dir = std::env::temp_dir().join("tester-switch-test");
        let _ = std::fs::remove_dir_all(&dir);
        let t = Tester::load(&dir);
        assert!(t.enabled(), "on by default");
        assert!(t.route(&Method::GET, "/test", "", "localhost").is_some());

        assert!(!t.set_enabled(br#"{"enabled":false}"#).unwrap());
        assert!(t.route(&Method::GET, "/test", "", "localhost").is_none());
        assert!(t
            .route(&Method::GET, "/test/a/run1/x.gif", "", "localhost")
            .is_none());
        assert!(!Tester::load(&dir).enabled(), "the file remembers");

        assert!(t.set_enabled(b"{}").is_err(), "no flag, no change");
        assert!(!t.enabled(), "a rejected update leaves the switch alone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every row is pass or fail. There is no third verdict and no gate: one
    /// rule type missing from the list fails its own row and no other.
    #[test]
    fn a_row_is_only_ever_enforced_or_not() {
        assert!(!PAGE.contains("\"info\""), "no row may report a third verdict");
    }

    #[tokio::test]
    async fn an_asset_is_recorded_and_read_back_for_its_own_run() {
        let res = Tester::memory().route(
            &Method::GET,
            "/test/a/run1/removeparam.json",
            "utm_source=x&keep=1",
            "localhost",
        )
        .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let seen = body_of(Tester::memory().route(&Method::GET, "/test/hits", "run=run1", "localhost").unwrap()).await;
        assert!(seen.contains("/test/a/run1/removeparam.json?utm_source=x&keep=1"));
        assert!(seen.contains("GET"));

        let other = body_of(Tester::memory().route(&Method::GET, "/test/hits", "run=run2", "localhost").unwrap()).await;
        assert!(!other.contains("removeparam"), "runs must not see each other");
    }

    #[tokio::test]
    async fn a_frame_asset_knows_its_own_run_and_name() {
        let body = body_of(Tester::memory().route(&Method::GET, "/test/a/run3/ghide.html", "", "localhost").unwrap()).await;
        assert!(body.contains("frame: 'ghide'"));
        assert!(body.contains("/test/a/run3/gb-probe.gif"));
    }

    #[tokio::test]
    async fn others_gets_its_own_page_not_the_shared_frame() {
        let body = body_of(Tester::memory().route(&Method::GET, "/test/a/run4/others.html", "", "localhost").unwrap()).await;
        assert!(body.contains("frame: 'others'"));
        assert!(!body.contains("gb-probe.gif"), ":others() must not share the frame fixtures");
    }
}
