//! Local DNS records ("rewrites"): persisted to a text file and edited from
//! the admin UI.

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;

use crate::support::error::{Error, Result};
use crate::support::persist::{Entry, PersistedSet};

const FILE_HEADER: &str = "# DNS rewrites (local records). Managed by the admin UI; one\n\
                           # \"domain answer\" per line. *.domain covers the domain and subdomains.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RewriteAnswer {
    V4(Ipv4Addr),
    Cname(String),
}

impl RewriteAnswer {
    pub fn parse(s: &str) -> std::result::Result<Self, String> {
        let s = s.trim().trim_end_matches('.');
        if s.is_empty() {
            return Err("answer is empty".into());
        }
        if let Ok(v4) = s.parse::<Ipv4Addr>() {
            return Ok(Self::V4(v4));
        }
        if s.parse::<Ipv6Addr>().is_ok() {
            return Err(format!("'{s}': IPv6 answers are not supported (IPv4-only resolver)"));
        }
        let host = s.to_ascii_lowercase();
        if !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        {
            return Err(format!("'{s}' is not an IP address or a hostname"));
        }
        Ok(Self::Cname(host))
    }
}

impl fmt::Display for RewriteAnswer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::V4(ip) => ip.fmt(f),
            Self::Cname(host) => host.fmt(f),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rewrite {
    pub domain: String,
    pub answer: RewriteAnswer,
}

impl Entry for Rewrite {
    fn parse(line: &str) -> Option<Self> {
        let (domain, answer) = line.split_once(char::is_whitespace)?;
        Some(Rewrite {
            domain: normalize_pattern(domain).ok()?,
            answer: RewriteAnswer::parse(answer).ok()?,
        })
    }

    fn format(&self) -> String {
        format!("{} {}", self.domain, self.answer)
    }
}

pub struct RewriteStore {
    rewrites: PersistedSet<Rewrite>,
}

impl RewriteStore {
    pub fn load(path: PathBuf) -> Self {
        Self { rewrites: PersistedSet::load(path, FILE_HEADER, |_| {}) }
    }

    pub fn matching(&self, domain: &str) -> Vec<RewriteAnswer> {
        self.rewrites.read(|rewrites| {
            let best = rewrites
                .iter()
                .filter(|r| pattern_matches(&r.domain, domain))
                .map(|r| r.domain.as_str())
                .max_by_key(|p| pattern_specificity(p));
            let Some(best) = best else { return Vec::new() };
            rewrites
                .iter()
                .filter(|r| r.domain == best)
                .map(|r| r.answer.clone())
                .collect()
        })
    }

    pub fn list(&self) -> Vec<Rewrite> {
        self.rewrites.snapshot()
    }

    pub fn add(&self, domain: &str, answer: &str) -> Result<()> {
        let domain = normalize_pattern(domain)?;
        let answer = RewriteAnswer::parse(answer).map_err(Error::Config)?;
        if matches!(&answer, RewriteAnswer::Cname(t) if *t == domain.trim_start_matches("*.")) {
            return Err(Error::Config(format!("CNAME '{answer}' points at itself")));
        }
        let entry = Rewrite { domain, answer };
        self.rewrites.mutate(|rewrites| {
            if rewrites.contains(&entry) {
                return (Ok(()), false);
            }
            let same_pattern = || rewrites.iter().filter(|r| r.domain == entry.domain);
            let conflict = match &entry.answer {
                RewriteAnswer::Cname(_) => same_pattern().next().is_some(),
                RewriteAnswer::V4(_) => {
                    same_pattern().any(|r| matches!(r.answer, RewriteAnswer::Cname(_)))
                }
            };
            if conflict {
                return (
                    Err(Error::Config(format!(
                        "'{}' already has a rewrite that cannot coexist with '{}' — \
                         a CNAME excludes every other answer for a domain; remove the \
                         existing rewrite first",
                        entry.domain, entry.answer
                    ))),
                    false,
                );
            }
            rewrites.push(entry);
            rewrites.sort_by(|a, b| {
                (&a.domain, a.answer.to_string()).cmp(&(&b.domain, b.answer.to_string()))
            });
            (Ok(()), true)
        })?
    }

    pub fn remove(&self, domain: &str, answer: &str) -> Result<bool> {
        let domain = normalize_pattern(domain)?;
        let answer = RewriteAnswer::parse(answer).map_err(Error::Config)?;
        self.rewrites.mutate(|rewrites| {
            let before = rewrites.len();
            rewrites.retain(|r| !(r.domain == domain && r.answer == answer));
            let removed = rewrites.len() != before;
            (removed, removed)
        })
    }
}

fn pattern_matches(pattern: &str, domain: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(suffix) => domain == suffix || domain.ends_with(&format!(".{suffix}")),
        None => domain == pattern,
    }
}

fn pattern_specificity(pattern: &str) -> (bool, usize) {
    (!pattern.starts_with("*."), pattern.len())
}

