//! End-to-end tests over the public library surface (no live network):
//! config loading and ad-block network/cosmetic filtering.

use std::path::PathBuf;
use std::sync::Arc;

use adBlock::adblock::api::{AdBlocker, AdblockConfig, ListCuration, MemoryListStore};
use adBlock::proxy::api::{ExclusionStore, ServerConfig};

fn blocker(rules: &[&str]) -> Arc<AdBlocker> {
    parts(rules).0
}

/// The query + curation pair over an in-memory store.
fn parts(rules: &[&str]) -> (Arc<AdBlocker>, Arc<ListCuration>) {
    let cfg = AdblockConfig {
        enabled: true,
        custom_rules: rules.iter().map(|s| s.to_string()).collect(),
        data_dir: PathBuf::from("/nonexistent-for-tests"),
        auto_update_hours: 0,
        inject_scriptlets: false,
        scriptlet_resources: PathBuf::new(),
    };
    adBlock::adblock::api::with_store(&cfg, Arc::new(MemoryListStore::new())).unwrap()
}

#[test]
fn unparseable_listen_addrs_fail_validation() {
    // Proxy owns its listen address and validates it.
    let mut cfg = ServerConfig::default();
    cfg.listen = "not-an-addr".into();
    assert!(cfg.validate().is_err());

    let cfg = ServerConfig::default();
    assert!(cfg.validate().is_ok());
    // admin_listen is validated by the root wiring, not by Proxy, so it is not
    // exercised here.
}

#[test]
fn excluded_domain_matching() {
    let path = std::env::temp_dir().join("proxy-it-excl.conf");
    let _ = std::fs::remove_file(&path);
    let excl = ExclusionStore::load(path);
    excl.add("bank.com").unwrap();
    assert_eq!(excl.matching("bank.com").as_deref(), Some("bank.com"));
    assert_eq!(excl.matching("secure.bank.com").as_deref(), Some("bank.com"));
    assert_eq!(excl.matching("notbank.com"), None);
}

#[test]
fn network_rule_blocks_host() {
    let b = blocker(&["||ads.example.com^"]);
    assert!(b.check("https://ads.example.com/x.js", "", "script").blocked);
    assert!(!b.check("https://cdn.example.org/x.js", "", "script").blocked);
}

/// Adblock rewrites the page itself: the caller hands over the bytes it
/// received and forwards the bytes it gets back.
#[test]
fn cosmetic_rule_is_spliced_into_the_page() {
    let b = blocker(&["example.com##.ad-banner"]);
    let page = b"<html><head></head><body>x</body></html>";

    let filtered = |url: &str| -> String {
        let decision = b.check(url, url, "document");
        assert!(decision.wants_body, "a page is a page Adblock wants to read");
        let mut parts = hyper::Response::builder().status(200).body(()).unwrap().into_parts().0;
        let edit = b.filter_response(url, &decision, &mut parts, Some(page));
        String::from_utf8(edit.body.unwrap_or_else(|| page.to_vec())).unwrap()
    };

    let html = filtered("https://example.com/");
    assert!(html.contains(".ad-banner"), "page was: {html}");
    assert!(html.contains("display:none"));
    // A different site shouldn't get this site-specific rule.
    assert!(!filtered("https://other.test/").contains(".ad-banner"));
}
