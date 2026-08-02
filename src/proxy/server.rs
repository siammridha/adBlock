//! The proxy server: accepts connections, MITMs HTTPS via CONNECT, and blocks
//! or forwards each request.
//!
//! It never edits a request or a response. Every request and every response
//! goes past the Adblock API on the way through, and whatever Adblock hands
//! back is what gets forwarded.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

use crate::adblock::api::AdBlocker;
use crate::proxy::error::{Error, Result};
use crate::proxy::exclusions::ExclusionStore;
use crate::stats::api::Metric;
use super::http_client::HttpClient;
use crate::proxy::blackhole::{BlackholeProbe, EgressResolver, Resolver};
use crate::proxy::ca::CertAuthority;
use crate::proxy::{capture, pipeline};
use crate::stats::api::{CaptureSlot, EventKind, Exchange, RequestFacts, SharedState};

/// What the record shows for this request. The resource type is Adblock's
/// name for it, carried on the decision — the proxy only reports it.
fn request_facts<'a>(plan: &'a pipeline::RequestPlan, req_type: &'a str) -> RequestFacts<'a> {
    RequestFacts { method: &plan.method, req_type, url: &plan.url, host: &plan.host }
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type BodyError = std::io::Error;
type ResBody = BoxBody<Bytes, BodyError>;
type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

pub(crate) trait Upstream: Send + Sync {
    fn send(
        &self,
        req: Request<Full<Bytes>>,
        host: &str,
        port: u16,
        tls: bool,
    ) -> BoxFuture<'_, std::result::Result<(Response<ResBody>, bool), BoxError>>;

    fn connect_raw(
        &self,
        host: &str,
        port: u16,
    ) -> BoxFuture<'_, std::result::Result<TcpStream, BoxError>> {
        let err = format!("raw connect to {host}:{port} unsupported by this upstream");
        Box::pin(async move { Err(err.into()) })
    }
}

impl Upstream for HttpClient {
    fn send(
        &self,
        req: Request<Full<Bytes>>,
        host: &str,
        port: u16,
        tls: bool,
    ) -> BoxFuture<'_, std::result::Result<(Response<ResBody>, bool), BoxError>> {
        let host = host.to_string();
        Box::pin(async move {
            let (resp, ech) = HttpClient::send(self, req, &host, port, tls).await?;
            Ok((resp.map(|b| b.map_err(std::io::Error::other).boxed()), ech))
        })
    }

    fn connect_raw(
        &self,
        host: &str,
        port: u16,
    ) -> BoxFuture<'_, std::result::Result<TcpStream, BoxError>> {
        let host = host.to_string();
        Box::pin(async move { self.connect_tcp(&host, port).await })
    }
}

// Concrete error type for the hyper services below. Using `BoxError` directly
// trips rustc's "implementation of `From` is not general enough" higher-ranked
// lifetime bug in `serve_connection`; the newtype sidesteps it.
#[derive(Debug)]
pub struct ProxyError(BoxError);

impl std::fmt::Display for ProxyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ProxyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.0)
    }
}

impl From<BoxError> for ProxyError {
    fn from(e: BoxError) -> Self {
        ProxyError(e)
    }
}

#[derive(Clone)]
pub struct Proxy {
    inner: Arc<Inner>,
}

struct Inner {
    adblock: Arc<AdBlocker>,
    exclusions: Arc<ExclusionStore>,
    ca: Arc<CertAuthority>,
    state: Arc<SharedState>,
    client: Arc<dyn Upstream>,
    blackhole: BlackholeProbe,
}

impl Proxy {
    pub fn new(
        adblock: Arc<AdBlocker>,
        exclusions: Arc<ExclusionStore>,
        ca: Arc<CertAuthority>,
        state: Arc<SharedState>,
        client: Arc<HttpClient>,
        egress: Arc<crate::proxy::egress::EgressPolicy>,
    ) -> Self {
        let resolver = Arc::new(EgressResolver(egress));
        Self::with_seams(adblock, exclusions, ca, state, client, resolver)
    }

