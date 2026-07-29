//! Ad blocking: wraps the `adblock` crate engine and owns the filter lists
//! behind it.

use std::sync::{Arc, Mutex, RwLock};

use adblock::cosmetic_filter_cache::ProceduralOrActionFilter;
use adblock::lists::{FilterSet, ParseOptions};
use adblock::request::Request;
use adblock::resources::Resource;
use adblock::Engine;

use error::Result;

pub mod api;
pub mod commands;
pub mod config;
mod curation;
pub mod error;
pub mod fetch;
pub mod maintenance;
mod scriptlets;
pub mod settings;
mod store;
pub mod updater;
pub use config::AdblockConfig;
pub use curation::{ListCuration, RulesUpdate};
pub(crate) use curation::normalize_list_url;
pub use scriptlets::{ScriptletInfo, ScriptletInjection, ScriptletLibrary};
pub use store::{DiskStore, ListId, ListStore, MemoryListStore, StoredList};

use scriptlets::ScriptletLibrary as Scriptlets;

pub const CUSTOM_LIST: &str = "custom";
const CUSTOM_SOURCE: &str = "config + ui";

/// Loosen rule-tester input into a full URL: the tester accepts bare hosts,
/// protocol-relative URLs, and bare paths. Owned by adblock because the rule
/// tester is adblock's; callers pass raw input through.
pub fn normalize_test_url(input: &str) -> String {
    let s = input.trim();
    if s.starts_with("http://") || s.starts_with("https://") {
        s.to_string()
    } else if let Some(rest) = s.strip_prefix("//") {
        format!("https://{rest}")
    } else if s.starts_with('/') {
        format!("https://any-host.invalid{s}")
    } else {
        format!("https://{s}")
    }
}

struct EngineCore {
    enabled: bool,
    engine: RwLock<Arc<Engine>>,
    lists: Mutex<Vec<ListEntry>>,
    scriptlets: Scriptlets,
}

impl EngineCore {
    fn rebuild(&self, lists: &[ListEntry]) {
        let resources = self.scriptlets.resources();
        let engine = build_engine(lists, &resources);
        *self.engine.write().expect("engine lock") = Arc::new(engine);
    }

    fn mutate<R>(&self, f: impl FnOnce(&mut Vec<ListEntry>) -> R) -> R {
        let mut lists = self.lists.lock().expect("lists lock");
        let out = f(&mut lists);
        self.rebuild(&lists);
        out
    }

    fn read<R>(&self, f: impl FnOnce(&[ListEntry]) -> R) -> R {
        f(&self.lists.lock().expect("lists lock"))
    }
}

pub struct AdBlocker {
    core: Arc<EngineCore>,
    /// Whether a decision may carry a `$redirect` body or a `$removeparam`
    /// URL. Adblock's own switches: the caller never asks for either.
    decisions: settings::DecisionPolicy,
}

#[derive(Clone, Debug)]
pub struct ListEntry {
    pub name: String,
    pub source: String,
    pub rules: usize,
    text: String,
}

impl ListEntry {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn categories(&self) -> (usize, usize, usize) {
        rule_categories(&self.text)
    }
}

pub struct BlockDecision {
    pub blocked: bool,
    pub attribution: BlockAttribution,
    /// What to serve instead of the blocked resource, from a `$redirect` or
    /// `$redirect-rule` option. Only ever set when `blocked` is true: a
    /// `$redirect-rule` supplies a replacement without blocking on its own, and
    /// serving one for a request that was going to be allowed would break it.
    pub redirect: Option<Redirect>,
    /// The request URL with tracking parameters stripped, from `$removeparam`.
    /// Only meaningful when the request is not blocked.
    pub rewritten_url: Option<String>,
}

impl BlockDecision {
    fn pass() -> Self {
        Self {
            blocked: false,
            attribution: BlockAttribution { rule: None, list: None },
            redirect: None,
            rewritten_url: None,
        }
    }
}

/// A stand-in body for a blocked request: a neutered copy of the real resource
/// (an analytics script that does nothing, an empty image) so the page's own
/// code keeps running instead of tripping over a missing file.
pub struct Redirect {
    pub mime: String,
    pub body: Vec<u8>,
}

pub struct BlockAttribution {
    pub rule: Option<String>,
    pub list: Option<String>,
}

