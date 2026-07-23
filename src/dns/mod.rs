//! Built-in DNS server: blocks ad domains, caches, applies local rewrites,
//! and forwards the rest to upstream resolvers.

pub mod api;
mod cache;
pub mod commands;
pub mod config;
pub mod control;
pub mod error;
mod lookup;
mod persist;
mod plan;
mod response;
mod rewrites;
mod server;
mod settings;
mod tls;
mod upstream;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hickory_proto::op::{Message, Query, ResponseCode};
use hickory_proto::rr::rdata::{A, CNAME};
use hickory_proto::rr::{Name, RData, Record, RecordType};
use serde::Serialize;

use crate::adblock::api::AdBlocker;
use error::{Error, Result};
pub use config::{BlockingMode, DnsConfig, UpstreamMode};
use crate::stats::api::Metric;
use crate::stats::api::{DnsOutcome, DnsRecord, EventKind, SharedState};

use response::{base_response, error_response, finish_response, rcode_str, strip_ech_params, summarize_answers};

pub use cache::{CacheStatus, DnsCache};
pub use rewrites::{Rewrite, RewriteAnswer, RewriteStore};
pub use settings::DnsOverrides;
use settings::EffectiveDnsSettings;
pub use upstream::UpstreamStat;

const REWRITE_TTL: u32 = 30;

#[derive(Clone, Debug, Serialize)]
pub struct DnsStatus {
    pub listen: String,
    pub upstreams: Vec<String>,
    pub upstream_mode: &'static str,
    pub upstream_stats: Vec<UpstreamStat>,
    pub bootstrap: Vec<String>,
    pub blocking_mode: &'static str,
    pub strip_ech: bool,
    pub ech_probe_domain: String,
    pub log_ipv6: bool,
    pub cache: CacheStatus,
}

pub struct DnsHandles {
    udp: tokio::task::JoinHandle<()>,
    tcp: tokio::task::JoinHandle<()>,
    probe: tokio::task::JoinHandle<()>,
}

impl DnsHandles {
    pub async fn shutdown(self) {
        let tasks = [self.udp, self.tcp, self.probe];
        for t in &tasks {
            t.abort();
        }
        for t in tasks {
            let _ = t.await;
        }
    }
}

pub struct DnsService {
    adblock: Arc<AdBlocker>,
    state: Arc<SharedState>,
    cache: DnsCache,
    live: RwLock<Arc<LiveDns>>,
    rewrites: RewriteStore,
    settings: settings::SettingsStore,
    base: DnsConfig,
    listen: RwLock<SocketAddr>,
}

struct LiveDns {
    resolver: Arc<upstream::Resolver>,
    settings: EffectiveDnsSettings,
}