    pub(crate) fn with_seams(
        adblock: Arc<AdBlocker>,
        exclusions: Arc<ExclusionStore>,
        ca: Arc<CertAuthority>,
        state: Arc<SharedState>,
        client: Arc<dyn Upstream>,
        resolver: Arc<dyn Resolver>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                adblock,
                exclusions,
                ca,
                state,
                client,
                blackhole: BlackholeProbe::new(resolver),
            }),
        }
    }

    pub async fn bind(addr: SocketAddr) -> Result<TcpListener> {
        TcpListener::bind(addr)
            .await
            .map_err(|e| Error::Config(format!("binding proxy {addr}: {e}")))
    }

    pub async fn accept_loop(self, listener: TcpListener) {
        let addr = listener.local_addr().ok();
        tracing::info!(?addr, "proxy listening");
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    let proxy = self.clone();
                    tokio::spawn(async move {
                        // serve_conn logs its own meaningful failures and stays
                        // quiet on routine closes; nothing to report here.
                        let _ = proxy.serve_conn(stream, peer).await;
                    });
                }
                Err(e) => tracing::debug!(error = %e, "proxy accept"),
            }
        }
    }

    async fn serve_conn(&self, stream: TcpStream, peer: SocketAddr) -> Result<()> {
        let opened = std::time::Instant::now();
        let io = TokioIo::new(stream);
        let proxy = self.clone();
        let served = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = served.clone();
        let service = service_fn(move |req| {
            let proxy = proxy.clone();
            seen.store(true, std::sync::atomic::Ordering::Relaxed);
            async move { proxy.dispatch(req).await }
        });
        let mut blocked_drop = false;
        if let Err(e) = hyper::server::conn::http1::Builder::new()
            .serve_connection(io, service)
            .with_upgrades()
            .await
        {
            blocked_drop = is_blocked_drop(&e);
            if !is_routine_close(&e) {
                tracing::warn!(%peer, error = %e, "client connection ended with error");
            }
        }
        // A connection we dropped on purpose for a blocked host is already
        // reported as a block; don't log its close too. Nor a connection that
        // never carried a request — a client's spare connection, usually one it
        // gives up on right after we killed a blocked one.
        if !blocked_drop && served.load(std::sync::atomic::Ordering::Relaxed) {
            tracing::debug!(%peer, ms = opened.elapsed().as_millis(), "client connection closed");
        }
        Ok(())
    }

    async fn dispatch(
        &self,
        req: Request<Incoming>,
    ) -> std::result::Result<Response<ResBody>, ProxyError> {
        let result = if req.method() == Method::CONNECT {
            self.handle_connect(req).await
        } else {
            self.handle_forward(req, false).await
        };
        result.map_err(ProxyError::from)
    }

    /// Count, log, and record a blocked request. The request headers (and body,
    /// when present) are captured the same way a forwarded request's are, so a
    /// blocked request's detail view shows exactly what the client sent.
    fn record_block(
        &self,
        facts: RequestFacts<'_>,
        host: &str,
        by: &str,
        req_hdrs: &str,
        req_body: &[u8],
        enc: &str,
    ) {
        let state = &self.inner.state;
        state.count_block(Metric::Blocked, host);
        let label = if facts.method == "CONNECT" { "CONNECT" } else { facts.req_type };
        if state.log_actions() {
            state.log_event(EventKind::Blocked, format!("{label}  {}  [{by}]", facts.url));
        }
        let exchange = state.record_blocked(facts, by);
        if !req_body.is_empty() {
            exchange.capture_request_body(req_body, req_body.len(), enc);
        }
        exchange.attach(CaptureSlot::ReqHeaders, || req_hdrs.to_string());
    }

    async fn handle_connect(
        &self,
        req: Request<Incoming>,
    ) -> std::result::Result<Response<ResBody>, BoxError> {
        let authority = req
            .uri()
            .authority()
            .map(|a| a.to_string())
            .ok_or("CONNECT without authority")?;
        let state = &self.inner.state;

        let plan = pipeline::plan_connect(
            &authority,
            |host| {
                let decision = self.inner.adblock.check_host("https", host);
                decision.blocked.then(|| decision.attribution.display())
            },
            |host| self.inner.exclusions.matching(host),
        );
        if let pipeline::ConnectVerdict::Deny { blocked_by } = &plan.verdict {
            let facts = RequestFacts {
                method: "CONNECT",
                req_type: "blocked",
                url: &plan.url,
                host: &plan.host,
            };
            // A CONNECT block happens before any request body exists, but the
            // CONNECT request's headers are available — capture what we have.
            let hdrs = capture::headers_text(req.headers());
            self.record_block(facts, &plan.host, blocked_by, &hdrs, &[], "");
            return Err(Box::new(BlockedDropped));
        }

        // Held for the tunnel's life so its close can be reported to the UI.
        let exchange = state.record_tunnel(
            RequestFacts {
                method: "CONNECT",
                req_type: plan.record_label(),
                url: &plan.url,
                host: &plan.host,
            },
            &plan.record_tag(),
        );
        let blind = matches!(plan.verdict, pipeline::ConnectVerdict::BlindTunnel { .. });
        let host = plan.host;
        let proxy = self.clone();

        tokio::spawn(async move {
            match hyper::upgrade::on(req).await {
                Ok(upgraded) => {
                    let opened = std::time::Instant::now();
                    let io = TokioIo::new(upgraded);
                    if blind {
                        let _ = proxy.blind_tunnel(io, &authority).await;
                    } else if let Err(e) = proxy.mitm(io, &host).await {
                        tracing::warn!(%host, error = %e, "could not start MITM (certificate setup failed)");
                    }
                    let ms = opened.elapsed().as_millis() as u64;
                    exchange.closed(ms);
                    tracing::debug!(
                        %host,
                        mode = if blind { "blind" } else { "mitm" },
                        ms,
                        "tunnel closed",
                    );
                }
                Err(e) => tracing::debug!(%host, error = %e, "upgrade failed"),
            }
        });

        Ok(Response::new(empty_body()))
    }

    async fn mitm<S>(&self, stream: S, host: &str) -> Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let server_config = self.inner.ca.server_config_for(host).await?;
        let acceptor = TlsAcceptor::from(server_config);
        let tls = match acceptor.accept(stream).await {
            Ok(tls) => tls,
            Err(e) => {
                // A client that just closed mid-handshake (UnexpectedEof) is
                // routine — a probe or an aborted connection, not a rejection.
                // Only a real certificate rejection (a TLS alert, surfaced as
                // InvalidData) means cert pinning or a distrusted proxy CA.
                if !is_routine_close(&e) {
                    tracing::warn!(
                        %host, error = %e,
                        "client rejected proxy certificate during TLS handshake \
                         (cert pinning or untrusted proxy CA) — cannot intercept",
                    );
                }
                return Ok(());
            }
        };
        let tls_io = TokioIo::new(tls);

        let proxy = self.clone();
        let service = service_fn(move |req| {
            let proxy = proxy.clone();
            async move { proxy.handle_forward(req, true).await.map_err(ProxyError::from) }
        });
        if let Err(e) = hyper::server::conn::http1::Builder::new()
            .serve_connection(tls_io, service)
            .await
        {
            if !is_routine_close(&e) {
                tracing::warn!(%host, error = %e, "MITM connection ended with error");
            }
        }
        Ok(())
    }

    async fn blind_tunnel<S>(&self, mut stream: S, authority: &str) -> Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (host, port) = crate::proxy::target::split_host_port(authority, 443);
        let mut upstream = self
            .inner
            .client
            .connect_raw(&host, port)
            .await
            .map_err(|e| Error::Other(format!("blind tunnel dial: {e}")))?;
        tokio::io::copy_bidirectional(&mut stream, &mut upstream)
            .await
            .map(|_| ())
            .map_err(Error::Io)
    }

    async fn handle_forward<B>(
        &self,
        req: Request<B>,
        secure: bool,
    ) -> std::result::Result<Response<ResBody>, BoxError>
    where
        B: hyper::body::Body<Data = Bytes> + Send,
        B::Error: Into<BoxError>,
    {
        let state = &self.inner.state;

        let plan = pipeline::plan_request(&req, secure)?;

        // Adblock reads the request as it arrived and answers what happens to
        // it, including what kind of resource it decided this is.
        let decision = self.inner.adblock.check_request(&plan.url, &req);

        // Collect the request up front so blocked and forwarded requests capture
        // their headers and body through the same path. A block happens before
        // the body would otherwise be read, so without this a blocked request
        // would have no stored body.
        let (mut parts, body) = req.into_parts();
        let req_bytes = body.collect().await.map_err(Into::into)?.to_bytes();
        parts.headers.remove(hyper::header::TRANSFER_ENCODING);
        parts.headers.remove(hyper::header::CONTENT_LENGTH);
        let req_enc = capture::content_encoding(&parts.headers);
        let req_hdrs = capture::headers_text(&parts.headers);

        if decision.blocked {
            let by = decision.attribution.display();
            self.record_block(
                request_facts(&plan, &decision.req_type),
                &plan.host,
                &by,
                &req_hdrs,
                &req_bytes,
                &req_enc,
            );
            // Dropping the connection or answering on it is the proxy's call,
            // and it only drops when the whole host is blocked and Adblock has
            // no stand-in to serve: a path-level block gets an answer so the
            // other requests sharing this connection keep working.
            let stand_in = decision.redirect.is_some();
            if !stand_in && self.inner.adblock.check_host(&plan.scheme, &plan.host).blocked {
                return Err(Box::new(BlockedDropped));
            }
            if stand_in {
                tracing::debug!(url = %plan.url, "serving redirect resource");
            }
            // What a block looks like is Adblock's; the proxy only sends it.
            let blocked = self.inner.adblock.blocked_response(decision);
            return Ok(blocked.map(|b| full_body(Bytes::from(b))));
        }

        if self.inner.blackhole.is_blackholed(&plan.host, plan.port).await {
            // A blackholed host resolves to nowhere, so the whole host is
            // effectively blocked: drop the connection.
            self.record_block(
                request_facts(&plan, &decision.req_type),
                &plan.host,
                "DNS blackhole",
                &req_hdrs,
                &req_bytes,
                &req_enc,
            );
            return Err(Box::new(BlockedDropped));
        }

        state.count(Metric::Requests, &plan.host);

        // Adblock decides what the request going upstream looks like — a
        // `$removeparam` cleaned URL, and asking for a body it can read. The
        // proxy hands over the request and forwards what comes back.
        self.inner.adblock.filter_request(&decision, &mut parts);

        let fwd = Request::from_parts(parts, Full::new(req_bytes.clone()));

        let (upstream, ech) = match self
            .inner
            .client
            .send(fwd, &plan.host, plan.port, plan.scheme == "https")
            .await
        {
            Ok(r) => r,
            Err(e) => {
                state.count(Metric::Errors, &plan.host);
                let cause = format!("upstream {}: {}", plan.host, error_chain(e.as_ref()));
                tracing::warn!(url = %plan.url, error = %cause, "upstream send failed");
                let exchange =
                    state.record_failed(request_facts(&plan, &decision.req_type), &cause);
                if !req_bytes.is_empty() {
                    exchange.capture_request_body(&req_bytes, req_bytes.len(), &req_enc);
                }
                exchange.attach(CaptureSlot::ReqHeaders, || req_hdrs);
                return Err(e);
            }
        };
        let exchange = state.record_forwarded(
            request_facts(&plan, &decision.req_type),
            upstream.status().as_u16(),
            ech,
        );
        if !req_bytes.is_empty() {
            exchange.capture_request_body(&req_bytes, req_bytes.len(), &req_enc);
        }
        exchange.attach(CaptureSlot::ReqHeaders, || req_hdrs);
        exchange.attach(CaptureSlot::RespHeaders, || {
            format!(
                "{:?} {}\n{}",
                upstream.version(),
                upstream.status(),
                capture::headers_text(upstream.headers())
            )
        });
        self.filter_response(upstream, &plan.url, &decision, exchange).await
    }

    /// Run the upstream response past Adblock and forward what it hands back.
    /// The proxy only decides whether to buffer the body or stream it — Adblock
    /// answers that too, because only Adblock knows whether it needs to read it.
    async fn filter_response(
        &self,
        resp: Response<ResBody>,
        url: &str,
        decision: &crate::adblock::api::BlockDecision,
        exchange: Exchange,
    ) -> std::result::Result<Response<ResBody>, BoxError> {
        let adblock = &self.inner.adblock;
        let buffer = adblock.reads_body(decision, resp.status().as_u16(), resp.headers());
        let (mut parts, body) = resp.into_parts();
        let resp_enc = capture::content_encoding(&parts.headers);

        if !buffer {
            // Header-only work — a `$csp` rule — still belongs to Adblock.
            adblock.filter_response(url, decision, &mut parts, None);
            let captured = capture::stream_response(exchange, body, resp_enc);
            return Ok(Response::from_parts(parts, captured.boxed()));
        }

        let collected = body.collect().await?.to_bytes();
        exchange.capture_response_body(&collected, collected.len(), &resp_enc);
        let edit = adblock.filter_response(url, decision, &mut parts, Some(&collected));
        if !edit.scriptlets.is_empty() {
            let names = edit.scriptlets.join(", ");
            tracing::info!(%url, scriptlets = %names, "scriptlets injected");
            exchange.attach(CaptureSlot::Scriptlets, || names.clone());
        }
        let out = edit.body.map_or(collected, Bytes::from);
        Ok(Response::from_parts(parts, full_body(out)))
    }
}