impl BlockAttribution {
    pub fn display(&self) -> String {
        let rule = self.rule.as_deref().unwrap_or("?");
        match &self.list {
            Some(list) => format!("{rule} — {list}"),
            None => rule.to_string(),
        }
    }
}

pub fn from_config(cfg: &AdblockConfig) -> Result<(Arc<AdBlocker>, Arc<ListCuration>)> {
    // Adblock owns its own directories: create them up front so first reads and
    // writes (lists, scriptlet bundle) always have a home.
    for dir in [cfg.blocklists_dir(), cfg.scriptlets_dir()] {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(error = %e, dir = %dir.display(), "creating adblock data dir");
        }
    }
    with_store(cfg, Arc::new(DiskStore::new(cfg.blocklists_dir())))
}

pub fn with_store(
    cfg: &AdblockConfig,
    store: Arc<dyn ListStore>,
) -> Result<(Arc<AdBlocker>, Arc<ListCuration>)> {
    let scriptlets = Scriptlets::from_config(cfg)?;

    let mut lists = Vec::new();

    let mut custom_lines: Vec<String> = cfg
        .custom_rules
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let stored = store.load();
    let outcome = curation::reconcile(&mut lists, stored);
    custom_lines.extend(outcome.custom_lines);
    for id in &outcome.remove {
        store.remove(id);
    }

    let mut seen = std::collections::HashSet::new();
    custom_lines.retain(|l| seen.insert(l.clone()));
    let text = custom_lines.join("\n");
    let _ = store.persist(&ListId::of(CUSTOM_LIST), &format!("! source: {CUSTOM_SOURCE}\n{text}"));
    lists.push(ListEntry {
        name: CUSTOM_LIST.into(),
        source: CUSTOM_SOURCE.into(),
        rules: count_rules(&text),
        text,
    });

    let engine = build_engine(&lists, &scriptlets.resources());
    let core = Arc::new(EngineCore {
        enabled: cfg.enabled,
        engine: RwLock::new(Arc::new(engine)),
        lists: Mutex::new(lists),
        scriptlets,
    });
    Ok((
        Arc::new(AdBlocker {
            core: core.clone(),
            decisions: settings::DecisionPolicy::load(cfg.settings_path()),
        }),
        Arc::new(ListCuration::new(core, store)),
    ))
}

impl AdBlocker {
    pub fn enabled(&self) -> bool {
        self.core.enabled
    }

    pub fn check(&self, url: &str, source_url: &str, request_type: &str) -> BlockDecision {
        if !self.core.enabled {
            return BlockDecision::pass();
        }
        let request_type = filter_request_type(request_type);
        let request = match Request::new(url, source_url, request_type) {
            Ok(r) => r,
            Err(_) => return BlockDecision::pass(),
        };
        let engine = self.core.engine.read().expect("engine lock").clone();
        let result = engine.check_network_request(&request);
        let allow = self.decisions.settings();
        BlockDecision {
            blocked: result.matched,
            attribution: self.attribution(result.filter.as_deref()),
            redirect: (result.matched && allow.redirect)
                .then(|| result.redirect.as_deref().and_then(decode_redirect))
                .flatten(),
            rewritten_url: allow.removeparam.then_some(result.rewritten_url).flatten(),
        }
    }

    /// Adblock's own switches, for the settings UI.
    pub fn decisions(&self) -> settings::DecisionSettings {
        self.decisions.settings()
    }

    /// Change those switches. The caller hands over the raw request body;
    /// Adblock parses it and answers with the new settings or why it was
    /// rejected.
    pub fn set_decisions(
        &self,
        body: &[u8],
    ) -> std::result::Result<settings::DecisionSettings, String> {
        Ok(self.decisions.apply(&settings::DecisionOverrides::parse(body)?))
    }

    pub fn check_dns(&self, domain: &str) -> BlockDecision {
        let url = format!("https://{domain}/");
        self.check(&url, &url, "other")
    }

