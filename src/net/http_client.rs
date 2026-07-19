//! HTTP/1.1 client with per-host connection pooling and optional TLS/ECH.
//! Used both as the proxy upstream and for blocklist downloads.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Method, Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

use crate::proxy::egress::EgressPolicy;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type Sender = hyper::client::conn::http1::SendRequest<Full<Bytes>>;
type PoolKey = (String, u16, bool);

struct Pooled {
    sender: Sender,
    ech: bool,
}

trait Idle {
    fn is_live(&self) -> bool;
    fn ech(&self) -> bool;
}

impl Idle for Pooled {
    fn is_live(&self) -> bool {
        !self.sender.is_closed()
    }

    fn ech(&self) -> bool {
        self.ech
    }
}

struct Pool<C = Pooled> {
    idle: Mutex<HashMap<PoolKey, Vec<C>>>,
}

const MAX_IDLE_PER_HOST: usize = 8;

impl<C: Idle> Pool<C> {
    fn new() -> Self {
        Self { idle: Mutex::new(HashMap::new()) }
    }

    fn checkout(&self, key: &PoolKey, want_ech: bool) -> Option<C> {
        let mut idle = self.idle.lock().expect("pool lock");
        let v = idle.get_mut(key)?;
        while let Some(pos) = v.iter().rposition(|p| p.is_live() && (!want_ech || p.ech())) {
            let p = v.remove(pos);
            if p.is_live() {
                return Some(p);
            }
        }
        None
    }

    fn park(&self, key: PoolKey, entry: C) {
        let mut idle = self.idle.lock().expect("pool lock");
        let v = idle.entry(key).or_default();
        if v.len() < MAX_IDLE_PER_HOST {
            v.push(entry);
        }
    }
}

impl Pool<Pooled> {
    fn park_when_ready(self: &Arc<Self>, key: PoolKey, mut sender: Sender, ech: bool) {
        let pool = Arc::clone(self);
        tokio::spawn(async move {
            if sender.ready().await.is_ok() && !sender.is_closed() {
                pool.park(key, Pooled { sender, ech });
            }
        });
    }
}

fn replay_or_surface(
    mut e: hyper::client::conn::TrySendError<Request<Full<Bytes>>>,
    retry: Request<Full<Bytes>>,
) -> std::result::Result<Request<Full<Bytes>>, hyper::Error> {
    if let Some(req) = e.take_message() {
        return Ok(req);
    }
    let err = e.into_error();
    if err.is_incomplete_message() {
        Ok(retry)
    } else {
        Err(err)
    }
}

