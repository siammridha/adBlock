//! Stats's persistence helpers: a line-based set file (`PersistedSet`) and
//! a JSON settings-override file (`OverrideStore`). Each module keeps its own
//! copy — no shared helpers.

use std::path::PathBuf;
use std::sync::RwLock;

use serde::de::DeserializeOwned;
use serde::Serialize;

use super::error::{Error, Result};

pub trait Entry: Sized {
    fn parse(line: &str) -> Option<Self>;
    fn format(&self) -> String;
}

pub struct PersistedSet<T> {
    entries: RwLock<Vec<T>>,
    path: PathBuf,
    header: &'static str,
}

impl<T: Entry + Clone> PersistedSet<T> {
    pub fn load(path: PathBuf, header: &'static str, normalize: impl FnOnce(&mut Vec<T>)) -> Self {
        let mut entries: Vec<T> = std::fs::read_to_string(&path)
            .map(|t| parse_lines(&t))
            .unwrap_or_default();
        normalize(&mut entries);
        Self { entries: RwLock::new(entries), path, header }
    }

    pub fn read<R>(&self, f: impl FnOnce(&[T]) -> R) -> R {
        f(&self.entries.read().expect("persisted set lock"))
    }

    pub fn snapshot(&self) -> Vec<T> {
        self.entries.read().expect("persisted set lock").clone()
    }

    pub fn mutate<R>(&self, f: impl FnOnce(&mut Vec<T>) -> (R, bool)) -> Result<R> {
        let mut entries = self.entries.write().expect("persisted set lock");
        let mut next = entries.clone();
        let (out, changed) = f(&mut next);
        if changed {
            self.persist(&next)?;
            *entries = next;
        }
        Ok(out)
    }

    fn persist(&self, entries: &[T]) -> Result<()> {
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .map_err(|e| Error::Config(format!("creating {}: {e}", dir.display())))?;
        }
        let body = format!(
            "{}\n{}\n",
            self.header,
            entries.iter().map(Entry::format).collect::<Vec<_>>().join("\n")
        );
        std::fs::write(&self.path, body)
            .map_err(|e| Error::Config(format!("writing {}: {e}", self.path.display())))
    }
}

pub struct OverrideStore<T> {
    path: PathBuf,
    _overrides: std::marker::PhantomData<fn() -> T>,
}

impl<T: Default + Serialize + DeserializeOwned> OverrideStore<T> {
    pub fn new(path: PathBuf) -> Self {
        Self { path, _overrides: std::marker::PhantomData }
    }

    pub fn load(&self) -> T {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, overrides: &T) -> std::result::Result<(), String> {
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("creating {}: {e}", dir.display()))?;
        }
        let body = serde_json::to_string_pretty(overrides).map_err(|e| e.to_string())?;
        std::fs::write(&self.path, body)
            .map_err(|e| format!("writing {}: {e}", self.path.display()))
    }

    /// Create the file populated with `default` when it does not exist yet, so a
    /// fresh install starts with a full, editable settings file. An existing file
    /// is left untouched.
    pub fn ensure(&self, default: &T) {
        if !self.path.exists() {
            let _ = self.save(default);
        }
    }
}

fn parse_lines<T: Entry>(text: &str) -> Vec<T> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .filter_map(T::parse)
        .collect()
}

