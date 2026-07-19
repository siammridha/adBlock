//! Persisted list of domains kept out of the statistics: excluded hosts are not
//! counted, don't appear in the 24-hour history or top-domain tables, aren't
//! written to the request/query logs, and aren't pushed to the live dashboard.

use std::path::PathBuf;

use crate::stats::error::{Error, Result};
use crate::stats::persist::{Entry, PersistedSet};

const FILE_HEADER: &str = "# Domains excluded from statistics and logging. Managed by the admin UI;\n\
                           # one host per line. Matches the exact host or any subdomain (a leading\n\
                           # *. is accepted and means the same thing).";

impl Entry for String {
    fn parse(line: &str) -> Option<Self> {
        let d = normalize(line);
        (!d.is_empty()).then_some(d)
    }

    fn format(&self) -> String {
        self.clone()
    }
}

pub struct StatsExclusions {
    domains: PersistedSet<String>,
}

impl StatsExclusions {
    pub fn load(path: PathBuf) -> Self {
        Self {
            domains: PersistedSet::load(path, FILE_HEADER, |domains| {
                domains.sort();
                domains.dedup();
            }),
        }
    }

    /// True if `host` (or a parent domain of it) is excluded from stats.
    pub fn excludes(&self, host: &str) -> bool {
        if host.is_empty() {
            return false;
        }
        self.domains.read(|domains| {
            domains.iter().any(|d| host == d.as_str() || host.ends_with(&format!(".{d}")))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn store(tag: &str) -> StatsExclusions {
        let path = std::env::temp_dir().join(format!("proxy-stats-excl-{tag}.conf"));
        let _ = std::fs::remove_file(&path);
        StatsExclusions::load(path)
    }

    #[test]
    fn matches_exact_subdomain_and_normalizes() {
        let s = store("match");
        s.add("*.Noise.example").unwrap();
        assert_eq!(s.list(), vec!["noise.example".to_string()]);
        assert!(s.excludes("noise.example"));
        assert!(s.excludes("api.noise.example"));
        assert!(!s.excludes("noise.example.evil.com"));
        assert!(!s.excludes(""));
        assert!(s.remove("noise.example").unwrap());
        assert!(!s.excludes("api.noise.example"));
    }

    #[test]
    fn survives_reload() {
        let path = std::env::temp_dir().join("proxy-stats-excl-reload.conf");
        let _ = std::fs::remove_file(&path);
        StatsExclusions::load(path.clone()).add("noise.example").unwrap();
        assert!(StatsExclusions::load(path).excludes("sub.noise.example"));
    }
}