const DEFAULT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Clone)]
pub struct HttpClient {
    tls: tokio_rustls::TlsConnector,
    pool: Arc<Pool>,
    connect_timeout: Option<std::time::Duration>,
    egress: Option<Arc<EgressPolicy>>,
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpClient {
    pub fn new() -> Self {
        Self::with_tls_config(default_client_config())
    }

    pub fn with_tls_config(config: Arc<rustls::ClientConfig>) -> Self {
        Self {
            tls: tokio_rustls::TlsConnector::from(config),
            pool: Arc::new(Pool::new()),
            connect_timeout: Some(DEFAULT_CONNECT_TIMEOUT),
            egress: None,
        }
    }

    pub fn with_connect_timeout(mut self, millis: u64) -> Self {
        self.connect_timeout =
            (millis > 0).then(|| std::time::Duration::from_millis(millis));
        self
    }

    pub fn with_egress(mut self, egress: Arc<EgressPolicy>) -> Self {
        self.egress = Some(egress);
        self
    }

    pub async fn send(
        &self,
        req: Request<Full<Bytes>>,
        host: &str,
        port: u16,
        tls: bool,
    ) -> std::result::Result<(Response<Incoming>, bool), BoxError> {
        let key: PoolKey = (host.to_string(), port, tls);
        let want_ech = tls && self.egress.as_ref().is_some_and(|e| e.use_ech());
        let mut req = to_origin_form(req);

        if let Some(mut pooled) = self.pool.checkout(&key, want_ech) {
            let retry = clone_request(&req);
            match pooled.sender.try_send_request(req).await {
                Ok(resp) => {
                    let ech = pooled.ech;
                    self.pool.park_when_ready(key, pooled.sender, ech);
                    return Ok((resp, ech));
                }
                Err(e) => req = replay_or_surface(e, retry).map_err(Box::new)?,
            }
        }

        let (mut sender, ech) = self.dial(host, port, tls).await?;
        let resp = sender.send_request(req).await?;
        self.pool.park_when_ready(key, sender, ech);
        Ok((resp, ech))
    }

    async fn dial(
        &self,
        host: &str,
        port: u16,
        tls: bool,
    ) -> std::result::Result<(Sender, bool), BoxError> {
        let tcp = self.connect_tcp(host, port).await?;
        if !tls {
            return Ok((handshake(TokioIo::new(tcp)).await?, false));
        }

        if let Some(egress) = self.egress.as_ref().filter(|e| e.use_ech()) {
            if let Some(list) = egress.ech_config_list(host).await {
                match ech_connect(&list, host, tcp).await {
                    Ok((stream, ech_used)) => {
                        let io = TokioIo::new(IgnoreTlsCloseNotify(stream));
                        return Ok((handshake(io).await?, ech_used));
                    }
                    Err(e) => {
                        tracing::debug!(%host, error = %e, "ECH connect failed; retrying without ECH");
                        let tcp = self.connect_tcp(host, port).await?;
                        let stream = self.plain_tls_connect(host, tcp).await?;
                        let io = TokioIo::new(IgnoreTlsCloseNotify(stream));
                        return Ok((handshake(io).await?, false));
                    }
                }
            }
        }

        let stream = self.plain_tls_connect(host, tcp).await?;
        let io = TokioIo::new(IgnoreTlsCloseNotify(stream));
        Ok((handshake(io).await?, false))
    }

    pub(crate) async fn connect_tcp(
        &self,
        host: &str,
        port: u16,
    ) -> std::result::Result<TcpStream, BoxError> {
        let addrs = self.egress_addrs(host, port).await?;
        let tcp = match addrs {
            Some(addrs) => self.connect_first(host, port, &addrs).await?,
            None => self.connect_timed(host, (host, port)).await?,
        };
        tcp.set_nodelay(true)?;
        Ok(tcp)
    }

    async fn egress_addrs(
        &self,
        host: &str,
        port: u16,
    ) -> std::result::Result<Option<Vec<SocketAddr>>, BoxError> {
        let Some(egress) = self.egress.as_ref().filter(|e| e.resolver_only()) else {
            return Ok(None);
        };
        if host.parse::<std::net::IpAddr>().is_ok() {
            return Ok(None);
        }
        let ips = egress.resolve(host).await?;
        Ok(Some(ips.into_iter().map(|ip| SocketAddr::new(ip, port)).collect()))
    }

    async fn connect_first(
        &self,
        host: &str,
        port: u16,
        addrs: &[SocketAddr],
    ) -> std::result::Result<TcpStream, BoxError> {
        let mut last: BoxError = format!("no addresses resolved for {host}:{port}").into();
        for addr in addrs {
            match self.connect_timed(host, *addr).await {
                Ok(tcp) => return Ok(tcp),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    async fn connect_timed<A: tokio::net::ToSocketAddrs>(
        &self,
        host: &str,
        addr: A,
    ) -> std::result::Result<TcpStream, BoxError> {
        let connect = TcpStream::connect(addr);
        match self.connect_timeout {
            Some(d) => Ok(tokio::time::timeout(d, connect)
                .await
                .map_err(|_| format!("connecting to {host} timed out after {d:?}"))??),
            None => Ok(connect.await?),
        }
    }

    async fn plain_tls_connect(
        &self,
        host: &str,
        tcp: TcpStream,
    ) -> std::result::Result<tokio_rustls::client::TlsStream<TcpStream>, BoxError> {
        let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
            .map_err(|e| format!("bad server name: {e}"))?;
        Ok(self.tls.connect(server_name, tcp).await?)
    }

    pub async fn get_text(&self, url: &str) -> std::result::Result<String, String> {
        const MAX_BYTES: usize = 20 * 1024 * 1024;
        let bytes = self.get_bytes(url, MAX_BYTES).await?;
        String::from_utf8(bytes).map_err(|_| "list is not UTF-8".into())
    }

    pub async fn get_bytes(
        &self,
        url: &str,
        max_bytes: usize,
    ) -> std::result::Result<Vec<u8>, String> {
        const MAX_REDIRECTS: usize = 5;

        let mut url = url.to_string();
        for _ in 0..=MAX_REDIRECTS {
            let t = crate::net::target::HttpTarget::parse(&url)?;

            let req = Request::builder()
                .method(Method::GET)
                .uri(&t.path)
                .header(hyper::header::HOST, &t.host)
                .header(hyper::header::USER_AGENT, "proxy")
                .body(Full::new(Bytes::new()))
                .map_err(|e| e.to_string())?;

            let (resp, _ech) = self
                .send(req, &t.host, t.port, t.scheme == "https")
                .await
                .map_err(|e| format!("{}:{}: {e}", t.host, t.port))?;

            if resp.status().is_redirection() {
                let loc = resp
                    .headers()
                    .get(hyper::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or("redirect without Location")?;
                url = if loc.starts_with("http") {
                    loc.to_string()
                } else {
                    format!("{}://{}{loc}", t.scheme, t.host)
                };
                continue;
            }
            if !resp.status().is_success() {
                return Err(format!("HTTP {}", resp.status()));
            }
            let bytes = resp
                .into_body()
                .collect()
                .await
                .map_err(|e| e.to_string())?
                .to_bytes();
            if bytes.len() > max_bytes {
                return Err(format!("download larger than {} bytes", max_bytes));
            }
            return Ok(bytes.to_vec());
        }
        Err("too many redirects".into())
    }
}

struct IgnoreTlsCloseNotify<S>(S);

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for IgnoreTlsCloseNotify<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match Pin::new(&mut self.0).poll_read(cx, buf) {
            Poll::Ready(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for IgnoreTlsCloseNotify<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }
}

async fn handshake<S>(io: TokioIo<S>) -> std::result::Result<Sender, BoxError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (sender, conn) = hyper::client::conn::http1::handshake(io).await?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::debug!(error = %e, "upstream conn error");
        }
    });
    Ok(sender)
}

fn clone_request(req: &Request<Full<Bytes>>) -> Request<Full<Bytes>> {
    let mut out = Request::new(req.body().clone());
    *out.method_mut() = req.method().clone();
    *out.uri_mut() = req.uri().clone();
    *out.version_mut() = req.version();
    out.headers_mut().clone_from(req.headers());
    out
}

fn to_origin_form<B>(req: Request<B>) -> Request<B> {
    let (mut parts, body) = req.into_parts();
    if let Some(pq) = parts.uri.path_and_query().cloned() {
        if let Ok(uri) = hyper::Uri::builder().path_and_query(pq).build() {
            parts.uri = uri;
        }
    }
    Request::from_parts(parts, body)
}

pub fn ensure_crypto_provider() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
    });
}

