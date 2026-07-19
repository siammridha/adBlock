//! Managing the set of filter lists: add, remove, rename, and update rules,
//! kept in sync with the list store.

use std::sync::Arc;

use crate::adblock::error::{Error, Result};

use super::scriptlets::ScriptletLibrary;
use super::store::{ListId, ListStore, StoredList};
use super::{count_rules, EngineCore, ListEntry, CUSTOM_LIST, CUSTOM_SOURCE};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RulesUpdate {
    Append,
    Replace,
}

pub struct ListCuration {
    core: Arc<EngineCore>,
    store: Arc<dyn ListStore>,
}

impl ListCuration {
    pub(super) fn new(core: Arc<EngineCore>, store: Arc<dyn ListStore>) -> Self {
        Self { core, store }
    }

    pub fn lists(&self) -> Vec<ListEntry> {
        self.core.read(|lists| lists.to_vec())
    }

    pub fn stale_url_lists(&self, max_age: std::time::Duration) -> Vec<(String, String)> {
        self.core.read(|lists| {
            lists
                .iter()
                .filter(|l| l.source.starts_with("http://") || l.source.starts_with("https://"))
                .filter(|l| {
                    self.store
                        .age(&ListId::of(&l.name))
                        .is_none_or(|age| age > max_age)
                })
                .map(|l| (l.name.clone(), l.source.clone()))
                .collect()
        })
    }

    pub fn add_list(&self, name: &str, source: &str, text: String) -> Result<ListEntry> {
        let name = sanitize_name(name);
        if name.is_empty() {
            return Err(Error::Config("blocklist name is empty".into()));
        }
        let rules = count_rules(&text);

        self.store.persist(
            &ListId::of(&name),
            &format!("! source: {source}\n{text}"),
        )?;

        let entry = ListEntry {
            name: name.clone(),
            source: source.to_string(),
            rules,
            text,
        };
        self.core.mutate(|lists| {
            lists.retain(|l| l.name != name);
            lists.push(entry.clone());
        });
        tracing::info!(%name, %source, rules, "blocklist added, engine rebuilt");
        Ok(entry)
    }

    pub fn apply_rules(&self, name: Option<&str>, rules: &str, update: RulesUpdate) -> Result<ListEntry> {
        let name = match name {
            Some(n) if !n.trim().is_empty() => n,
            _ => CUSTOM_LIST,
        };
        let source = if name == CUSTOM_LIST { CUSTOM_SOURCE } else { "web ui" };
        match update {
            RulesUpdate::Replace => self.add_list(name, source, rules.to_string()),
            RulesUpdate::Append => self.append_rules(name, source, rules),
        }
    }

    pub fn append_rules(&self, name: &str, source: &str, rules: &str) -> Result<ListEntry> {
        let key = sanitize_name(name);
        let existing = self
            .core
            .read(|lists| lists.iter().find(|l| l.name == key).map(|l| l.text.clone()));
        let text = match existing {
            Some(t) => format!("{t}\n{rules}"),
            None => rules.to_string(),
        };
        self.add_list(name, source, text)
    }

    pub fn remove_list(&self, name: &str) -> Result<bool> {
        let present = self.core.read(|lists| lists.iter().any(|l| l.name == name));
        let removed = present
            && self.core.mutate(|lists| {
                let before = lists.len();
                lists.retain(|l| l.name != name);
                lists.len() != before
            });
        let id = ListId::of(&sanitize_name(name));
        if !id.is_empty() {
            self.store.remove(&id);
        }
        Ok(removed)
    }

    pub fn install_downloaded(
        &self,
        given_url: &str,
        url: &str,
        text: String,
    ) -> Result<ListEntry> {
        if text.trim_start().starts_with('<') {
            return Err(Error::Config(format!(
                "{url} returned an HTML page, not a filter list — use the raw file URL"
            )));
        }
        if count_rules(&text) == 0 {
            return Err(Error::Config(format!(
                "no rules found in list fetched from {url}"
            )));
        }
        let name = list_title(&text).unwrap_or_else(|| name_from_url(url));
        let sanitized = sanitize_name(&name);
        let stale: Vec<String> = self.core.read(|lists| {
            lists
                .iter()
                .filter(|l| (l.source == url || l.source == given_url) && l.name != sanitized)
                .map(|l| l.name.clone())
                .collect()
        });
        for old_name in stale {
            let _ = self.remove_list(&old_name);
        }
        self.add_list(&name, url, text)
    }

