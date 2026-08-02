//! Admin web server: serves the dashboard page and routes the JSON API.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::BodyExt;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::json;
use tokio::net::TcpListener;

use crate::adblock::api::ScriptletUpdater;
use crate::adblock::api::{AdBlocker, ListCuration};
use crate::dns::api::DnsRuntime;
use crate::dns::api::DnsService;
use crate::proxy::api::ProxyRuntime;
use crate::proxy::api::EgressPolicy;
use crate::proxy::api::CertStore;
use crate::proxy::api::ExclusionStore;
use crate::adblock::api::BlocklistFetcher;
use crate::stats::api::SharedState;

mod blocklists;
mod certs;
mod dns;
mod exclusions;
mod logs;
mod meta;
mod respond;
mod server;
mod sse;
mod stats;

use self::blocklists::{
    adblock_settings_json, blocklist_text_json, blocklists_json, check_rule, cosmetic_for_page,
    edit_adblock_config, scriptlet_source_json, scriptlets_json,
};
use self::dns::{
    check_dns_rule, dns_json, edit_dns_config, edit_dns_upstreams, edit_rewrites, probe_ech,
    rewrites_json,
};
use self::exclusions::{edit_exclusions, exclusions_json};
use self::respond::{html, json_ok, json_status, text_status};
use self::server::{edit_proxy_config, edit_server_config, proxy_settings_json};
use self::sse::sse_stream;
use self::stats::stats_json;

type AdminResponse = Response<BoxBody<Bytes, Infallible>>;

pub type Result<T> = std::result::Result<T, Error>;

