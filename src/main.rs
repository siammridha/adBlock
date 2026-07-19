//! Binary entry point: loads config, wires the services together, and serves
//! until shutdown.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use proxy::adblock::updater::ScriptletUpdater;
use proxy::Config;
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

    // Each module owns and creates its own data directories; the entry point
    // only hands each one its config section and wires the results together.
    let (adblock, curation) = proxy::adblock::from_config(&config.adblock)?;

    // The active CA (from the certificates tab) wins over the config CA; when
    // nothing is selected, active_paths() returns the config paths. Switching is
    // applied here at startup, so a change takes effect on the next run.
    let certs = Arc::new(CertStore::load(
        config.server.certs_dir(),
        config.server.active_ca_path(),
        config.tls.ca_cert.clone(),
        config.tls.ca_key.clone(),
    ));
    let (ca_cert_path, ca_key_path) = certs.active_paths();
    let ca = Arc::new(CertAuthority::load(&ca_cert_path, &ca_key_path)?);

    let exclusions = Arc::new(ExclusionStore::load(config.server.exclusions_path()));

    let info = StaticInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        listen: config.server.listen.clone(),
        admin_listen: config.server.admin_listen.clone(),
        started: Instant::now(),
    };
    let state =
        Arc::new(SharedState::new(info, &config.logging).with_data_dir(&config.logging.data_dir));

    tracing::info!(
        "starting proxy: adblock={} dns={}",
        config.adblock.enabled,
        config.dns.enabled
    );

    // The DNS service always exists so the proxy can resolve through it even
    // while the DNS listener is disabled; enable/disable only touches the
    // listener.
    let dns = DnsService::new(
        &config.dns,
        &config.dns.settings_dir(),
        adblock.clone(),
        state.clone(),
    )?;
    let egress = EgressPolicy::load(config.server.egress_settings_path(), dns.clone());

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
        config.performance.max_inspect_bytes,
        adblock.clone(),
        exclusions.clone(),
        ca,
        state.clone(),
        client,
        egress.clone(),
    );
    // Each service owns its lifecycle behind its settings interface; the old
    // combined server-settings.json seeds them once if their own files are
    // missing. Each module names its own files (via its config).
    let proxy_runtime = ProxyRuntime::new(
        state.clone(),
        config.server.server_settings_path(),
        Some(config.server.legacy_server_settings_path()),
        Some(std::sync::Arc::new(proxy)),
        &config.server.listen,
        config.server.enabled,
    )?;
    let dns_runtime = DnsRuntime::new(
        state.clone(),
        config.dns.server_settings_path(),
        Some(config.dns.legacy_server_settings_path()),
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
