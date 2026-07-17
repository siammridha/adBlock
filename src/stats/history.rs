//! 24-hour metric history in 10-minute buckets, plus top-domain counts.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Metric {
    Requests,
    Blocked,
    Errors,
    DnsQueries,
    DnsBlocked,
    DnsCached,
}

pub const METRIC_COUNT: usize = 6;

impl Metric {
    pub const ALL: [Metric; METRIC_COUNT] = [
        Metric::Requests,
        Metric::Blocked,
        Metric::Errors,
        Metric::DnsQueries,
        Metric::DnsBlocked,
        Metric::DnsCached,
    ];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn key(self) -> &'static str {
        match self {
            Metric::Requests => "requests",
            Metric::Blocked => "blocked",
            Metric::Errors => "errors",
            Metric::DnsQueries => "dns_queries",
            Metric::DnsBlocked => "dns_blocked",
            Metric::DnsCached => "dns_cached",
        }
    }

    fn domain_table(self) -> Option<Table> {
        match self {
            Metric::Requests | Metric::DnsQueries => Some(Table::Queried),
            Metric::Blocked | Metric::DnsBlocked => Some(Table::Blocked),
            Metric::Errors | Metric::DnsCached => None,
        }
    }
}

#[derive(Clone, Copy)]
enum Table {
    Queried,
    Blocked,
}

const BUCKET_SECS: u64 = 600;
const BUCKETS_PER_HOUR: usize = 6;
const WINDOW_BUCKETS: usize = 24 * BUCKETS_PER_HOUR;
const TOP_N: usize = 25;

struct Bucket {
    idx: u64,
    counts: [u64; METRIC_COUNT],
    queried: HashMap<String, u64>,
    blocked: HashMap<String, u64>,
}

impl Bucket {
    fn new(idx: u64) -> Self {
        Self { idx, counts: [0; METRIC_COUNT], queried: HashMap::new(), blocked: HashMap::new() }
    }
}

struct Inner {
    buckets: VecDeque<Bucket>,
    totals: [u64; METRIC_COUNT],
    queried: HashMap<String, u64>,
    blocked: HashMap<String, u64>,
    window_buckets: usize,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            buckets: VecDeque::new(),
            totals: [0; METRIC_COUNT],
            queried: HashMap::new(),
            blocked: HashMap::new(),
            window_buckets: WINDOW_BUCKETS,
        }
    }
}

pub struct Snapshot {
    pub bucket_secs: u64,
    pub totals: [u64; METRIC_COUNT],
    pub series: [Vec<u64>; METRIC_COUNT],
    pub top_queried: Vec<(String, u64)>,
    pub top_blocked: Vec<(String, u64)>,
}

#[derive(Default)]
pub struct History(Mutex<Inner>);

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, metric: Metric, domain: &str) {
        self.record_at(now_secs(), metric, domain);
    }

    fn record_at(&self, now: u64, metric: Metric, domain: &str) {
        let idx = now / BUCKET_SECS;
        let mut g = self.0.lock().expect("history lock");
        let inner = &mut *g;
        inner.rotate(idx);
        if inner.buckets.back().is_none_or(|b| b.idx != idx) {
            inner.buckets.push_back(Bucket::new(idx));
        }
        let bucket = inner.buckets.back_mut().expect("bucket just ensured");
        bucket.counts[metric.index()] += 1;
        inner.totals[metric.index()] += 1;
        if domain.is_empty() {
            return;
        }
        if let Some(table) = metric.domain_table() {
            let (in_bucket, in_window) = match table {
                Table::Queried => (&mut bucket.queried, &mut inner.queried),
                Table::Blocked => (&mut bucket.blocked, &mut inner.blocked),
            };
            *in_bucket.entry(domain.to_string()).or_insert(0) += 1;
            *in_window.entry(domain.to_string()).or_insert(0) += 1;
        }
    }

    pub fn reset(&self) {
        let mut g = self.0.lock().expect("history lock");
        *g = Inner { window_buckets: g.window_buckets, ..Inner::default() };
    }

    pub fn set_retention_hours(&self, hours: u32) {
        self.0.lock().expect("history lock").window_buckets = hours as usize * BUCKETS_PER_HOUR;
    }

    pub fn retention_hours(&self) -> u32 {
        (self.0.lock().expect("history lock").window_buckets / BUCKETS_PER_HOUR) as u32
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot_at(now_secs())
    }

    fn snapshot_at(&self, now: u64) -> Snapshot {
        let idx = now / BUCKET_SECS;
        let mut g = self.0.lock().expect("history lock");
        g.rotate(idx);
        let window = g.window_buckets;
        let mut series: [Vec<u64>; METRIC_COUNT] = std::array::from_fn(|_| vec![0; window]);
        for b in &g.buckets {
            let age = (idx - b.idx) as usize;
            let pos = window - 1 - age;
            for (s, count) in series.iter_mut().zip(b.counts) {
                s[pos] = count;
            }
        }
        Snapshot {
            bucket_secs: BUCKET_SECS,
            totals: g.totals,
            series,
            top_queried: top(&g.queried),
            top_blocked: top(&g.blocked),
        }
    }
}

