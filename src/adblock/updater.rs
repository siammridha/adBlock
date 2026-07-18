//! Rebuilds the scriptlet resource file from the uBlock Origin source tarball.

use std::sync::Arc;
use std::time::Duration;

use crate::net::http_client::HttpClient;

use super::ListCuration;

type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

pub const UBO_TARBALL_PAGE: &str = "https://github.com/gorhill/uBlock";
const UBO_TARBALL_URL: &str =
    "https://github.com/gorhill/uBlock/archive/refs/heads/master.tar.gz";

pub trait ScriptletSource: Send + Sync {
    fn fetch(&self) -> BoxFuture<'_, std::result::Result<Vec<u8>, String>>;
}

pub struct UboShellSource {
    client: Arc<HttpClient>,
}

impl ScriptletSource for UboShellSource {
    fn fetch(&self) -> BoxFuture<'_, std::result::Result<Vec<u8>, String>> {
        Box::pin(async move {
            let tmp = std::env::temp_dir().join("proxy-ubo-update");
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            tokio::fs::create_dir_all(&tmp)
                .await
                .map_err(|e| format!("creating {}: {e}", tmp.display()))?;

            let tarball = self
                .client
                .get_bytes(UBO_TARBALL_URL, 128 * 1024 * 1024)
                .await
                .map_err(|e| format!("download uBO: {e}"))?;
            let tar_path = tmp.join("ubo.tar.gz");
            tokio::fs::write(&tar_path, &tarball)
                .await
                .map_err(|e| format!("writing {}: {e}", tar_path.display()))?;
            run_step(
                "extract uBO",
                "tar",
                &[
                    "xzf",
                    &tar_path.to_string_lossy(),
                    "-C",
                    &tmp.to_string_lossy(),
                ],
            )
            .await?;

            let checkout = tmp.join("uBlock-master");
            let tmp_out = tmp.join("scriptlets.json");
            run_step(
                "convert scriptlets",
                &js_runtime(),
                &[
                    "tools/convert-ubo-scriptlets.mjs",
                    &checkout.to_string_lossy(),
                    &tmp_out.to_string_lossy(),
                ],
            )
            .await?;

            let bytes = tokio::fs::read(&tmp_out)
                .await
                .map_err(|e| format!("reading converter output: {e}"))?;
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            Ok(bytes)
        })
    }
}

fn js_runtime() -> String {
    if let Ok(bin) = std::env::var("PROXY_JS_RUNTIME") {
        if !bin.trim().is_empty() {
            return bin;
        }
    }
    if on_path("llrt") {
        return "llrt".into();
    }
    "node".into()
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .any(|dir| std::fs::metadata(dir.join(bin)).map(|m| m.is_file()).unwrap_or(false))
        })
        .unwrap_or(false)
}

async fn run_step(what: &str, cmd: &str, args: &[&str]) -> std::result::Result<(), String> {
    let out = tokio::process::Command::new(cmd)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("{what}: spawning {cmd}: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("{what}: {cmd} failed: {}", err.trim()));
    }
    Ok(())
}

pub struct ScriptletUpdater {
    source: Box<dyn ScriptletSource>,
}

impl ScriptletUpdater {
    pub fn ubo(client: Arc<HttpClient>) -> Self {
        Self::with_source(Box::new(UboShellSource { client }))
    }

    pub fn with_source(source: Box<dyn ScriptletSource>) -> Self {
        Self { source }
    }