    pub fn cosmetic_css(&self, url: &str, classes: &[String], ids: &[String]) -> String {
        if !self.core.enabled {
            return String::new();
        }
        let engine = self.core.engine.read().expect("engine lock").clone();
        let resources = engine.url_cosmetic_resources(url);
        let mut selectors: std::collections::BTreeSet<String> =
            resources.hide_selectors.into_iter().collect();
        if !resources.generichide {
            selectors.extend(engine.hidden_class_id_selectors(
                classes,
                ids,
                &resources.exceptions,
            ));
            selectors.extend(
                self.custom_generic_selectors()
                    .into_iter()
                    .filter(|s| !resources.exceptions.contains(s)),
            );
        }
        let mut css = hide_rules(selectors);
        // Rules the engine could not reduce to a plain hide come back separately.
        // Keep the ones that are still pure CSS — `:style()`, mostly unbreak
        // rules like `body:style(overflow: auto !important)` — and drop the rest,
        // which need a live DOM. They go last on purpose: both halves are
        // `!important`, so on a shared element the later rule wins, and the
        // unbreak rule is the one that has to win.
        let mut actions: Vec<String> = resources
            .procedural_actions
            .iter()
            .filter_map(|json| serde_json::from_str::<ProceduralOrActionFilter>(json).ok())
            .filter_map(|filter| filter.as_css())
            .map(|(selector, style)| format!("{selector}{{{style}}}\n"))
            .collect();
        actions.sort();
        for rule in actions {
            css.push_str(&rule);
        }
        css
    }

    /// Cosmetic rules for class and id names a page grew after it was served.
    ///
    /// Only the rules those names select: everything that does not depend on
    /// the page's names — the hostname-specific rules, the custom generic ones,
    /// the `:style()` unbreak rules — already went out with the page itself, so
    /// repeating it on every question would just re-send the whole stylesheet.
    pub fn cosmetic_css_for_names(&self, url: &str, classes: &[String], ids: &[String]) -> String {
        if !self.core.enabled || (classes.is_empty() && ids.is_empty()) {
            return String::new();
        }
        let engine = self.core.engine.read().expect("engine lock").clone();
        let resources = engine.url_cosmetic_resources(url);
        if resources.generichide {
            return String::new();
        }
        hide_rules(
            engine
                .hidden_class_id_selectors(classes, ids, &resources.exceptions)
                .into_iter()
                .collect(),
        )
    }

    pub fn scriptlets_enabled(&self) -> bool {
        self.core.scriptlets.enabled()
    }

    pub fn scriptlet_injection(&self, url: &str) -> Option<ScriptletInjection> {
        if !self.core.enabled || !self.scriptlets_enabled() {
            return None;
        }
        let engine = self.core.engine.read().expect("engine lock").clone();
        let js = engine.url_cosmetic_resources(url).injected_script;
        if js.is_empty() {
            return None;
        }
        let names = self.core.scriptlets.names_in(&js);
        let js = format!("const scriptletGlobals = {{}};\n{js}");
        Some(ScriptletInjection { js, names })
    }

    pub(crate) fn attribution(&self, filter: Option<&str>) -> BlockAttribution {
        let rule = filter.map(str::to_string);
        let list = rule.as_deref().and_then(|f| self.filter_origin(f));
        BlockAttribution { rule, list }
    }

    fn custom_generic_selectors(&self) -> Vec<String> {
        self.core.read(|lists| {
            let Some(custom) = lists.iter().find(|l| l.name == CUSTOM_LIST) else {
                return Vec::new();
            };
            custom
                .text
                .lines()
                .filter_map(|line| line.trim().strip_prefix("##"))
                .filter(|sel| !sel.is_empty() && !sel.starts_with("+js(") && !sel.starts_with('^'))
                .map(str::to_string)
                .collect()
        })
    }

    fn filter_origin(&self, filter: &str) -> Option<String> {
        let needle = filter.trim();
        if needle.is_empty() {
            return None;
        }
        self.core.read(|lists| {
            lists
                .iter()
                .find(|l| l.text.lines().any(|line| line.trim() == needle))
                .map(|l| l.name.clone())
        })
    }
}

fn filter_request_type(req_type: &str) -> &str {
    if req_type == "fetch" {
        "xmlhttprequest"
    } else {
        req_type
    }
}

/// Emit one hide rule per selector, in a stable order.
fn hide_rules(selectors: std::collections::BTreeSet<String>) -> String {
    let mut css = String::new();
    for sel in selectors {
        css.push_str(&sel);
        css.push_str("{display:none !important}\n");
    }
    css
}

/// Decode the engine's `data:<mime>;base64,<payload>` redirect into a body the
/// proxy can serve. Anything not of that shape is dropped, and the request
/// falls back to the plain block response.
fn decode_redirect(data_url: &str) -> Option<Redirect> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let (mime, payload) = data_url.strip_prefix("data:")?.split_once(";base64,")?;
    Some(Redirect { mime: mime.to_string(), body: STANDARD.decode(payload).ok()? })
}

