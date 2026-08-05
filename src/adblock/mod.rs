//! Ad blocking: wraps the `adblock` crate engine and owns the filter lists
//! behind it.

use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use adblock::cosmetic_filter_cache::ProceduralOrActionFilter;
use adblock::lists::{FilterSet, ParseOptions};
use adblock::request::Request;
use adblock::resources::Resource;
use adblock::Engine;

use error::Result;

pub mod api;
mod classify;
pub mod commands;
pub mod config;
mod curation;
pub mod error;
pub mod fetch;
pub mod maintenance;
mod rewrite;
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

/// The in-page evaluator for procedural cosmetic rules. What `:has-text` or
/// `:remove()` means is a filter-rule question, so the rules and the evaluator
/// that reads them both belong here.
const PROCEDURAL_RUNTIME: &str = include_str!("procedural_runtime.js");

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
    /// Where the compiled engine is cached, as the list store reports it.
    engine_cache: Option<std::path::PathBuf>,
}

impl EngineCore {
    fn rebuild(&self, lists: &[ListEntry]) {
        let resources = self.scriptlets.resources();
        let (engine, unsaved) = build_engine(lists, &resources, self.engine_cache.as_deref());
        let engine = Arc::new(engine);
        *self.engine.write().expect("engine lock") = engine.clone();
        if let (Some(path), Some(key)) = (self.engine_cache.clone(), unsaved) {
            cache_engine_in_background(path, key, engine);
        }
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
    /// Which rules Adblock may act on: `$redirect`, `$removeparam`, `$csp`,
    /// and what it puts into a page. Adblock's own switches — the caller hands
    /// over a request or a response and never asks for a rule to be applied.
    decisions: settings::DecisionPolicy,
    /// The live-DOM cosmetic script, built once from the admin address the root
    /// wiring hands over. Unset (or `None`) means no admin server for a page to
    /// ask, so nothing is injected.
    runtime: std::sync::OnceLock<Option<String>>,
}

#[derive(Clone, Debug)]
pub struct ListEntry {
    pub name: String,
    pub source: String,
    pub rules: usize,
    /// A disabled list stays stored and listed, but none of its rules reach the
    /// engine.
    pub enabled: bool,
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
    /// Adblock applies it itself in `filter_request`; it is kept here so that
    /// call does not have to match the rules again.
    rewritten_url: Option<String>,
    /// Content-Security-Policy directives a `$csp` rule wants added to this
    /// page. Adblock adds them itself in `filter_response`; this is kept in the
    /// decision so the second call does not have to match the rules again. Only
    /// ever set for a document or subdocument request that was not blocked — a
    /// blocked page has no response to add a header to.
    csp: Option<String>,
    /// Adblock will want to read this response's body. The caller uses it to
    /// ask upstream for an unencoded body and to buffer the response instead of
    /// streaming it; it never decides for itself what happens to the bytes.
    pub wants_body: bool,
    /// The `$type` this request was matched as. Adblock names it; the caller
    /// only reports it.
    pub req_type: String,
}

impl BlockDecision {
    fn pass() -> Self {
        Self {
            blocked: false,
            attribution: BlockAttribution { rule: None, list: None },
            redirect: None,
            rewritten_url: None,
            csp: None,
            wants_body: false,
            req_type: String::new(),
        }
    }
}

/// What Adblock made of a response. The caller forwards this and nothing else.
#[derive(Default)]
pub struct ResponseEdit {
    /// The page as Adblock rewrote it. `None` when it left the body alone.
    pub body: Option<Vec<u8>>,
    /// Scriptlets that went into the page, for the caller's log. Empty unless
    /// the page was rewritten.
    pub scriptlets: Vec<String>,
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

