//! Binary entry point: loads config, wires the services together, and serves
//! until shutdown.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use std::sync::RwLock;

use proxy::adblock::updater::ScriptletUpdater;
use proxy::support::config::Config;
use proxy::net::egress::{DnsSlot, EgressPolicy};
use proxy::proxy::exclusions::ExclusionStore;
use proxy::net::http_client::HttpClient;
use proxy::adblock::maintenance::{self, BlocklistFetcher};
use proxy::proxy::ca::CertAuthority;
use proxy::proxy::Proxy;
use proxy::web::runtime::Runtime;
use proxy::stats::{SharedState, StaticInfo};
use proxy::web;
use proxy::Result;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        std::env::set_var("RUST_BACKTRACE", "1");
    }

    let _ = rustls::crypto::ring::default_provider().install_default();

    let config_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.toml"));

    let config = if config_path.exists() {
        Config::load(&config_path)?
    } else {
        eprintln!("no config at {}, using defaults", config_path.display());
        Config::default()
    };

    init_logging(&config.logging.level);

    let (adblock, curation) = proxy::adblock::from_config(&config.adblock)?;
    let ca = Arc::new(CertAuthority::load(&config.tls.ca_cert, &config.tls.ca_key)?);

    let exclusions = Arc::new(ExclusionStore::load(
        config.adblock.lists_dir.join("excluded-domains.conf"),
    ));

    let info = StaticInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        listen: config.server.listen.clone(),
        admin_listen: config.server.admin_listen.clone(),
        ca_pem: ca.root_pem().to_string(),
        started: Instant::now(),
    };
    let state =
        Arc::new(SharedState::new(info, &config.logging).with_data_dir(&config.adblock.lists_dir));

    tracing::info!(
        "starting proxy: adblock={} dns={}",
        config.adblock.enabled,
        config.dns.enabled
    );

    let dns_slot: DnsSlot = Arc::new(RwLock::new(None));
    let egress = EgressPolicy::load(
        config.adblock.lists_dir.join("proxy-settings.json"),
        dns_slot.clone(),
    );

    let client = Arc::new(
        HttpClient::new()
            .with_connect_timeout(config.performance.upstream_timeout_ms)
            .with_egress(egress.clone()),
    );
    let updater = Arc::new(ScriptletUpdater::ubo(client.clone()));
    let fetcher = Arc::new(BlocklistFetcher::new(curation.clone(), client.clone()));

    let proxy = Proxy::new(
        config.clone(),
        adblock.clone(),
        exclusions.clone(),
        ca,
        state.clone(),
        client,
        egress.clone(),
    );
    let runtime = Runtime::new(
        state.clone(),
        config.adblock.lists_dir.join("server-settings.json"),
        Some(std::sync::Arc::new(proxy)),
        &config.server.listen,
        config.server.enabled,
        config.dns.clone(),
        config.adblock.lists_dir.clone(),
        adblock.clone(),
        dns_slot,
    )?;

    let admin_listen = config.server.admin_listen.clone();
    if !admin_listen.is_empty() {
        let addr = web::parse_addr(&admin_listen)?;
        let admin = web::Admin::new(
            state.clone(),
            adblock.clone(),
            curation.clone(),
            exclusions.clone(),
            updater.clone(),
            fetcher.clone(),
            runtime.clone(),
            egress.clone(),
        );
        tokio::spawn(async move {
            if let Err(e) = admin.serve(addr).await {
                tracing::error!(error = %e, "admin web UI stopped");
            }
        });
    }

    maintenance::spawn_blocklist_updater(
        state.clone(),
        curation.clone(),
        fetcher,
        updater,
        config.adblock.auto_update_hours,
    );

    runtime.start_initial().await?;
    std::future::pending::<()>().await;
    Ok(())
}

fn init_logging(level: &str) {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(level))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).try_init();
}