impl DnsService {
    pub fn new(
        cfg: &DnsConfig,
        data_dir: &Path,
        adblock: Arc<AdBlocker>,
        state: Arc<SharedState>,
    ) -> Result<Arc<Self>> {
        let listen: SocketAddr = cfg
            .listen
            .parse()
            .map_err(|e| Error::Config(format!("invalid dns.listen '{}': {e}", cfg.listen)))?;
        // DNS owns its settings directory; create it so its rewrite and settings
        // files have a home on first write.
        if let Err(e) = std::fs::create_dir_all(data_dir) {
            tracing::warn!(error = %e, dir = %data_dir.display(), "creating dns settings dir");
        }
        let settings = settings::SettingsStore::new(data_dir.join("dns-settings.json"));

        let base_eff = EffectiveDnsSettings::from_config(cfg);
        let mut eff = base_eff.clone().with(&settings.load());
        let resolver = match upstream::Resolver::new(
            &eff.upstreams,
            eff.upstream_mode,
            &eff.bootstrap,
            cfg.upstream_timeout_ms,
        ) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "saved dns upstream override is invalid, using config.toml values");
                eff.upstreams = base_eff.upstreams;
                eff.upstream_mode = base_eff.upstream_mode;
                eff.bootstrap = base_eff.bootstrap;
                upstream::Resolver::new(
                    &eff.upstreams,
                    eff.upstream_mode,
                    &eff.bootstrap,
                    cfg.upstream_timeout_ms,
                )
                .map_err(Error::Config)?
            }
        };
        Ok(Arc::new(Self {
            adblock,
            state,
            cache: DnsCache::new(eff.cache_size, eff.min_ttl_secs, eff.max_ttl_secs),
            live: RwLock::new(Arc::new(LiveDns { resolver: Arc::new(resolver), settings: eff })),
            rewrites: RewriteStore::load(data_dir.join("dns-rewrites.conf")),
            settings,
            base: cfg.clone(),
            listen: RwLock::new(listen),
        }))
    }

    /// Bind the UDP/TCP listeners on `listen` and start serving. The service
    /// itself (resolver, cache, rewrites, settings) is independent of the
    /// listeners: it keeps answering in-process callers when they are down.
    pub async fn start(self: &Arc<Self>, listen: SocketAddr) -> Result<DnsHandles> {
        let (udp, tcp) = server::bind(listen).await?;
        *self.listen.write().expect("dns listen lock") = listen;
        let (udp, tcp) = server::spawn_listeners(self.clone(), udp, tcp);
        let probe = self.spawn_ech_probe();
        Ok(DnsHandles { udp, tcp, probe })
    }

    fn spawn_ech_probe(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let svc = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            loop {
                let domain = svc.ech_probe_domain();
                if !domain.is_empty() {
                    svc.resolver().probe_ech(&domain).await;
                }
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        })
    }

    fn ech_probe_domain(&self) -> String {
        self.live().settings.ech_probe_domain.clone()
    }

    pub fn cache(&self) -> &DnsCache {
        &self.cache
    }

    pub fn rewrites(&self) -> &RewriteStore {
        &self.rewrites
    }

    fn live(&self) -> Arc<LiveDns> {
        self.live.read().expect("live dns lock").clone()
    }

    fn resolver(&self) -> Arc<upstream::Resolver> {
        self.live().resolver.clone()
    }

    #[cfg(test)]
    fn upstream_specs(&self) -> Vec<String> {
        self.live().settings.upstreams.clone()
    }

    #[cfg(test)]
    fn upstream_mode(&self) -> crate::dns::config::UpstreamMode {
        self.live().settings.upstream_mode
    }

    pub fn reset_upstream_stats(&self) {
        self.resolver().reset_stats();
    }

    pub fn status(&self) -> DnsStatus {
        let live = self.live();
        DnsStatus {
            listen: self.listen.read().expect("dns listen lock").to_string(),
            upstreams: live.settings.upstreams.clone(),
            upstream_mode: live.settings.upstream_mode.as_str(),
            upstream_stats: live.resolver.upstream_stats(),
            bootstrap: live.settings.bootstrap.clone(),
            blocking_mode: self.blocking_mode(),
            strip_ech: self.base.strip_ech,
            ech_probe_domain: live.settings.ech_probe_domain.clone(),
            log_ipv6: live.settings.log_ipv6,
            cache: self.cache.status(),
        }
    }

    pub fn apply_settings(&self, upd: DnsOverrides) -> std::result::Result<(), String> {
        let eff = self.live().settings.clone().with(&upd);
        self.realize(eff, || self.settings.save(&self.settings.load().merged_with(&upd)))
    }

    pub fn reset_settings(&self) -> std::result::Result<(), String> {
        self.realize(EffectiveDnsSettings::from_config(&self.base), || self.settings.reset())
    }

    fn realize(
        &self,
        eff: EffectiveDnsSettings,
        persist: impl FnOnce() -> std::result::Result<(), String>,
    ) -> std::result::Result<(), String> {
        eff.validate()?;
        let current = self.live();
        let resolver = if eff.upstreams != current.settings.upstreams
            || eff.upstream_mode != current.settings.upstream_mode
            || eff.bootstrap != current.settings.bootstrap
        {
            Arc::new(upstream::Resolver::new(
                &eff.upstreams,
                eff.upstream_mode,
                &eff.bootstrap,
                self.base.upstream_timeout_ms,
            )?)
        } else {
            current.resolver.clone()
        };

        persist()?;

        self.cache.set_config(eff.cache_size, eff.min_ttl_secs, eff.max_ttl_secs);
        *self.live.write().expect("live dns lock") = Arc::new(LiveDns { resolver, settings: eff });
        Ok(())
    }

    fn blocking_mode(&self) -> &'static str {
        match self.base.blocking_mode {
            BlockingMode::NullIp => "null-ip",
            BlockingMode::Nxdomain => "nxdomain",
            BlockingMode::Refused => "refused",
        }
    }

    pub async fn handle(&self, request: &Message) -> Message {
        self.handle_from(request, false).await
    }

    pub async fn handle_proxy(&self, request: &Message) -> Message {
        self.handle_from(request, true).await
    }

    async fn handle_from(&self, request: &Message, proxy: bool) -> Message {
        let started = Instant::now();

        let (query, domain, verdict) =
            match plan::plan_query(request, &self.rewrites, &self.adblock) {
                plan::QueryPlan::Invalid(code) => {
                    self.state.count(Metric::DnsQueries, "");
                    return error_response(request, code);
                }
                plan::QueryPlan::Answer { query, domain, verdict } => (query, domain, verdict),
            };
        if !matches!(verdict, plan::Verdict::Blocked { .. }) {
            self.state.count(Metric::DnsQueries, &domain);
        }
        let log = QueryLog { query: &query, domain: &domain, started, proxy };

        match verdict {
            plan::Verdict::Nodata => {
                let resp = base_response(request);
                if self.live().settings.log_ipv6 {
                    self.record(&log, &resp, RecordedAs::Nodata);
                }
                resp
            }
            plan::Verdict::Rewrite(answers) => {
                let resp = self.rewrite_response(request, &query, &answers).await;
                self.record(&log, &resp, RecordedAs::Rewritten);
                resp
            }
            plan::Verdict::Blocked { attribution } => {
                self.state.count_block(Metric::DnsBlocked, &domain);
                if self.state.log_actions() {
                    self.state.log_event(
                        EventKind::Blocked,
                        format!("DNS {} {domain} — {attribution}", query.query_type()),
                    );
                }
                let resp = self.blocked_response(request, &query);
                self.record(&log, &resp, RecordedAs::Blocked { attribution });
                resp
            }
            plan::Verdict::Resolve(key) => {
                if let Some(mut cached) = self.cache.get(&key) {
                    self.state.count(Metric::DnsCached, &domain);
                    if self.base.strip_ech {
                        strip_ech_params(&mut cached);
                    }
                    let resp = finish_response(cached, request);
                    self.record(&log, &resp, RecordedAs::Cached);
                    return resp;
                }
                match self.resolver().resolve(&query).await {
                    Ok((mut resp, upstream)) => {
                        self.cache.put(key, &resp);
                        if self.base.strip_ech {
                            let stripped = strip_ech_params(&mut resp);
                            if stripped > 0 {
                                tracing::debug!(%domain, stripped, "removed ECH configs from answer");
                            }
                        }
                        let resp = finish_response(resp, request);
                        self.record(&log, &resp, RecordedAs::Resolved { upstream });
                        resp
                    }
                    Err(e) => {
                        self.state.count_dns_error();
                        self.state
                            .log_event(EventKind::Error, format!("dns {domain}: {e}"));
                        let resp = error_response(request, ResponseCode::ServFail);
                        self.record(&log, &resp, RecordedAs::Error { cause: e });
                        resp
                    }
                }
            }
        }
    }

    async fn rewrite_response(
        &self,
        request: &Message,
        query: &Query,
        answers: &[RewriteAnswer],
    ) -> Message {
        let mut resp = base_response(request);
        let qtype = query.query_type();
        for answer in answers {
            match answer {
                RewriteAnswer::V4(ip) if qtype == RecordType::A => {
                    resp.add_answer(Record::from_rdata(
                        query.name().clone(),
                        REWRITE_TTL,
                        RData::A(A(*ip)),
                    ));
                }
                RewriteAnswer::Cname(target) => {
                    let Ok(target) = Name::from_utf8(format!("{target}.")) else { continue };
                    resp.add_answer(Record::from_rdata(
                        query.name().clone(),
                        REWRITE_TTL,
                        RData::CNAME(CNAME(target.clone())),
                    ));
                    if qtype == RecordType::A {
                        if let Ok((upstream_resp, _)) =
                            self.resolver().resolve(&Query::query(target, qtype)).await
                        {
                            for r in upstream_resp.answers {
                                resp.add_answer(r);
                            }
                        }
                    }
                    break;
                }
                _ => {}
            }
        }
        resp
    }

    fn blocked_response(&self, request: &Message, query: &Query) -> Message {
        let mut resp = base_response(request);
        match self.base.blocking_mode {
            BlockingMode::Refused => {
                resp.metadata.response_code = ResponseCode::Refused;
            }
            BlockingMode::Nxdomain => {
                resp.metadata.response_code = ResponseCode::NXDomain;
            }
            BlockingMode::NullIp => {
                if query.query_type() == RecordType::A {
                    resp.add_answer(Record::from_rdata(
                        query.name().clone(),
                        self.base.blocked_ttl_secs,
                        RData::A(A(std::net::Ipv4Addr::UNSPECIFIED)),
                    ));
                }
            }
        }
        resp
    }

    fn record(&self, log: &QueryLog<'_>, resp: &Message, outcome: RecordedAs) {
        let (kind, upstream, blocked_by, cause) = match outcome {
            RecordedAs::Nodata => (DnsOutcome::Resolved, String::new(), String::new(), None),
            RecordedAs::Resolved { upstream } => (DnsOutcome::Resolved, upstream, String::new(), None),
            RecordedAs::Rewritten => (DnsOutcome::Rewritten, "rewrite".into(), String::new(), None),
            RecordedAs::Cached => (DnsOutcome::Cached, String::new(), String::new(), None),
            RecordedAs::Blocked { attribution } => (DnsOutcome::Blocked, String::new(), attribution, None),
            RecordedAs::Error { cause } => (DnsOutcome::Error, String::new(), String::new(), Some(cause)),
        };
        self.state.record_dns(DnsRecord {
            seq: 0, // assigned by record_dns
            ts_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64),
            domain: log.domain.to_string(),
            qtype: log.query.query_type().to_string(),
            outcome: kind,
            rcode: cause
                .unwrap_or_else(|| rcode_str(resp.metadata.response_code).to_string()),
            answers: summarize_answers(resp),
            upstream,
            ech: upstream::answer_has_ech(resp),
            blocked_by,
            elapsed_ms: log.started.elapsed().as_millis() as u64,
            proxy: log.proxy,
        });
    }
}

