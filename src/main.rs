//! Binary entry point: loads config, wires the services together, and serves
//! until shutdown.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use proxy::adblock::updater::ScriptletUpdater;
use proxy::support::config::Config;
use proxy::dns::DnsService;
use proxy::proxy::egress::EgressPolicy;
use proxy::proxy::exclusions::ExclusionStore;
use proxy::proxy::http_client::HttpClient;
use proxy::adblock::maintenance::{self, BlocklistFetcher};
use proxy::proxy::ca::CertAuthority;
use proxy::proxy::certs::CertStore;
use proxy::dns::control::DnsRuntime;
use proxy::proxy::control::ProxyRuntime;
use proxy::proxy::Proxy;
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

    // Everything the proxy persists lives under one data root, split into
    // blocklists/, settings/, logs/, scriptlets/, and certs/. Create them up
    // front so first writes (and the log-writer threads) always have a home.
    let settings_dir = config.adblock.settings_dir();
    for dir in [
        config.adblock.blocklists_dir(),
        settings_dir.clone(),
        config.adblock.logs_dir(),
        config.adblock.scriptlets_dir(),
        config.adblock.certs_dir(),
    ] {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("creating {}: {e}", dir.display());
        }
    }

    let (adblock, curation) = proxy::adblock::from_config(&config.adblock)?;

    // The active CA (from the certificates tab) wins over the config CA; when
    // nothing is selected, active_paths() returns the config paths. Switching is
    // applied here at startup, so a change takes effect on the next run.
    let certs = Arc::new(CertStore::load(
        config.adblock.certs_dir(),
        settings_dir.join("active-ca.json"),
        config.tls.ca_cert.clone(),
        config.tls.ca_key.clone(),
    ));
    let (ca_cert_path, ca_key_path) = certs.active_paths();
    let ca = Arc::new(CertAuthority::load(&ca_cert_path, &ca_key_path)?);

    let exclusions = Arc::new(ExclusionStore::load(
        settings_dir.join("excluded-domains.conf"),
    ));

    let info = StaticInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        listen: config.server.listen.clone(),
        admin_listen: config.server.admin_listen.clone(),
        ca_pem: ca.root_pem().to_string(),
        started: Instant::now(),
    };
    let state =
        Arc::new(SharedState::new(info, &config.logging).with_data_dir(&config.adblock.data_dir));

    tracing::info!(
        "starting proxy: adblock={} dns={}",
        config.adblock.enabled,
        config.dns.enabled
    );

    // The DNS service always exists so the proxy can resolve through it even
    // while the DNS listener is disabled; enable/disable only touches the
    // listener.
    let dns = DnsService::new(&config.dns, &settings_dir, adblock.clone(), state.clone())?;
    let egress = EgressPolicy::load(
        settings_dir.join("proxy-settings.json"),
        dns.clone(),
    );

    // Each module owns its outbound networking: the proxy's pooled client
    // dials through the egress policy, adblock fetches lists with its own
    // client.
    let client = Arc::new(
        HttpClient::new()
            .with_connect_timeout(config.performance.upstream_timeout_ms)
            .with_egress(egress.clone()),
    );
    let fetch_client = Arc::new(
        proxy::adblock::fetch::HttpClient::new()
            .with_connect_timeout(config.performance.upstream_timeout_ms),
    );
    let updater = Arc::new(ScriptletUpdater::ubo(fetch_client.clone()));
    let fetcher = Arc::new(BlocklistFetcher::new(curation.clone(), fetch_client));

    let proxy = Proxy::new(
        config.clone(),
        adblock.clone(),
        exclusions.clone(),
        ca,
        state.clone(),
        client,
        egress.clone(),
    );
    // Each service owns its lifecycle behind its settings interface; the old
    // combined server-settings.json seeds them once if their own files are
    // missing.
    let legacy_settings = settings_dir.join("server-settings.json");
    let proxy_runtime = ProxyRuntime::new(
        state.clone(),
        settings_dir.join("proxy-server.json"),
        Some(legacy_settings.clone()),
        Some(std::sync::Arc::new(proxy)),
        &config.server.listen,
        config.server.enabled,
    )?;
    let dns_runtime = DnsRuntime::new(
        state.clone(),
        settings_dir.join("dns-server.json"),
        Some(legacy_settings),
        dns,
        &config.dns.listen,
        config.dns.enabled,
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
            proxy_runtime.clone(),
            dns_runtime.clone(),
            egress.clone(),
            certs.clone(),
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

    proxy_runtime.start_initial().await?;
    dns_runtime.start_initial().await?;
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
