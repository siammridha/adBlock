//! Persisted list of domains that bypass MITM inspection and get a blind
//! tunnel instead.

use std::path::PathBuf;

use crate::proxy::error::{Error, Result};
use crate::proxy::persist::{Entry, PersistedSet};

const FILE_HEADER: &str = "# Domains that bypass MITM inspection (blind tunnel). Managed by the\n\
                           # admin UI; one host per line. A bare domain (example.com) matches the\n\
                           # domain and every subdomain; a leading *. (*.example.com) matches only\n\
                           # subdomains, not the domain itself. A leading ! parks the entry: kept\n\
                           # and listed, but inspected like any other host.";

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Exclusion {
    pub domain: String,
    /// A `!` prefix on the line parks the entry: it stays listed, but traffic
    /// to it is inspected as if the entry were not there.
    pub enabled: bool,
}

impl Entry for Exclusion {
    fn parse(line: &str) -> Option<Self> {
        let (line, enabled) = match line.strip_prefix('!') {
            Some(rest) => (rest, false),
            None => (line, true),
        };
        let d = normalize(line);
        (!d.is_empty()).then_some(Exclusion { domain: d, enabled })
    }

    fn format(&self) -> String {
        let mark = if self.enabled { "" } else { "!" };
        format!("{mark}{}", self.domain)
    }
}

pub struct ExclusionStore {
    domains: PersistedSet<Exclusion>,
}

impl ExclusionStore {
    pub fn load(path: PathBuf) -> Self {
        Self {
            domains: PersistedSet::load(path, FILE_HEADER, |domains: &mut Vec<Exclusion>| {
                domains.sort_by(|a, b| a.domain.cmp(&b.domain));
                domains.dedup_by(|a, b| a.domain == b.domain);
            }),
        }
    }

    pub fn matching(&self, host: &str) -> Option<String> {
        self.domains.read(|domains| {
            domains
                .iter()
                .filter(|e| e.enabled)
                .find(|e| match e.domain.strip_prefix("*.") {
                    // "*.example.com" — subdomains only, never the apex itself.
                    Some(base) => host.ends_with(&format!(".{base}")),
                    // "example.com" — the domain itself and any subdomain.
                    None => host == e.domain || host.ends_with(&format!(".{}", e.domain)),
                })
                .map(|e| e.domain.clone())
        })
    }

    pub fn list(&self) -> Vec<Exclusion> {
        self.domains.snapshot()
    }

    pub fn add(&self, domain: &str) -> Result<()> {
        let domain = normalize(domain);
        if domain.is_empty() {
            return Err(Error::Config("excluded domain is empty".into()));
        }
        self.domains.mutate(|domains| {
            if domains.iter().any(|e| e.domain == domain) {
                return ((), false);
            }
            domains.push(Exclusion { domain, enabled: true });
            domains.sort_by(|a, b| a.domain.cmp(&b.domain));
            ((), true)
        })
    }

    pub fn remove(&self, domain: &str) -> Result<bool> {
        let domain = normalize(domain);
        self.domains.mutate(|domains| {
            let before = domains.len();
            domains.retain(|e| e.domain != domain);
            let removed = domains.len() != before;
            (removed, removed)
        })
    }

    /// Park or un-park one entry. `false` when there is no such entry.
    pub fn set_enabled(&self, domain: &str, enabled: bool) -> Result<bool> {
        let domain = normalize(domain);
        self.domains.mutate(|domains| {
            match domains.iter_mut().find(|e| e.domain == domain) {
                Some(e) if e.enabled != enabled => {
                    e.enabled = enabled;
                    (true, true)
                }
                Some(_) => (true, false),
                None => (false, false),
            }
        })
    }
}

fn normalize(domain: &str) -> String {
    let d = domain.trim().to_lowercase();
    // A leading "*." is kept as a marker meaning "subdomains only". A bare
    // domain means the domain itself and every subdomain.
    let (wildcard, rest) = match d.strip_prefix("*.") {
        Some(rest) => (true, rest),
        None => (false, d.as_str()),
    };
    let core = rest.trim_matches('.').trim();
    if core.is_empty() {
        String::new()
    } else if wildcard {
        format!("*.{core}")
    } else {
        core.to_string()
    }
}

/// A raw admin command against the exclusion list. Callers (the web app) hand
/// bytes here and render the result; the proxy decides what is valid.
#[derive(Debug, PartialEq)]
pub enum ExclusionCommand {
    Add { domain: String },
    Delete { domain: String },
    SetEnabled { domain: String, enabled: bool },
}