struct QueryLog<'a> {
    query: &'a Query,
    domain: &'a str,
    started: Instant,
    proxy: bool,
}

enum RecordedAs {
    Nodata,
    Resolved { upstream: String },
    Rewritten,
    Cached,
    Blocked { attribution: String },
    Error { cause: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    use crate::adblock::api::{with_store, MemoryListStore};
    use crate::adblock::api::AdblockConfig;
    use crate::stats::api::LoggingConfig;
    use crate::stats::api::StaticInfo;
    use hickory_proto::op::OpCode;
    use hickory_proto::rr::rdata::svcb::{EchConfigList, SvcParamKey, SvcParamValue, SVCB};
    use hickory_proto::rr::rdata::HTTPS;
    use hickory_proto::rr::Name;
    use std::path::PathBuf;
    use std::str::FromStr;

    fn build_service(
        rules: &[&str],
        mode: BlockingMode,
        upstreams: Vec<String>,
        data_dir: PathBuf,
    ) -> Arc<DnsService> {
        let cfg = DnsConfig {
            blocking_mode: mode,
            upstreams,
            upstream_timeout_ms: 500,
            ..DnsConfig::default()
        };
        build_service_cfg(rules, cfg, data_dir)
    }

    fn build_service_cfg(rules: &[&str], cfg: DnsConfig, data_dir: PathBuf) -> Arc<DnsService> {
        let adblock_cfg = AdblockConfig {
            enabled: true,
            custom_rules: rules.iter().map(|s| s.to_string()).collect(),
            data_dir: PathBuf::from("/nonexistent-for-tests"),
            auto_update_hours: 0,
            inject_scriptlets: false,
            scriptlet_resources: PathBuf::new(),
        };
        let (adblock, _) = with_store(&adblock_cfg, Arc::new(MemoryListStore::new())).unwrap();
        let state = Arc::new(SharedState::new(
            StaticInfo {
                version: "test".into(),
                listen: String::new(),
                admin_listen: String::new(),
                started: Instant::now(),
            },
            &LoggingConfig { level: "info".into(), log_actions: true, log_requests: true, ..Default::default() },
        ));
        DnsService::new(&cfg, &data_dir, adblock, state).unwrap()
    }

    fn service_with(rules: &[&str], mode: BlockingMode, upstreams: Vec<String>) -> Arc<DnsService> {
        build_service(rules, mode, upstreams, PathBuf::from("/nonexistent-for-tests"))
    }

    fn service_in(tag: &str, upstreams: Vec<String>) -> Arc<DnsService> {
        let dir = std::env::temp_dir().join(format!("proxy-dns-svc-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        build_service(&[], BlockingMode::NullIp, upstreams, dir)
    }

    fn service(rules: &[&str], mode: BlockingMode) -> Arc<DnsService> {
        service_with(rules, mode, vec!["udp://127.0.0.1:1".into()])
    }

    fn query_msg(domain: &str, qtype: RecordType) -> Message {
        let mut msg = Message::query();
        msg.metadata.id = 4242;
        msg.metadata.recursion_desired = true;
        msg.add_query(Query::query(Name::from_str(domain).unwrap(), qtype));
        msg
    }

    #[tokio::test]
    async fn blocked_domain_gets_null_ip_for_a_and_nodata_for_https() {
        let svc = service(&["||ads.example.com^"], BlockingMode::NullIp);

        let resp = svc.handle(&query_msg("ads.example.com.", RecordType::A)).await;
        assert_eq!(resp.metadata.id, 4242);
        assert_eq!(resp.metadata.response_code, ResponseCode::NoError);
        assert_eq!(resp.answers.len(), 1);
        assert_eq!(resp.answers[0].data, RData::A(A(std::net::Ipv4Addr::UNSPECIFIED)));

        let resp = svc.handle(&query_msg("ads.example.com.", RecordType::HTTPS)).await;
        assert_eq!(resp.metadata.response_code, ResponseCode::NoError);
        assert!(resp.answers.is_empty());
    }

    #[tokio::test]
    async fn aaaa_queries_get_nodata_without_touching_upstream() {
        let svc = service(&["||ads.example.com^"], BlockingMode::NullIp);
        for domain in ["fine.example.com.", "ads.example.com."] {
            let resp = svc.handle(&query_msg(domain, RecordType::AAAA)).await;
            assert_eq!(resp.metadata.id, 4242);
            assert_eq!(resp.metadata.response_code, ResponseCode::NoError);
            assert!(resp.answers.is_empty(), "{domain} must get NODATA");
        }
    }

    #[tokio::test]
    async fn aaaa_queries_stay_out_of_the_query_log_unless_enabled() {
        let svc = service_in("log-ipv6", vec!["udp://127.0.0.1:1".into()]);
        let mut ui = svc.state.observe();

        svc.handle(&query_msg("fine.example.com.", RecordType::AAAA)).await;
        assert!(ui.dns().is_empty(), "AAAA must not be logged by default");

        svc.apply_settings(DnsOverrides { log_ipv6: Some(true), ..Default::default() })
            .unwrap();
        svc.handle(&query_msg("fine.example.com.", RecordType::AAAA)).await;
        let logged = ui.dns();
        assert_eq!(logged.len(), 1);
        assert_eq!(logged[0].qtype, "AAAA");
    }

    #[tokio::test]
    async fn blocking_modes_shape_the_rcode() {
        let svc = service(&["||ads.example.com^"], BlockingMode::Nxdomain);
        let resp = svc.handle(&query_msg("ads.example.com.", RecordType::A)).await;
        assert_eq!(resp.metadata.response_code, ResponseCode::NXDomain);
        assert!(resp.answers.is_empty());

        let svc = service(&["||ads.example.com^"], BlockingMode::Refused);
        let resp = svc.handle(&query_msg("ads.example.com.", RecordType::A)).await;
        assert_eq!(resp.metadata.response_code, ResponseCode::Refused);
    }

    #[tokio::test]
    async fn unblocked_domain_with_dead_upstream_returns_servfail() {
        let svc = service(&["||ads.example.com^"], BlockingMode::NullIp);
        let resp = svc.handle(&query_msg("fine.example.com.", RecordType::A)).await;
        assert_eq!(resp.metadata.response_code, ResponseCode::ServFail);
        assert_eq!(resp.metadata.id, 4242);
    }

    #[tokio::test]
    async fn non_query_opcode_is_notimp_and_no_query_is_formerr() {
        let svc = service(&[], BlockingMode::NullIp);
        let mut msg = query_msg("example.com.", RecordType::A);
        msg.metadata.op_code = OpCode::Update;
        let resp = svc.handle(&msg).await;
        assert_eq!(resp.metadata.response_code, ResponseCode::NotImp);

        let mut msg = Message::query();
        msg.metadata.id = 7;
        let resp = svc.handle(&msg).await;
        assert_eq!(resp.metadata.response_code, ResponseCode::FormErr);
    }

    fn https_record(domain: &str, with_ech: bool) -> Record {
        let mut params = vec![(
            SvcParamKey::Alpn,
            SvcParamValue::Alpn(hickory_proto::rr::rdata::svcb::Alpn(vec!["h2".into()])),
        )];
        if with_ech {
            params.push((
                SvcParamKey::EchConfigList,
                SvcParamValue::EchConfigList(EchConfigList(vec![1, 2, 3])),
            ));
        }
        let name = Name::from_str(domain).unwrap();
        Record::from_rdata(
            name.clone(),
            300,
            RData::HTTPS(HTTPS(SVCB::new(1, name, params))),
        )
    }

    #[test]
    fn strip_ech_removes_only_the_ech_param() {
        let mut msg = Message::response(1, OpCode::Query);
        msg.add_answer(https_record("site.example.", true));
        msg.add_answer(https_record("other.example.", false));

        assert_eq!(strip_ech_params(&mut msg), 1);
        for r in &msg.answers {
            let RData::HTTPS(h) = &r.data else { panic!("https record") };
            assert!(h.0.svc_params.iter().all(|(k, _)| *k != SvcParamKey::EchConfigList));
            assert!(h.0.svc_params.iter().any(|(k, _)| *k == SvcParamKey::Alpn));
        }
        assert_eq!(strip_ech_params(&mut msg), 0);
    }

    #[test]
    fn answer_summary_flags_ech_presence() {
        let mut msg = Message::response(1, OpCode::Query);
        msg.add_answer(https_record("site.example.", true));
        assert_eq!(summarize_answers(&msg), "HTTPS +ech");
    }

    async fn fake_upstream(ip: std::net::Ipv4Addr) -> std::net::SocketAddr {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            while let Ok((n, peer)) = sock.recv_from(&mut buf).await {
                let Ok(req) = Message::from_vec(&buf[..n]) else { continue };
                let mut resp = Message::response(req.metadata.id, OpCode::Query);
                resp.add_queries(req.queries.iter().cloned());
                if let Some(q) = req.queries.first() {
                    match q.query_type() {
                        RecordType::A => {
                            resp.add_answer(Record::from_rdata(
                                q.name().clone(),
                                300,
                                RData::A(A(ip)),
                            ));
                        }
                        RecordType::HTTPS => {
                            resp.add_answer(https_record(&q.name().to_utf8(), true));
                        }
                        _ => {}
                    }
                }
                let _ = sock.send_to(&resp.to_vec().unwrap(), peer).await;
            }
        });
        addr
    }

    // `resolve`/`ech_config_list` are the in-process client entry points (the
    // proxy egress is one such client). The proxy's own caching layer over them
    // is tested in the proxy module; here we test the resolver itself.
    #[tokio::test]
    async fn resolver_serves_in_process_clients() {
        let up = fake_upstream(std::net::Ipv4Addr::new(9, 9, 9, 9)).await;
        let svc = service_with(
            &["||ads.example.com^"],
            BlockingMode::NullIp,
            vec![format!("udp://{up}")],
        );
        let m = &svc.state.metrics;

        let addrs = svc.resolve("fine.example.com", true).await.unwrap();
        assert_eq!(addrs, vec![std::net::IpAddr::V4(std::net::Ipv4Addr::new(9, 9, 9, 9))]);
        let queries_after_first = m.dns_queries_total.load(Ordering::Relaxed);
        assert!(queries_after_first >= 1, "expected at least one query");

        // A repeat resolve goes back through the pipeline and is served by the
        // DNS answer cache.
        let hits = svc.cache().hits();
        svc.resolve("fine.example.com", true).await.unwrap();
        assert!(m.dns_queries_total.load(Ordering::Relaxed) > queries_after_first);
        assert!(m.dns_cached_total.load(Ordering::Relaxed) >= 1);
        assert!(svc.cache().hits() > hits);

        assert_eq!(svc.ech_config_list("fine.example.com").await, Some(vec![1, 2, 3]));

        let err = svc.resolve("ads.example.com", true).await.unwrap_err();
        assert!(err.to_string().contains("blocked"), "err: {err}");
        assert!(m.dns_blocked_total.load(Ordering::Relaxed) >= 1);
    }

    #[tokio::test]
    async fn resolver_honors_rewrites_for_in_process_clients() {
        let up = fake_upstream(std::net::Ipv4Addr::new(1, 2, 3, 4)).await;
        let dir = std::env::temp_dir().join("proxy-dns-svc-rewrite-client");
        let _ = std::fs::remove_dir_all(&dir);
        let svc = build_service(
            &["||app.lab.example^"],
            BlockingMode::NullIp,
            vec![format!("udp://{up}")],
            dir,
        );
        svc.rewrites().add("*.lab.example", "10.9.8.7").unwrap();

        let addrs = svc.resolve("app.lab.example", true).await.unwrap();
        assert_eq!(addrs, vec![std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 9, 8, 7))]);

