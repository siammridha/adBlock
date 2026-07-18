//! Shared runtime state: counters, event log, request/DNS records, and the
//! broadcast channel feeding the live UI.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;
use tokio::sync::broadcast;

use crate::support::config::LoggingConfig;
use crate::support::persist::OverrideStore;

use history::{History, Metric};

mod errors;
pub mod history;
mod logs;
mod records;

use errors::ErrorLog;
use logs::RotatingLog;
pub use records::{
    CaptureSlot, DnsOutcome, DnsRecord, RequestDetail, RequestKind, RequestRecord, UiMsg,
};

#[derive(Clone, Copy)]
pub struct RequestFacts<'a> {
    pub method: &'a str,
    pub req_type: &'a str,
    pub url: &'a str,
}

#[derive(Default)]
pub struct Metrics {
    pub requests_total: AtomicU64,
    pub blocked_total: AtomicU64,
    pub errors_total: AtomicU64,
    pub dns_queries_total: AtomicU64,
    pub dns_blocked_total: AtomicU64,
    pub dns_cached_total: AtomicU64,
    pub dns_errors_total: AtomicU64,
}

impl Metrics {
    fn counters(&self) -> [(&'static str, Option<&'static str>, &AtomicU64); 7] {
        [
            ("requests_total", None, &self.requests_total),
            ("blocked_total", None, &self.blocked_total),
            ("errors_total", None, &self.errors_total),
            ("dns_queries_total", Some("queries"), &self.dns_queries_total),
            ("dns_blocked_total", Some("blocked"), &self.dns_blocked_total),
            ("dns_cached_total", Some("cached"), &self.dns_cached_total),
            ("dns_errors_total", Some("errors"), &self.dns_errors_total),
        ]
    }

    pub fn view(&self) -> serde_json::Value {
        let map: serde_json::Map<String, serde_json::Value> = self
            .counters()
            .iter()
            .map(|(long, _, a)| ((*long).to_string(), json!(a.load(Ordering::Relaxed))))
            .collect();
        serde_json::Value::Object(map)
    }

    pub fn dns_view(&self) -> serde_json::Value {
        let map: serde_json::Map<String, serde_json::Value> = self
            .counters()
            .iter()
            .filter_map(|(_, short, a)| {
                short.map(|s| (s.to_string(), json!(a.load(Ordering::Relaxed))))
            })
            .collect();
        serde_json::Value::Object(map)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    Blocked,
    Error,
    Info,
}

impl EventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::Blocked => "blocked",
            EventKind::Error => "error",
            EventKind::Info => "info",
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Event {
    pub ts_ms: u64,
    pub kind: EventKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<String>,
}

pub const DEFAULT_RETENTION_HOURS: u32 = 24;
pub const DEFAULT_LOG_ROTATE_HOURS: u32 = 24;
pub const MAX_RETENTION_HOURS: u32 = 168;
pub const MAX_LOG_ROTATE_HOURS: u32 = 2160;

#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StatsOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_hours: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_rotate_hours: Option<u32>,
}

pub struct StaticInfo {
    pub version: String,
    pub listen: String,
    pub admin_listen: String,
    pub ca_pem: String,
    pub started: Instant,
}

pub struct SharedState {
    pub metrics: Metrics,
    pub history: History,
    log_actions: bool,
    log_requests: bool,
    ui: broadcast::Sender<UiMsg>,
    next_seq: AtomicU64,
    errors: ErrorLog,
    request_log: Option<RotatingLog>,
    request_detail: Option<RotatingLog>,
    query_log: Option<RotatingLog>,
    settings_store: Option<OverrideStore<StatsOverrides>>,
    log_rotate_hours: std::sync::atomic::AtomicU32,
    pub info: StaticInfo,
}

impl SharedState {
    pub fn new(info: StaticInfo, logging: &LoggingConfig) -> Self {
        let (ui, _) = broadcast::channel(1024);
        Self {
            metrics: Metrics::default(),
            history: History::new(),
            log_actions: logging.log_actions,
            log_requests: logging.log_requests,
            ui,
            next_seq: AtomicU64::new(1),
            errors: ErrorLog::memory(),
            request_log: None,
            request_detail: None,
            query_log: None,
            settings_store: None,
            log_rotate_hours: std::sync::atomic::AtomicU32::new(DEFAULT_LOG_ROTATE_HOURS),
            info,
        }
    }