impl ExclusionCommand {
    pub fn parse(body: &[u8]) -> std::result::Result<Self, String> {
        let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        let Some(domain) = v.get("domain").and_then(serde_json::Value::as_str).map(str::trim)
        else {
            return Err("expected 'domain'".into());
        };
        let domain = domain.to_string();
        if v.get("delete").and_then(serde_json::Value::as_bool) == Some(true) {
            return Ok(Self::Delete { domain });
        }
        Ok(match v.get("enabled").and_then(serde_json::Value::as_bool) {
            Some(enabled) => Self::SetEnabled { domain, enabled },
            None => Self::Add { domain },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &str, domains: &[&str]) -> ExclusionStore {
        let path = std::env::temp_dir().join(format!("proxy-excl-{dir}.conf"));
        let _ = std::fs::remove_file(&path);
        let s = ExclusionStore::load(path);
        for d in domains {
            s.add(d).unwrap();
        }
        s
    }

    #[test]
    fn exclusion_commands_parse_before_any_io() {
        assert_eq!(
            ExclusionCommand::parse(br#"{"domain": "  bank.com "}"#).unwrap(),
            ExclusionCommand::Add { domain: "bank.com".into() }
        );
        assert_eq!(
            ExclusionCommand::parse(br#"{"domain": "bank.com", "delete": true}"#).unwrap(),
            ExclusionCommand::Delete { domain: "bank.com".into() }
        );
        assert!(matches!(
            ExclusionCommand::parse(br#"{"domain": "x.com", "delete": false}"#),
            Ok(ExclusionCommand::Add { .. })
        ));
        assert!(ExclusionCommand::parse(br#"{"delete": true}"#).is_err());
        assert!(ExclusionCommand::parse(b"not json").is_err());
    }

    #[test]
    fn matches_exact_and_subdomain() {
        let s = store("match", &["bank.com"]);
        assert_eq!(s.matching("bank.com").as_deref(), Some("bank.com"));
        assert_eq!(s.matching("secure.bank.com").as_deref(), Some("bank.com"));
        assert_eq!(s.matching("notbank.com"), None);
        assert_eq!(s.matching("bank.com.evil.com"), None);
    }

    fn domains(s: &ExclusionStore) -> Vec<String> {
        s.list().into_iter().map(|e| e.domain).collect()
    }

    #[test]
    fn wildcard_matches_subdomains_only() {
        let s = store("wild", &["*.sentryvault.com"]);
        assert_eq!(domains(&s), vec!["*.sentryvault.com".to_string()]);
        assert_eq!(
            s.matching("xyz.sentryvault.com").as_deref(),
            Some("*.sentryvault.com")
        );
        assert_eq!(
            s.matching("deep.xyz.sentryvault.com").as_deref(),
            Some("*.sentryvault.com")
        );
        // the apex is NOT matched by a wildcard entry
        assert_eq!(s.matching("sentryvault.com"), None);
        assert_eq!(s.matching("notsentryvault.com"), None);
        assert!(s.remove("*.sentryvault.com").unwrap());
        assert_eq!(s.matching("xyz.sentryvault.com"), None);
    }

    #[test]
    fn bare_and_wildcard_are_distinct_entries() {
        let s = store("distinct", &["apple.com", "*.example.com"]);
        // bare covers apex + subdomains
        assert_eq!(s.matching("apple.com").as_deref(), Some("apple.com"));
        assert_eq!(s.matching("gdmf.apple.com").as_deref(), Some("apple.com"));
        // wildcard covers subdomains only
        assert_eq!(s.matching("www.example.com").as_deref(), Some("*.example.com"));
        assert_eq!(s.matching("example.com"), None);
    }

    #[test]
    fn add_is_idempotent_and_sorted() {
        let s = store("idem", &["b.com", "a.com", "b.com"]);
        assert_eq!(domains(&s), vec!["a.com".to_string(), "b.com".to_string()]);
    }

    #[test]
    fn a_parked_entry_stays_listed_but_stops_matching() {
        let path = std::env::temp_dir().join("proxy-excl-parked.conf");
        let _ = std::fs::remove_file(&path);
        let s = ExclusionStore::load(path.clone());
        s.add("bank.com").unwrap();
        assert!(s.set_enabled("BANK.com", false).unwrap());
        assert_eq!(s.matching("secure.bank.com"), None, "parked entries never match");
        assert_eq!(domains(&s), vec!["bank.com".to_string()], "but they stay listed");
        assert!(!ExclusionStore::load(path.clone()).list()[0].enabled, "parked survives reload");
        assert!(!s.set_enabled("nope.com", true).unwrap(), "unknown domain reports missing");
        assert!(s.set_enabled("bank.com", true).unwrap());
        assert!(s.matching("bank.com").is_some());
    }

    #[test]
    fn remove_reports_presence() {
        let s = store("rm", &["bank.com"]);
        assert!(s.remove("BANK.com").unwrap());
        assert!(!s.remove("bank.com").unwrap());
        assert_eq!(s.matching("bank.com"), None);
    }

    #[test]
    fn survives_reload() {
        let path = std::env::temp_dir().join("proxy-excl-reload.conf");
        let _ = std::fs::remove_file(&path);
        ExclusionStore::load(path.clone()).add("bank.com").unwrap();
        let reloaded = ExclusionStore::load(path);
        assert!(reloaded.matching("secure.bank.com").is_some());
    }
}