    // Startup is dominated by these two steps — reading the lists, then getting an
    // engine out of them — so both say how long they took.
    let started = std::time::Instant::now();
    let stored = store.load();
    let outcome = curation::reconcile(&mut lists, stored);
    tracing::info!(
        lists = lists.len(),
        bytes = lists.iter().map(|l| l.text.len()).sum::<usize>(),
        ms = started.elapsed().as_millis(),
        "blocklists read"
    );
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
        enabled: true,
        text,
    });

    let engine_cache = store.engine_cache();
    let started = std::time::Instant::now();
    let (engine, unsaved) = build_engine(&lists, &scriptlets.resources(), engine_cache.as_deref());
    tracing::info!(ms = started.elapsed().as_millis(), "engine ready");
    let engine = Arc::new(engine);
    if let (Some(path), Some(key)) = (engine_cache.clone(), unsaved) {
        cache_engine_in_background(path, key, engine.clone());
    }
    let core = Arc::new(EngineCore {
        enabled: cfg.enabled,
        engine: RwLock::new(engine),
        lists: Mutex::new(lists),
        scriptlets,
        engine_cache,
    });
    Ok((
        Arc::new(AdBlocker {
            core: core.clone(),
            decisions: settings::DecisionPolicy::load(cfg.settings_path()),
            runtime: std::sync::OnceLock::new(),
        }),
        Arc::new(ListCuration::new(core, store)),
    ))
}

impl AdBlocker {
    /// Whether Adblock does anything at all: the config switch it was built
    /// with, and the master switch the dashboard turns. Off, nothing matches
    /// and no page is touched.
    pub fn enabled(&self) -> bool {
        self.core.enabled && self.decisions.settings().enabled
    }

    /// What happens to this request, as it arrived. Adblock reads the resource
    /// type and the page it came from off the request itself — both are things
    /// filter rules match on, so neither is the caller's to decide.
    pub fn check_request<B>(&self, url: &str, req: &hyper::Request<B>) -> BlockDecision {
        let req_type = classify::request_type(req);
        let source = classify::source_url(req, url, &req_type);
        let decision = self.check(url, &source, &req_type);
        // A `navigator.sendBeacon()` call arrives as an ordinary no-cors POST,
        // so it was matched above as a `fetch`. Most `$ping` rules name only
        // that type, and would miss it. Ask a second time as a `ping` — but
        // only for a request already shaped like a beacon, and only when
        // nothing matched it the first way, so an ordinary fetch keeps its own
        // verdict and the extra lookup stays off the common path.
        if !decision.blocked && req_type != "ping" && classify::is_beacon_shaped(req) {
            let as_beacon = self.check(url, &source, "ping");
            if as_beacon.blocked {
                tracing::debug!(%url, "matched as a beacon, not a fetch");
                return as_beacon;
            }
        }
        decision
    }

    /// Whether a rule blocks the host outright — one like `||ads.example^`,
    /// which also matches the host's root document. A rule that only blocks a
    /// specific resource does not.
    pub fn check_host(&self, scheme: &str, host: &str) -> BlockDecision {
        self.check(&format!("{scheme}://{host}/"), "", "document")
    }

    pub fn check(&self, url: &str, source_url: &str, request_type: &str) -> BlockDecision {
        BlockDecision {
            req_type: request_type.to_string(),
            ..self.match_request(url, source_url, request_type)
        }
    }