    pub fn file_stale(&self, curation: &ListCuration, max_age: Duration) -> bool {
        let path = curation.scriptlets().path();
        if path.as_os_str().is_empty() {
            return false;
        }
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > max_age)
    }

    pub async fn refresh(&self, curation: &Arc<ListCuration>) -> std::result::Result<usize, String> {
        let out_path = curation.scriptlets().path().to_path_buf();
        if out_path.as_os_str().is_empty() {
            return Err("adblock.scriptlet_resources is not configured".into());
        }
        let bytes = self.source.fetch().await?;
        if bytes.len() < 10_000 {
            return Err(format!(
                "converter output suspiciously small ({} bytes)",
                bytes.len()
            ));
        }
        tokio::fs::write(&out_path, &bytes)
            .await
            .map_err(|e| format!("writing {}: {e}", out_path.display()))?;

        let c = curation.clone();
        tokio::task::spawn_blocking(move || c.reload_scriptlet_resources())
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adblock::MemoryListStore;
    use crate::support::config::AdblockConfig;
    use base64::{engine::general_purpose::STANDARD, Engine as _};

    struct CannedSource(std::result::Result<Vec<u8>, String>);

    impl ScriptletSource for CannedSource {
        fn fetch(&self) -> BoxFuture<'_, std::result::Result<Vec<u8>, String>> {
            let out = self.0.clone();
            Box::pin(async move { out })
        }
    }

    fn canned_resources(name: &str) -> Vec<u8> {
        let js = format!("function cannedFn(){{}} // {}", " ".repeat(12_000));
        serde_json::json!([{
            "name": name,
            "kind": {"mime": "application/javascript"},
            "content": STANDARD.encode(js),
        }])
        .to_string()
        .into_bytes()
    }

    fn curation_with_resource_file(dir: &std::path::Path) -> Arc<ListCuration> {
        let res_path = dir.join("resources.json");
        std::fs::write(&res_path, canned_resources("initial.js")).unwrap();
        let cfg = AdblockConfig {
            enabled: true,
            custom_rules: Vec::new(),
            data_dir: std::path::PathBuf::from("/nonexistent-for-tests"),
            auto_update_hours: 0,
            inject_scriptlets: true,
            scriptlet_resources: res_path,
        };
        super::super::with_store(&cfg, Arc::new(MemoryListStore::new())).unwrap().1
    }

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sp-updater-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn refresh_writes_the_file_and_reloads_the_library() {
        let dir = temp_dir();
        let curation = curation_with_resource_file(&dir);
        let updater =
            ScriptletUpdater::with_source(Box::new(CannedSource(Ok(canned_resources("fresh.js")))));

        let count = updater.refresh(&curation).await.unwrap();
        assert_eq!(count, 1);
        let on_disk = std::fs::read_to_string(curation.scriptlets().path()).unwrap();
        assert!(on_disk.contains("fresh.js"));
        let names: Vec<String> = curation.scriptlets().library().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["fresh.js"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn refresh_rejects_suspiciously_small_output() {
        let dir = temp_dir();
        let curation = curation_with_resource_file(&dir);
        let updater = ScriptletUpdater::with_source(Box::new(CannedSource(Ok(b"[]".to_vec()))));

        let err = updater.refresh(&curation).await.unwrap_err();
        assert!(err.contains("suspiciously small"), "err: {err}");
        let on_disk = std::fs::read_to_string(curation.scriptlets().path()).unwrap();
        assert!(on_disk.contains("initial.js"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn refresh_requires_a_configured_resource_path() {
        let cfg = AdblockConfig {
            enabled: true,
            custom_rules: Vec::new(),
            data_dir: std::path::PathBuf::from("/nonexistent-for-tests"),
            auto_update_hours: 0,
            inject_scriptlets: false,
            scriptlet_resources: std::path::PathBuf::new(),
        };
        let curation =
            super::super::with_store(&cfg, Arc::new(MemoryListStore::new())).unwrap().1;
        let updater =
            ScriptletUpdater::with_source(Box::new(CannedSource(Ok(canned_resources("x.js")))));
        assert!(updater.refresh(&curation).await.unwrap_err().contains("not configured"));
    }

    #[test]
    fn staleness_needs_an_existing_file() {
        let dir = temp_dir();
        let curation = curation_with_resource_file(&dir);
        let updater = ScriptletUpdater::with_source(Box::new(CannedSource(Ok(Vec::new()))));
        assert!(!updater.file_stale(&curation, Duration::from_secs(3600)));
        assert!(updater.file_stale(&curation, Duration::ZERO));
        std::fs::remove_dir_all(dir).ok();
    }
}
