//! uBlock Origin scriptlet resources: loading them from disk and picking the
//! injections for a given site.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use adblock::resources::Resource;

use crate::adblock::AdblockConfig;
use crate::adblock::error::{Error, Result};

pub struct ScriptletInjection {
    pub js: String,
    pub names: Vec<String>,
}

pub struct ScriptletInfo {
    pub name: String,
    pub aliases: Vec<String>,
    pub injectable: bool,
    pub bytes: usize,
}

pub struct ScriptletLibrary {
    inject: bool,
    path: PathBuf,
    resources: RwLock<Arc<Vec<Resource>>>,
    fn_to_scriptlet: RwLock<HashMap<String, String>>,
}

impl ScriptletLibrary {
    // The resource file holds both scriptlets and the `$redirect` stand-in
    // bodies, and redirects apply whether or not scriptlet injection is on. So
    // load the file whenever there is one; `inject` only gates injection.
    pub fn from_config(cfg: &AdblockConfig) -> Result<Self> {
        let resources = if cfg.scriptlet_resources.as_os_str().is_empty() {
            Vec::new()
        } else if cfg.scriptlet_resources.exists() {
            let resources = load_resources(&cfg.scriptlet_resources)?;
            tracing::info!(count = resources.len(), "scriptlet resources loaded");
            resources
        } else {
            if cfg.inject_scriptlets {
                tracing::warn!(
                    path = %cfg.scriptlet_resources.display(),
                    "scriptlet injection is on but the resource file is missing — no \
                     scriptlets loaded; use the admin UI's \"Update from uBO\" button"
                );
            }
            Vec::new()
        };
        Ok(Self {
            inject: cfg.inject_scriptlets,
            path: cfg.scriptlet_resources.clone(),
            fn_to_scriptlet: RwLock::new(build_fn_map(&resources)),
            resources: RwLock::new(Arc::new(resources)),
        })
    }

    pub fn enabled(&self) -> bool {
        self.inject && !self.resources.read().expect("resources lock").is_empty()
    }

    pub fn resources(&self) -> Arc<Vec<Resource>> {
        self.resources.read().expect("resources lock").clone()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// When the resource file was last written, in epoch milliseconds. `None`
    /// when there is no file yet. Adblock owns the file, so Adblock reads it —
    /// callers never go to disk on its behalf.
    pub fn updated_ms(&self) -> Option<u64> {
        std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
    }

    pub fn reload_from_disk(&self) -> Result<usize> {
        let resources = load_resources(&self.path)?;
        if resources.is_empty() {
            return Err(Error::Config("scriptlet resource file is empty".into()));
        }
        let count = resources.len();
        *self.fn_to_scriptlet.write().expect("fn map lock") = build_fn_map(&resources);
        *self.resources.write().expect("resources lock") = Arc::new(resources);
        Ok(count)
    }

    pub fn names_in(&self, injected_js: &str) -> Vec<String> {
        let mut out = Vec::new();
        let map = self.fn_to_scriptlet.read().expect("fn map lock");
        for block in injected_js.split("try {\n").skip(1) {
            let call = block.split('\n').next().unwrap_or("");
            let fn_name = call.split('(').next().unwrap_or("").trim();
            if let Some(name) = map.get(fn_name) {
                if !out.contains(name) {
                    out.push(name.clone());
                }
            }
        }
        out
    }

    pub fn library(&self) -> Vec<ScriptletInfo> {
        let resources = self.resources.read().expect("resources lock").clone();
        let mut out: Vec<ScriptletInfo> = resources
            .iter()
            .map(|r| ScriptletInfo {
                name: r.name.clone(),
                aliases: r.aliases.clone(),
                // The pack also holds the `$redirect` stand-ins — images, an
                // mp4, an mp3 — which are served, never injected, and the `.fn`
                // helpers, which only other scriptlets use.
                injectable: r.kind.supports_scriptlet_injection(),
                bytes: r.content.len(),
            })
            .collect();
        out.sort_by(|a, b| b.injectable.cmp(&a.injectable).then(a.name.cmp(&b.name)));
        out
    }

    pub fn source(&self, name: &str) -> Option<String> {
        use base64::Engine as _;
        let resources = self.resources.read().expect("resources lock").clone();
        let r = resources.iter().find(|r| r.name == name)?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&r.content)
            .ok()?;
        String::from_utf8(bytes).ok()
    }
}

fn load_resources(path: &Path) -> Result<Vec<Resource>> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        Error::Config(format!("reading scriptlet resources {}: {e}", path.display()))
    })?;
    serde_json::from_str(&text)
        .map_err(|e| Error::Config(format!("parsing scriptlet resources: {e}")))
}

fn build_fn_map(resources: &[Resource]) -> HashMap<String, String> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let mut map = HashMap::new();
    for r in resources {
        if r.name.ends_with(".fn") {
            continue;
        }
        let Ok(bytes) = STANDARD.decode(&r.content) else {
            continue;
        };
        let Ok(src) = std::str::from_utf8(&bytes) else {
            continue;
        };
        if let Some(rest) = src.strip_prefix("function ") {
            let fn_name = rest
                .split(|c: char| c == '(' || c.is_whitespace())
                .next()
                .unwrap_or("");
            if !fn_name.is_empty() {
                map.insert(fn_name.to_string(), r.name.clone());
            }
        }
    }
    map
}
