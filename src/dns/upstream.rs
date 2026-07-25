//! Upstream resolvers over UDP, TCP, DoT, or DoH, with per-server health
//! stats and weighted selection.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::stream::{FuturesUnordered, StreamExt};
use http_body_util::{BodyExt, Full};
use hickory_proto::op::{Message, Query};
use hickory_proto::rr::{Name, RData, RecordType};
use hyper_util::rt::TokioIo;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::Mutex;

pub use super::config::UpstreamMode;

type TlsStream = tokio_rustls::client::TlsStream<TcpStream>;
type HttpSender = hyper::client::conn::http1::SendRequest<Full<Bytes>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    Udp,
    Tcp,
    Tls,
    Https,
}

impl Scheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Udp => "udp",
            Self::Tcp => "tcp",
            Self::Tls => "tls",
            Self::Https => "https",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpstreamAddr {
    pub spec: String,
    /// The operator's label for this server, from a trailing `#name` in the
    /// spec. Empty when the spec carries no label.
    pub name: String,
    /// A `!` prefix on the spec parks the server: it stays configured and
    /// listed, but no query is sent to it.
    pub enabled: bool,
    pub scheme: Scheme,
    /// The name this server is reached and verified by — a hostname, or an
    /// address when that is all the operator gave. Used for TLS SNI and the
    /// DoH `Host` header.
    pub host: String,
    /// The address `host` resolved to when it was added, from `?ip=` in the
    /// spec. Queries connect straight to it, so no lookup is needed at query
    /// time. Absent for specs that name a host and were never resolved.
    pub ip: Option<std::net::Ipv4Addr>,
    pub port: u16,
    pub path: String,
}

impl UpstreamAddr {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        // A trailing `#name` labels the server; it is not part of the address.
        let (addr, name) = match spec.split_once('#') {
            Some((a, n)) => (a.trim_end(), n.trim().to_string()),
            None => (spec, String::new()),
        };
        // A leading `!` parks the server without removing it.
        let (addr, enabled) = match addr.strip_prefix('!') {
            Some(a) => (a.trim_start(), false),
            None => (addr, true),
        };
        let (scheme, rest) = match addr.split_once("://") {
            Some(("https", r)) => (Scheme::Https, r),
            Some(("tls", r)) => (Scheme::Tls, r),
            Some(("udp", r)) => (Scheme::Udp, r),
            Some(("tcp", r)) => (Scheme::Tcp, r),
            Some((s, _)) => return Err(format!("upstream '{spec}': unknown scheme '{s}'")),
            None => (Scheme::Udp, addr),
        };
        // `?ip=1.2.3.4` pins the address the host resolved to when it was added.
        let (rest, ip) = match rest.split_once('?') {
            Some((r, q)) => (r, parse_pinned_ip(q).map_err(|e| format!("upstream '{spec}': {e}"))?),
            None => (rest, None),
        };
        let (authority, path) = match rest.split_once('/') {
            Some((a, p)) => (a, format!("/{p}")),
            None => (rest, "/dns-query".to_string()),
        };
        let default_port = match scheme {
            Scheme::Https => 443,
            Scheme::Tls => 853,
            Scheme::Udp | Scheme::Tcp => 53,
        };
        let (host, port) = split_host_port(authority, default_port)
            .map_err(|e| format!("upstream '{spec}': {e}"))?;
        if host.is_empty() {
            return Err(format!("upstream '{spec}': empty host"));
        }
        if host.parse::<std::net::Ipv6Addr>().is_ok() {
            return Err(format!(
                "upstream '{spec}': IPv6 upstreams are not supported (IPv4-only resolver)"
            ));
        }
        Ok(Self { spec: spec.to_string(), name, enabled, scheme, host, ip, port, path })
    }

    /// What the query log and status show: the operator's name when set, else
    /// the raw spec.
    pub fn label(&self) -> String {
        if self.name.is_empty() {
            self.spec.clone()
        } else {
            self.name.clone()
        }
    }

    /// The hostname queries are sent to, or empty when the operator gave a bare
    /// address and there is no name involved.
    pub fn hostname(&self) -> &str {
        if self.host.parse::<std::net::Ipv4Addr>().is_ok() {
            ""
        } else {
            &self.host
        }
    }

    /// `ip:port`, the address queries actually connect to. Falls back to the
    /// hostname when no address is pinned (it is looked up at query time).
    pub fn address(&self) -> String {
        match self.ip {
            Some(ip) => format!("{ip}:{}", self.port),
            None => format!("{}:{}", self.host, self.port),
        }
    }

    fn host_header(&self) -> String {
        if self.port == 443 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// The wire form of an ECH probe: an HTTPS query for the canary domain.
fn probe_query(domain: &str) -> Option<(Vec<u8>, u16)> {
    let name = match Name::from_utf8(domain) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(%domain, error = %e, "invalid ech_probe_domain; skipping ECH probe");
            return None;
        }
    };
    let mut wire = Message::query();
    wire.metadata.id = random_id();
    wire.metadata.recursion_desired = true;
    wire.add_query(Query::query(name, RecordType::HTTPS));
    let mut edns = hickory_proto::op::Edns::new();
    edns.set_max_payload(1232);
    wire.edns = Some(edns);
    let id = wire.metadata.id;
    match wire.to_vec() {
        Ok(bytes) => Some((bytes, id)),
        Err(e) => {
            tracing::warn!(error = %e, "encoding ECH probe query");
            None
        }
    }
}

