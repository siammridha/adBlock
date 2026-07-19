//! Persisted list of domains that bypass MITM inspection and get a blind
//! tunnel instead.

use std::path::PathBuf;

use crate::proxy::error::{Error, Result};
use crate::proxy::persist::{Entry, PersistedSet};

const FILE_HEADER: &str = "# Domains that bypass MITM inspection (blind tunnel). Managed by the\n\
                           # admin UI; one host per line. Matches the exact host or any subdomain\n\
                           # (a leading *. is accepted and means the same thing).";

impl Entry for String {
    fn parse(line: &str) -> Option<Self> {
        let d = normalize(line);
        (!d.is_empty()).then_some(d)
    }

    fn format(&self) -> String {
        self.clone()
    }
}

pub struct ExclusionStore {
    domains: PersistedSet<String>,
}

impl ExclusionStore {
    pub fn load(path: PathBuf) -> Self {
        Self {
            domains: PersistedSet::load(path, FILE_HEADER, |domains| {
                domains.sort();
                domains.dedup();
            }),
        }
    }

    pub fn matching(&self, host: &str) -> Option<String> {
        self.domains.read(|domains| {
            domains
                .iter()
                .find(|d| host == d.as_str() || host.ends_with(&format!(".{d}")))
                .cloned()
        })
    }

    pub fn list(&self) -> Vec<String> {
        self.domains.snapshot()
    }

    pub fn add(&self, domain: &str) -> Result<Vec<String>> {
        let domain = normalize(domain);
        if domain.is_empty() {
            return Err(Error::Config("excluded domain is empty".into()));
        }
        self.domains.mutate(|domains| {
            if !domains.iter().any(|d| d == &domain) {
                domains.push(domain);
                domains.sort();
            }
            (domains.clone(), true)
        })
    }

    pub fn remove(&self, domain: &str) -> Result<bool> {
        let domain = normalize(domain);
        self.domains.mutate(|domains| {
            let before = domains.len();
            domains.retain(|d| d != &domain);
            let removed = domains.len() != before;
            (removed, removed)
        })
    }
}

fn normalize(domain: &str) -> String {
    let d = domain.trim().to_lowercase();
    let d = d.strip_prefix("*.").unwrap_or(&d);
    d.trim_matches('.').trim().to_string()
}

/// A raw admin command against the exclusion list. Callers (the web app) hand
/// bytes here and render the result; the proxy decides what is valid.
#[derive(Debug, PartialEq)]
pub enum ExclusionCommand {
    Add { domain: String },
    Delete { domain: String },
}

impl ExclusionCommand {
    pub fn parse(body: &[u8]) -> std::result::Result<Self, String> {
        let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        let Some(domain) = v.get("domain").and_then(serde_json::Value::as_str).map(str::trim)
        else {
            return Err("expected 'domain'".into());
        };
        let domain = domain.to_string();
        Ok(if v.get("delete").and_then(serde_json::Value::as_bool) == Some(true) {
            Self::Delete { domain }
        } else {
            Self::Add { domain }
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

    #[test]
    fn wildcard_normalizes_to_bare_domain() {
        let s = store("wild", &["*.sentryvault.com"]);
        assert_eq!(s.list(), vec!["sentryvault.com".to_string()]);
        assert_eq!(
            s.matching("xyz.sentryvault.com").as_deref(),
            Some("sentryvault.com")
        );
        assert_eq!(
            s.matching("sentryvault.com").as_deref(),
            Some("sentryvault.com")
        );
        assert_eq!(s.matching("notsentryvault.com"), None);
        assert!(s.remove("*.sentryvault.com").unwrap());
        assert_eq!(s.matching("xyz.sentryvault.com"), None);
    }

    #[test]
    fn add_is_idempotent_and_sorted() {
        let s = store("idem", &["b.com", "a.com", "b.com"]);
        assert_eq!(s.list(), vec!["a.com".to_string(), "b.com".to_string()]);
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
