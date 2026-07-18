//! Background jobs: periodic blocklist refresh and scriptlet updates.

use std::sync::Arc;
use std::time::Duration;

use crate::adblock::updater::ScriptletUpdater;
use crate::adblock::{ListCuration, ListEntry};
use crate::support::error::Error;
use crate::net::http_client::HttpClient;
use crate::stats::{EventKind, SharedState};

type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

pub fn event_list_change(state: &SharedState, entry: &ListEntry, verb: &str) {
    state.log_event(
        EventKind::Info,
        format!(
            "blocklist {verb}: {} ({} rules) from {}",
            entry.name, entry.rules, entry.source
        ),
    );
}

pub fn event_scriptlets(state: &SharedState, count: usize, verb: &str) {
    state.log_event(
        EventKind::Info,
        format!("scriptlet library {verb}: {count} resources"),
    );
}

pub trait Downloader: Send + Sync {
    fn fetch_text(&self, url: &str) -> BoxFuture<'_, std::result::Result<String, String>>;
}

impl Downloader for HttpClient {
    fn fetch_text(&self, url: &str) -> BoxFuture<'_, std::result::Result<String, String>> {
        let url = url.to_string();
        Box::pin(async move { self.get_text(&url).await })
    }
}

#[derive(Debug)]
pub enum RefreshError {
    Fetch { url: String, error: String },
    Install(Error),
    Internal(String),
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefreshError::Fetch { url, error } => write!(f, "fetching {url}: {error}"),
            RefreshError::Install(e) => write!(f, "{e}"),
            RefreshError::Internal(e) => write!(f, "{e}"),
        }
    }
}

pub struct BlocklistFetcher {
    curation: Arc<ListCuration>,
    downloader: Arc<dyn Downloader>,
}

impl BlocklistFetcher {
    pub fn new(curation: Arc<ListCuration>, downloader: Arc<dyn Downloader>) -> Self {
        Self { curation, downloader }
    }

    pub async fn install_from_url(
        &self,
        state: &SharedState,
        given: &str,
        verb: &str,
    ) -> std::result::Result<ListEntry, RefreshError> {
        let url = crate::adblock::normalize_list_url(given);
        let text = self
            .downloader
            .fetch_text(&url)
            .await
            .map_err(|error| RefreshError::Fetch { url: url.clone(), error })?;
        let curation = self.curation.clone();
        let given = given.to_string();
        let entry =
            tokio::task::spawn_blocking(move || curation.install_downloaded(&given, &url, text))
                .await
                .map_err(|e| RefreshError::Internal(e.to_string()))?
                .map_err(RefreshError::Install)?;
        event_list_change(state, &entry, verb);
        Ok(entry)
    }
}

pub fn spawn_blocklist_updater(
    state: Arc<SharedState>,
    curation: Arc<ListCuration>,
    fetcher: Arc<BlocklistFetcher>,
    updater: Arc<ScriptletUpdater>,
    hours: u64,
) {
    if hours == 0 {
        return;
    }
    let max_age = Duration::from_secs(hours * 3600);
    let check_every = Duration::from_secs(3600.min(hours * 3600));
    tokio::spawn(async move {
        loop {
            for (name, url) in curation.stale_url_lists(max_age) {
                if let Err(e) = fetcher.install_from_url(&state, &url, "auto-updated").await {
                    state.log_event(
                        EventKind::Error,
                        format!("blocklist auto-update {name}: {e}"),
                    );
                }
            }
            if updater.file_stale(&curation, max_age) {
                match updater.refresh(&curation).await {
                    Ok(count) => event_scriptlets(&state, count, "auto-updated from uBO master"),
                    Err(e) => state
                        .log_event(EventKind::Error, format!("scriptlet library auto-update: {e}")),
                }
            }
            tokio::time::sleep(check_every).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adblock::MemoryListStore;
    use crate::support::config::{AdblockConfig, LoggingConfig};
    use crate::stats::StaticInfo;

    struct CannedDownloader(std::result::Result<&'static str, &'static str>);

    impl Downloader for CannedDownloader {
        fn fetch_text(&self, _url: &str) -> BoxFuture<'_, std::result::Result<String, String>> {
            let out = self.0.map(str::to_string).map_err(str::to_string);
            Box::pin(async move { out })
        }
    }

    fn curation() -> Arc<ListCuration> {
        let cfg = AdblockConfig {
            enabled: true,
            custom_rules: Vec::new(),
            data_dir: std::path::PathBuf::from("/nonexistent-for-tests"),
            auto_update_hours: 0,
            inject_scriptlets: false,
            scriptlet_resources: std::path::PathBuf::new(),
        };
        let (_adblock, curation) =
            crate::adblock::with_store(&cfg, Arc::new(MemoryListStore::new())).unwrap();
        curation
    }

    fn state() -> Arc<SharedState> {
        Arc::new(SharedState::new(
            StaticInfo {
                version: "test".into(),
                listen: String::new(),
                admin_listen: String::new(),
                ca_pem: String::new(),
                started: std::time::Instant::now(),
            },
            &LoggingConfig {
                level: "info".into(),
                log_actions: true,
                log_requests: true,
            },
        ))
    }

    #[tokio::test]
    async fn install_from_url_installs_and_reports() {
        let curation = curation();
        let fetcher = BlocklistFetcher::new(
            curation.clone(),
            Arc::new(CannedDownloader(Ok("! Title: Canned List\n||ads.example^\n"))),
        );
        let state = state();
        let mut obs = state.observe();

        let entry = fetcher
            .install_from_url(&state, "https://x.example/list.txt", "added")
            .await
            .unwrap();
        assert_eq!(entry.name, "Canned-List");
        assert_eq!(entry.rules, 1);
        assert!(curation.lists().iter().any(|l| l.name == "Canned-List"));
        assert!(obs.events().iter().any(|e| {
            e.kind == EventKind::Info && e.message == "blocklist added: Canned-List (1 rules) from https://x.example/list.txt"
        }));
    }

    #[tokio::test]
    async fn fetch_failure_is_typed_and_nothing_is_installed() {
        let curation = curation();
        let fetcher = BlocklistFetcher::new(
            curation.clone(),
            Arc::new(CannedDownloader(Err("connect refused (canned)"))),
        );
        let state = state();

        let err = fetcher
            .install_from_url(&state, "https://x.example/list.txt", "added")
            .await
            .unwrap_err();
        assert!(matches!(err, RefreshError::Fetch { .. }), "got: {err}");
        assert!(err.to_string().contains("fetching https://x.example/list.txt"));
        assert!(!curation.lists().iter().any(|l| l.source.contains("x.example")));
    }

    #[tokio::test]
    async fn rejected_list_is_an_install_error() {
        let fetcher = BlocklistFetcher::new(
            curation(),
            Arc::new(CannedDownloader(Ok("<html>not a list</html>"))),
        );
        let err = fetcher
            .install_from_url(&state(), "https://x.example/list.txt", "added")
            .await
            .unwrap_err();
        assert!(matches!(err, RefreshError::Install(_)), "got: {err}");
    }
}
