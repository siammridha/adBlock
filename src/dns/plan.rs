//! Decides what to do with an incoming query: reject, block, answer from a
//! rewrite, or forward upstream.

use hickory_proto::op::{Message, OpCode, Query, ResponseCode};
use hickory_proto::rr::RecordType;

use crate::adblock::AdBlocker;

use super::cache;
use super::rewrites::{RewriteAnswer, RewriteStore};

pub(super) enum QueryPlan {
    Invalid(ResponseCode),
    Answer {
        query: Query,
        domain: String,
        verdict: Verdict,
    },
}

pub(super) enum Verdict {
    Nodata,
    Rewrite(Vec<RewriteAnswer>),
    Blocked { attribution: String },
    Resolve(cache::Key),
}

pub(super) fn plan_query(
    request: &Message,
    rewrites: &RewriteStore,
    adblock: &AdBlocker,
) -> QueryPlan {
    if request.metadata.op_code != OpCode::Query {
        return QueryPlan::Invalid(ResponseCode::NotImp);
    }
    let Some(query) = request.queries.first().cloned() else {
        return QueryPlan::Invalid(ResponseCode::FormErr);
    };
    let domain = query
        .name()
        .to_utf8()
        .trim_end_matches('.')
        .to_ascii_lowercase();

    let verdict = if query.query_type() == RecordType::AAAA {
        Verdict::Nodata
    } else {
        let rewrite_answers = rewrites.matching(&domain);
        if !rewrite_answers.is_empty() {
            Verdict::Rewrite(rewrite_answers)
        } else {
            let decision = adblock.check_dns(&domain);
            if decision.blocked {
                Verdict::Blocked { attribution: decision.attribution.display() }
            } else {
                Verdict::Resolve(cache::Key::of(&query))
            }
        }
    };
    QueryPlan::Answer { query, domain, verdict }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adblock::{with_store, MemoryListStore};
    use crate::support::config::AdblockConfig;
    use hickory_proto::rr::Name;
    use std::path::PathBuf;
    use std::str::FromStr;
    use std::sync::Arc;

    fn blocker(rules: &[&str]) -> Arc<AdBlocker> {
        let cfg = AdblockConfig {
            enabled: true,
            custom_rules: rules.iter().map(|s| s.to_string()).collect(),
            data_dir: PathBuf::from("/nonexistent-for-tests"),
            auto_update_hours: 0,
            inject_scriptlets: false,
            scriptlet_resources: PathBuf::new(),
        };
        with_store(&cfg, Arc::new(MemoryListStore::new())).unwrap().0
    }

    fn empty_rewrites() -> RewriteStore {
        RewriteStore::load(PathBuf::from("/nonexistent-for-tests/rewrites.conf"))
    }

    fn rewrites(tag: &str) -> RewriteStore {
        let path = std::env::temp_dir().join(format!("proxy-dns-plan-{tag}.conf"));
        let _ = std::fs::remove_file(&path);
        RewriteStore::load(path)
    }

    fn request(domain: &str, qtype: RecordType) -> Message {
        let mut msg = Message::query();
        msg.add_query(Query::query(Name::from_str(domain).unwrap(), qtype));
        msg
    }

    fn verdict(msg: &Message, rewrites: &RewriteStore, adblock: &AdBlocker) -> Verdict {
        match plan_query(msg, rewrites, adblock) {
            QueryPlan::Answer { verdict, .. } => verdict,
            QueryPlan::Invalid(code) => panic!("expected a verdict, got Invalid({code:?})"),
        }
    }

    #[test]
    fn malformed_requests_are_rejected_before_any_lookup() {
        let (rw, ab) = (empty_rewrites(), blocker(&[]));
        let mut msg = request("example.com.", RecordType::A);
        msg.metadata.op_code = OpCode::Update;
        assert!(matches!(
            plan_query(&msg, &rw, &ab),
            QueryPlan::Invalid(ResponseCode::NotImp)
        ));
        assert!(matches!(
            plan_query(&Message::query(), &rw, &ab),
            QueryPlan::Invalid(ResponseCode::FormErr)
        ));
    }

    #[test]
    fn aaaa_gate_answers_first_even_for_rewritten_and_blocked_domains() {
        let rw = rewrites("aaaa");
        rw.add("app.example.com", "10.0.0.1").unwrap();
        let ab = blocker(&["||app.example.com^"]);
        assert!(matches!(
            verdict(&request("app.example.com.", RecordType::AAAA), &rw, &ab),
            Verdict::Nodata
        ));
    }

    #[test]
    fn rewrites_win_over_blocklists() {
        let rw = rewrites("precedence");
        rw.add("app.example.com", "10.1.2.3").unwrap();
        let ab = blocker(&["||app.example.com^"]);
        let Verdict::Rewrite(answers) =
            verdict(&request("app.example.com.", RecordType::A), &rw, &ab)
        else {
            panic!("rewrite must win over the blocklist");
        };
        assert_eq!(answers, vec![RewriteAnswer::V4([10, 1, 2, 3].into())]);
    }

    #[test]
    fn blocked_domains_never_get_a_cache_key() {
        let (rw, ab) = (empty_rewrites(), blocker(&["||ads.example.com^"]));
        let Verdict::Blocked { attribution } =
            verdict(&request("ads.example.com.", RecordType::A), &rw, &ab)
        else {
            panic!("filter must block");
        };
        assert!(attribution.contains("||ads.example.com^"), "attribution: {attribution}");
    }

    #[test]
    fn clean_domains_fall_through_to_resolve_with_a_cache_key() {
        let (rw, ab) = (empty_rewrites(), blocker(&["||ads.example.com^"]));
        assert!(matches!(
            verdict(&request("fine.example.com.", RecordType::A), &rw, &ab),
            Verdict::Resolve(_)
        ));
    }

    #[test]
    fn domain_is_canonicalized_before_matching() {
        let (rw, ab) = (empty_rewrites(), blocker(&["||ads.example.com^"]));
        let QueryPlan::Answer { domain, verdict, .. } =
            plan_query(&request("Sub.ADS.Example.COM.", RecordType::A), &rw, &ab)
        else {
            panic!("well-formed query");
        };
        assert_eq!(domain, "sub.ads.example.com");
        assert!(matches!(verdict, Verdict::Blocked { .. }));
    }
}