    pub fn with_error_log(mut self, path: PathBuf) -> Self {
        self.errors = ErrorLog::load(path);
        self
    }

    /// Wire persistence under a data root: rotating logs go to `<dir>/logs/`,
    /// the stats settings file to `<dir>/settings/`. Both subfolders are created
    /// up front so the log writer threads have somewhere to append.
    pub fn with_data_dir(mut self, dir: &std::path::Path) -> Self {
        let logs = dir.join("logs");
        let settings = dir.join("settings");
        let _ = std::fs::create_dir_all(&logs);
        let _ = std::fs::create_dir_all(&settings);
        self = self.with_error_log(logs.join("error-log.jsonl"));
        let store = OverrideStore::new(settings.join("stats-settings.json"));
        let saved: StatsOverrides = store.load();
        let retention = saved.retention_hours.unwrap_or(DEFAULT_RETENTION_HOURS);
        let rotate = saved.log_rotate_hours.unwrap_or(DEFAULT_LOG_ROTATE_HOURS);
        self.history.set_retention_hours(retention);
        self.log_rotate_hours = std::sync::atomic::AtomicU32::new(rotate);
        self.request_log = Some(RotatingLog::open(logs.join("request-log.jsonl"), rotate));
        self.request_detail = Some(RotatingLog::open(logs.join("request-detail.jsonl"), rotate));
        self.query_log = Some(RotatingLog::open(logs.join("query-log.jsonl"), rotate));
        self.settings_store = Some(store);
        self
    }

    pub fn stats_settings(&self) -> StatsOverrides {
        StatsOverrides {
            retention_hours: Some(self.history.retention_hours()),
            log_rotate_hours: Some(self.log_rotate_hours.load(Ordering::Relaxed)),
        }
    }

    pub fn apply_stats_settings(&self, change: StatsOverrides) -> Result<(), String> {
        if let Some(h) = change.retention_hours {
            if h == 0 || h > MAX_RETENTION_HOURS {
                return Err(format!("retention_hours must be 1..={MAX_RETENTION_HOURS}"));
            }
        }
        if let Some(h) = change.log_rotate_hours {
            if h == 0 || h > MAX_LOG_ROTATE_HOURS {
                return Err(format!("log_rotate_hours must be 1..={MAX_LOG_ROTATE_HOURS}"));
            }
        }
        let current = self.stats_settings();
        let merged = StatsOverrides {
            retention_hours: change.retention_hours.or(current.retention_hours),
            log_rotate_hours: change.log_rotate_hours.or(current.log_rotate_hours),
        };
        if let Some(store) = &self.settings_store {
            store.save(&merged)?;
        }
        if let Some(h) = change.retention_hours {
            self.history.set_retention_hours(h);
        }
        if let Some(h) = change.log_rotate_hours {
            self.log_rotate_hours.store(h, Ordering::Relaxed);
            for log in [&self.request_log, &self.request_detail, &self.query_log]
                .into_iter()
                .flatten()
            {
                log.set_rotate_hours(h);
            }
        }
        Ok(())
    }

    pub fn count(&self, metric: Metric, domain: &str) {
        debug_assert!(
            !matches!(metric, Metric::Blocked | Metric::DnsBlocked),
            "denials go through count_block, which pairs the traffic bump"
        );
        self.bump(metric, domain);
    }

    pub fn count_block(&self, blocked: Metric, domain: &str) {
        let traffic = match blocked {
            Metric::Blocked => Metric::Requests,
            Metric::DnsBlocked => Metric::DnsQueries,
            other => {
                debug_assert!(false, "count_block on non-block metric {other:?}");
                return;
            }
        };
        self.bump(traffic, "");
        self.bump(blocked, domain);
    }

