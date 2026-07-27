//! Proxy's persistence helpers: a line-based set file (`PersistedSet`) and
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

    /// Write these keys into the file, leaving every other key it holds alone.
    /// Several settings groups share one file, each writing only what it owns.
    pub fn save(&self, overrides: &T) -> std::result::Result<(), String> {
        let mut merged = self.raw();
        let next = serde_json::to_value(overrides).map_err(|e| e.to_string())?;
        merge(&mut merged, next);
        self.write(&merged)
    }

    /// Fill in `default` for the keys the file does not have yet, so a fresh
    /// install starts with a full, editable settings file. Values already in the
    /// file win.
    pub fn ensure(&self, default: &T) {
        let existing = self.raw();
        let Ok(mut merged) = serde_json::to_value(default) else { return };
        merge(&mut merged, existing);
        let _ = self.write(&merged);
    }

    /// The file as raw JSON, or `Null` when it is missing or unreadable.
    fn raw(&self) -> serde_json::Value {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(serde_json::Value::Null)
    }

    fn write(&self, value: &serde_json::Value) -> std::result::Result<(), String> {
        // No path means no settings file — an in-memory policy, nothing to do.
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("creating {}: {e}", dir.display()))?;
        }
        let body = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
        std::fs::write(&self.path, body)
            .map_err(|e| format!("writing {}: {e}", self.path.display()))
    }
}

/// Copy `next`'s keys over `base`. Only objects merge; anything else replaces.
fn merge(base: &mut serde_json::Value, next: serde_json::Value) {
    match (base.as_object_mut(), next) {
        (Some(base), serde_json::Value::Object(next)) => base.extend(next),
        (_, serde_json::Value::Null) => {}
        (_, next) => *base = next,
    }
}

fn parse_lines<T: Entry>(text: &str) -> Vec<T> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .filter_map(T::parse)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct Kv(String, u32);

    impl Entry for Kv {
        fn parse(line: &str) -> Option<Self> {
            let (k, v) = line.split_once('=')?;
            Some(Kv(k.trim().to_string(), v.trim().parse().ok()?))
        }

        fn format(&self) -> String {
            format!("{}={}", self.0, self.1)
        }
    }

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("proxy-persist-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("set.conf")
    }

    fn load(path: std::path::PathBuf) -> PersistedSet<Kv> {
        PersistedSet::load(path, "# test set", |_| {})
    }

    #[test]
    fn roundtrips_through_the_file_and_applies_normalize_on_load() {
        let path = temp_path("roundtrip");
        let set = load(path.clone());
        assert!(set.read(<[Kv]>::is_empty), "absent file is an empty set");

        set.mutate(|v| {
            v.push(Kv("b".into(), 2));
            v.push(Kv("a".into(), 1));
            ((), true)
        })
        .unwrap();

        let reloaded = PersistedSet::<Kv>::load(path, "# test set", |v| {
            v.sort_by(|x, y| x.0.cmp(&y.0));
        });
        assert_eq!(reloaded.snapshot(), vec![Kv("a".into(), 1), Kv("b".into(), 2)]);
    }

    #[test]
    fn comments_blanks_and_malformed_lines_drop_not_fatal() {
        let path = temp_path("tolerant");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "# header comment\n\na=1\nbogus line without equals\nb=2  # trailing comment\nc=not-a-number\n",
        )
        .unwrap();
        let set = load(path);
        assert_eq!(
            set.snapshot(),
            vec![Kv("a".into(), 1), Kv("b".into(), 2)],
            "malformed lines mean stale, not fatal"
        );
    }

    #[test]
    fn a_failed_write_leaves_the_served_set_untouched() {
        let dir = std::env::temp_dir().join("proxy-persist-test-rollback");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("not-a-dir");
        std::fs::write(&blocker, "file, not dir").unwrap();

        let set = load(blocker.join("set.conf"));
        let err = set
            .mutate(|v| {
                v.push(Kv("a".into(), 1));
                ((), true)
            })
            .unwrap_err();
        assert!(err.to_string().contains("not-a-dir"), "err: {err}");
        assert!(set.read(<[Kv]>::is_empty), "a rejected change must not be served");
    }

    #[test]
    fn unchanged_mutations_write_nothing() {
        let path = temp_path("nochange");
        let set = load(path.clone());
        let looked = set.mutate(|v| (v.len(), false)).unwrap();
        assert_eq!(looked, 0);
        assert!(!path.exists(), "no change, no file");
    }

    #[derive(Default, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
    struct Knobs {
        speed: Option<u32>,
    }

    #[test]
    fn override_store_treats_absent_or_corrupt_as_no_overrides() {
        let dir = std::env::temp_dir().join("proxy-persist-test-overrides");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("knobs.json");
        let store: OverrideStore<Knobs> = OverrideStore::new(path.clone());

        assert_eq!(store.load(), Knobs::default(), "absent file = no overrides");
        store.save(&Knobs { speed: Some(9) }).unwrap();
        assert_eq!(store.load(), Knobs { speed: Some(9) });

        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(store.load(), Knobs::default(), "corrupt file = no overrides, not a crash");
    }

    #[derive(Default, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
    struct OtherKnobs {
        colour: Option<String>,
    }

    #[test]
    fn two_settings_groups_share_one_file_without_wiping_each_other() {
        let dir = std::env::temp_dir().join("proxy-persist-test-shared");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let speed: OverrideStore<Knobs> = OverrideStore::new(path.clone());
        let colour: OverrideStore<OtherKnobs> = OverrideStore::new(path.clone());

        speed.ensure(&Knobs { speed: Some(1) });
        colour.ensure(&OtherKnobs { colour: Some("red".into()) });
        speed.save(&Knobs { speed: Some(9) }).unwrap();

        assert_eq!(speed.load().speed, Some(9));
        assert_eq!(colour.load().colour, Some("red".into()), "the other group survived");

        colour.save(&OtherKnobs { colour: Some("blue".into()) }).unwrap();
        assert_eq!(speed.load().speed, Some(9), "still there after the other group wrote");

        speed.ensure(&Knobs { speed: Some(1) });
        assert_eq!(speed.load().speed, Some(9), "ensure never overwrites a saved value");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