    pub fn scriptlets(&self) -> &ScriptletLibrary {
        &self.core.scriptlets
    }

    pub fn reload_scriptlet_resources(&self) -> Result<usize> {
        let count = self.core.scriptlets.reload_from_disk()?;
        self.core.mutate(|_| ());
        tracing::info!(count, "scriptlet resources reloaded, engine rebuilt");
        Ok(count)
    }
}

pub(super) struct Reconciled {
    pub(super) custom_lines: Vec<String>,
    pub(super) remove: Vec<ListId>,
}

pub(super) fn reconcile(lists: &mut Vec<ListEntry>, stored: Vec<StoredList>) -> Reconciled {
    let mut custom_lines = Vec::new();
    let mut remove = Vec::new();
    let mut loaded_ids: std::collections::HashMap<String, ListId> =
        std::collections::HashMap::new();

    for s in stored {
        if s.id == ListId::of(CUSTOM_LIST) || s.id == ListId::of("custom-web") {
            for line in strip_source_header(&s.text).lines() {
                let t = line.trim();
                if !t.is_empty() {
                    custom_lines.push(t.to_string());
                }
            }
            if s.id == ListId::of("custom-web") {
                remove.push(s.id);
            }
            continue;
        }
        let entry = entry_from_text(s.id.as_str(), &s.origin, s.text.clone());
        if let Some(prev) = lists.iter().position(|l| l.name == entry.name) {
            if s.id == ListId::of(&entry.name) {
                if let Some(old_id) = loaded_ids.get(&entry.name) {
                    remove.push(old_id.clone());
                }
                loaded_ids.insert(entry.name.clone(), s.id);
                lists[prev] = entry;
            } else {
                remove.push(s.id);
            }
            continue;
        }
        loaded_ids.insert(entry.name.clone(), s.id);
        lists.push(entry);
    }

    Reconciled {
        custom_lines,
        remove,
    }
}

fn strip_source_header(text: &str) -> &str {
    match text.strip_prefix("! source: ") {
        Some(rest) => rest.split_once('\n').map(|(_, body)| body).unwrap_or(""),
        None => text,
    }
}

fn entry_from_text(fallback_name: &str, fallback_source: &str, text: String) -> ListEntry {
    let (source, text) = match text.strip_prefix("! source: ") {
        Some(rest) => {
            let (src, body) = rest.split_once('\n').unwrap_or((rest, ""));
            (src.to_string(), body.to_string())
        }
        None => (fallback_source.to_string(), text),
    };
    let name = list_title(&text)
        .map(|t| sanitize_name(&t))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| fallback_name.to_string());
    ListEntry {
        name,
        rules: count_rules(&text),
        source,
        text,
    }
}

fn list_title(text: &str) -> Option<String> {
    text.lines()
        .take(30)
        .filter(|l| l.starts_with('!') || l.starts_with('#'))
        .find_map(|l| {
            let rest = l.trim_start_matches(['!', '#']).trim_start();
            rest.strip_prefix("Title:").or_else(|| rest.strip_prefix("title:"))
        })
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

fn sanitize_name(name: &str) -> String {
    let name = name.trim().trim_end_matches(".txt");
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

pub(crate) fn normalize_list_url(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("https://github.com/") {
        let parts: Vec<&str> = rest.splitn(4, '/').collect();
        if parts.len() == 4 && parts[2] == "blob" {
            return format!(
                "https://raw.githubusercontent.com/{}/{}/{}",
                parts[0], parts[1], parts[3]
            );
        }
    }
    url.to_string()
}

fn name_from_url(url: &str) -> String {
    url.rsplit('/')
        .find(|s| !s.is_empty() && !s.contains(':'))
        .unwrap_or("list")
        .trim_end_matches(".txt")
        .to_string()
}