/// The web app's own error: bad admin listen address or a serving I/O error.
#[derive(Debug)]
pub enum Error {
    Config(String),
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Config(m) => write!(f, "web error: {m}"),
            Error::Io(e) => write!(f, "web io error: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub struct Admin {
    state: Arc<SharedState>,
    adblock: Arc<AdBlocker>,
    curation: Arc<ListCuration>,
    exclusions: Arc<ExclusionStore>,
    updater: Arc<ScriptletUpdater>,
    fetcher: Arc<BlocklistFetcher>,
    proxy_runtime: Arc<ProxyRuntime>,
    dns_runtime: Arc<DnsRuntime>,
    egress: Arc<EgressPolicy>,
    certs: Arc<CertStore>,
}

impl Admin {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        state: Arc<SharedState>,
        adblock: Arc<AdBlocker>,
        curation: Arc<ListCuration>,
        exclusions: Arc<ExclusionStore>,
        updater: Arc<ScriptletUpdater>,
        fetcher: Arc<BlocklistFetcher>,
        proxy_runtime: Arc<ProxyRuntime>,
        dns_runtime: Arc<DnsRuntime>,
        egress: Arc<EgressPolicy>,
        certs: Arc<CertStore>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state,
            adblock,
            curation,
            exclusions,
            updater,
            fetcher,
            proxy_runtime,
            dns_runtime,
            egress,
            certs,
        })
    }

    pub async fn serve(self: Arc<Self>, addr: SocketAddr) -> Result<()> {
        let listener = TcpListener::bind(addr).await?;
        tracing::info!(%addr, "admin web UI listening — open http://{addr}/");

        loop {
            let (stream, _peer) = listener.accept().await?;
            let admin = self.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let svc = service_fn(move |req| {
                    let admin = admin.clone();
                    async move { Ok::<_, Infallible>(admin.route(req).await) }
                });
                if let Err(e) = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, svc)
                    .await
                {
                    tracing::debug!(error = %e, "admin conn ended");
                }
            });
        }
    }

    pub async fn route<B>(&self, req: Request<B>) -> AdminResponse
    where
        B: hyper::body::Body<Data = Bytes> + Send,
    {
        let method = req.method().clone();
        let path = req.uri().path().to_string();
        let query = req.uri().query().unwrap_or("").to_string();

        match (&method, path.as_str()) {
            (&Method::GET, "/") => html(dashboard()),
            (&Method::GET, "/api/stream") => sse_stream(self.state.clone()),
            (&Method::GET, "/api/requests") => logs::requests_page(&self.state, &query),
            (&Method::GET, "/api/request") => logs::request_detail(&self.state, &query),
            (&Method::GET, "/api/request/body") => logs::request_body_decode(&self.state, &query),
            (&Method::GET, "/api/queries") => logs::queries_page(&self.state, &query),
            (&Method::GET, "/api/stats") => json_ok(stats_json(&self.state)),
            (&Method::GET, "/api/errors") => meta::error_log(&self.state),
            (&Method::GET, "/api/scriptlets") => json_ok(scriptlets_json(&self.curation)),
            (&Method::GET, "/api/scriptlet") => {
                json_ok(scriptlet_source_json(&self.curation, &query))
            }
            (&Method::GET, "/api/adblock") => json_ok(adblock_settings_json(&self.adblock)),
            (&Method::GET, "/api/blocklists") => json_ok(blocklists_json(&self.curation)),
            (&Method::GET, "/api/blocklist") => {
                json_ok(blocklist_text_json(&self.curation, &query))
            }
            (&Method::GET, "/api/exclusions") => json_ok(exclusions_json(&self.exclusions)),
            (&Method::GET, "/api/stats/exclusions") => json_ok(stats::exclusions_json(&self.state)),
            (&Method::GET, "/api/server") => {
                json_ok(server::server_status_json(&self.proxy_runtime, &self.dns_runtime).await)
            }
            (&Method::GET, "/api/proxy") => {
                json_ok(proxy_settings_json(&self.egress))
            }
            (&Method::GET, "/api/dns") => {
                json_ok(dns_json(&self.state, &self.dns_runtime.service()))
            }
            (&Method::GET, "/api/dns/rewrites") => {
                self.with_dns(|dns| json_ok(rewrites_json(&dns)))
            }
            (&Method::GET, "/api/certs") => json_ok(certs::certs_json(&self.certs)),
            (&Method::GET, "/api/cert") => certs::cert_download(&self.certs, &query),
            // Legacy path: download the active CA, served by the proxy (which
            // owns certificate state) — same bytes and filename as before.
            (&Method::GET, "/ca-cert.pem") => certs::cert_download(&self.certs, ""),
            (&Method::POST, _) => {
                let body = match req.into_body().collect().await {
                    Ok(b) => b.to_bytes(),
                    Err(_) => {
                        return json_status(StatusCode::BAD_REQUEST, json!({"error": "read body"}))
                    }
                };
                self.execute(&path, &body).await
            }
            _ => text_status(StatusCode::NOT_FOUND, "not found"),
        }
    }

    async fn execute(&self, path: &str, body: &[u8]) -> AdminResponse {
        match path {
            "/api/scriptlets/update" => {
                blocklists::update_scriptlets(&self.state, &self.updater, &self.curation).await
            }
            "/api/stats/reset" => stats::reset(&self.state, &self.dns_runtime.service()),
            "/api/stats/config" => stats::config(&self.state, body),
            "/api/stats/exclusions" => stats::edit_exclusions(&self.state, body),
            "/api/errors/clear" => meta::clear_errors(&self.state),
            "/api/server/config" => {
                edit_server_config(&self.proxy_runtime, &self.dns_runtime, body).await
            }
            "/api/proxy/config" => {
                edit_proxy_config(&self.state, &self.egress, body)
            }
            "/api/certs" => certs::edit_certs(&self.certs, &self.state, body),
            "/api/blocklists" => self.add_blocklist(body).await,
            "/api/exclusions" => edit_exclusions(&self.state, &self.exclusions, body),
            "/api/adblock/config" => edit_adblock_config(&self.state, &self.adblock, body),
            "/api/check" => check_rule(&self.adblock, body),
            "/api/cosmetic" => cosmetic_for_page(&self.adblock, body),
            "/api/dns/flush" => self.with_dns(|dns| {
                let cleared = dns.cache().clear();
                self.state.log_event(
                    crate::stats::api::EventKind::Info,
                    format!("dns cache flushed ({cleared} entries)"),
                );
                json_ok(json!({"ok": true, "cleared": cleared}))
            }),
            "/api/dns/test" => check_dns_rule(&self.adblock, body),
            "/api/dns/rewrites" => self.with_dns(|dns| edit_rewrites(&self.state, &dns, body)),
            "/api/dns/config" => self.with_dns(|dns| edit_dns_config(&self.state, &dns, body)),
            "/api/dns/upstreams" => {
                edit_dns_upstreams(&self.state, &self.dns_runtime.service(), body).await
            }
            "/api/dns/ech-probe" => probe_ech(&self.state, &self.dns_runtime.service()).await,
            _ => text_status(StatusCode::NOT_FOUND, "not found"),
        }
    }

    // The DNS resolver is always available, listener up or not, so rewrites,
    // settings, and cache management keep working while DNS is disabled.
    fn with_dns(&self, f: impl FnOnce(Arc<DnsService>) -> AdminResponse) -> AdminResponse {
        f(self.dns_runtime.service())
    }
}