        assert_eq!(svc.ech_config_list("app.lab.example").await, None);

        svc.rewrites().add("alias.example", "target.example").unwrap();
        let addrs = svc.resolve("alias.example", true).await.unwrap();
        assert_eq!(addrs, vec![std::net::IpAddr::V4(std::net::Ipv4Addr::new(1, 2, 3, 4))]);
    }

    #[tokio::test]
    async fn strip_ech_applies_to_in_process_clients() {
        let up = fake_upstream(std::net::Ipv4Addr::new(1, 1, 1, 1)).await;
        let cfg = DnsConfig {
            upstreams: vec![format!("udp://{up}")],
            upstream_timeout_ms: 500,
            strip_ech: true,
            ..DnsConfig::default()
        };
        let svc = build_service_cfg(&[], cfg, PathBuf::from("/nonexistent-for-tests"));

        let resp = svc.handle(&query_msg("site.example.", RecordType::HTTPS)).await;
        assert_eq!(summarize_answers(&resp), "HTTPS");
        let hits = svc.cache().hits();
        let resp = svc.handle(&query_msg("site.example.", RecordType::HTTPS)).await;
        assert_eq!(summarize_answers(&resp), "HTTPS");
        assert_eq!(svc.cache().hits(), hits + 1);

        assert_eq!(svc.ech_config_list("site.example").await, None);
    }

    #[tokio::test]
    async fn end_to_end_over_udp_and_tcp_resolves_blocks_and_caches() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let up = fake_upstream(std::net::Ipv4Addr::new(1, 2, 3, 4)).await;
        let svc = service_with(
            &["||ads.example.com^"],
            BlockingMode::NullIp,
            vec![format!("udp://{up}")],
        );
        let (udp_addr, tcp_addr) = server::bind_ephemeral(svc.clone()).await;

        let ask = |domain: &str| {
            let mut m = query_msg(domain, RecordType::A);
            m.metadata.id = 99;
            m.to_vec().unwrap()
        };
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(udp_addr).await.unwrap();
        let mut buf = [0u8; 2048];

        client.send(&ask("fine.example.com.")).await.unwrap();
        let n = client.recv(&mut buf).await.unwrap();
        let resp = Message::from_vec(&buf[..n]).unwrap();
        assert_eq!(resp.metadata.id, 99);
        assert_eq!(resp.answers[0].data, RData::A(A(std::net::Ipv4Addr::new(1, 2, 3, 4))));

        let hits_before = svc.cache().hits();
        client.send(&ask("fine.example.com.")).await.unwrap();
        let n = client.recv(&mut buf).await.unwrap();
        let resp = Message::from_vec(&buf[..n]).unwrap();
        assert_eq!(resp.answers[0].data, RData::A(A(std::net::Ipv4Addr::new(1, 2, 3, 4))));
        assert_eq!(svc.cache().hits(), hits_before + 1);

        client.send(&ask("ads.example.com.")).await.unwrap();
        let n = client.recv(&mut buf).await.unwrap();
        let resp = Message::from_vec(&buf[..n]).unwrap();
        assert_eq!(resp.answers[0].data, RData::A(A(std::net::Ipv4Addr::UNSPECIFIED)));

        let mut tcp = tokio::net::TcpStream::connect(tcp_addr).await.unwrap();
        let wire = ask("fine.example.com.");
        let mut framed = (wire.len() as u16).to_be_bytes().to_vec();
        framed.extend_from_slice(&wire);
        tcp.write_all(&framed).await.unwrap();
        let mut len = [0u8; 2];
        tcp.read_exact(&mut len).await.unwrap();
        let mut body = vec![0u8; usize::from(u16::from_be_bytes(len))];
        tcp.read_exact(&mut body).await.unwrap();
        let resp = Message::from_vec(&body).unwrap();
        assert_eq!(resp.metadata.id, 99);
        assert_eq!(resp.answers[0].data, RData::A(A(std::net::Ipv4Addr::new(1, 2, 3, 4))));
    }

    #[tokio::test]
    async fn rewrites_win_over_blocklist_and_respect_the_query_family() {
        let dir = std::env::temp_dir().join("proxy-dns-svc-rw-filter");
        let _ = std::fs::remove_dir_all(&dir);
        let svc = build_service(
            &["||app.example.com^"],
            BlockingMode::NullIp,
            vec!["udp://127.0.0.1:1".into()],
            dir,
        );
        svc.rewrites().add("app.example.com", "10.1.2.3").unwrap();

        let resp = svc.handle(&query_msg("app.example.com.", RecordType::A)).await;
        assert_eq!(resp.metadata.response_code, ResponseCode::NoError);
        assert_eq!(resp.answers[0].data, RData::A(A(std::net::Ipv4Addr::new(10, 1, 2, 3))));
        assert_eq!(resp.answers[0].ttl, REWRITE_TTL);

        let resp = svc.handle(&query_msg("app.example.com.", RecordType::TXT)).await;
        assert_eq!(resp.metadata.response_code, ResponseCode::NoError);
        assert!(resp.answers.is_empty());

        svc.rewrites().add("*.lab.example", "10.9.8.7").unwrap();
        let resp = svc.handle(&query_msg("deep.sub.lab.example.", RecordType::A)).await;
        assert_eq!(resp.answers[0].data, RData::A(A(std::net::Ipv4Addr::new(10, 9, 8, 7))));

        assert!(svc.rewrites().add("v6.lab.example", "::1").is_err());
    }

    #[tokio::test]
    async fn cname_rewrite_resolves_the_target_upstream() {
        let up = fake_upstream(std::net::Ipv4Addr::new(5, 6, 7, 8)).await;
        let svc = service_in("rw-cname", vec![format!("udp://{up}")]);
        svc.rewrites().add("alias.example", "target.example").unwrap();

        let resp = svc.handle(&query_msg("alias.example.", RecordType::A)).await;
        assert_eq!(
            resp.answers[0].data,
            RData::CNAME(CNAME(Name::from_str("target.example.").unwrap()))
        );
        assert_eq!(resp.answers[1].data, RData::A(A(std::net::Ipv4Addr::new(5, 6, 7, 8))));
    }

    #[tokio::test]
    async fn a_failed_persist_rejects_the_whole_settings_change() {
        let base_upstreams = vec!["udp://127.0.0.1:1".to_string()];
        let svc = service_with(&[], BlockingMode::NullIp, base_upstreams.clone());
        let err = svc
            .apply_settings(DnsOverrides {
                upstreams: Some(vec!["udp://127.0.0.2:53".into()]),
                ..Default::default()
            })
            .unwrap_err();
        assert!(err.contains("/nonexistent-for-tests"), "err: {err}");
        assert_eq!(svc.upstream_specs(), base_upstreams);
    }

    #[tokio::test]
    async fn settings_apply_live_validate_and_persist() {
        let dir = std::env::temp_dir().join("proxy-dns-svc-settings");
        let _ = std::fs::remove_dir_all(&dir);
        let base_upstreams = vec!["udp://127.0.0.1:1".to_string()];
        let svc = build_service(&[], BlockingMode::NullIp, base_upstreams.clone(), dir.clone());

        assert!(svc.apply_settings(DnsOverrides {
            upstreams: Some(vec![]), ..Default::default()
        }).is_err());
        assert!(svc.apply_settings(DnsOverrides {
            upstreams: Some(vec!["quic://nope".into()]), ..Default::default()
        }).is_err());
        assert!(svc.apply_settings(DnsOverrides {
            min_ttl_secs: Some(100), max_ttl_secs: Some(50), ..Default::default()
        }).is_err());
        assert_eq!(svc.upstream_specs(), base_upstreams);

        svc.apply_settings(DnsOverrides {
            upstreams: Some(vec!["udp://127.0.0.2:53".into()]),
            upstream_mode: Some(crate::dns::config::UpstreamMode::Parallel),
            cache_size: Some(2),
            min_ttl_secs: Some(5),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(svc.upstream_specs(), vec!["udp://127.0.0.2:53".to_string()]);
        assert_eq!(svc.upstream_mode(), crate::dns::config::UpstreamMode::Parallel);
        assert_eq!(svc.cache().capacity(), 2);
        assert_eq!(svc.cache().min_ttl(), 5);

        let cfg = DnsConfig {
            upstreams: base_upstreams.clone(),
            upstream_timeout_ms: 500,
            ..DnsConfig::default()
        };
        let svc2 = DnsService::new(&cfg, &dir, svc.adblock.clone(), svc.state.clone()).unwrap();
        assert_eq!(svc2.upstream_specs(), vec!["udp://127.0.0.2:53".to_string()]);
        assert_eq!(svc2.upstream_mode(), crate::dns::config::UpstreamMode::Parallel);
        assert_eq!(svc2.cache().capacity(), 2);

        svc2.reset_settings().unwrap();
        assert_eq!(svc2.upstream_specs(), base_upstreams);
        assert_eq!(svc2.upstream_mode(), crate::dns::config::UpstreamMode::Failover);
        assert_eq!(svc2.cache().capacity(), cfg.cache_size);
        let svc3 = DnsService::new(&cfg, &dir, svc.adblock.clone(), svc.state.clone()).unwrap();
        assert_eq!(svc3.upstream_specs(), base_upstreams);
    }
}