fn normalize_pattern(domain: &str) -> Result<String> {
    let raw = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    let (wild, host) = match raw.strip_prefix("*.") {
        Some(h) => (true, h),
        None => (false, raw.as_str()),
    };
    let host = host.trim_matches('.');
    if host.is_empty()
        || host.contains('*')
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return Err(Error::Config(format!("invalid rewrite domain '{domain}'")));
    }
    Ok(if wild { format!("*.{host}") } else { host.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(tag: &str) -> RewriteStore {
        let path = std::env::temp_dir().join(format!("proxy-rewrites-{tag}.conf"));
        let _ = std::fs::remove_file(&path);
        RewriteStore::load(path)
    }

    #[test]
    fn parses_all_answer_shapes() {
        assert_eq!(RewriteAnswer::parse("1.2.3.4"), Ok(RewriteAnswer::V4([1, 2, 3, 4].into())));
        assert_eq!(
            RewriteAnswer::parse("Target.Example.com."),
            Ok(RewriteAnswer::Cname("target.example.com".into()))
        );
        let err = RewriteAnswer::parse("::1").unwrap_err();
        assert!(err.contains("IPv6"), "err: {err}");
        assert!(RewriteAnswer::parse("").is_err());
        assert!(RewriteAnswer::parse("bad host").is_err());
        assert!(RewriteAnswer::parse("http://x").is_err());
    }

    #[test]
    fn exact_and_wildcard_matching() {
        let s = store("match");
        s.add("app.example.com", "1.2.3.4").unwrap();
        s.add("*.lab.example", "10.0.0.1").unwrap();
        assert_eq!(s.matching("app.example.com").len(), 1);
        assert!(s.matching("sub.app.example.com").is_empty());
        assert_eq!(s.matching("lab.example").len(), 1);
        assert_eq!(s.matching("a.b.lab.example").len(), 1);
        assert!(s.matching("notlab.example").is_empty());
    }

    #[test]
    fn multiple_answers_per_domain_and_pair_removal() {
        let s = store("multi");
        s.add("dual.example", "1.2.3.4").unwrap();
        s.add("dual.example", "5.6.7.8").unwrap();
        s.add("dual.example", "1.2.3.4").unwrap();
        assert_eq!(s.matching("dual.example").len(), 2);
        assert!(s.remove("dual.example", "5.6.7.8").unwrap());
        assert!(!s.remove("dual.example", "5.6.7.8").unwrap());
        assert_eq!(s.matching("dual.example").len(), 1);
    }

    #[test]
    fn a_cname_cannot_coexist_with_other_answers_on_one_pattern() {
        let s = store("mix");
        s.add("app.example", "1.2.3.4").unwrap();
        let err = s.add("app.example", "alias.example").unwrap_err();
        assert!(err.to_string().contains("cannot coexist"), "err: {err}");
        s.add("alias.example", "target.example").unwrap();
        assert!(s.add("alias.example", "5.6.7.8").is_err());
        assert!(s.add("alias.example", "other.example").is_err());
        assert!(s.remove("alias.example", "target.example").unwrap());
        s.add("alias.example", "5.6.7.8").unwrap();
    }

    #[test]
    fn most_specific_pattern_answers_alone() {
        let s = store("specificity");
        s.add("*.lab.example", "cname.target.example").unwrap();
        s.add("host.lab.example", "10.0.0.7").unwrap();
        assert_eq!(
            s.matching("host.lab.example"),
            vec![RewriteAnswer::V4([10, 0, 0, 7].into())]
        );
        assert_eq!(
            s.matching("other.lab.example"),
            vec![RewriteAnswer::Cname("cname.target.example".into())]
        );
        s.add("*.example", "1.1.1.1").unwrap();
        assert_eq!(
            s.matching("x.lab.example"),
            vec![RewriteAnswer::Cname("cname.target.example".into())]
        );
    }

    #[test]
    fn rejects_self_cname_and_bad_patterns() {
        let s = store("bad");
        assert!(s.add("loop.example", "loop.example").is_err());
        assert!(s.add("", "1.2.3.4").is_err());
        assert!(s.add("a*b.example", "1.2.3.4").is_err());
        assert!(s.add("x.example", "not an ip or host").is_err());
    }

    #[test]
    fn survives_reload() {
        let path = std::env::temp_dir().join("proxy-rewrites-reload.conf");
        let _ = std::fs::remove_file(&path);
        let s = RewriteStore::load(path.clone());
        s.add("App.Example.COM", "1.2.3.4").unwrap();
        s.add("*.lab.example", "cname.target.example").unwrap();
        let reloaded = RewriteStore::load(path);
        assert_eq!(reloaded.list().len(), 2);
        assert_eq!(reloaded.matching("app.example.com"), vec![RewriteAnswer::V4([1, 2, 3, 4].into())]);
        assert_eq!(
            reloaded.matching("x.lab.example"),
            vec![RewriteAnswer::Cname("cname.target.example".into())]
        );
    }
}