fn build_engine(lists: &[ListEntry], resources: &[Resource]) -> Engine {
    let mut filter_set = FilterSet::new(true);
    for l in lists {
        filter_set.add_filter_list(&l.text, ParseOptions::default());
    }
    let mut engine = Engine::from_filter_set(filter_set, false);
    if !resources.is_empty() {
        engine.use_resources(resources.iter().cloned());
    }
    engine
}

fn rule_categories(text: &str) -> (usize, usize, usize) {
    let (mut network, mut cosmetic, mut exception) = (0usize, 0usize, 0usize);
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('!') || l.starts_with('[') {
            continue;
        }
        if l.starts_with("@@") {
            exception += 1;
        } else if l.contains("##") || l.contains("#@#") || l.contains("#?#") {
            cosmetic += 1;
        } else {
            network += 1;
        }
    }
    (network, cosmetic, exception)
}

pub(crate) fn count_rules(text: &str) -> usize {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('!') && !l.starts_with('['))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cfg(rules: &[&str]) -> AdblockConfig {
        AdblockConfig {
            enabled: true,
            custom_rules: rules.iter().map(|s| s.to_string()).collect(),
            data_dir: PathBuf::from("/nonexistent-for-tests"),
            auto_update_hours: 0,
            inject_scriptlets: false,
            scriptlet_resources: PathBuf::new(),
        }
    }

    fn blocker_with(rules: &[&str]) -> (Arc<AdBlocker>, Arc<ListCuration>) {
        with_store(&cfg(rules), Arc::new(MemoryListStore::new())).unwrap()
    }

    #[test]
    fn blocks_matching_url() {
        let (b, _) = blocker_with(&["||ads.example.com^"]);
        let d = b.check(
            "https://ads.example.com/banner.png",
            "https://news.example.org/",
            "image",
        );
        assert!(d.blocked);
    }

    #[test]
    fn allows_unmatched_url() {
        let (b, _) = blocker_with(&["||ads.example.com^"]);
        let d = b.check(
            "https://cdn.example.org/logo.png",
            "https://news.example.org/",
            "image",
        );
        assert!(!d.blocked);
    }

    #[test]
    fn xhr_filter_option_matches_fetch_and_xmlhttprequest_types() {
        let (b, _) = blocker_with(&["||track.example.com^$xhr"]);
        assert!(b.check("https://track.example.com/beacon", "", "fetch").blocked);
        assert!(b.check("https://track.example.com/beacon", "", "xmlhttprequest").blocked);
        assert!(!b.check("https://track.example.com/img.png", "", "image").blocked);
        for t in ["document", "subdocument", "image", "script", "other"] {
            assert_eq!(filter_request_type(t), t);
        }
    }

    #[test]
    fn dns_check_applies_hostname_rules_only() {
        let (b, _) = blocker_with(&[
            "||ads.example.com^",
            "@@||good.ads.example.com^",
            "||tracker.example.com^$third-party",
            "||scripts.example.com^$script",
        ]);
        assert!(b.check_dns("ads.example.com").blocked);
        assert!(b.check_dns("sub.ads.example.com").blocked);
        assert!(!b.check_dns("good.ads.example.com").blocked);
        assert!(!b.check_dns("tracker.example.com").blocked);
        assert!(!b.check_dns("scripts.example.com").blocked);
        assert!(!b.check_dns("example.org").blocked);
        let d = b.check_dns("ads.example.com");
        assert_eq!(d.attribution.rule.as_deref(), Some("||ads.example.com^"));
        assert_eq!(d.attribution.list.as_deref(), Some("custom"));
    }

    #[test]
    fn disabled_blocker_allows_everything() {
        let mut c = cfg(&["||ads.example.com^"]);
        c.enabled = false;
        let (b, _) = with_store(&c, Arc::new(MemoryListStore::new())).unwrap();
        assert!(!b.check("https://ads.example.com/x", "", "image").blocked);
    }

    #[test]
    fn reports_matching_rule() {
        let (b, _) = blocker_with(&["||ads.example.com^"]);
        let d = b.check("https://ads.example.com/banner.png", "", "image");
        assert!(d.blocked);
        assert_eq!(d.attribution.rule.as_deref(), Some("||ads.example.com^"));
    }

    #[test]
    fn add_list_rebuilds_engine_and_persists() {
        let store = Arc::new(MemoryListStore::new());
        let (b, c) = with_store(&cfg(&[]), store.clone()).unwrap();
        assert!(!b.check("https://ads.example.com/x", "", "image").blocked);

        c.add_list("test-list", "web ui", "||ads.example.com^".into())
            .unwrap();
        assert!(b.check("https://ads.example.com/x", "", "image").blocked);
        assert_eq!(c.lists().len(), 2);
        assert!(store
            .get("test-list")
            .unwrap()
            .starts_with("! source: web ui\n"));
    }

    #[test]
    fn list_id_canonicalizes_display_names() {
        assert_eq!(ListId::of("EasyList"), ListId::raw("easylist"));
        assert_ne!(ListId::raw("EasyList"), ListId::of("EasyList"), "raw keeps what the store found");
        assert_eq!(ListId::of("custom").as_str(), "custom");
    }

    #[test]
    fn disk_store_round_trips() {
        let dir = std::env::temp_dir().join(format!("sp-store-test-{}", std::process::id()));
        let store = DiskStore::new(dir.clone());
        let id = ListId::of("Some-List");
        store.persist(&id, "! source: web ui\n||x.com^").unwrap();
        let loaded = store.load();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, id, "canonical id survives the disk round-trip");
        assert!(loaded[0].text.contains("||x.com^"));
        assert!(store.age(&id).is_some());
        store.remove(&id);
        assert!(store.load().is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn reconcile_folds_custom_lists() {
        let mut lists = Vec::new();
        let stored = vec![
            StoredList {
                id: ListId::raw("custom"),
                origin: "custom.txt".into(),
                text: "! source: config + ui\n||a.com^".into(),
            },
            StoredList {
                id: ListId::raw("custom-web"),
                origin: "custom-web.txt".into(),
                text: "||b.com^\n".into(),
            },
        ];
        let out = curation::reconcile(&mut lists, stored);
        assert!(lists.is_empty(), "custom lists must not become entries");
        assert_eq!(out.custom_lines, vec!["||a.com^", "||b.com^"]);
        assert_eq!(out.remove, vec![ListId::raw("custom-web")]);
    }

    #[test]
    fn reconcile_keeps_canonical_copy_after_rename() {
        let mut lists = Vec::new();
        let titled = "! source: https://x/l.txt\n! Title: EasyList\n||ads.com^";
        let stored = vec![
            StoredList {
                id: ListId::raw("l"),
                origin: "l.txt".into(),
                text: titled.into(),
            },
            StoredList {
                id: ListId::of("EasyList"),
                origin: "easylist.txt".into(),
                text: titled.into(),
            },
        ];
        let out = curation::reconcile(&mut lists, stored);
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0].name, "EasyList");
        assert_eq!(out.remove, vec![ListId::raw("l")]);
    }

    #[test]
    fn reconcile_removes_non_canonical_duplicate() {
        let mut lists = Vec::new();
        let titled = "! Title: EasyList\n||ads.com^";
        let stored = vec![
            StoredList {
                id: ListId::raw("easylist"),
                origin: "easylist.txt".into(),
                text: titled.into(),
            },
            StoredList {
                id: ListId::raw("stray-copy"),
                origin: "stray-copy.txt".into(),
                text: titled.into(),
            },
        ];
        let out = curation::reconcile(&mut lists, stored);
        assert_eq!(lists.len(), 1);
        assert_eq!(out.remove, vec![ListId::raw("stray-copy")]);
    }

    #[test]
    fn startup_folds_persisted_custom_web_into_custom() {
        let store = Arc::new(MemoryListStore::new());
        store.seed("custom-web", "||legacy.com^");
        let (b, c) = with_store(&cfg(&["||config.com^"]), store.clone()).unwrap();

        assert!(b.check("https://legacy.com/x", "", "image").blocked);
        assert!(b.check("https://config.com/x", "", "image").blocked);
        let custom = c.lists().into_iter().find(|l| l.name == "custom").unwrap();
        assert_eq!(custom.rules, 2);
        let on_disk = store.get("custom").unwrap();
        assert!(on_disk.contains("||config.com^") && on_disk.contains("||legacy.com^"));
        assert!(store.get("custom-web").is_none());
    }

    #[test]
    fn apply_rules_defaults_to_custom_and_accumulates() {
        let store = Arc::new(MemoryListStore::new());
        let (b, c) = with_store(&cfg(&[]), store.clone()).unwrap();
        c.apply_rules(None, "||a.com^", RulesUpdate::Append).unwrap();
        c.apply_rules(None, "||b.com^", RulesUpdate::Append).unwrap();
        assert!(b.check("https://a.com/x", "", "image").blocked);
        assert!(b.check("https://b.com/x", "", "image").blocked);
        let custom = c.lists().into_iter().find(|l| l.name == CUSTOM_LIST).unwrap();
        assert_eq!(custom.source, "config + ui");
        let text = store.get("custom").unwrap();
        assert!(text.contains("||a.com^") && text.contains("||b.com^"));
    }

    #[test]
    fn apply_rules_replace_overwrites_and_names_pick_the_source() {
        let (b, c) = blocker_with(&[]);
        c.apply_rules(None, "||a.com^", RulesUpdate::Append).unwrap();
        c.apply_rules(None, "||b.com^", RulesUpdate::Replace).unwrap();
        assert!(!b.check("https://a.com/x", "", "image").blocked, "replace must overwrite");
        assert!(b.check("https://b.com/x", "", "image").blocked);
        let entry = c.apply_rules(Some("mine"), "||c.com^", RulesUpdate::Append).unwrap();
        assert_eq!(entry.source, "web ui");
    }

    #[test]
    fn attribution_names_rule_and_list() {
        let (b, _) = blocker_with(&["||ads.example.com^"]);
        let d = b.check("https://ads.example.com/x", "", "image");
        assert_eq!(d.attribution.rule.as_deref(), Some("||ads.example.com^"));
        assert_eq!(d.attribution.list.as_deref(), Some("custom"));
        assert_eq!(d.attribution.display(), "||ads.example.com^ — custom");
        let miss = b.check("https://cdn.example.org/logo.png", "", "image");
        assert!(!miss.blocked);
        assert_eq!(miss.attribution.display(), "?");
        assert!(b.attribution(Some("||nowhere^")).list.is_none());
    }

    #[test]
    fn install_downloaded_names_from_title_and_replaces_stale() {
        let store = Arc::new(MemoryListStore::new());
        let (b, c) = with_store(&cfg(&[]), store.clone()).unwrap();
        let url = "https://x.example/l.txt";
        c.add_list("l", url, "||old.com^".into()).unwrap();

        c.install_downloaded(url, url, "! Title: Nice List\n||new.com^".into())
            .unwrap();
        let names: Vec<String> = c.lists().into_iter().map(|l| l.name).collect();
        assert!(names.contains(&"Nice-List".to_string()));
        assert!(!names.contains(&"l".to_string()), "stale entry must go");
        assert!(store.get("l").is_none());
        assert!(b.check("https://new.com/x", "", "image").blocked);
    }

    #[test]
    fn install_downloaded_rejects_html_and_empty() {
        let (_, c) = blocker_with(&[]);
        let url = "https://x.example/l.txt";
        assert!(c
            .install_downloaded(url, url, "<html>viewer page</html>".into())
            .is_err());
        assert!(c
            .install_downloaded(url, url, "! just a comment\n".into())
            .is_err());
    }

    #[test]
    fn scriptlets_disabled_without_resources() {
        let (b, _) = blocker_with(&["example.com##+js(noop)"]);
        assert!(!b.scriptlets_enabled());
        assert!(b.scriptlet_injection("https://example.com/").is_none());
    }

    fn blocker_with_resources(
        rules: &[&str],
        resources: serde_json::Value,
    ) -> (Arc<AdBlocker>, Arc<ListCuration>) {
        blocker_with_resources_inject(rules, resources, true)
    }

    fn blocker_with_resources_inject(
        rules: &[&str],
        resources: serde_json::Value,
        inject: bool,
    ) -> (Arc<AdBlocker>, Arc<ListCuration>) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sp-scriptlet-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let res_path = dir.join("resources.json");
        std::fs::write(&res_path, resources.to_string()).unwrap();
        let mut c = cfg(rules);
        c.inject_scriptlets = inject;
        c.scriptlet_resources = res_path;
        with_store(&c, Arc::new(MemoryListStore::new())).unwrap()
    }

    #[test]
    fn scriptlet_injection_resolves_matching_rule() {
        let (b, _) = blocker_with_resources(
            &["example.com##+js(sptest)"],
            serde_json::json!([{
                "name": "sptest.js",
                "kind": {"mime": "application/javascript"},
                "content": "d2luZG93Ll9fc3Bfc2NyaXB0bGV0X3Jhbj10cnVlOw=="
            }]),
        );
        assert!(b.scriptlets_enabled());
        let inj = b.scriptlet_injection("https://example.com/").unwrap();
        assert!(inj.js.contains("window.__sp_scriptlet_ran"), "js was: {:?}", inj.js);
        assert!(b.scriptlet_injection("https://other.test/").is_none());
    }

    #[test]
    fn scriptlet_injection_names_function_style_scriptlet() {
        let (b, c) = blocker_with_resources(
            &["example.com##+js(nooper, 1)"],
            serde_json::json!([{
                "name": "nooper.js",
                "aliases": ["noop-alias.js"],
                "kind": {"mime": "application/javascript"},
                "content": "ZnVuY3Rpb24gbm9vcGVyRm4oYSl7dm9pZCBhO30="
            }]),
        );
        let inj = b.scriptlet_injection("https://example.com/").unwrap();
        assert!(inj.js.contains("function nooperFn"), "js: {:?}", inj.js);
        assert!(inj.js.contains("nooperFn(\"1\")"), "invocation missing: {:?}", inj.js);
        assert_eq!(inj.names, vec!["nooper.js".to_string()]);
        let lib = c.scriptlets().library();
        assert!(lib.iter().any(|s| s.name == "nooper.js" && s.injectable));
    }

    #[test]
    fn redirect_rules_carry_a_decoded_stand_in_body() {
        // Redirects are not scriptlets: they must work with injection off too,
        // which is why the resource file loads regardless of that switch.
        for inject in [true, false] {
            let (b, _) = blocker_with_resources_inject(
                &["||ads.example.com/analytics.js^$script,redirect=noopjs"],
                serde_json::json!([{
                    "name": "noop.js",
                    "aliases": ["noopjs"],
                    "kind": {"mime": "application/javascript"},
                    "content": "Ly8gbm9vcA=="
                }]),
                inject,
            );
            let d = b.check("https://ads.example.com/analytics.js", "https://news.test/", "script");
            assert!(d.blocked);
            let r = d.redirect.expect("a $redirect rule must supply a body");
            assert_eq!(r.mime, "application/javascript");
            assert_eq!(r.body, b"// noop");
            assert_eq!(b.scriptlets_enabled(), inject, "the inject switch still gates injection");
        }
    }

    #[test]
    fn a_plain_block_has_no_stand_in_body() {
        let (b, _) = blocker_with(&["||ads.example.com^"]);
        let d = b.check("https://ads.example.com/x.js", "", "script");
        assert!(d.blocked);
        assert!(d.redirect.is_none(), "nothing to serve for a rule without $redirect");
        assert!(b.check("https://ok.example.com/x.js", "", "script").redirect.is_none());
    }

    #[test]
    fn removeparam_reports_the_cleaned_url_without_blocking() {
        let (b, _) = blocker_with(&["$removeparam=utm_source"]);
        let d = b.check("https://shop.example/item?id=7&utm_source=ad", "", "document");
        assert!(!d.blocked, "$removeparam cleans, it does not block");
        assert_eq!(d.rewritten_url.as_deref(), Some("https://shop.example/item?id=7"));
        let clean = b.check("https://shop.example/item?id=7", "", "document");
        assert!(clean.rewritten_url.is_none(), "nothing to strip, nothing to report");
    }

    #[test]
    fn the_switches_decide_what_a_decision_carries() {
        let (b, _) = blocker_with_resources_inject(
            &[
                "||ads.example.com/analytics.js^$script,redirect=noopjs",
                "$removeparam=utm_source",
            ],
            serde_json::json!([{
                "name": "noop.js",
                "aliases": ["noopjs"],
                "kind": {"mime": "application/javascript"},
                "content": "Ly8gbm9vcA=="
            }]),
            false,
        );
        let redirect = |b: &AdBlocker| {
            b.check("https://ads.example.com/analytics.js", "https://news.test/", "script")
        };
        let cleaned =
            |b: &AdBlocker| b.check("https://shop.example/i?id=7&utm_source=ad", "", "document");

        b.set_decisions(br#"{"redirect":false}"#).unwrap();
        assert!(redirect(&b).blocked, "the block itself is not a switch");
        assert!(redirect(&b).redirect.is_none(), "no stand-in body once it is off");
        assert!(cleaned(&b).rewritten_url.is_some(), "the other switch is untouched");

        b.set_decisions(br#"{"redirect":true,"removeparam":false}"#).unwrap();
        assert!(redirect(&b).redirect.is_some(), "back on");
        assert!(cleaned(&b).rewritten_url.is_none(), "no cleaned url once it is off");

        assert!(b.set_decisions(b"[]").is_err(), "adblock rejects what is not an object");
    }

    #[test]
    fn cosmetic_css_keeps_the_pure_css_rules_and_drops_the_ones_needing_a_dom() {
        let (b, _) = blocker_with(&[
            "example.com##.ad-banner",
            "example.com###sponsored",
            "example.com##body:style(overflow: auto !important)",
            "example.com##.promo:has-text(Ad)",
        ]);
        let css = b.cosmetic_css("https://example.com/", &[], &[]);
        assert!(css.contains(".ad-banner{display:none !important}\n"), "hide rule: {css}");
        assert!(css.contains("#sponsored{display:none !important}\n"), "hide rule: {css}");
        assert!(
            css.contains("body{overflow: auto !important}\n"),
            ":style() is pure CSS and must be emitted: {css}"
        );
        assert!(!css.contains(":has-text"), "an operator rule needs a live DOM: {css}");
        assert!(
            css.find("body{overflow").unwrap() > css.find(".ad-banner").unwrap(),
            "unbreak rules must come after the hide rules so they win: {css}"
        );
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn generic_cosmetic_rules_match_the_pages_classes_and_ids() {
        let (b, c) = blocker_with(&[]);
        c.add_list(
            "some-list",
            "https://x.example/l.txt",
            "##.adsbox\n###ad-container\n##.adbox.banner_ads".into(),
        )
        .unwrap();
        let css = b.cosmetic_css(
            "https://any.example/",
            &strings(&["adsbox", "adbox", "banner_ads"]),
            &strings(&["ad-container"]),
        );
        assert!(css.contains(".adsbox{display:none !important}\n"), "css was: {css}");
        assert!(css.contains("#ad-container{display:none !important}\n"), "css was: {css}");
        assert!(css.contains(".adbox.banner_ads{display:none !important}\n"), "css was: {css}");
        assert!(b.cosmetic_css("https://any.example/", &[], &[]).is_empty());
    }

    #[test]
    fn custom_generic_rules_inject_unconditionally() {
        let (b, _) = blocker_with(&["##.adsbygoogle", "example.com##+js(noop)", "##^script:has-text(x)"]);
        let css = b.cosmetic_css("https://any.example/", &[], &[]);
        assert!(css.contains(".adsbygoogle{display:none !important}\n"), "css was: {css}");
        assert!(!css.contains("+js"), "css was: {css}");
        assert!(!css.contains("^script"), "css was: {css}");
    }

    #[test]
    fn generichide_exception_suppresses_generic_rules_only() {
        let (b, _) = blocker_with(&[
            "##.adsbox",
            "quiet.example##.site-ad",
            "@@||quiet.example^$generichide",
        ]);
        let css = b.cosmetic_css("https://quiet.example/", &strings(&["adsbox"]), &[]);
        assert!(!css.contains(".adsbox"), "generic rule must be off here: {css}");
        assert!(css.contains(".site-ad"), "site-specific rule stays: {css}");
        let css = b.cosmetic_css("https://other.example/", &strings(&["adsbox"]), &[]);
        assert!(css.contains(".adsbox"), "css was: {css}");
    }

    #[test]
    fn github_blob_urls_are_normalized() {
        assert_eq!(
            normalize_list_url("https://github.com/org/repo/blob/main/list.txt"),
            "https://raw.githubusercontent.com/org/repo/main/list.txt"
        );
        let plain = "https://easylist.to/easylist/easylist.txt";
        assert_eq!(normalize_list_url(plain), plain);
    }

    #[test]
    fn tester_input_is_lax() {
        assert_eq!(normalize_test_url("https://a.com/x"), "https://a.com/x");
        assert_eq!(normalize_test_url("http://a.com"), "http://a.com");
        assert_eq!(normalize_test_url("a.com"), "https://a.com");
        assert_eq!(normalize_test_url(" ads.host.com/pixel?id=1 "), "https://ads.host.com/pixel?id=1");
        assert_eq!(normalize_test_url("//cdn.a.com/x.js"), "https://cdn.a.com/x.js");
        assert_eq!(normalize_test_url("/ads.js"), "https://any-host.invalid/ads.js");
    }
}