    fn match_request(&self, url: &str, source_url: &str, request_type: &str) -> BlockDecision {
        if !self.enabled() {
            return BlockDecision::pass();
        }
        // Only a page the browser renders can be rewritten; a script or an
        // image has nothing to inject into.
        let renders_a_page = matches!(request_type, "document" | "subdocument");
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
            // `$csp` only ever applies to a page the browser is going to render,
            // and the engine itself ignores every other request type, so this
            // costs nothing on the requests it does not concern.
            csp: (allow.csp && !result.matched)
                .then(|| engine.get_csp_directives(&request))
                .flatten(),
            wants_body: renders_a_page && !result.matched && allow.injects(),
            // Filled in by `check`, which is the only way in here.
            req_type: String::new(),
        }
    }

    /// The response a blocked request gets. A `$redirect` rule supplies a
    /// stand-in body — a neutered copy of the real resource — so the page's own
    /// code keeps running; without one, a block is an empty `403`. What a block
    /// looks like is a filtering decision, so it is made here and the caller
    /// only sends it.
    pub fn blocked_response(&self, decision: BlockDecision) -> hyper::Response<Vec<u8>> {
        let forbidden = || {
            hyper::Response::builder()
                .status(hyper::StatusCode::FORBIDDEN)
                .body(Vec::new())
                .expect("static blocked response is always valid")
        };
        let Some(redirect) = decision.redirect else { return forbidden() };
        // The mime comes from the resource file, so a malformed one falls back
        // to the plain block rather than failing the request.
        hyper::Response::builder()
            .status(hyper::StatusCode::OK)
            .header(hyper::header::CONTENT_TYPE, redirect.mime)
            .body(redirect.body)
            .unwrap_or_else(|_| forbidden())
    }

    /// Where a filtered page sends the questions it has after it was served.
    /// Root wiring: Adblock embeds the address it is handed, once, at startup.
    /// Never set means no admin server, so no live-DOM script goes out.
    pub fn set_admin_endpoint(&self, admin_listen: &str) {
        let _ = self.runtime.set(rewrite::cosmetic_runtime(admin_listen));
    }

    /// Apply the request-side rules to the request the caller is about to send:
    /// the `$removeparam` cleaned URL, and asking upstream for a body Adblock
    /// can read when it means to read one.
    pub fn filter_request(
        &self,
        decision: &BlockDecision,
        parts: &mut hyper::http::request::Parts,
    ) {
        if decision.wants_body {
            parts.headers.insert(
                hyper::header::ACCEPT_ENCODING,
                hyper::header::HeaderValue::from_static("identity"),
            );
        }
        // The browser is never told, so the parameter stays in its address bar;
        // only the request the site receives is stripped.
        if let Some(clean) = &decision.rewritten_url {
            if let Some(uri) = rewrite::rewrite_uri(&parts.uri, clean) {
                tracing::debug!(from = %parts.uri, to = %clean, "stripped tracking parameters");
                parts.uri = uri;
            }
        }
    }

    /// Whether Adblock needs this response's body. The caller buffers the
    /// response only when the answer is yes, and streams it otherwise.
    pub fn reads_body(&self, decision: &BlockDecision, status: u16, headers: &hyper::HeaderMap) -> bool {
        decision.wants_body && rewrite::response_is_editable(status, headers)
    }

    /// Turn the response the server sent into the response to forward.
    ///
    /// `body` is the collected page when the caller was told to buffer it
    /// (`reads_body`), and `None` otherwise — a streamed response can still
    /// pick up the header a `$csp` rule asks for.
    pub fn filter_response(
        &self,
        url: &str,
        decision: &BlockDecision,
        parts: &mut hyper::http::response::Parts,
        body: Option<&[u8]>,
    ) -> ResponseEdit {
        let edit = body.and_then(|b| self.edit_page(url, parts, b)).unwrap_or_default();
        // Appended rather than set, because two CSP headers are enforced
        // together and the site's own policy has to keep applying. This happens
        // after the page edit on purpose — injecting strips the site's CSP so
        // our own inline script can run, and adding ours before that would
        // strip it again.
        if let Some(csp) = &decision.csp {
            match hyper::header::HeaderValue::from_str(csp) {
                Ok(v) => {
                    tracing::debug!(%url, %csp, "adding content-security-policy");
                    parts.headers.append(hyper::header::CONTENT_SECURITY_POLICY, v);
                }
                Err(e) => tracing::warn!(error = %e, %csp, "unusable $csp directives"),
            }
        }
        edit
    }

    /// Splice this page's cosmetic CSS, scriptlets and runtime into it.
    /// `None` when there was nothing to put in, or the page is too big to be
    /// worth copying.
    fn edit_page(
        &self,
        url: &str,
        parts: &mut hyper::http::response::Parts,
        body: &[u8],
    ) -> Option<ResponseEdit> {
        if body.len() > rewrite::MAX_EDIT_BYTES {
            return None;
        }
        let on = self.decisions.settings();
        let scriptlets = on.scriptlets.then(|| self.scriptlet_injection(url)).flatten();
        // The scan below sees the page only as it was served, so on a page that
        // builds itself in JavaScript the runtime is the only thing that can ask
        // about the class and id names appearing later. Its own switch: it costs
        // a script tag and a request per page, which is worth turning off
        // separately from the CSS.
        let runtime = match on.runtime {
            true => self.runtime.get().and_then(Option::as_deref).unwrap_or(""),
            false => "",
        };
        // Procedural rules — `:has-text`, `:upward`, `:remove()` — are cosmetic
        // filtering that a stylesheet cannot carry, so they ride the cosmetic
        // switch and go out with the page rather than being asked for later.
        let procedural = on
            .cosmetic
            .then(|| self.procedural_injection(url))
            .flatten()
            .unwrap_or_default();
        let script = [
            scriptlets.as_ref().map_or("", |i| i.js.as_str()),
            procedural.as_str(),
            runtime,
        ]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
        // Only the class/id scan needs text, and it can be sloppy about bad
        // bytes: the names it finds are used to look up rules and never written
        // back into the page, so a mangled character just means one rule misses.
        let css = match on.cosmetic {
            true => {
                let (classes, ids) = rewrite::html_classes_and_ids(&String::from_utf8_lossy(body));
                self.cosmetic_css(url, &classes, &ids)
            }
            false => String::new(),
        };
        let out = rewrite::inject_into_html(body, &css, &script)?;
        // Any inline script of ours needs the page's CSP out of the way,
        // scriptlet or runtime alike.
        if !script.is_empty() {
            for h in rewrite::CSP_HEADERS {
                parts.headers.remove(h);
            }
        }
        parts.headers.remove(hyper::header::CONTENT_LENGTH);
        Some(ResponseEdit {
            body: Some(out),
            scriptlets: scriptlets.map(|i| i.names).unwrap_or_default(),
        })
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

    fn cosmetic_css(&self, url: &str, classes: &[String], ids: &[String]) -> String {
        if !self.enabled() {
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
        if !self.enabled() || (classes.is_empty() && ids.is_empty()) {
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

    /// The finished evaluator for the cosmetic rules a stylesheet cannot
    /// express: pick by the text inside an element, by an ancestor, by computed
    /// style, by XPath, and delete a node, an attribute or a class rather than
    /// only hiding it.
    ///
    /// Ready-to-inject JavaScript carrying this page's own rules, like
    /// `cosmetic_css` and `scriptlet_injection`. `None` when the page has no
    /// such rules, which is the common case.
    ///
    /// The rules go in as a JSON literal, so `<` is escaped: a rule matching on
    /// `</script>` text would otherwise end the tag it is sitting in. Inside
    /// JSON the character can only appear in a string, where `<` means the
    /// same thing.
    fn procedural_injection(&self, url: &str) -> Option<String> {
        let rules = self.procedural_actions(url);
        (!rules.is_empty()).then(|| {
            PROCEDURAL_RUNTIME.replace("__PROCEDURAL_FILTERS__", &rules.replace('<', "\\u003c"))
        })
    }

    /// This page's procedural rules as the engine's own JSON array. The rules
    /// that do reduce to CSS went out with `cosmetic_css` instead, so nothing
    /// is applied twice.
    fn procedural_actions(&self, url: &str) -> String {
        if !self.enabled() {
            return String::new();
        }
        let engine = self.core.engine.read().expect("engine lock").clone();
        let mut rules: Vec<String> = engine
            .url_cosmetic_resources(url)
            .procedural_actions
            .into_iter()
            .filter(|json| {
                serde_json::from_str::<ProceduralOrActionFilter>(json)
                    .is_ok_and(|f| f.as_css().is_none())
            })
            .collect();
        if rules.is_empty() {
            return String::new();
        }
        rules.sort();
        format!("[{}]", rules.join(","))
    }

    pub fn scriptlets_enabled(&self) -> bool {
        self.core.scriptlets.enabled()
    }

    fn scriptlet_injection(&self, url: &str) -> Option<ScriptletInjection> {
        if !self.enabled() || !self.scriptlets_enabled() {
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
                .filter(|l| l.enabled)
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

/// Bump when what goes into the engine changes in a way the rules themselves do
/// not show — the parse options, the debug/optimize flags, or an upgrade of the
/// `adblock` dependency. Every cached engine written under an older number is
/// then ignored and rebuilt.
const ENGINE_CACHE_FORMAT: u32 = 1;

/// The enabled lists in a fixed order, so the same rules always give the same
/// engine and the same cache key however the store handed them over. The disk
/// store lists a directory, and that order is not stable — writing the cache
/// file into it is enough to change it — so an order taken as given would miss
/// the cache on every start and rewrite it every time.
fn enabled_in_order(lists: &[ListEntry]) -> Vec<&ListEntry> {
    let mut out: Vec<&ListEntry> = lists.iter().filter(|l| l.enabled).collect();
    // Names are unique once reconciled; the text breaks a tie so the order is
    // total either way.
    out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.text.cmp(&b.text)));
    out
}

/// Compiling the lists costs seconds; reading back the compiled form costs
/// milliseconds. The key covers every rule that went in, so any edit to a list —
/// its text, its name, or its enabled flag — misses the cache and the engine is
/// built from the rules again.
fn engine_key(lists: &[ListEntry]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ENGINE_CACHE_FORMAT.hash(&mut h);
    for l in enabled_in_order(lists) {
        l.name.hash(&mut h);
        l.text.hash(&mut h);
    }
    h.finish()
}

/// The engine, and the key to cache it under when it had to be built from the
/// rules. `None` means it came out of the cache and is already on disk.
fn build_engine(
    lists: &[ListEntry],
    resources: &[Resource],
    cache: Option<&Path>,
) -> (Engine, Option<u64>) {
    let key = engine_key(lists);
    if let Some(path) = cache {
        if let Some(mut engine) = load_cached_engine(path, key) {
            if !resources.is_empty() {
                engine.use_resources(resources.iter().cloned());
            }
            tracing::info!(path = %path.display(), "engine loaded from cache");
            return (engine, None);
        }
    }

    let mut filter_set = FilterSet::new(true);
    for l in enabled_in_order(lists) {
        filter_set.add_filter_list(&l.text, ParseOptions::default());
    }
    let mut engine = Engine::from_filter_set(filter_set, false);
    if !resources.is_empty() {
        engine.use_resources(resources.iter().cloned());
    }
    (engine, cache.map(|_| key))
}

/// `None` whenever the cache cannot be used — missing, truncated, written for a
/// different set of rules, or in a format this build of the `adblock` crate does
/// not read. Every one of those just means building the engine from the rules.
fn load_cached_engine(path: &Path, key: u64) -> Option<Engine> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() < 8 || u64::from_le_bytes(bytes[..8].try_into().ok()?) != key {
        return None;
    }
    // `false` to match `from_filter_set` below: the rules are kept as written
    // rather than combined, so a matched rule can be reported as it was typed.
    let mut engine = Engine::new(false);
    match engine.deserialize(&bytes[8..]) {
        Ok(()) => Some(engine),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = ?e, "ignoring unreadable engine cache");
            None
        }
    }
}

/// Writing the cache takes seconds, and rebuilding the engine also happens when
/// a list is added, removed, or switched over from the admin UI — so the write
/// runs on its own thread and nobody waits for it. The engine is already in use
/// by then; the cache only matters to the next start.
fn cache_engine_in_background(path: std::path::PathBuf, key: u64, engine: Arc<Engine>) {
    std::thread::spawn(move || save_cached_engine(&path, key, &engine));
}

/// Best effort: a cache that cannot be written costs startup time and nothing
/// else, so a failure is logged and the engine is used as built.
fn save_cached_engine(path: &Path, key: u64, engine: &Engine) {
    // Scriptlet and `$redirect` resources are not part of the serialized form;
    // they are applied to the engine again on the way back in.
    let body = match engine.serialize_raw() {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = ?e, "serializing engine for cache");
            return;
        }
    };
    let started = std::time::Instant::now();
    let mut buf = key.to_le_bytes().to_vec();
    buf.extend_from_slice(&body);
    // Write beside the target and rename, so an interrupted write leaves the
    // previous cache in place instead of a half-written one. The name carries the
    // key, so two rebuilds in quick succession cannot write over each other's
    // half-finished file — the rename picks a winner instead.
    let tmp = path.with_extension(format!("{key:x}.tmp"));
    let written = std::fs::write(&tmp, &buf).and_then(|()| std::fs::rename(&tmp, path));
    match written {
        Ok(()) => tracing::info!(
            path = %path.display(),
            bytes = buf.len(),
            ms = started.elapsed().as_millis(),
            "engine cached"
        ),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "writing engine cache");
            let _ = std::fs::remove_file(&tmp);
        }
    }
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
    fn the_engine_cache_round_trips_and_is_keyed_to_the_rules() {
        let dir = std::env::temp_dir().join(format!(
            "sp-engine-cache-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("engine.dat");

        let named = |name: &str, text: &str| ListEntry {
            name: name.into(),
            source: "test".into(),
            rules: 1,
            enabled: true,
            text: text.into(),
        };
        let entry = |text: &str| named("test", text);
        let lists = vec![entry("||ads.example.com^")];
        let hits = |e: &Engine| {
            e.check_network_request(
                &Request::new("https://ads.example.com/x.png", "https://news.org/", "image")
                    .unwrap(),
            )
            .matched
        };

        // The engine read back from the cache blocks the same request, so the
        // rules survived the round trip. Saved here rather than through the
        // background writer so the test does not race it.
        let (built, unsaved) = build_engine(&lists, &[], Some(&path));
        assert!(hits(&built));
        save_cached_engine(&path, unsaved.expect("a fresh build must want saving"), &built);
        assert!(path.exists(), "the cache should have been written");
        let cached = load_cached_engine(&path, engine_key(&lists)).expect("cache should load");
        assert!(hits(&cached));

        // The order the store happens to hand the lists over in is not stable and
        // must not reach the key, or every start would miss and rewrite the cache.
        let shuffled = vec![named("b", "||b.example.com^"), named("a", "||a.example.com^")];
        let mut reordered = shuffled.clone();
        reordered.reverse();
        assert_eq!(
            engine_key(&shuffled),
            engine_key(&reordered),
            "the same lists in another order are the same engine"
        );

        // A changed rule, a renamed list, and a disabled list each change the key.
        for changed in [
            vec![entry("||other.example.com^")],
            vec![ListEntry { name: "renamed".into(), ..entry("||ads.example.com^") }],
            vec![ListEntry { enabled: false, ..entry("||ads.example.com^") }],
        ] {
            assert!(
                load_cached_engine(&path, engine_key(&changed)).is_none(),
                "a different set of rules must not load the old engine"
            );
        }

        // A truncated or junk file is ignored rather than trusted.
        std::fs::write(&path, b"junk").unwrap();
        assert!(load_cached_engine(&path, engine_key(&lists)).is_none());

        std::fs::remove_dir_all(&dir).unwrap();
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
    fn the_master_switch_stops_every_kind_of_filtering() {
        let (b, _) = blocker_with(&["||ads.example.com^", "example.com##.ad-slot"]);
        let url = "https://ads.example.com/banner.png";
        assert!(b.check(url, "", "image").blocked);
        assert!(!b.cosmetic_css("https://example.com/", &["ad-slot".into()], &[]).is_empty());

        b.set_decisions(br#"{"enabled":false}"#).unwrap();
        assert!(!b.enabled());
        assert!(!b.check(url, "", "image").blocked, "nothing matches with blocking off");
        assert!(!b.check_host("https", "ads.example.com").blocked);
        assert!(!b.check_dns("ads.example.com").blocked);
        assert!(b.cosmetic_css("https://example.com/", &["ad-slot".into()], &[]).is_empty());

        b.set_decisions(br#"{"enabled":true}"#).unwrap();
        assert!(b.check(url, "", "image").blocked, "and it comes back on");
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
    fn a_disabled_list_keeps_its_rules_out_of_the_engine() {
        let store = Arc::new(MemoryListStore::new());
        let (b, c) = with_store(&cfg(&[]), store.clone()).unwrap();
        c.add_list("ads", "https://x.example/l.txt", "||ads.example^".into()).unwrap();
        assert!(b.check("https://ads.example/x", "", "image").blocked);

        assert!(c.set_list_enabled("ads", false).unwrap());
        assert!(!b.check("https://ads.example/x", "", "image").blocked);
        assert!(c.lists().iter().any(|l| l.name == "ads"), "still listed");
        assert!(!c.set_list_enabled("nope", false).unwrap(), "unknown list reports missing");

        // Off survives a reload, and a refresh of the same list does not
        // switch it back on.
        let (b2, c2) = with_store(&cfg(&[]), store.clone()).unwrap();
        assert!(!b2.check("https://ads.example/x", "", "image").blocked);
        c2.add_list("ads", "https://x.example/l.txt", "||ads.example^\n||more.example^".into())
            .unwrap();
        assert!(!b2.check("https://more.example/x", "", "image").blocked);

        assert!(c2.set_list_enabled("ads", true).unwrap());
        assert!(b2.check("https://more.example/x", "", "image").blocked);
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
    fn a_stand_in_can_be_a_binary_file_and_is_never_injectable() {
        // 1x1.gif, uBO's transparent pixel, as raw bytes rather than text.
        let gif: [u8; 14] = [
            0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 1, 0, 1, 0, 0x80, 0, 0, 0,
        ];
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        let (b, c) = blocker_with_resources(
            &["||ads.example.com/px.gif^$image,redirect=1x1-transparent.gif"],
            serde_json::json!([{
                "name": "1x1.gif",
                "aliases": ["1x1-transparent.gif"],
                "kind": {"mime": "image/gif"},
                "content": STANDARD.encode(gif)
            }]),
        );
        let d = b.check("https://ads.example.com/px.gif", "https://news.test/", "image");
        assert!(d.blocked);
        let r = d.redirect.expect("an image rule must serve an image");
        assert_eq!(r.mime, "image/gif");
        assert_eq!(r.body, gif, "the bytes must survive the round trip");

        let lib = c.scriptlets().library();
        let entry = lib.iter().find(|s| s.name == "1x1.gif").unwrap();
        assert!(!entry.injectable, "a stand-in file is served, never injected");
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
                "$csp=worker-src 'none',domain=shop.example",
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
        let cleaned = |b: &AdBlocker| {
            b.check(
                "https://shop.example/i?id=7&utm_source=ad",
                "https://shop.example/",
                "document",
            )
        };

        b.set_decisions(br#"{"redirect":false}"#).unwrap();
        assert!(redirect(&b).blocked, "the block itself is not a switch");
        assert!(redirect(&b).redirect.is_none(), "no stand-in body once it is off");
        assert!(cleaned(&b).rewritten_url.is_some(), "the other switches are untouched");
        assert!(cleaned(&b).csp.is_some());

        b.set_decisions(br#"{"redirect":true,"removeparam":false,"csp":false}"#).unwrap();
        assert!(redirect(&b).redirect.is_some(), "back on");
        assert!(cleaned(&b).rewritten_url.is_none(), "no cleaned url once it is off");
        assert!(cleaned(&b).csp.is_none(), "no directives once it is off");

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
    fn procedural_rules_go_out_as_json_for_the_page_to_evaluate() {
        let (b, _) = blocker_with(&[
            "example.com##.promo:has-text(Ad)",
            "example.com##.wrap:upward(2)",
            "example.com##.tile:remove()",
            "example.com##.box:remove-attr(onclick)",
            "example.com##.ad-banner",
            "example.com##body:style(overflow: auto !important)",
        ]);
        let json = b.procedural_actions("https://example.com/");
        let rules: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(rules.len(), 4, "only the rules plain CSS cannot carry: {json}");
        assert!(json.contains(r#"{"type":"has-text","arg":"Ad"}"#), "{json}");
        assert!(json.contains(r#"{"type":"upward","arg":"2"}"#), "{json}");
        assert!(json.contains(r#""action":{"type":"remove"}"#), "{json}");
        assert!(json.contains(r#""action":{"type":"remove-attr","arg":"onclick"}"#), "{json}");
        assert!(!json.contains("ad-banner"), "a plain hide rule is CSS, not procedural: {json}");
        assert!(!json.contains("overflow"), ":style() alone is CSS too: {json}");

        assert!(b.procedural_actions("https://other.test/").is_empty(), "wrong site, no rules");

        // The `#?#` family is the same thing written the ABP way.
        let (b, _) = blocker_with(&["example.com#?#.promo:-abp-contains(Ad)"]);
        let json = b.procedural_actions("https://example.com/");
        assert!(json.contains(r#"{"type":"has-text","arg":"Ad"}"#), "{json}");

        // Every entry starts with the selector chain, which is what the
        // in-page evaluator walks.
        assert!(rules.iter().all(|r| r["selector"].is_array()), "{json}");
    }

    #[test]
    fn the_evaluator_comes_back_finished_and_carrying_its_rules() {
        let (b, _) = blocker_with(&["example.com##.promo:has-text(Ad)"]);
        let js = b.procedural_injection("https://example.com/").unwrap();
        assert!(js.contains(r#"{"type":"has-text","arg":"Ad"}"#), "{js}");
        assert!(!js.contains("__PROCEDURAL_FILTERS__"), "placeholder must be replaced");
        assert!(b.procedural_injection("https://other.test/").is_none(), "no rules, no script");

        // A rule that matches on markup must not be able to close the tag it is
        // sitting in.
        let (b, _) = blocker_with(&["example.com##.promo:has-text(</script>)"]);
        let js = b.procedural_injection("https://example.com/").unwrap();
        assert!(!js.contains("</script>"), "{js}");
        assert!(js.contains("u003c/script>"), "escaped, still the same string: {js}");
    }

    /// The evaluator ships as text and only ever runs in a browser, so a syntax
    /// error in it would reach every page before anyone noticed. Node parses it
    /// here as a stand-in; skipped where node is not installed.
    #[test]
    fn the_evaluator_parses_as_javascript() {
        if std::process::Command::new("node").arg("-v").output().is_err() {
            eprintln!("node not installed; skipping the evaluator syntax check");
            return;
        }
        let (b, _) = blocker_with(&["example.com##.promo:has-text(</b>)"]);
        let js = b.procedural_injection("https://example.com/").unwrap();
        let path =
            std::env::temp_dir().join(format!("adblock-js-check-{}.js", std::process::id()));
        std::fs::write(&path, &js).unwrap();
        let out = std::process::Command::new("node").arg("--check").arg(&path).output().unwrap();
        assert!(
            out.status.success(),
            "the evaluator does not parse: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn csp_rules_hand_their_directives_to_the_caller() {
        let (b, _) = blocker_with(&[
            "||ads.example.com^",
            "$csp=worker-src 'none',domain=example.com",
        ]);
        let page = b.check("https://example.com/", "https://example.com/", "document");
        assert!(!page.blocked);
        assert_eq!(page.csp.as_deref(), Some("worker-src 'none'"));

        let frame = b.check("https://example.com/f", "https://example.com/", "subdocument");
        assert_eq!(frame.csp.as_deref(), Some("worker-src 'none'"), "frames carry it too");

        let script = b.check("https://example.com/a.js", "https://example.com/", "script");
        assert!(script.csp.is_none(), "$csp is only for a page the browser renders");
        let other = b.check("https://other.test/", "https://other.test/", "document");
        assert!(other.csp.is_none(), "wrong site");
        let blocked = b.check("https://ads.example.com/", "https://example.com/", "document");
        assert!(blocked.blocked);
        assert!(blocked.csp.is_none(), "a blocked page has no response to add a header to");
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