fn empty_body() -> ResBody {
    Full::new(Bytes::new()).map_err(|e| match e {}).boxed()
}

fn full_body(bytes: Bytes) -> ResBody {
    Full::new(bytes).map_err(|e| match e {}).boxed()
}

fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        s.push_str(": ");
        s.push_str(&c.to_string());
        cur = c.source();
    }
    s
}

#[derive(Debug)]
struct BlockedDropped;

impl std::fmt::Display for BlockedDropped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "request blocked (connection dropped)")
    }
}

impl std::error::Error for BlockedDropped {}

/// Whether a connection ended because we deliberately dropped it for a blocked
/// host. The block itself is already recorded, so the close is not news.
fn is_blocked_drop(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        if e.is::<BlockedDropped>() {
            return true;
        }
        cur = e.source();
    }
    false
}

/// Whether a finished connection's error is routine and not worth a log line: a
/// client that simply disconnected, a connection we deliberately dropped for a
/// blocked host, or a handler error already reported at its own site. Real
/// trouble — TLS/cert failures, cert pinning, an upstream forced reset we have
/// not already logged — is not routine and should surface.
fn is_routine_close(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        // Anything our own request handler returned is already logged where it
        // happened (blocked-host drops via BlockedDropped, upstream send
        // failures), so don't repeat it at the connection layer.
        if e.is::<ProxyError>() || e.is::<BlockedDropped>() {
            return true;
        }
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind::{
                BrokenPipe, ConnectionAborted, ConnectionReset, NotConnected, UnexpectedEof,
            };
            if matches!(
                io.kind(),
                BrokenPipe | ConnectionReset | ConnectionAborted | NotConnected | UnexpectedEof
            ) {
                return true;
            }
        }
        cur = e.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::proxy::blackhole::BLACKHOLE_TTL;
    use crate::adblock::api::MemoryListStore;
    use crate::adblock::api::AdblockConfig;
    use crate::stats::api::LoggingConfig;
    use crate::stats::api::StaticInfo;
    use hyper::StatusCode;

    struct CannedUpstream {
        status: StatusCode,
        headers: Vec<(&'static str, &'static str)>,
        body: &'static [u8],
        calls: AtomicUsize,
    }

    impl CannedUpstream {
        fn new(
            status: StatusCode,
            headers: Vec<(&'static str, &'static str)>,
            body: &'static [u8],
        ) -> Arc<Self> {
            Arc::new(Self {
                status,
                headers,
                body,
                calls: AtomicUsize::new(0),
            })
        }
    }

    impl Upstream for CannedUpstream {
        fn send(
            &self,
            _req: Request<Full<Bytes>>,
            _host: &str,
            _port: u16,
            _tls: bool,
        ) -> BoxFuture<'_, std::result::Result<(Response<ResBody>, bool), BoxError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut b = Response::builder().status(self.status);
            for (k, v) in &self.headers {
                b = b.header(*k, *v);
            }
            let resp = b.body(full_body(Bytes::from_static(self.body))).unwrap();
            Box::pin(async move { Ok((resp, false)) })
        }
    }

    struct FailingUpstream;

    impl Upstream for FailingUpstream {
        fn send(
            &self,
            _req: Request<Full<Bytes>>,
            _host: &str,
            _port: u16,
            _tls: bool,
        ) -> BoxFuture<'_, std::result::Result<(Response<ResBody>, bool), BoxError>> {
            Box::pin(async move { Err("connect refused (canned)".into()) })
        }
    }

    struct FixedResolver {
        addrs: Vec<SocketAddr>,
        calls: AtomicUsize,
    }

    impl FixedResolver {
        fn to(addrs: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                addrs: addrs.iter().map(|a| a.parse().unwrap()).collect(),
                calls: AtomicUsize::new(0),
            })
        }
    }

    impl Resolver for FixedResolver {
        fn resolve(
            &self,
            _host: &str,
            _port: u16,
        ) -> BoxFuture<'static, std::io::Result<Vec<SocketAddr>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let addrs = self.addrs.clone();
            Box::pin(async move { Ok(addrs) })
        }
    }

    /// An Adblock built for these tests. It has no admin endpoint, so no
    /// live-DOM runtime goes into a page unless the test asks for one with
    /// `set_admin_endpoint`.
    fn blocker(rules: &[&str], scriptlet_resources: std::path::PathBuf) -> Arc<AdBlocker> {
        let cfg = AdblockConfig {
            enabled: true,
            custom_rules: rules.iter().map(|s| s.to_string()).collect(),
            data_dir: std::path::PathBuf::from("/nonexistent-for-tests"),
            auto_update_hours: 0,
            inject_scriptlets: !scriptlet_resources.as_os_str().is_empty(),
            scriptlet_resources,
        };
        crate::adblock::api::with_store(&cfg, Arc::new(MemoryListStore::new()))
            .unwrap()
            .0
    }

    fn test_proxy(
        rules: &[&str],
        client: Arc<dyn Upstream>,
        resolver: Arc<dyn Resolver>,
    ) -> (Proxy, Arc<SharedState>) {
        proxy_with_adblock(blocker(rules, std::path::PathBuf::new()), client, resolver)
    }

    fn proxy_with_adblock(
        adblock: Arc<AdBlocker>,
        client: Arc<dyn Upstream>,
        resolver: Arc<dyn Resolver>,
    ) -> (Proxy, Arc<SharedState>) {
        let state = Arc::new(SharedState::new(
            StaticInfo {
                version: "test".into(),
                listen: String::new(),
                admin_listen: String::new(),
                started: std::time::Instant::now(),
            },
            &LoggingConfig {
                level: "info".into(),
                log_actions: true,
                log_requests: true,
                ..Default::default()
            },
        ));
        let ca = Arc::new(CertAuthority::generate().unwrap());
        let exclusions = Arc::new(ExclusionStore::load(
            std::path::PathBuf::from("/nonexistent-for-tests/excluded-domains.conf"),
        ));
        let proxy = Proxy::with_seams(adblock, exclusions, ca, state.clone(), client, resolver);
        (proxy, state)
    }

    fn get(url: &str, headers: &[(&str, &str)]) -> Request<Full<Bytes>> {
        let mut b = Request::builder().uri(url);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(Full::new(Bytes::new())).unwrap()
    }

    fn post(url: &str, headers: &[(&str, &str)], body: &'static [u8]) -> Request<Full<Bytes>> {
        let mut b = Request::builder().method(hyper::Method::POST).uri(url);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(Full::new(Bytes::from_static(body))).unwrap()
    }

    #[tokio::test]
    async fn blocked_request_is_denied_and_fully_recorded() {
        let upstream = CannedUpstream::new(StatusCode::OK, vec![], b"");
        let (proxy, state) = test_proxy(
            &["||ads.example.com^"],
            upstream.clone(),
            FixedResolver::to(&["93.184.216.34:80"]),
        );
        let mut obs = state.observe();

        let req = get("http://ads.example.com/banner.js", &[("accept", "*/*")]);
        let err = proxy.handle_forward(req, false).await.unwrap_err();
        assert!(err.is::<BlockedDropped>(), "deny must drop the connection: {err}");

        assert_eq!(upstream.calls.load(Ordering::SeqCst), 0, "blocked requests must not go upstream");
        assert_eq!(state.metrics.blocked_total.load(Ordering::Relaxed), 1);
        assert_eq!(state.metrics.requests_total.load(Ordering::Relaxed), 1);
        let snap = state.history.snapshot();
        assert!(snap.top_queried.is_empty(), "blocked domains must not appear as queried");
        assert_eq!(snap.top_blocked, vec![("ads.example.com".to_string(), 1)]);
        let recs = obs.records();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].status, 0);
        assert_eq!(recs[0].blocked_by, "||ads.example.com^ — custom");
        assert!(recs[0].req_headers.contains("accept: */*"));
        assert!(obs.events().iter().any(|e| e.kind == EventKind::Blocked));
    }

    #[tokio::test]
    async fn blocked_request_captures_headers_and_body() {
        // A host-level block still drops the connection, but the record now keeps
        // the full request headers AND the request body that was attempted.
        let upstream = CannedUpstream::new(StatusCode::OK, vec![], b"");
        let (proxy, state) = test_proxy(
            &["||ads.example.com^"],
            upstream.clone(),
            FixedResolver::to(&["93.184.216.34:80"]),
        );
        let mut obs = state.observe();

        let req = post(
            "http://ads.example.com/collect",
            &[("accept", "*/*"), ("content-type", "text/plain")],
            b"tracking-payload=42",
        );
        let err = proxy.handle_forward(req, false).await.unwrap_err();
        assert!(err.is::<BlockedDropped>(), "host block still drops: {err}");

        assert_eq!(upstream.calls.load(Ordering::SeqCst), 0, "blocked requests must not go upstream");
        let recs = obs.records();
        assert_eq!(recs.len(), 1);
        assert!(recs[0].req_headers.contains("accept: */*"), "headers: {}", recs[0].req_headers);
        assert!(
            recs[0].req_headers.contains("content-type: text/plain"),
            "headers: {}",
            recs[0].req_headers
        );
        assert_eq!(recs[0].req_body, "tracking-payload=42", "blocked body must be captured");
    }

    #[tokio::test]
    async fn path_block_returns_a_synthetic_response_and_keeps_the_connection() {
        // Only the sub-resource is blocked; the host root is fine. The request
        // gets a synthetic 403 instead of dropping the connection.
        let upstream = CannedUpstream::new(StatusCode::OK, vec![], b"");
        let (proxy, state) = test_proxy(
            &["||allowed.example/ads/tracker.js"],
            upstream.clone(),
            FixedResolver::to(&["93.184.216.34:80"]),
        );
        let mut obs = state.observe();

        let req = get("http://allowed.example/ads/tracker.js", &[("accept", "*/*")]);
        let resp = proxy
            .handle_forward(req, false)
            .await
            .expect("a path-level block returns Ok, not a dropped connection");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        assert_eq!(upstream.calls.load(Ordering::SeqCst), 0, "blocked requests must not go upstream");
        // Still counted and recorded exactly like any other block.
        assert_eq!(state.metrics.blocked_total.load(Ordering::Relaxed), 1);
        assert_eq!(state.metrics.requests_total.load(Ordering::Relaxed), 1);
        let recs = obs.records();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].status, 0);
        assert!(recs[0].blocked_by.starts_with("||allowed.example/ads/tracker.js"));
        assert!(recs[0].req_headers.contains("accept: */*"));
    }

    #[tokio::test]
    async fn host_block_still_drops_the_connection() {
        // The whole host is blocked, so the connection is dropped as before.
        let upstream = CannedUpstream::new(StatusCode::OK, vec![], b"");
        let (proxy, _state) = test_proxy(
            &["||blocked.example^"],
            upstream,
            FixedResolver::to(&["93.184.216.34:80"]),
        );

        let req = get("http://blocked.example/ads/tracker.js", &[("accept", "*/*")]);
        let err = proxy.handle_forward(req, false).await.unwrap_err();
        assert!(err.is::<BlockedDropped>(), "a fully blocked host drops the connection: {err}");
    }

    /// The close log is suppressed by finding `BlockedDropped` in the error hyper
    /// hands back, so the chain has to survive hyper's own wrapping.
    #[tokio::test]
    async fn hyper_error_for_a_dropped_block_is_recognised() {
        let (mut client, server) = tokio::io::duplex(1024);
        let serving = tokio::spawn(async move {
            hyper::server::conn::http1::Builder::new()
                .serve_connection(
                    TokioIo::new(server),
                    service_fn(|_req: Request<Incoming>| async {
                        std::result::Result::<Response<ResBody>, ProxyError>::Err(
                            ProxyError::from(Box::new(BlockedDropped) as BoxError),
                        )
                    }),
                )
                .with_upgrades()
                .await
        });

        use tokio::io::AsyncWriteExt;
        client
            .write_all(b"CONNECT blocked.example:443 HTTP/1.1\r\nHost: blocked.example:443\r\n\r\n")
            .await
            .unwrap();

        let err = serving.await.unwrap().expect_err("the drop must surface as an error");
        assert!(is_blocked_drop(&err), "hyper error must still name the block: {err}");
    }

    #[tokio::test(start_paused = true)]
    async fn dns_blackhole_is_denied_and_verdict_cached_until_ttl() {
        let upstream = CannedUpstream::new(StatusCode::OK, vec![], b"");
        let resolver = FixedResolver::to(&["0.0.0.0:80"]);
        let (proxy, state) = test_proxy(&[], upstream.clone(), resolver.clone());
        let mut obs = state.observe();

        for _ in 0..2 {
            let req = get("http://blackholed.example/x", &[("accept", "*/*")]);
            let err = proxy.handle_forward(req, false).await.unwrap_err();
            assert!(err.is::<BlockedDropped>());
        }
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1, "verdict must be cached within the TTL");
        assert_eq!(obs.records()[0].blocked_by, "DNS blackhole");
        assert_eq!(upstream.calls.load(Ordering::SeqCst), 0);

        tokio::time::advance(BLACKHOLE_TTL + std::time::Duration::from_secs(1)).await;
        let req = get("http://blackholed.example/x", &[("accept", "*/*")]);
        let _ = proxy.handle_forward(req, false).await.unwrap_err();
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn html_document_gets_cosmetic_css_injected() {
        let upstream = CannedUpstream::new(
            StatusCode::OK,
            vec![("content-type", "text/html"), ("content-length", "38")],
            b"<html><head></head><body>x</body></html>",
        );
        let (proxy, state) = test_proxy(
            &["example.com##.ad-banner"],
            upstream,
            FixedResolver::to(&["93.184.216.34:80"]),
        );

        let mut obs = state.observe();
        let req = get("http://example.com/", &[("accept", "text/html")]);
        let resp = proxy.handle_forward(req, false).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers().get(hyper::header::CONTENT_LENGTH).is_none(),
            "stale content-length must go once the body was edited"
        );
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains(".ad-banner{display:none !important}"), "html: {html}");

        let recs = obs.records();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].status, 200);
        assert!(recs[0].resp_headers.contains("content-type: text/html"));
        assert!(recs[0].resp_body.contains("<html>"));
    }

    #[tokio::test]
    async fn a_ping_rule_catches_a_beacon_without_swallowing_ordinary_fetches() {
        let upstream = CannedUpstream::new(StatusCode::OK, vec![], b"ok");
        let (proxy, _state) = test_proxy(
            // Both shapes the lists use: a `$ping`-only rule, and a plain one.
            &["||track.example/collect^$ping", "||other.example/api^$xhr"],
            upstream,
            FixedResolver::to(&["93.184.216.34:80"]),
        );
        let beacon = |url: &str| {
            Request::builder()
                .method(hyper::Method::POST)
                .uri(url)
                .header("sec-fetch-mode", "no-cors")
                .header("sec-fetch-dest", "empty")
                .body(Full::new(Bytes::new()))
                .unwrap()
        };

        let resp = proxy.handle_forward(beacon("http://track.example/collect"), false).await;
        assert!(
            resp.is_err() || resp.unwrap().status() == StatusCode::FORBIDDEN,
            "a sendBeacon call must match the $ping rule written for it"
        );

        // The second look only ever adds a block; a request nothing matches
        // still goes through, and an $xhr rule keeps its own verdict.
        let resp = proxy.handle_forward(beacon("http://allowed.example/collect"), false).await;
        assert_eq!(resp.unwrap().status(), StatusCode::OK, "unmatched beacons still pass");

        let xhr = Request::builder()
            .method(hyper::Method::POST)
            .uri("http://other.example/api")
            .header("sec-fetch-mode", "cors")
            .header("sec-fetch-dest", "empty")
            .body(Full::new(Bytes::new()))
            .unwrap();
        let resp = proxy.handle_forward(xhr, false).await;
        assert!(
            resp.is_err() || resp.unwrap().status() == StatusCode::FORBIDDEN,
            "an ordinary fetch is still matched as one"
        );
    }

    #[tokio::test]
    async fn procedural_rules_ride_along_with_the_page() {
        let upstream = CannedUpstream::new(
            StatusCode::OK,
            vec![("content-type", "text/html")],
            b"<html><head></head><body>x</body></html>",
        );
        let (proxy, _state) = test_proxy(
            &["example.com##.promo:has-text(Sponsored)", "example.com##.ad-banner"],
            upstream,
            FixedResolver::to(&["93.184.216.34:80"]),
        );

        let req = get("http://example.com/", &[("accept", "text/html")]);
        let resp = proxy.handle_forward(req, false).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains(r#"{"type":"has-text","arg":"Sponsored"}"#), "html: {html}");
        assert!(html.contains("MutationObserver"), "the evaluator itself: {html}");
        assert!(
            html.contains(".ad-banner{display:none !important}"),
            "a plain rule still goes out as CSS: {html}"
        );
    }

    #[tokio::test]
    async fn a_page_without_procedural_rules_gets_no_evaluator() {
        let upstream = CannedUpstream::new(
            StatusCode::OK,
            vec![("content-type", "text/html")],
            b"<html><head></head><body>x</body></html>",
        );
        let (proxy, _state) = test_proxy(
            &["example.com##.ad-banner"],
            upstream,
            FixedResolver::to(&["93.184.216.34:80"]),
        );

        let req = get("http://example.com/", &[("accept", "text/html")]);
        let resp = proxy.handle_forward(req, false).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(!html.contains("<script"), "nothing to evaluate, no script: {html}");
        assert!(html.contains(".ad-banner"), "html: {html}");
    }

    #[tokio::test]
    async fn a_csp_rule_adds_its_directives_to_the_page() {
        let upstream = CannedUpstream::new(
            StatusCode::OK,
            vec![
                ("content-type", "text/html"),
                ("content-security-policy", "img-src 'self'"),
            ],
            b"<html><head></head><body>x</body></html>",
        );
        let (proxy, _state) = test_proxy(
            &["$csp=worker-src 'none',domain=example.com"],
            upstream,
            FixedResolver::to(&["93.184.216.34:80"]),
        );

        let req = get("http://example.com/", &[("accept", "text/html")]);
        let resp = proxy.handle_forward(req, false).await.unwrap();
        let csp: Vec<&str> = resp
            .headers()
            .get_all("content-security-policy")
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(
            csp,
            vec!["img-src 'self'", "worker-src 'none'"],
            "the site's own policy has to keep applying alongside ours"
        );
    }

    #[tokio::test]
    async fn the_live_dom_runtime_is_injected_and_takes_the_csp_with_it() {
        let upstream = CannedUpstream::new(
            StatusCode::OK,
            vec![
                ("content-type", "text/html"),
                ("content-security-policy", "script-src 'self'"),
            ],
            b"<html><head></head><body>x</body></html>",
        );
        let adblock = blocker(&["example.com##.ad-banner"], std::path::PathBuf::new());
        adblock.set_admin_endpoint("127.0.0.1:8081");
        let (proxy, _state) =
            proxy_with_adblock(adblock, upstream, FixedResolver::to(&["93.184.216.34:80"]));

        let req = get("http://example.com/", &[("accept", "text/html")]);
        let resp = proxy.handle_forward(req, false).await.unwrap();
        assert!(
            resp.headers().get("content-security-policy").is_none(),
            "an inline script of ours needs the CSP out of the way, scriptlet or not"
        );
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains("http://127.0.0.1:8081/api/cosmetic"), "html: {html}");
        assert!(html.contains("MutationObserver"), "html: {html}");
        assert!(html.contains(".ad-banner{display:none !important}"), "html: {html}");
    }

    #[tokio::test]
    async fn no_admin_server_means_no_live_dom_runtime() {
        let upstream = CannedUpstream::new(
            StatusCode::OK,
            vec![("content-type", "text/html"), ("content-security-policy", "script-src 'self'")],
            b"<html><head></head><body>x</body></html>",
        );
        let (proxy, _state) = test_proxy(
            &["example.com##.ad-banner"],
            upstream,
            FixedResolver::to(&["93.184.216.34:80"]),
        );

        let req = get("http://example.com/", &[("accept", "text/html")]);
        let resp = proxy.handle_forward(req, false).await.unwrap();
        assert!(
            resp.headers().get("content-security-policy").is_some(),
            "CSS-only injection has no script and must leave the CSP alone"
        );
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(!html.contains("/api/cosmetic"), "nothing to ask, nothing injected: {html}");
        assert!(html.contains(".ad-banner{display:none !important}"), "html: {html}");
    }

    #[tokio::test]
    async fn cosmetic_css_is_not_injected_once_the_setting_is_off() {
        let upstream = CannedUpstream::new(
            StatusCode::OK,
            vec![("content-type", "text/html")],
            b"<html><head></head><body>x</body></html>",
        );
        let adblock = blocker(&["example.com##.ad-banner"], std::path::PathBuf::new());
        adblock.set_admin_endpoint("127.0.0.1:8081");
        adblock.set_decisions(br#"{"cosmetic": false}"#).unwrap();
        let (proxy, _state) =
            proxy_with_adblock(adblock, upstream, FixedResolver::to(&["93.184.216.34:80"]));

        let req = get("http://example.com/", &[("accept", "text/html")]);
        let resp = proxy.handle_forward(req, false).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(!html.contains("display:none"), "html: {html}");
        assert!(
            html.contains("/api/cosmetic"),
            "the runtime has its own switch and stays on: {html}"
        );
    }

    #[tokio::test]
    async fn the_runtime_switch_turns_off_the_live_dom_script() {
        let upstream = CannedUpstream::new(
            StatusCode::OK,
            vec![("content-type", "text/html")],
            b"<html><head></head><body><div class=\"ad-banner\">ad</div></body></html>",
        );
        let adblock = blocker(&["example.com##.ad-banner"], std::path::PathBuf::new());
        adblock.set_admin_endpoint("127.0.0.1:8081");
        adblock.set_decisions(br#"{"runtime": false}"#).unwrap();
        let (proxy, _state) =
            proxy_with_adblock(adblock, upstream, FixedResolver::to(&["93.184.216.34:80"]));

        let req = get("http://example.com/", &[("accept", "text/html")]);
        let resp = proxy.handle_forward(req, false).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(!html.contains("/api/cosmetic"), "html: {html}");
        assert!(
            html.contains("display:none"),
            "the CSS has its own switch and stays on: {html}"
        );
    }

    #[tokio::test]
    async fn generic_cosmetic_rule_hides_by_page_class() {
        let upstream = CannedUpstream::new(
            StatusCode::OK,
            vec![("content-type", "text/html")],
            b"<html><head></head><body><div class=\"adsbox\">ad</div></body></html>",
        );
        let (proxy, _state) = test_proxy(
            &["##.adsbox"],
            upstream,
            FixedResolver::to(&["93.184.216.34:80"]),
        );

        let req = get("http://anysite.example/", &[("accept", "text/html")]);
        let resp = proxy.handle_forward(req, false).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains(".adsbox{display:none !important}"), "html: {html}");
    }

    #[tokio::test]
    async fn scriptlet_injection_strips_csp_and_annotates_the_record() {
        let dir = std::env::temp_dir().join(format!(
            "sp-server-scriptlet-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let res_path = dir.join("resources.json");
        std::fs::write(
            &res_path,
            serde_json::json!([{
                "name": "sptest.js",
                "kind": {"mime": "application/javascript"},
                "content": "ZnVuY3Rpb24gc3B0ZXN0Rm4oKXt3aW5kb3cuX19zcF9zY3JpcHRsZXRfcmFuPXRydWU7fQ=="
            }])
            .to_string(),
        )
        .unwrap();
        let adblock = blocker(&["example.com##+js(sptest)"], res_path);
        let upstream = CannedUpstream::new(
            StatusCode::OK,
            vec![
                ("content-type", "text/html"),
                ("content-security-policy", "script-src 'self'"),
                ("content-security-policy-report-only", "script-src 'self'"),
            ],
            b"<html><head></head><body>x</body></html>",
        );
        let (proxy, state) =
            proxy_with_adblock(adblock, upstream, FixedResolver::to(&["93.184.216.34:80"]));

        let mut obs = state.observe();
        let req = get("http://example.com/", &[("accept", "text/html")]);
        let resp = proxy.handle_forward(req, false).await.unwrap();
        assert!(
            resp.headers().get("content-security-policy").is_none()
                && resp.headers().get("content-security-policy-report-only").is_none(),
            "scriptlet injection must strip the CSP or the browser refuses the script"
        );
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains("window.__sp_scriptlet_ran"), "html: {html}");

        assert_eq!(obs.records()[0].scriptlets, "sptest.js");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn css_only_injection_keeps_the_csp() {
        let upstream = CannedUpstream::new(
            StatusCode::OK,
            vec![
                ("content-type", "text/html"),
                ("content-security-policy", "script-src 'self'"),
            ],
            b"<html><head></head><body>x</body></html>",
        );
        let (proxy, _state) = test_proxy(
            &["example.com##.ad-banner"],
            upstream,
            FixedResolver::to(&["93.184.216.34:80"]),
        );

        let req = get("http://example.com/", &[("accept", "text/html")]);
        let resp = proxy.handle_forward(req, false).await.unwrap();
        assert_eq!(
            resp.headers().get("content-security-policy").map(|v| v.to_str().unwrap()),
            Some("script-src 'self'")
        );
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(std::str::from_utf8(&body).unwrap().contains(".ad-banner{display:none !important}"));
    }

    #[tokio::test]
    async fn non_html_streams_through_with_prefix_capture() {
        let upstream = CannedUpstream::new(
            StatusCode::OK,
            vec![("content-type", "application/javascript")],
            b"console.log('untouched')",
        );
        let (proxy, state) = test_proxy(
            &["example.com##.ad-banner"],
            upstream,
            FixedResolver::to(&["93.184.216.34:80"]),
        );

        let mut obs = state.observe();
        let req = get("http://example.com/app.js", &[("sec-fetch-dest", "script")]);
        let resp = proxy.handle_forward(req, false).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"console.log('untouched')");

        assert_eq!(obs.records()[0].resp_body, "console.log('untouched')");
    }

    #[tokio::test]
    async fn upstream_failure_is_a_tagged_record_not_an_error_event() {
        let (proxy, state) = test_proxy(
            &[],
            Arc::new(FailingUpstream),
            FixedResolver::to(&["93.184.216.34:80"]),
        );

        let mut obs = state.observe();
        let req = get("http://down.example/x", &[("accept", "*/*")]);
        assert!(proxy.handle_forward(req, false).await.is_err());
        assert_eq!(state.metrics.errors_total.load(Ordering::Relaxed), 1);
        let recs = obs.records();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].kind, crate::stats::api::RequestKind::Failed);
        assert!(
            recs[0].blocked_by.starts_with("upstream down.example:"),
            "tag: {}",
            recs[0].blocked_by
        );
        assert!(recs[0].req_headers.contains("accept: */*"));
        let events = obs.events();
        assert!(
            !events.iter().any(|e| e.kind == EventKind::Error),
            "events: {events:?}"
        );
    }
}