pub fn parse_addr(s: &str) -> Result<SocketAddr> {
    s.parse()
        .map_err(|e| Error::Config(format!("invalid admin_listen '{s}': {e}")))
}

const DASHBOARD: &str = include_str!("dashboard.html");

fn dashboard() -> Bytes {
    #[cfg(debug_assertions)]
    if let Ok(live) =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/web/dashboard.html"))
    {
        return Bytes::from(live);
    }
    Bytes::from_static(DASHBOARD.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::api::Metric;
    use http_body_util::Full;
    use serde_json::Value;

    use crate::adblock::api::MemoryListStore;
    use crate::adblock::api::AdblockConfig;
    use crate::dns::api::DnsConfig;
    use crate::stats::api::LoggingConfig;
    use crate::adblock::api::{BlocklistFetcher, Downloader};
    use crate::stats::api::StaticInfo;

    struct CannedDownloader(&'static str);

    impl Downloader for CannedDownloader {
        fn fetch_text(
            &self,
            _url: &str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = std::result::Result<String, String>> + Send + '_>,
        > {
            let text = self.0.to_string();
            Box::pin(async move { Ok(text) })
        }
    }

    fn admin_in(
        rules: &[&str],
        downloader: Arc<dyn Downloader>,
        dns_dir: &std::path::Path,
    ) -> Arc<Admin> {
        let cfg = AdblockConfig {
            enabled: true,
            custom_rules: rules.iter().map(|s| s.to_string()).collect(),
            data_dir: std::path::PathBuf::from("/nonexistent-for-tests"),
            auto_update_hours: 0,
            inject_scriptlets: false,
            scriptlet_resources: std::path::PathBuf::new(),
        };
        let (adblock, curation) =
            crate::adblock::api::with_store(&cfg, Arc::new(MemoryListStore::new())).unwrap();
        let state = Arc::new(SharedState::new(
            StaticInfo {
                version: "test".into(),
                listen: String::new(),
                admin_listen: String::new(),
                started: std::time::Instant::now(),
            },
            &LoggingConfig { level: "info".into(), log_actions: true, log_requests: true, ..Default::default() },
        ));
        let exclusions = Arc::new(ExclusionStore::load(std::path::PathBuf::from(
            "/nonexistent-for-tests/excluded-domains.conf",
        )));
        let updater = Arc::new(ScriptletUpdater::ubo(Arc::new(
            crate::adblock::api::HttpClient::new(),
        )));
        let fetcher = Arc::new(BlocklistFetcher::new(curation.clone(), downloader));
        let dns_cfg = DnsConfig::default();
        let dns_svc =
            DnsService::new(&dns_cfg, dns_dir, adblock.clone(), state.clone()).unwrap();
        let egress = crate::proxy::api::EgressPolicy::load(
            dns_dir.join("proxy-settings.json"),
            dns_svc.clone(),
        );
        let proxy_runtime = ProxyRuntime::new(
            state.clone(),
            dns_dir.join("proxy-server.json"),
            None,
            "127.0.0.1:8080",
            true,
        )
        .unwrap();
        let dns_runtime = DnsRuntime::new(
            state.clone(),
            dns_dir.join("dns-server.json"),
            dns_svc,
            &dns_cfg.listen,
            dns_cfg.enabled,
        )
        .unwrap();
        let certs = Arc::new(CertStore::load(
            std::path::PathBuf::from("/nonexistent-for-tests/certs"),
            dns_dir.join("active-ca.json"),
            std::path::PathBuf::from("/nonexistent-for-tests/ca-cert.pem"),
            std::path::PathBuf::from("/nonexistent-for-tests/ca-key.pem"),
        ));
        Admin::new(
            state,
            adblock,
            curation,
            exclusions,
            updater,
            fetcher,
            proxy_runtime,
            dns_runtime,
            egress,
            certs,
        )
    }

    fn admin(rules: &[&str], downloader: Arc<dyn Downloader>) -> Arc<Admin> {
        admin_in(rules, downloader, std::path::Path::new("/nonexistent-for-tests"))
    }

    fn get(path: &str) -> Request<Full<Bytes>> {
        Request::builder().uri(path).body(Full::new(Bytes::new())).unwrap()
    }

    fn post(path: &str, body: &str) -> Request<Full<Bytes>> {
        Request::builder()
            .method(Method::POST)
            .uri(path)
            .body(Full::new(Bytes::from(body.to_string())))
            .unwrap()
    }

    async fn body_json(resp: AdminResponse) -> Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn stats_endpoint_answers_in_process() {
        let admin = admin(&[], Arc::new(CannedDownloader("")));
        let resp = admin.route(get("/api/stats")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["metrics"]["requests_total"], 0);
        assert_eq!(v["info"]["version"], "test");
    }

    #[tokio::test]
    async fn stats_config_round_trips_and_rejects_bad_values() {
        let admin = admin(&[], Arc::new(CannedDownloader("")));
        let v = body_json(admin.route(get("/api/stats")).await).await;
        assert_eq!(v["settings"]["retention_hours"], 24);
        assert_eq!(v["settings"]["log_rotate_hours"], 24);
        let resp =
            admin.route(post("/api/stats/config", r#"{"retention_hours": 48}"#)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["settings"]["retention_hours"], 48);
        assert_eq!(v["settings"]["log_rotate_hours"], 24, "untouched knob stays");
        let resp =
            admin.route(post("/api/stats/config", r#"{"retention_hours": 0}"#)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let resp = admin.route(post("/api/stats/config", "not json")).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = body_json(admin.route(get("/api/stats")).await).await;
        assert_eq!(v["settings"]["retention_hours"], 48);
    }

    #[tokio::test]
    async fn add_url_downloads_through_the_seam_and_installs() {
        let admin = admin(
            &[],
            Arc::new(CannedDownloader("! Title: In Proc List\n||ads.example^\n")),
        );
        let resp = admin
            .route(post("/api/blocklists", r#"{"url": "https://x.example/l.txt"}"#))
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let names: Vec<&str> =
            v["lists"].as_array().unwrap().iter().filter_map(|l| l["name"].as_str()).collect();
        assert!(names.contains(&"In-Proc-List"), "lists: {names:?}");
    }

    #[tokio::test]
    async fn add_url_maps_a_rejected_list_to_400() {
        let admin = admin(&[], Arc::new(CannedDownloader("<html>nope</html>")));
        let resp = admin
            .route(post("/api/blocklists", r#"{"url": "https://x.example/l.txt"}"#))
            .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn rule_tester_answers_through_the_query_interface() {
        let admin = admin(&["||ads.example.com^"], Arc::new(CannedDownloader("")));
        let resp = admin
            .route(post("/api/check", r#"{"url": "https://ads.example.com/x.js", "type": "script"}"#))
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["blocked"], true);
        assert_eq!(v["list"], "custom");
    }

    #[tokio::test]
    async fn a_filtered_page_can_ask_about_names_it_grew_later() {
        let admin = admin(&["##.adsbox", "##.promo-unit"], Arc::new(CannedDownloader("")));

        let resp = admin
            .route(post(
                "/api/cosmetic",
                r#"{"url": "https://spa.example/feed", "classes": ["adsbox"], "ids": []}"#,
            ))
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("access-control-allow-origin").unwrap(),
            "*",
            "the page lives on another origin and could not read the answer otherwise"
        );
        let v = body_json(resp).await;
        let css = v["css"].as_str().unwrap();
        assert!(css.contains(".adsbox{display:none !important}"), "css was: {css}");
        assert!(!css.contains(".promo-unit"), "only the names asked about: {css}");

        // Adblock owns the validating; the web app just renders the refusal.
        let resp = admin.route(post("/api/cosmetic", r#"{"classes": []}"#)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(resp).await["error"], "missing 'url'");
    }

    #[tokio::test]
    async fn adblock_settings_round_trip_and_reject_bad_input() {
        let admin = admin(&[], Arc::new(CannedDownloader("")));

        let v = body_json(admin.route(get("/api/adblock")).await).await;
        assert_eq!((&v["redirect"], &v["removeparam"]), (&json!(true), &json!(true)));

        let resp = admin.route(post("/api/adblock/config", r#"{"redirect":false}"#)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["redirect"], false);
        assert_eq!(v["removeparam"], true, "an absent key leaves that switch alone");

        // Adblock owns the validating; the web app just renders the refusal.
        let resp = admin.route(post("/api/adblock/config", "[]")).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(resp).await["error"], "expected a JSON object");
    }

    #[tokio::test]
    async fn dns_status_flush_and_tester_answer_in_process() {
        let admin = admin(&["||ads.example.com^"], Arc::new(CannedDownloader("")));

        let resp = admin.route(get("/api/dns")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["enabled"], true);
        assert_eq!(v["blocking_mode"], "null-ip");
        assert_eq!(v["strip_ech"], false);
        assert_eq!(v["upstreams"].as_array().unwrap().len(), 0, "no upstreams configured by default");
        assert_eq!(v["cache"]["entries"], 0);

        let resp = admin.route(post("/api/dns/flush", "")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["cleared"], 0);

        let resp = admin
            .route(post("/api/dns/test", r#"{"domain": "Sub.ADS.example.com."}"#))
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["blocked"], true);
        assert_eq!(v["filter"], "||ads.example.com^");
        assert_eq!(v["list"], "custom");

        let resp = admin.route(post("/api/dns/test", r#"{"domain": "fine.example.org"}"#)).await;
        assert_eq!(body_json(resp).await["blocked"], false);

        let resp = admin.route(post("/api/dns/test", r#"{}"#)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn dns_rewrites_add_list_and_delete_over_the_api() {
        let dir = std::env::temp_dir().join("proxy-web-rewrites-test");
        let _ = std::fs::remove_dir_all(&dir);
        let admin = admin_in(&[], Arc::new(CannedDownloader("")), &dir);

        let v = body_json(admin.route(get("/api/dns/rewrites")).await).await;
        assert_eq!(v["rewrites"].as_array().unwrap().len(), 0);

        let resp = admin
            .route(post("/api/dns/rewrites", r#"{"domain": "App.Example.com", "answer": "1.2.3.4"}"#))
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["rewrites"][0]["domain"], "app.example.com");
        assert_eq!(v["rewrites"][0]["answer"], "1.2.3.4");

        let resp = admin
            .route(post("/api/dns/rewrites", r#"{"domain": "x.example", "answer": "not valid!"}"#))
            .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let resp = admin.route(post("/api/dns/rewrites", r#"{"domain": "x.example"}"#)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let resp = admin
            .route(post(
                "/api/dns/rewrites",
                r#"{"domain": "app.example.com", "answer": "1.2.3.4", "delete": true}"#,
            ))
            .await;
        assert_eq!(body_json(resp).await["rewrites"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn dns_config_updates_live_and_resets() {
        let dir = std::env::temp_dir().join("proxy-web-dnscfg-test");
        let _ = std::fs::remove_dir_all(&dir);
        let admin = admin_in(&[], Arc::new(CannedDownloader("")), &dir);

        let resp = admin
            .route(post(
                "/api/dns/config",
                r#"{"upstreams": ["udp://127.0.0.2:53"], "cache_size": 128, "override_min_ttl_secs": 30}"#,
            ))
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["upstreams"], json!(["udp://127.0.0.2:53"]));
        assert_eq!(v["cache"]["capacity"], 128);
        assert_eq!(v["cache"]["override_min_ttl_secs"], 30);

        // Empty upstreams is allowed now: DNS may run with none configured.
        let resp = admin.route(post("/api/dns/config", r#"{"upstreams": []}"#)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = admin
            .route(post("/api/dns/config", r#"{"override_min_ttl_secs": 999, "override_max_ttl_secs": 1}"#))
            .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let resp = admin.route(post("/api/dns/config", r#"{"reset": true}"#)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["cache"]["capacity"], 4096);
        assert_eq!(v["upstreams"].as_array().unwrap().len(), 0, "reset returns the empty default");
    }

    #[tokio::test]
    async fn server_status_reports_services_and_toggles_dns_live() {
        let dir = std::env::temp_dir().join("proxy-web-server-test");
        let _ = std::fs::remove_dir_all(&dir);
        let admin = admin_in(&[], Arc::new(CannedDownloader("")), &dir);

        let v = body_json(admin.route(get("/api/server")).await).await;
        assert_eq!(v["dns_enabled"], true);
        assert_eq!(v["proxy_controllable"], false);

        let resp = admin
            .route(post("/api/server/config", r#"{"dns_listen": "127.0.0.1:0"}"#))
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["dns_enabled"], true);
        assert_eq!(v["dns_running"], true);

        let resp = admin
            .route(post("/api/server/config", r#"{"dns_listen": "not-an-addr"}"#))
            .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(admin.route(get("/api/dns")).await).await["enabled"], true);

        let resp = admin
            .route(post("/api/server/config", r#"{"dns_enabled": false}"#))
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["dns_enabled"], false);
        assert_eq!(v["dns_running"], false);
        // Disabling only stops the listener. The resolver stays up for the
        // proxy, so the DNS status and cache management keep working.
        assert_eq!(body_json(admin.route(get("/api/dns")).await).await["enabled"], true);
        assert_eq!(
            admin.route(post("/api/dns/flush", "")).await.status(),
            StatusCode::OK
        );
    }


    #[tokio::test]
    async fn stats_carry_dns_counters() {
        let admin = admin(&[], Arc::new(CannedDownloader("")));
        let v = body_json(admin.route(get("/api/stats")).await).await;
        assert_eq!(v["metrics"]["dns_queries_total"], 0);
        assert_eq!(v["metrics"]["dns_blocked_total"], 0);
    }

    #[tokio::test]
    async fn stats_carry_the_24h_window_with_series_and_top_domains() {
        let admin = admin(&[], Arc::new(CannedDownloader("")));
        admin.state.count(Metric::Requests, "site.example");
        for _ in 0..2 {
            admin.state.count_block(Metric::Blocked, "ads.example");
        }
        let v = body_json(admin.route(get("/api/stats")).await).await;
        let w = &v["window"];
        assert_eq!(w["totals"]["requests"], 3);
        assert_eq!(w["totals"]["blocked"], 2);
        let series = w["series"]["blocked"].as_array().unwrap();
        assert_eq!(series.last().unwrap(), 2, "activity lands in the newest bucket");
        assert_eq!(w["top_queried"], json!([{"domain": "site.example", "count": 1}]));
        assert_eq!(w["top_blocked"][0], json!({"domain": "ads.example", "count": 2}));
        assert_eq!(v["metrics"]["blocked_total"], 2);
    }

    #[tokio::test]
    async fn stats_reset_zeroes_counters_and_the_window() {
        let admin = admin(&[], Arc::new(CannedDownloader("")));
        admin.state.count(Metric::Requests, "site.example");
        admin.state.count_block(Metric::Blocked, "ads.example");

        let resp = admin.route(post("/api/stats/reset", "")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["metrics"]["requests_total"], 0);
        assert_eq!(v["metrics"]["blocked_total"], 0);
        assert_eq!(v["window"]["totals"]["requests"], 0);
        assert_eq!(v["window"]["top_queried"].as_array().unwrap().len(), 0);
        assert_eq!(v["window"]["top_blocked"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn error_log_is_served_and_cleared_over_the_api() {
        let admin = admin(&[], Arc::new(CannedDownloader("")));
        admin.state.log_event(crate::stats::api::EventKind::Error, "boom");
        admin.state.log_event(crate::stats::api::EventKind::Info, "not an error");

        let v = body_json(admin.route(get("/api/errors")).await).await;
        let errors = v["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0]["message"], "boom");
        assert_eq!(errors[0]["kind"], "error", "same keys as the SSE event frame");
        assert!(errors[0]["ts_ms"].as_u64().unwrap() > 0);

        let resp = admin.route(post("/api/errors/clear", "")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["cleared"], 1);
        let v = body_json(admin.route(get("/api/errors")).await).await;
        assert_eq!(v["errors"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn log_endpoints_answer_with_page_and_detail_shapes() {
        let admin = admin(&[], Arc::new(CannedDownloader("")));

        // No data dir on the test state, so the pages are empty but well-formed.
        for path in ["/api/requests", "/api/queries"] {
            let resp = admin.route(get(path)).await;
            assert_eq!(resp.status(), StatusCode::OK, "{path}");
            let v = body_json(resp).await;
            assert_eq!(v["records"].as_array().unwrap().len(), 0, "{path}");
            assert_eq!(v["done"], true, "{path}");
        }

        // The detail endpoint echoes the seq even when nothing was captured.
        let resp = admin.route(get("/api/request?seq=7")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["seq"], 7);

        // A detail request without a seq is a client error.
        let resp = admin.route(get("/api/request")).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn body_decode_endpoint_validates_and_reports_missing_captures() {
        let admin = admin(&[], Arc::new(CannedDownloader("")));
        // Missing seq is a client error.
        let resp = admin.route(get("/api/request/body?slot=resp")).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // An unknown slot is a client error.
        let resp = admin.route(get("/api/request/body?seq=1&slot=nope")).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // Well-formed, but nothing was captured (no data dir) → not found.
        let resp = admin.route(get("/api/request/body?seq=1&slot=resp")).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_paths_are_404() {
        let admin = admin(&[], Arc::new(CannedDownloader("")));
        let resp = admin.route(get("/api/nope")).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