pub fn default_client_config() -> Arc<rustls::ClientConfig> {
    use std::sync::OnceLock;
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            ensure_crypto_provider();
            let verifier = rustls::client::WebPkiServerVerifier::builder(root_store())
                .build()
                .expect("root store is non-empty (webpki bundle is compiled in)");
            let config = rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(IssuerLoggingVerifier {
                    inner: verifier,
                }))
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

fn root_store() -> Arc<rustls::RootCertStore> {
    use std::sync::OnceLock;
    static ROOTS: OnceLock<Arc<rustls::RootCertStore>> = OnceLock::new();
    ROOTS
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            let native = rustls_native_certs::load_native_certs();
            for e in &native.errors {
                tracing::warn!(error = %e, "reading a native root cert");
            }
            let (mut added, mut skipped) = (0usize, 0usize);
            for cert in native.certs {
                match roots.add(cert) {
                    Ok(()) => added += 1,
                    Err(_) => skipped += 1,
                }
            }
            let before = roots.len();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            tracing::info!(
                native_added = added,
                native_skipped = skipped,
                webpki_added = roots.len() - before,
                total = roots.len(),
                "upstream TLS root store built",
            );
            Arc::new(roots)
        })
        .clone()
}

async fn ech_connect(
    list: &[u8],
    host: &str,
    tcp: TcpStream,
) -> std::result::Result<(tokio_rustls::client::TlsStream<TcpStream>, bool), BoxError> {
    let config = ech_client_config(list)?;
    let connector = tokio_rustls::TlsConnector::from(config);
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| format!("bad server name: {e}"))?;
    let stream = connector.connect(server_name, tcp).await?;
    let accepted = matches!(
        stream.get_ref().1.ech_status(),
        rustls::client::EchStatus::Accepted
    );
    Ok((stream, accepted))
}