fn parse_pinned_ip(query: &str) -> Result<Option<std::net::Ipv4Addr>, String> {
    match query.split('&').find_map(|kv| kv.trim().strip_prefix("ip=")) {
        None => Ok(None),
        Some(v) => v
            .parse()
            .map(Some)
            .map_err(|_| format!("'{v}' is not an IPv4 address")),
    }
}

fn split_host_port(s: &str, default_port: u16) -> Result<(String, u16), String> {
    if let Some(rest) = s.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| "unclosed '[' in address".to_string())?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| format!("bad port '{p}'"))?,
            None => default_port,
        };
        return Ok((host.to_string(), port));
    }
    if s.parse::<std::net::Ipv6Addr>().is_ok() {
        return Ok((s.to_string(), default_port));
    }
    match s.rsplit_once(':') {
        Some((host, p)) => Ok((
            host.to_string(),
            p.parse().map_err(|_| format!("bad port '{p}'"))?,
        )),
        None => Ok((s.to_string(), default_port)),
    }
}

#[derive(Default)]
struct Stats {
    queries: AtomicU64,
    failures: AtomicU64,
    ewma_rtt_us: AtomicU64,
}

impl Stats {
    fn record(&self, rtt: Duration, failed: bool) {
        self.queries.fetch_add(1, Ordering::Relaxed);
        if failed {
            self.failures.fetch_add(1, Ordering::Relaxed);
        }
        let sample = rtt.as_micros().min(u128::from(u64::MAX)) as u64;
        let old = self.ewma_rtt_us.load(Ordering::Relaxed);
        let new = if old == 0 { sample } else { old - old / 8 + sample / 8 };
        self.ewma_rtt_us.store(new.max(1), Ordering::Relaxed);
    }

    fn reset(&self) {
        self.queries.store(0, Ordering::Relaxed);
        self.failures.store(0, Ordering::Relaxed);
        self.ewma_rtt_us.store(0, Ordering::Relaxed);
    }

