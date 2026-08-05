//! Storage for downloaded filter list text: on disk in production, in memory
//! for tests.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use crate::adblock::error::{Error, Result};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ListId(String);

impl ListId {
    pub fn of(name: &str) -> Self {
        Self(name.to_lowercase())
    }

    pub fn raw(key: &str) -> Self {
        Self(key.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Display for ListId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub struct StoredList {
    pub id: ListId,
    pub origin: String,
    pub text: String,
}

pub trait ListStore: Send + Sync {
    fn load(&self) -> Vec<StoredList>;
    fn persist(&self, id: &ListId, text: &str) -> Result<()>;
    fn remove(&self, id: &ListId);
    fn age(&self, id: &ListId) -> Option<Duration>;
    /// Where the compiled engine built from these lists may be cached, if the
    /// store keeps anything on disk. A store that does not (the in-memory one)
    /// answers `None` and the engine is rebuilt from the rules every time.
    fn engine_cache(&self) -> Option<PathBuf> {
        None
    }
}

pub struct DiskStore {
    dir: PathBuf,
}

impl DiskStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn path(&self, id: &ListId) -> PathBuf {
        self.dir.join(format!("{id}.txt"))
    }
}

impl ListStore for DiskStore {
    fn load(&self) -> Vec<StoredList> {
        let mut out = Vec::new();
        let Ok(dir) = std::fs::read_dir(&self.dir) else {
            return out;
        };
        for f in dir.flatten() {
            let path = f.path();
            if path.extension().and_then(|e| e.to_str()) != Some("txt") {
                continue;
            }
            let id = ListId::raw(path.file_stem().and_then(|s| s.to_str()).unwrap_or(""));
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "skipping list");
                    continue;
                }
            };
            out.push(StoredList {
                id,
                origin: path.display().to_string(),
                text,
            });
        }
        out
    }

    fn persist(&self, id: &ListId, text: &str) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| Error::Config(format!("creating {}: {e}", self.dir.display())))?;
        let path = self.path(id);
        std::fs::write(&path, text)
            .map_err(|e| Error::Config(format!("writing {}: {e}", path.display())))
    }

    fn remove(&self, id: &ListId) {
        let _ = std::fs::remove_file(self.path(id));
    }

    fn age(&self, id: &ListId) -> Option<Duration> {
        self.path(id)
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
    }

    // Sits beside the lists it was compiled from. `load` only picks up `.txt`,
    // so this file is never mistaken for a blocklist.
    fn engine_cache(&self) -> Option<PathBuf> {
        Some(self.dir.join("engine.dat"))
    }
}

#[derive(Default)]
pub struct MemoryListStore {
    lists: Mutex<BTreeMap<ListId, String>>,
}

impl MemoryListStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn seed(&self, key: &str, text: &str) {
        self.lists
            .lock()
            .expect("store lock")
            .insert(ListId::raw(key), text.to_string());
    }

    pub fn get(&self, key: &str) -> Option<String> {
        self.lists
            .lock()
            .expect("store lock")
            .get(&ListId::raw(key))
            .cloned()
    }
}

impl ListStore for MemoryListStore {
    fn load(&self) -> Vec<StoredList> {
        self.lists
            .lock()
            .expect("store lock")
            .iter()
            .map(|(id, text)| StoredList {
                id: id.clone(),
                origin: format!("memory:{id}"),
                text: text.clone(),
            })
            .collect()
    }

    fn persist(&self, id: &ListId, text: &str) -> Result<()> {
        self.seed(id.as_str(), text);
        Ok(())
    }

    fn remove(&self, id: &ListId) {
        self.lists.lock().expect("store lock").remove(id);
    }

    fn age(&self, _id: &ListId) -> Option<Duration> {
        None
    }
}
