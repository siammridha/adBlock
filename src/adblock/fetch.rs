//! Adblock's own HTTP fetcher for blocklists, scriptlets, and other remote
//! resources. One connection per download, plain TLS, redirect-following.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Method, Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

const DEFAULT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Clone)]
pub struct HttpClient {
    tls: tokio_rustls::TlsConnector,
    connect_timeout: Option<std::time::Duration>,
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpClient {
    pub fn new() -> Self {
        Self {
            tls: tokio_rustls::TlsConnector::from(client_config()),
            connect_timeout: Some(DEFAULT_CONNECT_TIMEOUT),
        }
    }

    pub fn with_connect_timeout(mut self, millis: u64) -> Self {
        self.connect_timeout =
            (millis > 0).then(|| std::time::Duration::from_millis(millis));
        self
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
            let t = FetchTarget::parse(&url)?;
            let resp = self
                .get(&t)
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

    async fn get(
        &self,
        t: &FetchTarget,
    ) -> std::result::Result<Response<Incoming>, Box<dyn std::error::Error + Send + Sync>> {
        let req = Request::builder()
            .method(Method::GET)
            .uri(&t.path)
            .header(hyper::header::HOST, &t.host)
            .header(hyper::header::USER_AGENT, "proxy")
            .body(Full::new(Bytes::new()))?;

        let connect = TcpStream::connect((t.host.as_str(), t.port));
        let tcp = match self.connect_timeout {
            Some(d) => tokio::time::timeout(d, connect)
                .await
                .map_err(|_| format!("connecting to {} timed out after {d:?}", t.host))??,
            None => connect.await?,
        };
        tcp.set_nodelay(true)?;

        let mut sender = if t.scheme == "https" {
            let server_name = rustls::pki_types::ServerName::try_from(t.host.clone())
                .map_err(|e| format!("bad server name: {e}"))?;
            let stream = self.tls.connect(server_name, tcp).await?;
            handshake(TokioIo::new(stream)).await?
        } else {
            handshake(TokioIo::new(tcp)).await?
        };
        Ok(sender.send_request(req).await?)
    }
}

async fn handshake<S>(
    io: TokioIo<S>,
) -> std::result::Result<
    hyper::client::conn::http1::SendRequest<Full<Bytes>>,
    Box<dyn std::error::Error + Send + Sync>,
>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (sender, conn) = hyper::client::conn::http1::handshake(io).await?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::debug!(error = %e, "blocklist download conn error");
        }
    });
    Ok(sender)
}

struct FetchTarget {
    scheme: String,
    host: String,
    port: u16,
    path: String,
}

impl FetchTarget {
    fn parse(url: &str) -> Result<Self, String> {
        let uri: hyper::Uri = url.parse().map_err(|e| format!("bad url: {e}"))?;
        let scheme = uri.scheme_str().unwrap_or("").to_string();
        if scheme != "http" && scheme != "https" {
            return Err(format!("unsupported scheme '{scheme}'"));
        }
        let host = uri.host().ok_or("url has no host")?.to_string();
        let default_port = if scheme == "https" { 443 } else { 80 };
        let port = uri.port_u16().unwrap_or(default_port);
        let path = uri
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/")
            .to_string();
        Ok(Self { scheme, host, port, path })
    }
}

fn client_config() -> Arc<rustls::ClientConfig> {
    use std::sync::OnceLock;
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            if rustls::crypto::CryptoProvider::get_default().is_none() {
                let _ = rustls::crypto::ring::default_provider().install_default();
            }
            let mut roots = rustls::RootCertStore::empty();
            let native = rustls_native_certs::load_native_certs();
            for e in &native.errors {
                tracing::warn!(error = %e, "reading a native root cert");
            }
            for cert in native.certs {
                let _ = roots.add(cert);
            }
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    async fn get_text_fetches_over_loopback() {
        let addr = one_shot_server(b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nrules").await;
        let client = HttpClient::new();
        let text = client.get_text(&format!("http://{addr}/list.txt")).await.unwrap();
        assert_eq!(text, "rules");
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

    #[tokio::test]
    async fn errors_are_reported_not_panicked() {
        let client = HttpClient::new();
        assert!(client.get_text("ftp://x/").await.is_err());
        let addr = one_shot_server(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n").await;
        let err = client.get_text(&format!("http://{addr}/")).await.unwrap_err();
        assert!(err.contains("404"), "err: {err}");
    }
}