    fn weight(&self) -> u64 {
        match self.ewma_rtt_us.load(Ordering::Relaxed) {
            0 => 10_000,
            us => (10_000 / (us / 1000).max(1)).max(1),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EchSupport {
    Unknown,
    Supported,
    Unsupported,
}

impl EchSupport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Supported,
            2 => Self::Unsupported,
            _ => Self::Unknown,
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Supported => 1,
            Self::Unsupported => 2,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct UpstreamStat {
    pub spec: String,
    pub name: String,
    pub host: String,
    pub address: String,
    pub scheme: &'static str,
    pub enabled: bool,
    pub queries: u64,
    pub failures: u64,
    pub avg_rtt_ms: Option<u64>,
    pub ech: EchSupport,
}

struct Upstream {
    addr: UpstreamAddr,
    stats: Stats,
    ech: AtomicU8,
    resolved: std::sync::Mutex<Option<(IpAddr, Instant)>>,
    dot: Mutex<Option<TlsStream>>,
    doh: Mutex<Option<HttpSender>>,
}

impl Upstream {
    fn ech(&self) -> EchSupport {
        EchSupport::from_u8(self.ech.load(Ordering::Relaxed))
    }

    fn set_ech(&self, support: EchSupport) {
        self.ech.store(support.to_u8(), Ordering::Relaxed);
    }
}

pub struct Resolver {
    upstreams: Vec<Arc<Upstream>>,
    mode: UpstreamMode,
    bootstrap: Vec<SocketAddr>,
    timeout: Duration,
    tls: tokio_rustls::TlsConnector,
    preferred: AtomicUsize,
}

impl Resolver {
    pub fn new(
        upstreams: &[String],
        mode: UpstreamMode,
        bootstrap: &[String],
        timeout_ms: u64,
    ) -> Result<Self, String> {
        let upstreams = upstreams
            .iter()
            .map(|s| Ok(Arc::new(Upstream {
                addr: UpstreamAddr::parse(s)?,
                stats: Stats::default(),
                ech: AtomicU8::new(0),
                resolved: std::sync::Mutex::new(None),
                dot: Mutex::new(None),
                doh: Mutex::new(None),
            })))
            .collect::<Result<Vec<_>, String>>()?;
        let bootstrap = bootstrap
            .iter()
            .map(|s| {
                let (host, port) = split_host_port(s.trim(), 53)?;
                let ip: std::net::Ipv4Addr = host
                    .parse()
                    .map_err(|_| format!("bootstrap '{s}' must be an IPv4 address"))?;
                Ok(SocketAddr::new(IpAddr::V4(ip), port))
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Self {
            upstreams,
            mode,
            bootstrap,
            timeout: Duration::from_millis(timeout_ms.max(100)),
            tls: tokio_rustls::TlsConnector::from(super::tls::client_config()),
            preferred: AtomicUsize::new(0),
        })
    }

    pub fn upstream_stats(&self) -> Vec<UpstreamStat> {
        self.upstreams
            .iter()
            .map(|u| UpstreamStat {
                spec: u.addr.spec.clone(),
                name: u.addr.label(),
                host: u.addr.hostname().to_string(),
                address: u.addr.address(),
                scheme: u.addr.scheme.as_str(),
                enabled: u.addr.enabled,
                queries: u.stats.queries.load(Ordering::Relaxed),
                failures: u.stats.failures.load(Ordering::Relaxed),
                avg_rtt_ms: match u.stats.ewma_rtt_us.load(Ordering::Relaxed) {
                    0 => None,
                    us => Some(us / 1000),
                },
                ech: u.ech(),
            })
            .collect()
    }

    pub fn reset_stats(&self) {
        for u in &self.upstreams {
            u.stats.reset();
        }
    }

    /// Indices of the upstreams that may be queried. A parked (`!`-prefixed)
    /// server keeps its slot and its stats but is never selected.
    fn active(&self) -> Vec<usize> {
        (0..self.upstreams.len())
            .filter(|&i| self.upstreams[i].addr.enabled)
            .collect()
    }

    /// Resolve a hostname to an IPv4 address using the bootstrap servers. An
    /// address is returned as-is.
    pub async fn resolve_host(&self, host: &str) -> Result<IpAddr, String> {
        match host.parse::<IpAddr>() {
            Ok(ip) => Ok(ip),
            Err(_) => self.bootstrap_lookup(host).await.map(|(ip, _)| ip),
        }
    }

    /// Copy learned ECH results from the resolver this one replaces. A settings
    /// change rebuilds the resolver; without this, every server would read as
    /// "not probed" until the next probe even though nothing about it changed.
    pub fn inherit_ech(&self, old: &Resolver) {
        for up in &self.upstreams {
            let same = old.upstreams.iter().find(|o| {
                (o.addr.scheme, &o.addr.host, o.addr.port) == (up.addr.scheme, &up.addr.host, up.addr.port)
            });
            if let Some(prev) = same {
                up.ech.store(prev.ech.load(Ordering::Relaxed), Ordering::Relaxed);
            }
        }
    }

    /// Probe every enabled upstream for ECH support. Returns how many answered.
    pub async fn probe_ech(&self, domain: &str) -> usize {
        let Some((bytes, id)) = probe_query(domain) else { return 0 };
        let probes: FuturesUnordered<_> = self
            .active()
            .into_iter()
            .map(|i| self.probe_one(&self.upstreams[i], &bytes, id))
            .collect();
        probes.filter(|got| std::future::ready(got.is_some())).count().await
    }

    /// Probe the single upstream with this spec. `None` means it did not answer —
    /// which is how a newly added server's test query is judged.
    pub async fn probe_ech_spec(&self, spec: &str, domain: &str) -> Option<EchSupport> {
        let up = self.upstreams.iter().find(|u| u.addr.spec == spec)?;
        let (bytes, id) = probe_query(domain)?;
        self.probe_one(up, &bytes, id).await
    }

    async fn probe_one(&self, up: &Upstream, bytes: &[u8], id: u16) -> Option<EchSupport> {
        match tokio::time::timeout(self.timeout, self.query_one(up, bytes)).await {
            Ok(Ok(resp)) if resp.metadata.id == id => {
                let support = if answer_has_ech(&resp) {
                    EchSupport::Supported
                } else {
                    EchSupport::Unsupported
                };
                up.set_ech(support);
                tracing::debug!(upstream = %up.addr.spec, ech = support.as_str(), "ECH probe");
                Some(support)
            }
            _ => None,
        }
    }

    pub async fn resolve(&self, query: &Query) -> Result<(Message, String), String> {
        let active = self.active();
        if active.is_empty() {
            return Err(if self.upstreams.is_empty() {
                "no DNS upstreams configured".into()
            } else {
                "every DNS upstream is disabled".to_string()
            });
        }
        let (bytes, id) = self.encode(query)?;
        match self.mode {
            UpstreamMode::Parallel if active.len() > 1 => {
                self.resolve_parallel(&active, &bytes, id).await
            }
            UpstreamMode::LoadBalance if active.len() > 1 => {
                self.resolve_sequential(self.load_balanced_order(active), &bytes, id).await
            }
            _ => self.resolve_sequential(self.failover_order(active), &bytes, id).await,
        }
    }

    fn encode(&self, query: &Query) -> Result<(Vec<u8>, u16), String> {
        let mut wire = Message::query();
        wire.metadata.id = random_id();
        wire.metadata.recursion_desired = true;
        wire.add_query(query.clone());
        let mut edns = hickory_proto::op::Edns::new();
        edns.set_max_payload(1232);
        wire.edns = Some(edns);
        let id = wire.metadata.id;
        let bytes = wire.to_vec().map_err(|e| format!("encoding query: {e}"))?;
        Ok((bytes, id))
    }

    fn failover_order(&self, active: Vec<usize>) -> Vec<usize> {
        let preferred = self.preferred.load(Ordering::Relaxed);
        let start = active.iter().position(|&i| i == preferred).unwrap_or(0);
        (0..active.len()).map(|k| active[(start + k) % active.len()]).collect()
    }

    fn load_balanced_order(&self, active: Vec<usize>) -> Vec<usize> {
        let weights: Vec<u64> =
            active.iter().map(|&i| self.upstreams[i].stats.weight()).collect();
        let picked = active[pick_weighted(&weights, random_u64())];
        let mut order = active;
        order.sort_by_key(|&i| self.upstreams[i].stats.ewma_rtt_us.load(Ordering::Relaxed));
        order.retain(|&i| i != picked);
        order.insert(0, picked);
        order
    }

    async fn resolve_sequential(
        &self,
        order: Vec<usize>,
        bytes: &[u8],
        id: u16,
    ) -> Result<(Message, String), String> {
        let mut last_err = String::new();
        for idx in order {
            let up = &self.upstreams[idx];
            let started = Instant::now();
            match tokio::time::timeout(self.timeout, self.query_one(up, bytes)).await {
                Ok(Ok(resp)) if resp.metadata.id == id => {
                    up.stats.record(started.elapsed(), false);
                    self.preferred.store(idx, Ordering::Relaxed);
                    return Ok((resp, up.addr.label()));
                }
                Ok(Ok(_)) => last_err = format!("{}: response id mismatch", up.addr.spec),
                Ok(Err(e)) => last_err = format!("{}: {e}", up.addr.spec),
                Err(_) => last_err = format!("{}: timed out", up.addr.spec),
            }
            up.stats.record(self.timeout * 2, true);
            tracing::debug!(upstream = %up.addr.spec, error = %last_err, "dns upstream failed");
        }
        Err(last_err)
    }

    async fn resolve_parallel(
        &self,
        active: &[usize],
        bytes: &[u8],
        id: u16,
    ) -> Result<(Message, String), String> {
        let mut in_flight: FuturesUnordered<_> = active
            .iter()
            .map(|&i| &self.upstreams[i])
            .map(|up| async move {
                let started = Instant::now();
                let outcome = tokio::time::timeout(self.timeout, self.query_one(up, bytes)).await;
                (up, started.elapsed(), outcome)
            })
            .collect();

        let mut last_err = String::new();
        while let Some((up, elapsed, outcome)) = in_flight.next().await {
            match outcome {
                Ok(Ok(resp)) if resp.metadata.id == id => {
                    up.stats.record(elapsed, false);
                    return Ok((resp, up.addr.label()));
                }
                Ok(Ok(_)) => last_err = format!("{}: response id mismatch", up.addr.spec),
                Ok(Err(e)) => last_err = format!("{}: {e}", up.addr.spec),
                Err(_) => last_err = format!("{}: timed out", up.addr.spec),
            }
            up.stats.record(self.timeout * 2, true);
            tracing::debug!(upstream = %up.addr.spec, error = %last_err, "dns upstream failed");
        }
        Err(last_err)
    }

    async fn query_one(&self, up: &Upstream, wire: &[u8]) -> Result<Message, String> {
        match up.addr.scheme {
            Scheme::Udp => {
                let resp = self.query_udp(up, wire).await?;
                if resp.metadata.truncation {
                    return self.query_tcp(up, wire).await;
                }
                Ok(resp)
            }
            Scheme::Tcp => self.query_tcp(up, wire).await,
            Scheme::Tls => self.query_dot(up, wire).await,
            Scheme::Https => self.query_doh(up, wire).await,
        }
    }

    async fn query_udp(&self, up: &Upstream, wire: &[u8]) -> Result<Message, String> {
        let target = self.target_addr(up).await?;
        let sock = UdpSocket::bind(("0.0.0.0", 0)).await.map_err(|e| e.to_string())?;
        sock.connect(target).await.map_err(|e| e.to_string())?;
        sock.send(wire).await.map_err(|e| e.to_string())?;
        let mut buf = [0u8; 4096];
        let n = sock.recv(&mut buf).await.map_err(|e| e.to_string())?;
        Message::from_vec(&buf[..n]).map_err(|e| format!("parsing response: {e}"))
    }

    async fn query_tcp(&self, up: &Upstream, wire: &[u8]) -> Result<Message, String> {
        let target = self.target_addr(up).await?;
        let mut stream = TcpStream::connect(target).await.map_err(|e| e.to_string())?;
        write_framed(&mut stream, wire).await?;
        read_framed(&mut stream).await
    }

    async fn query_dot(&self, up: &Upstream, wire: &[u8]) -> Result<Message, String> {
        let mut guard = up.dot.lock().await;
        if let Some(stream) = guard.as_mut() {
            if let Ok(resp) = exchange_framed(stream, wire).await {
                return Ok(resp);
            }
            *guard = None;
        }
        let mut stream = self.dial_tls(up).await?;
        let resp = exchange_framed(&mut stream, wire).await?;
        *guard = Some(stream);
        Ok(resp)
    }

    async fn query_doh(&self, up: &Upstream, wire: &[u8]) -> Result<Message, String> {
        let mut guard = up.doh.lock().await;
        if guard.as_ref().is_none_or(|s| s.is_closed()) {
            let stream = self.dial_tls(up).await?;
            let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
                .await
                .map_err(|e| format!("doh handshake: {e}"))?;
            tokio::spawn(async move {
                if let Err(e) = conn.await {
                    tracing::debug!(error = %e, "doh conn ended");
                }
            });
            *guard = Some(sender);
        }
        let sender = guard.as_mut().expect("doh sender just installed");

        let req = hyper::Request::builder()
            .method(hyper::Method::POST)
            .uri(&up.addr.path)
            .header(hyper::header::HOST, up.addr.host_header())
            .header(hyper::header::CONTENT_TYPE, "application/dns-message")
            .header(hyper::header::ACCEPT, "application/dns-message")
            .body(Full::new(Bytes::copy_from_slice(wire)))
            .map_err(|e| e.to_string())?;
        let resp = match sender.send_request(req).await {
            Ok(r) => r,
            Err(e) => {
                *guard = None;
                return Err(format!("doh request: {e}"));
            }
        };
        if resp.status() != hyper::StatusCode::OK {
            return Err(format!("doh HTTP {}", resp.status()));
        }
        let body = http_body_util::Limited::new(resp.into_body(), 65_536)
            .collect()
            .await
            .map_err(|e| format!("doh body: {e}"))?
            .to_bytes();
        Message::from_vec(&body).map_err(|e| format!("parsing response: {e}"))
    }

    async fn dial_tls(&self, up: &Upstream) -> Result<TlsStream, String> {
        let target = self.target_addr(up).await?;
        let tcp = TcpStream::connect(target)
            .await
            .map_err(|e| format!("connecting {target}: {e}"))?;
        tcp.set_nodelay(true).ok();
        let sni = rustls::pki_types::ServerName::try_from(up.addr.host.clone())
            .map_err(|e| format!("bad server name '{}': {e}", up.addr.host))?;
        self.tls
            .connect(sni, tcp)
            .await
            .map_err(|e| format!("tls to {}: {e}", up.addr.host))
    }

    async fn target_addr(&self, up: &Upstream) -> Result<SocketAddr, String> {
        // A pinned address wins: it was resolved when the server was added, so
        // no lookup (and no bootstrap server) is needed to use it.
        if let Some(ip) = up.addr.ip {
            return Ok(SocketAddr::new(IpAddr::V4(ip), up.addr.port));
        }
        if let Ok(ip) = up.addr.host.parse::<IpAddr>() {
            return Ok(SocketAddr::new(ip, up.addr.port));
        }
        if let Some((ip, expires)) = *up.resolved.lock().expect("resolved lock") {
            if expires > Instant::now() {
                return Ok(SocketAddr::new(ip, up.addr.port));
            }
        }
        let (ip, ttl) = self.bootstrap_lookup(&up.addr.host).await?;
        let expires = Instant::now() + Duration::from_secs(u64::from(ttl.clamp(60, 3600)));
        *up.resolved.lock().expect("resolved lock") = Some((ip, expires));
        Ok(SocketAddr::new(ip, up.addr.port))
    }

    async fn bootstrap_lookup(&self, host: &str) -> Result<(IpAddr, u32), String> {
        if self.bootstrap.is_empty() {
            return Err(format!(
                "upstream host '{host}' needs dns.bootstrap servers to resolve"
            ));
        }
        let name = Name::from_utf8(host).map_err(|e| format!("bad host '{host}': {e}"))?;
        for server in &self.bootstrap {
            let mut q = Message::query();
            q.metadata.id = random_id();
            q.metadata.recursion_desired = true;
            q.add_query(Query::query(name.clone(), RecordType::A));
            let wire = q.to_vec().map_err(|e| e.to_string())?;

            let sock = UdpSocket::bind(("0.0.0.0", 0)).await.map_err(|e| e.to_string())?;
            sock.connect(server).await.map_err(|e| e.to_string())?;
            if sock.send(&wire).await.is_err() {
                continue;
            }
            let mut buf = [0u8; 2048];
            let Ok(Ok(n)) =
                tokio::time::timeout(Duration::from_secs(3), sock.recv(&mut buf)).await
            else {
                continue;
            };
            let Ok(resp) = Message::from_vec(&buf[..n]) else { continue };
            for r in &resp.answers {
                if let RData::A(a) = &r.data {
                    if !a.0.is_unspecified() {
                        return Ok((IpAddr::V4(a.0), r.ttl));
                    }
                }
            }
        }
        Err(format!("bootstrap could not resolve '{host}'"))
    }
}

pub(super) fn answer_has_ech(resp: &Message) -> bool {
    resp.answers
        .iter()
        .chain(resp.additionals.iter())
        .any(super::response::record_has_ech)
}

async fn exchange_framed(stream: &mut TlsStream, wire: &[u8]) -> Result<Message, String> {
    write_framed(stream, wire).await?;
    read_framed(stream).await
}

async fn write_framed<S: tokio::io::AsyncWrite + Unpin>(
    stream: &mut S,
    wire: &[u8],
) -> Result<(), String> {
    let len = u16::try_from(wire.len()).map_err(|_| "query too large".to_string())?;
    let mut framed = Vec::with_capacity(2 + wire.len());
    framed.extend_from_slice(&len.to_be_bytes());
    framed.extend_from_slice(wire);
    stream.write_all(&framed).await.map_err(|e| e.to_string())
}

async fn read_framed<S: tokio::io::AsyncRead + Unpin>(stream: &mut S) -> Result<Message, String> {
    let mut len = [0u8; 2];
    stream.read_exact(&mut len).await.map_err(|e| e.to_string())?;
    let len = usize::from(u16::from_be_bytes(len));
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await.map_err(|e| e.to_string())?;
    Message::from_vec(&buf).map_err(|e| format!("parsing response: {e}"))
}

fn pick_weighted(weights: &[u64], r: u64) -> usize {
    let total: u64 = weights.iter().sum();
    if total == 0 {
        return 0;
    }
    let mut point = r % total;
    for (i, w) in weights.iter().enumerate() {
        if point < *w {
            return i;
        }
        point -= w;
    }
    weights.len() - 1
}

fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(std::time::UNIX_EPOCH.elapsed().map_or(0, |d| d.subsec_nanos() as u64));
    h.finish()
}

fn random_id() -> u16 {
    random_u64() as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_upstream_shapes() {
        let u = UpstreamAddr::parse("https://dns.cloudflare.com/dns-query").unwrap();
        assert_eq!(
            (u.scheme, u.host.as_str(), u.port, u.path.as_str()),
            (Scheme::Https, "dns.cloudflare.com", 443, "/dns-query")
        );
        let u = UpstreamAddr::parse("https://dns.example:8443/my/path").unwrap();
        assert_eq!((u.port, u.path.as_str()), (8443, "/my/path"));
        let u = UpstreamAddr::parse("tls://1.1.1.1").unwrap();
        assert_eq!((u.scheme, u.host.as_str(), u.port), (Scheme::Tls, "1.1.1.1", 853));
        let u = UpstreamAddr::parse("9.9.9.9").unwrap();
        assert_eq!((u.scheme, u.port), (Scheme::Udp, 53));
        let u = UpstreamAddr::parse("udp://9.9.9.9:5300").unwrap();
        assert_eq!((u.scheme, u.port), (Scheme::Udp, 5300));
        // The shape the UI saves: port, pinned ip and label all at once.
        let u = UpstreamAddr::parse("tls://dns.nextdns.io:853?ip=45.90.30.0#NextDNS").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.ip, u.name.as_str()),
            ("dns.nextdns.io", 853, Some("45.90.30.0".parse().unwrap()), "NextDNS")
        );
    }

    #[test]
    fn a_spec_carries_its_name_pinned_address_and_on_off_state() {
        let u = UpstreamAddr::parse(
            "https://dns.example:443/dns-query?ip=1.1.1.1#Cloudflare",
        )
        .unwrap();
        assert_eq!((u.name.as_str(), u.enabled), ("Cloudflare", true));
        // The hostname is what TLS verifies; the pinned address is what we dial.
        assert_eq!((u.host.as_str(), u.hostname()), ("dns.example", "dns.example"));
        assert_eq!(u.path, "/dns-query");
        assert_eq!(u.address(), "1.1.1.1:443");
        assert_eq!(u.label(), "Cloudflare");

        let u = UpstreamAddr::parse("!tls://9.9.9.9#Quad 9").unwrap();
        assert_eq!((u.name.as_str(), u.enabled, u.port), ("Quad 9", false, 853));
        assert_eq!(u.scheme.as_str(), "tls");
        // A bare address is its own target, and reads as "no hostname".
        assert_eq!((u.ip, u.hostname(), u.address().as_str()), (None, "", "9.9.9.9:853"));

        // An unnamed spec falls back to itself as the label.
        let u = UpstreamAddr::parse("udp://8.8.8.8").unwrap();
        assert_eq!((u.name.as_str(), u.label().as_str()), ("", "udp://8.8.8.8"));

        let err = UpstreamAddr::parse("tls://dns.example?ip=nope").unwrap_err();
        assert!(err.contains("not an IPv4 address"), "err: {err}");
    }

    #[tokio::test]
    async fn disabled_upstreams_are_never_queried() {
        let up = udp_upstream(std::net::Ipv4Addr::new(4, 4, 4, 4), Duration::ZERO).await;
        for mode in [UpstreamMode::Failover, UpstreamMode::LoadBalance, UpstreamMode::Parallel] {
            let resolver = Resolver::new(
                &[format!("!udp://{up}#parked"), "udp://127.0.0.1:1#live".into()],
                mode,
                &[],
                300,
            )
            .unwrap();
            // The only reachable server is parked, so the query fails on the live
            // (dead) one instead of quietly using it.
            let err = resolver.resolve(&a_query("example.com.")).await.unwrap_err();
            assert!(err.starts_with("udp://127.0.0.1:1#live"), "{mode:?}: {err}");
            let parked = resolver.upstream_stats().into_iter().find(|s| !s.enabled).unwrap();
            assert_eq!((parked.queries, parked.name.as_str()), (0, "parked"));
        }

        let all_off = Resolver::new(&[format!("!udp://{up}")], UpstreamMode::Failover, &[], 300).unwrap();
        let err = all_off.resolve(&a_query("example.com.")).await.unwrap_err();
        assert!(err.contains("every DNS upstream is disabled"), "err: {err}");
    }

    #[tokio::test]
    async fn resolve_host_passes_addresses_through_and_needs_bootstrap_for_names() {
        let resolver = Resolver::new(&[], UpstreamMode::Failover, &[], 300).unwrap();
        assert_eq!(resolver.resolve_host("1.2.3.4").await.unwrap().to_string(), "1.2.3.4");
        assert!(resolver.resolve_host("dns.example").await.is_err());
    }

    #[test]
    fn doh_host_header_keeps_odd_ports() {
        assert_eq!(
            UpstreamAddr::parse("https://dns.google/dns-query").unwrap().host_header(),
            "dns.google"
        );
        assert_eq!(UpstreamAddr::parse("https://10.0.0.1:8443/p").unwrap().host_header(), "10.0.0.1:8443");
    }

    #[test]
    fn rejects_bad_specs_and_ipv6() {
        assert!(UpstreamAddr::parse("quic://1.1.1.1").is_err());
        assert!(UpstreamAddr::parse("tls://1.1.1.1:notaport").is_err());
        assert!(UpstreamAddr::parse("https://").is_err());
        let err = UpstreamAddr::parse("tls://2620:fe::fe").unwrap_err();
        assert!(err.contains("IPv6"), "err: {err}");
        assert!(UpstreamAddr::parse("tcp://[2620:fe::fe]:53").is_err());
        assert!(UpstreamAddr::parse("https://[2606:4700::1111]/dns-query").is_err());
    }

    #[test]
    fn resolver_allows_empty_upstreams_and_rejects_non_ipv4_bootstrap() {
        // Empty upstreams is allowed at construction; resolve() returns an error
        // (rather than panicking) when there is nothing to forward to.
        assert!(Resolver::new(&[], UpstreamMode::Failover, &[], 5000).is_ok());
        for bad in ["dns.example.com", "::1", "[2620:fe::fe]:53"] {
            let err = Resolver::new(
                &["tls://1.1.1.1".into()],
                UpstreamMode::Failover,
                &[bad.into()],
                5000,
            )
            .err()
            .expect("non-IPv4 bootstrap must be rejected");
            assert!(err.contains("must be an IPv4 address"), "{bad}: {err}");
        }
    }

    #[test]
    fn weighted_pick_respects_weights_and_edges() {
        let w = [10, 1, 100];
        assert_eq!(pick_weighted(&w, 0), 0);
        assert_eq!(pick_weighted(&w, 9), 0);
        assert_eq!(pick_weighted(&w, 10), 1);
        assert_eq!(pick_weighted(&w, 11), 2);
        assert_eq!(pick_weighted(&w, 110), 2);
        assert_eq!(pick_weighted(&w, 111), 0);
        assert_eq!(pick_weighted(&[0, 0], 5), 0);
    }

    #[test]
    fn stats_weight_favors_fast_and_reliable_upstreams() {
        let fast = Stats::default();
        let slow = Stats::default();
        for _ in 0..8 {
            fast.record(Duration::from_millis(10), false);
            slow.record(Duration::from_millis(200), false);
        }
        assert!(fast.weight() > slow.weight());

        let flaky = Stats::default();
        flaky.record(Duration::from_millis(10), false);
        flaky.record(Duration::from_secs(10), true);
        assert!(flaky.weight() < fast.weight());
        assert_eq!(flaky.failures.load(Ordering::Relaxed), 1);

        assert!(Stats::default().weight() >= fast.weight());
    }

    async fn udp_upstream(ip: std::net::Ipv4Addr, delay: Duration) -> SocketAddr {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            while let Ok((n, peer)) = sock.recv_from(&mut buf).await {
                let Ok(req) = Message::from_vec(&buf[..n]) else { continue };
                let mut resp = Message::response(req.metadata.id, hickory_proto::op::OpCode::Query);
                resp.add_queries(req.queries.iter().cloned());
                if let Some(q) = req.queries.first() {
                    resp.add_answer(hickory_proto::rr::Record::from_rdata(
                        q.name().clone(),
                        60,
                        RData::A(hickory_proto::rr::rdata::A(ip)),
                    ));
                }
                tokio::time::sleep(delay).await;
                let _ = sock.send_to(&resp.to_vec().unwrap(), peer).await;
            }
        });
        addr
    }

    fn a_query(domain: &str) -> Query {
        Query::query(Name::from_utf8(domain).unwrap(), RecordType::A)
    }

    #[test]
    fn answer_has_ech_detects_the_ech_param_in_https_records() {
        use hickory_proto::rr::rdata::svcb::{Alpn, EchConfigList, SvcParamKey, SvcParamValue, SVCB};
        use hickory_proto::rr::rdata::HTTPS;
        use hickory_proto::rr::Record;

        let https = |with_ech: bool| {
            let mut params = vec![(SvcParamKey::Alpn, SvcParamValue::Alpn(Alpn(vec!["h2".into()])))];
            if with_ech {
                params.push((
                    SvcParamKey::EchConfigList,
                    SvcParamValue::EchConfigList(EchConfigList(vec![1, 2, 3])),
                ));
            }
            let name = Name::from_utf8("example.com.").unwrap();
            Record::from_rdata(name.clone(), 300, RData::HTTPS(HTTPS(SVCB::new(1, name, params))))
        };

        let mut with = Message::query();
        with.add_answer(https(true));
        assert!(answer_has_ech(&with));

        let mut without = Message::query();
        without.add_answer(https(false));
        assert!(!answer_has_ech(&without));

        assert!(!answer_has_ech(&Message::query()));
    }

    #[tokio::test]
    async fn bootstrap_skips_blackhole_answers() {
        let blackhole = udp_upstream(std::net::Ipv4Addr::UNSPECIFIED, Duration::ZERO).await;
        let resolver = Resolver::new(
            &["tls://dot.example.net".into()],
            UpstreamMode::Failover,
            &[blackhole.to_string()],
            500,
        )
        .unwrap();

        let err = resolver.resolve(&a_query("example.com.")).await.unwrap_err();
        assert!(err.contains("could not resolve"), "err: {err}");
    }

    #[tokio::test]
    async fn parallel_mode_returns_the_fastest_answer_despite_dead_upstreams() {
        let fast = udp_upstream(std::net::Ipv4Addr::new(1, 1, 1, 1), Duration::ZERO).await;
        let slow = udp_upstream(std::net::Ipv4Addr::new(2, 2, 2, 2), Duration::from_millis(300)).await;
        let resolver = Resolver::new(
            &["udp://127.0.0.1:1".into(), format!("udp://{slow}"), format!("udp://{fast}")],
            UpstreamMode::Parallel,
            &[],
            2000,
        )
        .unwrap();

        let (resp, who) = resolver.resolve(&a_query("example.com.")).await.unwrap();
        assert_eq!(who, format!("udp://{fast}"));
        assert_eq!(
            resp.answers[0].data,
            RData::A(hickory_proto::rr::rdata::A(std::net::Ipv4Addr::new(1, 1, 1, 1)))
        );
        let stats = resolver.upstream_stats();
        let fast_stat = stats.iter().find(|s| s.spec == who).unwrap();
        assert_eq!((fast_stat.queries, fast_stat.failures), (1, 0));
    }

    #[tokio::test]
    async fn load_balance_mode_resolves_and_learns_from_failures() {
        let up = udp_upstream(std::net::Ipv4Addr::new(3, 3, 3, 3), Duration::ZERO).await;
        let resolver = Resolver::new(
            &["udp://127.0.0.1:1".into(), format!("udp://{up}")],
            UpstreamMode::LoadBalance,
            &[],
            300,
        )
        .unwrap();

        for _ in 0..4 {
            let (_, who) = resolver.resolve(&a_query("example.com.")).await.unwrap();
            assert_eq!(who, format!("udp://{up}"));
        }
        let stats = resolver.upstream_stats();
        let live = stats.iter().find(|s| s.spec == format!("udp://{up}")).unwrap();
        assert_eq!(live.failures, 0);
        assert!(live.queries >= 4);
    }
}
