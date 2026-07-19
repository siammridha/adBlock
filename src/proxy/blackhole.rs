//! Remembers hosts whose DNS resolved to 0.0.0.0 (blackholed) so later
//! connects fail fast instead of timing out.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

pub(crate) const BLACKHOLE_TTL: Duration = Duration::from_secs(300);

fn is_dns_blackholed(addrs: &[SocketAddr]) -> bool {
    !addrs.is_empty() && addrs.iter().all(|a| a.ip().is_unspecified())
}

pub(crate) trait Resolver: Send + Sync {
    fn resolve(&self, host: &str, port: u16) -> BoxFuture<'static, std::io::Result<Vec<SocketAddr>>>;
}

pub(crate) struct EgressResolver(pub(crate) Arc<crate::proxy::egress::EgressPolicy>);

impl Resolver for EgressResolver {
    fn resolve(&self, host: &str, port: u16) -> BoxFuture<'static, std::io::Result<Vec<SocketAddr>>> {
        let egress = Arc::clone(&self.0);
        let host = host.to_string();
        Box::pin(async move {
            if egress.resolver_only() {
                let addrs = egress.resolve(&host).await?;
                return Ok(addrs.into_iter().map(|ip| SocketAddr::new(ip, port)).collect());
            }
            Ok(tokio::net::lookup_host((host.as_str(), port))
                .await?
                .collect())
        })
    }
}

pub(crate) struct BlackholeProbe {
    resolver: Arc<dyn Resolver>,
    cache: Mutex<HashMap<String, (bool, Instant)>>,
}

impl BlackholeProbe {
    pub(crate) fn new(resolver: Arc<dyn Resolver>) -> Self {
        Self {
            resolver,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) async fn is_blackholed(&self, host: &str, port: u16) -> bool {
        let key = format!("{host}:{port}");
        if let Some((v, at)) = self.cache.lock().expect("blackhole lock").get(&key) {
            if at.elapsed() < BLACKHOLE_TTL {
                return *v;
            }
        }
        let blackholed = match self.resolver.resolve(host, port).await {
            Ok(addrs) => is_dns_blackholed(&addrs),
            Err(_) => false,
        };
        self.cache
            .lock()
            .expect("blackhole lock")
            .insert(key, (blackholed, Instant::now()));
        blackholed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dns_blackhole_requires_all_addresses_unspecified() {
        let unspec4: SocketAddr = "0.0.0.0:443".parse().unwrap();
        let unspec6: SocketAddr = "[::]:443".parse().unwrap();
        let real: SocketAddr = "93.184.216.34:443".parse().unwrap();

        assert!(is_dns_blackholed(&[unspec4]));
        assert!(is_dns_blackholed(&[unspec4, unspec6]));
        assert!(!is_dns_blackholed(&[unspec4, real]));
        assert!(!is_dns_blackholed(&[]));
    }
}