impl Inner {
    fn rotate(&mut self, idx: u64) {
        while self.buckets.front().is_some_and(|b| b.idx + self.window_buckets as u64 <= idx) {
            let b = self.buckets.pop_front().expect("front just checked");
            for m in 0..METRIC_COUNT {
                self.totals[m] -= b.counts[m];
            }
            subtract(&mut self.queried, &b.queried);
            subtract(&mut self.blocked, &b.blocked);
        }
    }
}

fn subtract(window: &mut HashMap<String, u64>, expired: &HashMap<String, u64>) {
    for (domain, n) in expired {
        if let Some(count) = window.get_mut(domain) {
            *count = count.saturating_sub(*n);
            if *count == 0 {
                window.remove(domain);
            }
        }
    }
}

fn top(map: &HashMap<String, u64>) -> Vec<(String, u64)> {
    let mut v: Vec<(String, u64)> = map.iter().map(|(d, n)| (d.clone(), *n)).collect();
    v.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(TOP_N);
    v
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_700_000_000;

    #[test]
    fn counts_land_in_totals_and_the_series_tail() {
        let h = History::new();
        h.record_at(T0, Metric::Requests, "a.example");
        h.record_at(T0, Metric::Requests, "a.example");
        h.record_at(T0, Metric::Blocked, "ads.example");
        let s = h.snapshot_at(T0);
        assert_eq!(s.totals[Metric::Requests.index()], 2);
        assert_eq!(s.totals[Metric::Blocked.index()], 1);
        assert_eq!(s.series[Metric::Requests.index()][WINDOW_BUCKETS - 1], 2);
        assert_eq!(s.series[Metric::Requests.index()][WINDOW_BUCKETS - 2], 0);
        assert_eq!(s.series[Metric::Requests.index()].len(), WINDOW_BUCKETS);
    }

    #[test]
    fn buckets_age_out_of_the_window_with_their_domains() {
        let h = History::new();
        h.record_at(T0, Metric::DnsQueries, "old.example");
        h.record_at(T0 + BUCKET_SECS, Metric::DnsQueries, "new.example");
        let s = h.snapshot_at(T0 + (WINDOW_BUCKETS as u64 - 1) * BUCKET_SECS);
        assert_eq!(s.totals[Metric::DnsQueries.index()], 2);
        let s = h.snapshot_at(T0 + WINDOW_BUCKETS as u64 * BUCKET_SECS);
        assert_eq!(s.totals[Metric::DnsQueries.index()], 1);
        assert_eq!(s.top_queried, vec![("new.example".to_string(), 1)]);
        assert_eq!(s.series[Metric::DnsQueries.index()][0], 1, "survivor sits at the window's edge");
    }

    #[test]
    fn retention_is_live_and_survives_reset() {
        let h = History::new();
        assert_eq!(h.retention_hours(), 24);
        h.record_at(T0, Metric::Requests, "old.example");
        h.record_at(T0 + 2 * 3600, Metric::Requests, "new.example");
        let s = h.snapshot_at(T0 + 2 * 3600);
        assert_eq!(s.totals[Metric::Requests.index()], 2);
        h.set_retention_hours(1);
        let s = h.snapshot_at(T0 + 2 * 3600);
        assert_eq!(s.totals[Metric::Requests.index()], 1);
        assert_eq!(s.series[Metric::Requests.index()].len(), BUCKETS_PER_HOUR);
        assert_eq!(s.top_queried, vec![("new.example".to_string(), 1)]);
        h.reset();
        assert_eq!(h.retention_hours(), 1);
    }

    #[test]
    fn top_domains_sort_by_count_split_by_table_and_cap() {
        let h = History::new();
        for _ in 0..3 {
            h.record_at(T0, Metric::Requests, "big.example");
        }
        h.record_at(T0, Metric::DnsQueries, "small.example");
        h.record_at(T0, Metric::DnsBlocked, "ads.example");
        h.record_at(T0, Metric::Requests, "");
        h.record_at(T0, Metric::DnsCached, "cached.example");
        h.record_at(T0, Metric::Errors, "down.example");
        for i in 0..TOP_N + 5 {
            h.record_at(T0, Metric::Requests, &format!("filler-{i:02}.example"));
        }
        let s = h.snapshot_at(T0);
        assert_eq!(s.top_queried.len(), TOP_N);
        assert_eq!(s.top_queried[0], ("big.example".to_string(), 3));
        assert_eq!(s.top_blocked, vec![("ads.example".to_string(), 1)]);
        assert!(!s.top_queried.iter().any(|(d, _)| d == "cached.example"));
    }
}