fn ech_client_config(list: &[u8]) -> std::result::Result<Arc<rustls::ClientConfig>, BoxError> {
    use rustls::client::{EchConfig, EchMode};
    use std::sync::OnceLock;

    static PROVIDER: OnceLock<Arc<rustls::crypto::CryptoProvider>> = OnceLock::new();
    let provider = PROVIDER
        .get_or_init(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .clone();

    let ech = EchConfig::new(
        rustls::pki_types::EchConfigListBytes::from(list.to_vec()),
        rustls::crypto::aws_lc_rs::hpke::ALL_SUPPORTED_SUITES,
    )
    .map_err(|e| format!("no usable ECH config: {e}"))?;

    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_ech(EchMode::Enable(ech))
        .map_err(|e| format!("enabling ECH: {e}"))?
        .with_root_certificates((*root_store()).clone())
        .with_no_client_auth();
    Ok(Arc::new(config))
}

#[derive(Debug)]
struct IssuerLoggingVerifier {
    inner: Arc<rustls::client::WebPkiServerVerifier>,
}

impl rustls::client::danger::ServerCertVerifier for IssuerLoggingVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        self.inner
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
            .map_err(|e| {
                let (subject, issuer) = match x509_parser::parse_x509_certificate(end_entity) {
                    Ok((_, cert)) => (cert.subject().to_string(), cert.issuer().to_string()),
                    Err(_) => ("<unparseable>".into(), "<unparseable>".into()),
                };
                tracing::warn!(
                    server = %server_name.to_str(),
                    %subject,
                    %issuer,
                    intermediates = intermediates.len(),
                    error = %e,
                    "upstream certificate rejected",
                );
                if issuer.contains("proxy Root CA") {
                    rustls::Error::General(format!(
                        "proxy loop: connection to {} came back to proxy itself \
                         (cert issued by {issuer}); a system/Docker proxy is routing \
                         our egress back through us",
                        server_name.to_str(),
                    ))
                } else {
                    e
                }
            })
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct FakeConn {
        id: u32,
        ech: bool,
        live_for: std::cell::Cell<u32>,
    }

    impl FakeConn {
        fn new(id: u32, ech: bool, live_for: u32) -> Self {
            Self { id, ech, live_for: std::cell::Cell::new(live_for) }
        }
    }

    impl Idle for FakeConn {
        fn is_live(&self) -> bool {
            let n = self.live_for.get();
            if n == 0 {
                return false;
            }
            self.live_for.set(n - 1);
            true
        }

        fn ech(&self) -> bool {
            self.ech
        }
    }

    fn key() -> PoolKey {
        ("h.example".into(), 443, true)
    }

    #[test]
    fn checkout_is_newest_first_and_only_ech_satisfies_an_ech_wish() {
        let pool: Pool<FakeConn> = Pool::new();
        pool.park(key(), FakeConn::new(1, false, u32::MAX));
        pool.park(key(), FakeConn::new(2, true, u32::MAX));
        pool.park(key(), FakeConn::new(3, false, u32::MAX));

        assert_eq!(pool.checkout(&key(), true).unwrap().id, 2);
        assert_eq!(pool.checkout(&key(), false).unwrap().id, 3);
        assert_eq!(pool.checkout(&key(), false).unwrap().id, 1);
        assert!(pool.checkout(&key(), false).is_none());
    }

    #[test]
    fn checkout_skips_corpses_and_recovers_from_the_scan_pick_race() {
        let pool: Pool<FakeConn> = Pool::new();
        pool.park(key(), FakeConn::new(1, false, u32::MAX));
        pool.park(key(), FakeConn::new(2, false, 0));
        pool.park(key(), FakeConn::new(3, false, 1));

        assert_eq!(pool.checkout(&key(), false).unwrap().id, 1);
        assert!(pool.checkout(&key(), false).is_none());
    }

    #[test]
    fn park_caps_idle_connections_per_host() {
        let pool: Pool<FakeConn> = Pool::new();
        for i in 0..2 * MAX_IDLE_PER_HOST as u32 {
            pool.park(key(), FakeConn::new(i, false, u32::MAX));
        }
        let mut count = 0;
        while pool.checkout(&key(), false).is_some() {
            count += 1;
        }
        assert_eq!(count, MAX_IDLE_PER_HOST);
    }

    #[tokio::test]
    async fn pooled_send_replays_when_the_server_closed_the_idle_connection() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf).await.unwrap();
            s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nhi").await.unwrap();
            let _ = s.read(&mut buf).await.unwrap();
            drop(s);
            let (mut s, _) = listener.accept().await.unwrap();
            let _ = s.read(&mut buf).await.unwrap();
            s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok").await.unwrap();
        });

        let client = HttpClient::new();
        let req = || {
            Request::builder()
                .method(Method::GET)
                .uri("/x")
                .header(hyper::header::HOST, "127.0.0.1")
                .body(Full::new(Bytes::new()))
                .unwrap()
        };
        let (resp, _) = client.send(req(), "127.0.0.1", addr.port(), false).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"hi");

        for _ in 0..200 {
            if client.pool.idle.lock().unwrap().values().any(|v| !v.is_empty()) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            client.pool.idle.lock().unwrap().values().any(|v| !v.is_empty()),
            "first connection was never pooled"
        );

        let (resp, _) = client.send(req(), "127.0.0.1", addr.port(), false).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"ok", "the request must be replayed on a fresh connection");
    }

    async fn one_shot_server(response: &'static [u8]) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf).await.unwrap();
            s.write_all(response).await.unwrap();
        });
        addr
    }

    #[tokio::test]
    async fn send_roundtrips_over_loopback() {
        let addr = one_shot_server(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nhi").await;
        let client = HttpClient::new();
        let req = Request::builder()
            .method(Method::GET)
            .uri(format!("http://{addr}/x"))
            .header(hyper::header::HOST, "127.0.0.1")
            .body(Full::new(Bytes::new()))
            .unwrap();
        let (resp, ech) = client.send(req, "127.0.0.1", addr.port(), false).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert!(!ech, "plain HTTP is never ECH");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"hi");
    }

    #[tokio::test]
    async fn get_text_follows_redirects() {
        let target =
            one_shot_server(b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nrules").await;
        let redirect = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let raddr = redirect.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = redirect.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf).await.unwrap();
            let resp = format!(
                "HTTP/1.1 302 Found\r\nlocation: http://{target}/list.txt\r\ncontent-length: 0\r\n\r\n"
            );
            s.write_all(resp.as_bytes()).await.unwrap();
        });
        let client = HttpClient::new();
        let text = client.get_text(&format!("http://{raddr}/")).await.unwrap();
        assert_eq!(text, "rules");
    }
}