    fn bump(&self, metric: Metric, domain: &str) {
        let counter = match metric {
            Metric::Requests => &self.metrics.requests_total,
            Metric::Blocked => &self.metrics.blocked_total,
            Metric::Errors => &self.metrics.errors_total,
            Metric::DnsQueries => &self.metrics.dns_queries_total,
            Metric::DnsBlocked => &self.metrics.dns_blocked_total,
            Metric::DnsCached => &self.metrics.dns_cached_total,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        self.history.record(metric, domain);
    }

    pub fn count_dns_error(&self) {
        self.metrics.dns_errors_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn reset_stats(&self) {
        for (_, _, c) in self.metrics.counters() {
            c.store(0, Ordering::Relaxed);
        }
        self.history.reset();
    }

    pub fn log_actions(&self) -> bool {
        self.log_actions
    }

    pub fn subscribe_ui(&self) -> broadcast::Receiver<UiMsg> {
        self.ui.subscribe()
    }

    fn ui_connected(&self) -> bool {
        self.ui.receiver_count() > 0
    }

    pub fn log_event(&self, kind: EventKind, message: impl Into<String>) {
        let trace = (kind == EventKind::Error)
            .then(std::backtrace::Backtrace::capture)
            .filter(|b| b.status() == std::backtrace::BacktraceStatus::Captured)
            .map(|b| b.to_string());
        let event = Event { ts_ms: now_ms(), kind, message: message.into(), trace };
        if kind == EventKind::Error {
            self.errors.push(&event);
        }
        if self.ui_connected() {
            let _ = self.ui.send(UiMsg::Event(Arc::new(event)));
        }
    }

    pub fn error_log(&self) -> Vec<Event> {
        self.errors.snapshot()
    }

    pub fn clear_error_log(&self) -> usize {
        self.errors.clear()
    }

    pub fn record_forwarded(self: &Arc<Self>, facts: RequestFacts<'_>, status: u16, ech: bool) -> Exchange {
        self.begin(RequestKind::Forwarded, facts, status, String::new(), ech)
    }

    pub fn record_blocked(self: &Arc<Self>, facts: RequestFacts<'_>, blocked_by: &str) -> Exchange {
        self.begin(RequestKind::Blocked, facts, 0, blocked_by.to_string(), false)
    }

    pub fn record_failed(self: &Arc<Self>, facts: RequestFacts<'_>, error: &str) -> Exchange {
        self.begin(RequestKind::Failed, facts, 0, error.to_string(), false)
    }

    pub fn record_dns(&self, mut record: DnsRecord) {
        if !self.log_requests {
            return;
        }
        record.seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        if let Some(log) = &self.query_log {
            if let Ok(line) = serde_json::to_string(&record) {
                log.append(record.ts_ms, &line);
            }
        }
        if self.ui_connected() {
            let _ = self.ui.send(UiMsg::Dns(Arc::new(record)));
        }
    }

    pub fn record_tunnel(self: &Arc<Self>, facts: RequestFacts<'_>, attribution: &str) {
        // A tunnel never carries captured bodies, so the exchange just drops
        // after writing the lean line.
        let _ = self.begin(RequestKind::Tunnel, facts, 0, attribution.to_string(), false);
    }

    // Open a request record. The lean line lands in the request log now; the
    // heavy captures accumulate on the returned `Exchange` and flush to the
    // detail sidecar when it drops. The exchange stays inert (captures nothing,
    // broadcasts nothing) when logging is off, or when nothing would consume the
    // record — no live dashboard and no persistence.
    fn begin(
        self: &Arc<Self>,
        kind: RequestKind,
        facts: RequestFacts<'_>,
        status: u16,
        blocked_by: String,
        ech: bool,
    ) -> Exchange {
        let live = self.ui_connected();
        let persist = self.request_log.is_some();
        if !self.log_requests || (!live && !persist) {
            return Exchange { state: self.clone(), seq: 0, live: false, captures: None };
        }
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let record = RequestRecord {
            seq,
            ts_ms: now_ms(),
            method: facts.method.to_string(),
            status,
            kind,
            req_type: facts.req_type.to_string(),
            url: facts.url.to_string(),
            blocked_by,
            ech,
            ..Default::default()
        };
        if let Some(log) = &self.request_log {
            if let Ok(line) = serde_json::to_string(&record) {
                log.append(record.ts_ms, &line);
            }
        }
        if live {
            let _ = self.ui.send(UiMsg::Request(Arc::new(record.clone())));
        }
        Exchange { state: self.clone(), seq, live, captures: Some(Mutex::new(record)) }
    }

    fn attach(&self, seq: u64, slot: CaptureSlot, text: String) {
        let _ = self.ui.send(UiMsg::Attach { seq, slot, text: text.into() });
    }

    fn persist_detail(&self, record: &RequestRecord) {
        let Some(log) = &self.request_detail else { return };
        let Some(detail) = RequestDetail::from_record(record) else { return };
        if let Ok(line) = serde_json::to_string(&detail) {
            log.append(record.ts_ms, &line);
        }
    }

    /// A page of persisted request records, newest first. `before` is the
    /// exclusive upper bound on `seq` (pass the smallest seq already shown to
    /// walk backwards); `None` starts at the newest record.
    pub fn request_page(&self, before: Option<u64>, limit: usize) -> Vec<RequestRecord> {
        let Some(log) = &self.request_log else { return Vec::new() };
        log.page(before, limit, line_seq)
            .iter()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// The captured headers/bodies/scriptlets for one request, fetched on
    /// demand. Returns an empty detail (only the seq) when nothing was captured.
    pub fn request_detail(&self, seq: u64) -> RequestDetail {
        let found = self.request_detail.as_ref().and_then(|log| {
            log.find(|l| line_seq(l) == Some(seq))
                .and_then(|l| serde_json::from_str(&l).ok())
        });
        found.unwrap_or(RequestDetail { seq, ..Default::default() })
    }

    /// A page of persisted DNS query records, newest first. Cursor semantics
    /// match [`request_page`].
    pub fn query_page(&self, before: Option<u64>, limit: usize) -> Vec<DnsRecord> {
        let Some(log) = &self.query_log else { return Vec::new() };
        log.page(before, limit, line_seq)
            .iter()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    pub fn uptime_secs(&self) -> u64 {
        self.info.started.elapsed().as_secs()
    }
}

// A single request in flight. It carries the captured artifacts (headers,
// bodies, scriptlet names) as they land, streams each one live to a connected
// dashboard, and — on drop — writes them to the detail sidecar so they can be
// fetched on demand later. `captures` is `None` for an inert exchange, which
// short-circuits every capture path.
pub struct Exchange {
    state: Arc<SharedState>,
    seq: u64,
    live: bool,
    captures: Option<Mutex<RequestRecord>>,
}

impl Exchange {
    /// Whether captured artifacts should be collected at all — true when the
    /// record is live (a dashboard is watching) or being persisted for later
    /// on-demand fetch. An inert exchange has none of either.
    pub(crate) fn is_active(&self) -> bool {
        self.captures.is_some()
    }

    pub fn attach(&self, slot: CaptureSlot, text: impl FnOnce() -> String) {
        let Some(captures) = &self.captures else { return };
        let text = text();
        if let Ok(mut record) = captures.lock() {
            slot.apply(&mut record, text.clone());
        }
        if self.live {
            self.state.attach(self.seq, slot, text);
        }
    }
}

impl Drop for Exchange {
    fn drop(&mut self) {
        let Some(captures) = &self.captures else { return };
        if let Ok(record) = captures.lock() {
            self.state.persist_detail(&record);
        }
    }
}

/// Pull the `seq` out of a raw JSONL log line without fully deserializing it —
/// used to page and locate records by cursor.
fn line_seq(line: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(line).ok()?.get("seq")?.as_u64()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
pub use test_support::UiObserver;

#[cfg(test)]
mod test_support {
    use super::*;

    impl SharedState {
        pub fn observe(&self) -> UiObserver {
            UiObserver {
                rx: self.subscribe_ui(),
                records: Vec::new(),
                dns: Vec::new(),
                events: Vec::new(),
            }
        }

        /// Block until every queued log line has hit disk. Writes now happen on
        /// a background thread, so a test reading the files must wait first.
        pub fn flush_logs(&self) {
            for log in [&self.request_log, &self.request_detail, &self.query_log]
                .into_iter()
                .flatten()
            {
                log.flush();
            }
        }
    }

    pub struct UiObserver {
        rx: broadcast::Receiver<UiMsg>,
        records: Vec<RequestRecord>,
        dns: Vec<DnsRecord>,
        events: Vec<Event>,
    }

    impl UiObserver {
        fn pump(&mut self) {
            while let Ok(msg) = self.rx.try_recv() {
                match msg {
                    UiMsg::Request(r) => self.records.push((*r).clone()),
                    UiMsg::Attach { seq, slot, text } => {
                        if let Some(r) = self.records.iter_mut().rev().find(|r| r.seq == seq) {
                            slot.apply(r, text.to_string());
                        }
                    }
                    UiMsg::Dns(d) => self.dns.push((*d).clone()),
                    UiMsg::Event(e) => self.events.push((*e).clone()),
                }
            }
        }

        pub fn records(&mut self) -> Vec<RequestRecord> {
            self.pump();
            self.records.clone()
        }

        pub fn dns(&mut self) -> Vec<DnsRecord> {
            self.pump();
            self.dns.clone()
        }

        pub fn events(&mut self) -> Vec<Event> {
            self.pump();
            self.events.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(url: &str) -> RequestFacts<'_> {
        RequestFacts { method: "GET", req_type: "document", url }
    }

    fn state() -> Arc<SharedState> {
        let info = StaticInfo {
            version: "test".into(),
            listen: String::new(),
            admin_listen: String::new(),
            ca_pem: String::new(),
            started: Instant::now(),
        };
        let logging = LoggingConfig {
            level: "info".into(),
            log_actions: true,
            log_requests: true,
        };
        Arc::new(SharedState::new(info, &logging))
    }

    #[test]
    fn no_subscriber_means_no_recording_and_no_buffering() {
        let s = state();
        let ex = s.record_forwarded(doc("https://a.example/"), 200, false);
        assert!(!ex.is_active(), "inert without a subscriber");
        s.log_event(EventKind::Info, "dropped");
        let mut obs = s.observe();
        assert!(obs.records().is_empty());
        assert!(obs.events().is_empty());
    }

    #[test]
    fn records_stream_in_order_with_their_kinds() {
        let s = state();
        let mut obs = s.observe();
        s.record_forwarded(doc("https://ok.example/"), 200, false);
        s.record_blocked(
            RequestFacts { method: "GET", req_type: "image", url: "https://ads.example/x" },
            "||ads.example^",
        );
        s.record_tunnel(
            RequestFacts { method: "CONNECT", req_type: "tunnel-mitm", url: "https://t.example/" },
            "",
        );
        s.record_failed(doc("https://down.example/"), "upstream down.example: dns");

        let got = obs.records();
        assert_eq!(got.len(), 4, "everything streamed, oldest-first");
        assert_eq!(got[0].url, "https://ok.example/");
        assert_eq!(got[1].kind, RequestKind::Blocked);
        assert_eq!(got[2].kind, RequestKind::Tunnel);
        assert_eq!(got[3].kind, RequestKind::Failed);
        assert_eq!(got[3].blocked_by, "upstream down.example: dns");
    }

    #[test]
    fn ech_flag_rides_the_forwarded_record() {
        let s = state();
        let mut obs = s.observe();
        s.record_forwarded(doc("https://secret.example/"), 200, true);
        let got = obs.records();
        assert_eq!(got.len(), 1);
        assert!(got[0].ech, "the ECH tag must survive onto the streamed record");
        let v = serde_json::to_value(&got[0]).unwrap();
        assert_eq!(v["ech"], true);
    }

    #[test]
    fn attaches_fold_onto_their_record() {
        let s = state();
        let mut obs = s.observe();
        let a = s.record_forwarded(doc("https://a.example/"), 200, false);
        let b = s.record_forwarded(doc("https://b.example/"), 200, false);
        let (sa, sb) = (a.seq, b.seq);
        a.attach(CaptureSlot::ReqBody, || "body-a".into());
        b.attach(CaptureSlot::RespHeaders, || "HTTP/1.1 200 OK".into());
        a.attach(CaptureSlot::Scriptlets, || "set-constant.js, json-prune.js".into());

        let got = obs.records();
        let ra = got.iter().find(|r| r.seq == sa).unwrap();
        let rb = got.iter().find(|r| r.seq == sb).unwrap();
        assert_eq!(ra.req_body, "body-a");
        assert_eq!(ra.scriptlets, "set-constant.js, json-prune.js");
        assert!(ra.resp_headers.is_empty());
        assert_eq!(rb.resp_headers, "HTTP/1.1 200 OK");
    }

    #[test]
    fn capture_slots_name_real_serialized_record_fields() {
        // Populate every slot so the lean serializer (which omits empty
        // captures) still emits each key the dashboard's attach frames target.
        let mut record = RequestRecord::default();
        for slot in [
            CaptureSlot::ReqBody,
            CaptureSlot::RespBody,
            CaptureSlot::ReqHeaders,
            CaptureSlot::RespHeaders,
            CaptureSlot::Scriptlets,
        ] {
            slot.apply(&mut record, "x".into());
        }
        let v = serde_json::to_value(&record).unwrap();
        for slot in [
            CaptureSlot::ReqBody,
            CaptureSlot::RespBody,
            CaptureSlot::ReqHeaders,
            CaptureSlot::RespHeaders,
            CaptureSlot::Scriptlets,
        ] {
            assert!(
                v.get(slot.as_str()).is_some(),
                "slot key '{}' names no field in the serialized record",
                slot.as_str()
            );
        }
        assert!(v.get("type").is_some(), "the dashboard keys the type column on 'type'");
        assert_eq!(v["kind"], "forwarded", "kind serializes lowercase");

        // And the lean line really does drop empty captures.
        let lean = serde_json::to_value(RequestRecord::default()).unwrap();
        assert!(lean.get("req_body").is_none(), "empty captures stay out of the list line");
    }

    #[test]
    fn count_bumps_the_lifetime_counter_and_resets_clear_it() {
        let s = state();
        s.count(Metric::Requests, "a.example");
        s.count(Metric::Requests, "b.example");
        s.count_block(Metric::DnsBlocked, "ads.example");
        assert_eq!(s.metrics.requests_total.load(Ordering::Relaxed), 2);
        assert_eq!(s.metrics.dns_blocked_total.load(Ordering::Relaxed), 1);
        let snap = s.history.snapshot();
        assert_eq!(snap.totals[Metric::Requests.index()], 2);
        assert_eq!(snap.top_blocked, vec![("ads.example".to_string(), 1)]);

        s.reset_stats();
        assert_eq!(s.metrics.requests_total.load(Ordering::Relaxed), 0);
        assert_eq!(s.history.snapshot().totals[Metric::Requests.index()], 0);
    }

    #[test]
    fn count_block_pairs_the_traffic_bump_and_attributes_only_top_blocked() {
        let s = state();
        s.count_block(Metric::Blocked, "ads.example");
        s.count_block(Metric::DnsBlocked, "tracker.example");
        assert_eq!(s.metrics.requests_total.load(Ordering::Relaxed), 1);
        assert_eq!(s.metrics.blocked_total.load(Ordering::Relaxed), 1);
        assert_eq!(s.metrics.dns_queries_total.load(Ordering::Relaxed), 1);
        assert_eq!(s.metrics.dns_blocked_total.load(Ordering::Relaxed), 1);
        let snap = s.history.snapshot();
        assert!(snap.top_queried.is_empty(), "blocked domains must not pad top-queried");
        assert_eq!(snap.top_blocked.len(), 2);
    }

    #[test]
    fn dns_records_reach_a_live_subscriber_only() {
        let s = state();
        s.record_dns(DnsRecord {
            seq: 0,
            ts_ms: 0,
            domain: "dropped.example".into(),
            qtype: "A".into(),
            outcome: DnsOutcome::Resolved,
            rcode: "NOERROR".into(),
            answers: String::new(),
            upstream: String::new(),
            ech: false,
            blocked_by: String::new(),
            elapsed_ms: 0,
            proxy: false,
        });
        let mut obs = s.observe();
        assert!(obs.dns().is_empty());
        s.record_dns(DnsRecord {
            seq: 0,
            ts_ms: 0,
            domain: "kept.example".into(),
            qtype: "HTTPS".into(),
            outcome: DnsOutcome::Resolved,
            rcode: "NOERROR".into(),
            answers: String::new(),
            upstream: "1.1.1.1".into(),
            ech: true,
            blocked_by: String::new(),
            elapsed_ms: 1,
            proxy: true,
        });
        let got = obs.dns();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].domain, "kept.example");
        assert!(got[0].ech);
    }

    #[test]
    fn events_reach_a_live_subscriber() {
        let s = state();
        let mut obs = s.observe();
        s.log_event(EventKind::Error, "boom");
        let events = obs.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::Error);
        assert_eq!(events[0].message, "boom");
    }

    #[test]
    fn event_wire_format_omits_absent_trace_and_reads_old_lines() {
        let e = Event { ts_ms: 1, kind: EventKind::Error, message: "boom".into(), trace: None };
        let json = serde_json::to_string(&e).unwrap();
        assert!(!json.contains("trace"), "absent trace must not serialize: {json}");
        let old: Event =
            serde_json::from_str(r#"{"ts_ms":1,"kind":"error","message":"boom"}"#).unwrap();
        assert_eq!(old.trace, None);
        let e = Event { trace: Some("at main.rs:1".into()), ..old };
        let back: Event = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(back.trace.as_deref(), Some("at main.rs:1"));
    }

    fn state_with_log(path: &std::path::Path) -> Arc<SharedState> {
        let info = StaticInfo {
            version: "test".into(),
            listen: String::new(),
            admin_listen: String::new(),
            ca_pem: String::new(),
            started: Instant::now(),
        };
        let logging = LoggingConfig {
            level: "info".into(),
            log_actions: true,
            log_requests: true,
        };
        Arc::new(SharedState::new(info, &logging).with_error_log(path.to_path_buf()))
    }

    #[test]
    fn error_events_persist_across_restarts_unlike_everything_else() {
        let path = std::env::temp_dir().join("proxy-state-errlog-test.jsonl");
        let _ = std::fs::remove_file(&path);

        let s = state_with_log(&path);
        s.log_event(EventKind::Error, "boom");
        s.log_event(EventKind::Info, "chatter");
        s.log_event(EventKind::Error, "boom again");
        let errs = s.error_log();
        assert_eq!(errs.len(), 2);
        assert_eq!(errs[0].message, "boom");
        assert!(errs.iter().all(|e| e.kind == EventKind::Error));

        let s2 = state_with_log(&path);
        let errs = s2.error_log();
        assert_eq!(errs.len(), 2);
        assert_eq!(errs[1].message, "boom again");

        assert_eq!(s2.clear_error_log(), 2);
        assert!(s2.error_log().is_empty());
        assert!(state_with_log(&path).error_log().is_empty());
    }

    #[test]
    fn the_error_log_stays_bounded_in_memory_and_on_disk() {
        let path = std::env::temp_dir().join("proxy-state-errlog-bound-test.jsonl");
        let _ = std::fs::remove_file(&path);
        let s = state_with_log(&path);
        for i in 0..700 {
            s.log_event(EventKind::Error, format!("e{i}"));
        }
        let errs = s.error_log();
        assert_eq!(errs.len(), errors::ERROR_LOG_CAP);
        assert_eq!(errs.last().unwrap().message, "e699");
        let lines = std::fs::read_to_string(&path).unwrap().lines().count();
        assert!(lines < errors::ERROR_LOG_CAP * 2, "file must compact: {lines} lines");
        let errs = state_with_log(&path).error_log();
        assert_eq!(errs.len(), errors::ERROR_LOG_CAP);
        assert_eq!(errs[0].message, "e400");
    }

    #[test]
    fn request_and_dns_records_persist_without_a_dashboard() {
        let dir = std::env::temp_dir().join("proxy-stats-durable-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let info = StaticInfo {
            version: "test".into(),
            listen: String::new(),
            admin_listen: String::new(),
            ca_pem: String::new(),
            started: Instant::now(),
        };
        let logging =
            LoggingConfig { level: "info".into(), log_actions: true, log_requests: true };
        let s = Arc::new(SharedState::new(info, &logging).with_data_dir(&dir));

        let ex = s.record_forwarded(doc("https://kept.example/"), 200, false);
        // Persistence keeps the exchange active even with no dashboard, so its
        // captures land in the detail sidecar for later on-demand fetch.
        assert!(ex.is_active(), "persistence keeps the exchange capturing");
        let seq = ex.seq;
        ex.attach(CaptureSlot::ReqBody, || "hello-body".into());
        drop(ex); // flushes the detail line
        s.record_dns(DnsRecord {
            seq: 0,
            ts_ms: now_ms(),
            domain: "kept-dns.example".into(),
            qtype: "A".into(),
            outcome: DnsOutcome::Resolved,
            rcode: "NOERROR".into(),
            answers: String::new(),
            upstream: "1.1.1.1".into(),
            ech: false,
            blocked_by: String::new(),
            elapsed_ms: 1,
            proxy: false,
        });
        s.flush_logs(); // writes land on a background thread; wait for them
        let logs = dir.join("logs");
        let requests = std::fs::read_to_string(logs.join("request-log.jsonl")).unwrap();
        assert!(requests.contains("https://kept.example/"), "log: {requests}");
        // The list line stays lean — the body lives only in the sidecar.
        assert!(!requests.contains("hello-body"), "list line must not carry the body");
        let detail = std::fs::read_to_string(logs.join("request-detail.jsonl")).unwrap();
        assert!(detail.contains("hello-body"), "detail: {detail}");
        let queries = std::fs::read_to_string(logs.join("query-log.jsonl")).unwrap();
        assert!(queries.contains("kept-dns.example"), "log: {queries}");

        // The read accessors serve the persisted records back.
        let page = s.request_page(None, 100);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].url, "https://kept.example/");
        assert!(page[0].req_body.is_empty(), "the list record carries no body");
        assert_eq!(s.request_detail(seq).req_body, "hello-body");
        assert_eq!(s.query_page(None, 100).len(), 1);

        let mut obs = s.observe();
        s.record_forwarded(doc("https://both.example/"), 200, false);
        assert_eq!(obs.records().len(), 1);
        s.flush_logs();
        assert_eq!(
            std::fs::read_to_string(logs.join("request-log.jsonl")).unwrap().lines().count(),
            2
        );
    }

    #[test]
    fn stats_settings_validate_persist_and_apply_live() {
        let dir = std::env::temp_dir().join("proxy-stats-settings-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let info = StaticInfo {
            version: "test".into(),
            listen: String::new(),
            admin_listen: String::new(),
            ca_pem: String::new(),
            started: Instant::now(),
        };
        let logging =
            LoggingConfig { level: "info".into(), log_actions: true, log_requests: true };
        let s = SharedState::new(info, &logging).with_data_dir(&dir);

        let got = s.stats_settings();
        assert_eq!(got.retention_hours, Some(DEFAULT_RETENTION_HOURS));
        assert_eq!(got.log_rotate_hours, Some(DEFAULT_LOG_ROTATE_HOURS));

        assert!(s
            .apply_stats_settings(StatsOverrides {
                retention_hours: Some(0),
                log_rotate_hours: None
            })
            .is_err());
        assert!(s
            .apply_stats_settings(StatsOverrides {
                retention_hours: None,
                log_rotate_hours: Some(MAX_LOG_ROTATE_HOURS + 1)
            })
            .is_err());
        assert_eq!(s.stats_settings().retention_hours, Some(DEFAULT_RETENTION_HOURS));

        s.apply_stats_settings(StatsOverrides {
            retention_hours: Some(48),
            log_rotate_hours: None,
        })
        .unwrap();
        s.apply_stats_settings(StatsOverrides {
            retention_hours: None,
            log_rotate_hours: Some(168),
        })
        .unwrap();
        assert_eq!(s.history.retention_hours(), 48);
        assert_eq!(s.stats_settings().log_rotate_hours, Some(168));

        let info = StaticInfo {
            version: "test".into(),
            listen: String::new(),
            admin_listen: String::new(),
            ca_pem: String::new(),
            started: Instant::now(),
        };
        let s2 = SharedState::new(info, &logging).with_data_dir(&dir);
        assert_eq!(s2.stats_settings().retention_hours, Some(48));
        assert_eq!(s2.stats_settings().log_rotate_hours, Some(168));
    }

    #[test]
    fn an_inert_exchange_never_computes_or_attaches() {
        let info = StaticInfo {
            version: "test".into(),
            listen: String::new(),
            admin_listen: String::new(),
            ca_pem: String::new(),
            started: Instant::now(),
        };
        let s = Arc::new(SharedState::new(
            info,
            &LoggingConfig { level: "info".into(), log_actions: true, log_requests: false },
        ));
        let mut obs = s.observe();
        let ex = s.record_forwarded(doc("https://a.example/"), 200, false);
        assert!(!ex.is_active(), "inert when log_requests is off");
        ex.attach(CaptureSlot::ReqBody, || panic!("inert exchange must not render its text"));
        assert!(obs.records().is_empty());
    }

    #[test]
    fn seq_stays_monotonic_across_subscriber_churn() {
        let s = state();
        let first = {
            let _obs = s.observe();
            s.record_forwarded(doc("https://a.example/"), 200, false).seq
        };
        assert!(!s.record_forwarded(doc("https://b.example/"), 200, false).is_active());
        let _obs = s.observe();
        let second = s.record_forwarded(doc("https://c.example/"), 200, false).seq;
        assert!(second > first, "seq never reuses numbers across reconnects");
    }
}
