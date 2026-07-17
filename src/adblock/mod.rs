//! Ad blocking: wraps the `adblock` crate engine and owns the filter lists
//! behind it.

use std::sync::{Arc, Mutex, RwLock};

use adblock::lists::{FilterSet, ParseOptions};
use adblock::request::Request;
use adblock::resources::Resource;
use adblock::Engine;

use crate::support::config::AdblockConfig;
use crate::support::error::Result;

mod curation;
pub mod maintenance;
mod scriptlets;
mod store;
pub mod updater;
pub use curation::{ListCuration, RulesUpdate};
pub(crate) use curation::normalize_list_url;
pub use scriptlets::{ScriptletInfo, ScriptletInjection, ScriptletLibrary};
pub use store::{DiskStore, ListId, ListStore, MemoryListStore, StoredList};

use scriptlets::ScriptletLibrary as Scriptlets;

pub const CUSTOM_LIST: &str = "custom";
const CUSTOM_SOURCE: &str = "config + ui";

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
}

impl BlockDecision {
    fn pass() -> Self {
        Self {
            blocked: false,
            attribution: BlockAttribution { rule: None, list: None },
        }
    }
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
    with_store(cfg, Arc::new(DiskStore::new(cfg.lists_dir.clone())))
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
        Arc::new(AdBlocker { core: core.clone() }),
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
        BlockDecision {
            blocked: result.matched,
            attribution: self.attribution(result.filter.as_deref()),
        }
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
        let mut css = String::new();
        for sel in selectors {
            if !is_plain_css_selector(&sel) {
                continue;
            }
            css.push_str(&sel);
            css.push_str("{display:none !important}\n");
        }
        css
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

fn is_plain_css_selector(selector: &str) -> bool {
    const PROCEDURAL: [&str; 12] = [
        ":has-text(",
        ":matches-css",
        ":matches-attr(",
        ":matches-path(",
        ":matches-media(",
        ":min-text-length(",
        ":upward(",
        ":xpath(",
        ":remove(",
        ":style(",
        ":watch-attr(",
        ":others(",
    ];
    !PROCEDURAL.iter().any(|op| selector.contains(op))
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
            lists_dir: PathBuf::from("/nonexistent-for-tests"),
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
        c.inject_scriptlets = true;
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
    fn cosmetic_css_drops_procedural_selectors_and_emits_one_rule_each() {
        let (b, _) = blocker_with(&[
            "example.com##.ad-banner",
            "example.com###sponsored",
            "example.com##.promo:has-text(Ad)",
        ]);
        let css = b.cosmetic_css("https://example.com/", &[], &[]);
        assert!(css.contains(".ad-banner{display:none !important}\n"), "css was: {css}");
        assert!(css.contains("#sponsored{display:none !important}\n"), "css was: {css}");
        assert!(!css.contains(":has-text"), "css was: {css}");
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
}
