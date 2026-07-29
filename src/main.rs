//! Binary entry point: loads config, wires the services together, and serves
//! until shutdown.

// The crate is named `adBlock` (not snake_case) on purpose; silence the lint.
#![allow(non_snake_case)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use adBlock::adblock::api::{spawn_blocklist_updater, BlocklistFetcher, ScriptletUpdater};
use adBlock::dns::api::{DnsConfig, DnsRuntime, DnsService};
use adBlock::proxy::api::{
    CertAuthority, CertStore, EgressPolicy, ExclusionStore, HttpClient, InjectionPolicy, Proxy,
    ProxyBaseConfig, ProxyRuntime,
};
use adBlock::stats::api::{LoggingConfig, SharedState, StaticInfo};
use adBlock::web;
use adBlock::Result;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        std::env::set_var("RUST_BACKTRACE", "1");
    }

    let _ = rustls::crypto::ring::default_provider().install_default();

    // The root owns the on-disk data root and hands it to each module. Each
    // module builds its base config from its own built-in defaults; runtime
    // changes made in the admin UI persist to per-service files under the data
    // root and layer over these defaults.
    let data_dir = PathBuf::from("data");

    // Each module builds its own base config from its built-in defaults.
    let proxy_base = ProxyBaseConfig::load(&data_dir)?;
    let adblock_cfg = adBlock::adblock::api::AdblockConfig::load(&data_dir)?;
    let dns_cfg = DnsConfig::load(&data_dir)?;
    // Stats is loaded before init_logging, which uses its level.
    let logging_cfg = LoggingConfig::load(&data_dir)?;

    // `admin_listen` is wiring, not a Proxy concern: the root validates it, and
    // owns its one override. No module settings file holds it, so PROXY_ADMIN_LISTEN
    // is how a container binds the dashboard somewhere other than loopback.
    let admin_listen = std::env::var("PROXY_ADMIN_LISTEN")
        .unwrap_or_else(|_| proxy_base.server.admin_listen.clone());
    if !admin_listen.is_empty() {
        admin_listen.parse::<std::net::SocketAddr>().map_err(|e| {
            adBlock::Error::Config(format!("invalid admin_listen '{admin_listen}': {e}"))
        })?;
    }

    init_logging(&logging_cfg.level);

    // Each module owns and creates its own data directories; the entry point
    // only hands each one its base config and wires the results together.
    let (adblock, curation) = adBlock::adblock::api::from_config(&adblock_cfg)?;

    // The active CA (from the certificates tab) wins over the config CA; when
    // nothing is selected, active_paths() returns the config paths. Switching is
    // applied here at startup, so a change takes effect on the next run.
    let certs = Arc::new(CertStore::load(
        proxy_base.server.certs_dir(),
        proxy_base.server.active_ca_path(),
        proxy_base.tls.ca_cert.clone(),
        proxy_base.tls.ca_key.clone(),
    ));
    let (ca_cert_path, ca_key_path) = certs.active_paths();
    let ca = Arc::new(CertAuthority::load(&ca_cert_path, &ca_key_path)?);

    let exclusions = Arc::new(ExclusionStore::load(proxy_base.server.exclusions_path()));

    let info = StaticInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        listen: proxy_base.server.listen.clone(),
        admin_listen: admin_listen.clone(),
        started: Instant::now(),
    };
    let state =
        Arc::new(SharedState::new(info, &logging_cfg).with_data_dir(&logging_cfg.data_dir));

    tracing::info!(
        "starting proxy: adblock={} dns={}",
        adblock_cfg.enabled,
        dns_cfg.enabled
    );

    // The DNS service always exists so the proxy can resolve through it even
    // while the DNS listener is disabled; enable/disable only touches the
    // listener.
    let dns = DnsService::new(
        &dns_cfg,
        &dns_cfg.settings_dir(),
        adblock.clone(),
        state.clone(),
    )?;
    let egress = EgressPolicy::load(proxy_base.server.settings_path(), dns.clone());
    let injection = InjectionPolicy::load(proxy_base.server.settings_path());

    // Each module owns its outbound networking: the proxy's pooled client
    // dials through the egress policy, adblock fetches lists with its own
    // client.
    let client = Arc::new(
        HttpClient::with_extra_roots(&proxy_base.server.certs_dir())
            .with_connect_timeout(proxy_base.performance.upstream_timeout_ms)
            .with_egress(egress.clone()),
    );
    let fetch_client = Arc::new(
        adBlock::adblock::api::HttpClient::new()
            .with_connect_timeout(proxy_base.performance.upstream_timeout_ms),
    );
    let updater = Arc::new(ScriptletUpdater::ubo(fetch_client.clone()));
    let fetcher = Arc::new(BlocklistFetcher::new(curation.clone(), fetch_client));

    let proxy = Proxy::new(
        proxy_base.performance.max_inspect_bytes,
        adblock.clone(),
        exclusions.clone(),
        injection.clone(),
        ca,
        state.clone(),
        client,
        egress.clone(),
    );
    // Each service owns its lifecycle behind its settings interface. On first
    // run each writes its own settings file from built-in defaults; an existing
    // file is used as-is. Each module names its own files (via its config).
    let proxy_runtime = ProxyRuntime::new(
        state.clone(),
        proxy_base.server.server_settings_path(),
        Some(std::sync::Arc::new(proxy)),
        &proxy_base.server.listen,
        proxy_base.server.enabled,
    )?;
    let dns_runtime = DnsRuntime::new(
        state.clone(),
        dns_cfg.server_settings_path(),
        dns,
        &dns_cfg.listen,
        dns_cfg.enabled,
    )?;

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
            injection.clone(),
            certs.clone(),
        );
        tokio::spawn(async move {
            if let Err(e) = admin.serve(addr).await {
                tracing::error!(error = %e, "admin web UI stopped");
            }
        });
    }

    spawn_blocklist_updater(
        state.clone(),
        curation.clone(),
        fetcher,
        updater,
        adblock_cfg.auto_update_hours,
    );

    proxy_runtime.start_initial().await?;
    dns_runtime.start_initial().await?;
    std::future::pending::<()>().await;
    Ok(())
}

fn init_logging(level: &str) {
    use tracing_subscriber::{fmt, EnvFilter};
    // Show every log our own code emits, always — nothing is hidden by level.
    // `CARGO_CRATE_NAME` is our crate, so `<crate>=trace` turns on all of it and
    // survives any crate rename. Third-party crates stay at the configured level
    // (default info) so their internals don't bury ours. RUST_LOG still wins.
    let base = if level.trim().is_empty() { "info" } else { level.trim() };
    let ours = env!("CARGO_CRATE_NAME");
    let directives = format!("{base},{ours}=trace");
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&directives))
        .unwrap_or_else(|_| EnvFilter::new(format!("info,{ours}=trace")));
    let _ = fmt().with_env_filter(filter).try_init();
}
