//! Raw admin-command parsing for DNS-owned inputs. Callers (the web app)
//! hand bytes here and render the result; DNS decides what is valid.

use serde_json::Value;

use super::DnsOverrides;

#[derive(Debug, PartialEq)]
pub enum RewriteCommand {
    Add { domain: String, answer: String },
    Delete { domain: String, answer: String },
    SetEnabled { domain: String, answer: String, enabled: bool },
}

impl RewriteCommand {
    pub fn parse(body: &[u8]) -> Result<Self, String> {
        let v: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        let (Some(domain), Some(answer)) = (
            v.get("domain").and_then(Value::as_str),
            v.get("answer").and_then(Value::as_str),
        ) else {
            return Err("expected 'domain' and 'answer'".into());
        };
        let (domain, answer) = (domain.to_string(), answer.to_string());
        if v.get("delete").and_then(Value::as_bool) == Some(true) {
            return Ok(Self::Delete { domain, answer });
        }
        Ok(match v.get("enabled").and_then(Value::as_bool) {
            Some(enabled) => Self::SetEnabled { domain, answer, enabled },
            None => Self::Add { domain, answer },
        })
    }
}

/// One edit to the upstream server list. The web app posts the raw form
/// fields; DNS decides what is a valid server.
#[derive(Debug, PartialEq)]
pub enum UpstreamCommand {
    /// Add a server by name, hostname-or-IPv4, and transport. DNS resolves the
    /// host and stores the plain address.
    Add { name: String, host: String, scheme: &'static str },
    Delete { spec: String },
    SetEnabled { spec: String, enabled: bool },
}

impl UpstreamCommand {
    pub fn parse(body: &[u8]) -> Result<Self, String> {
        let v: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        let spec = |v: &Value| {
            v.get("spec")
                .and_then(Value::as_str)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "expected 'spec'".to_string())
        };
        if v.get("delete").and_then(Value::as_bool) == Some(true) {
            return Ok(Self::Delete { spec: spec(&v)? });
        }
        if let Some(enabled) = v.get("enabled").and_then(Value::as_bool) {
            return Ok(Self::SetEnabled { spec: spec(&v)?, enabled });
        }
        let field = |key: &str| {
            v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
        };
        let (name, host) = (field("name"), field("host"));
        if name.is_empty() {
            return Err("a name is required".into());
        }
        if name.contains('#') {
            return Err("the name cannot contain '#'".into());
        }
        if host.is_empty() {
            return Err("a hostname or IPv4 address is required".into());
        }
        // A host pasted with its own transport ("tls://dns.example") means that
        // transport, whatever the picker says.
        let (typed_scheme, host) = match host.split_once("://") {
            Some((s, h)) => (Some(s.trim().to_lowercase()), h.trim().to_string()),
            None => (None, host),
        };
        if host.is_empty() {
            return Err("a hostname or IPv4 address is required".into());
        }
        let scheme = typed_scheme
            .as_deref()
            .unwrap_or_else(|| v.get("scheme").and_then(Value::as_str).unwrap_or("https"));
        let scheme = match scheme {
            "https" => "https",
            "tls" | "dot" => "tls",
            "udp" => "udp",
            "tcp" => "tcp",
            other => return Err(format!("unknown transport '{other}'")),
        };
        Ok(Self::Add { name, host, scheme })
    }
}

#[derive(Debug, PartialEq)]
pub enum DnsConfigCommand {
    Reset,
    Apply(DnsOverrides),
}

impl DnsConfigCommand {
    pub fn parse(body: &[u8]) -> Result<Self, String> {
        let v: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        if v.get("reset").and_then(Value::as_bool) == Some(true) {
            return Ok(Self::Reset);
        }
        let str_list = |key: &str| -> Option<Vec<String>> {
            v.get(key).and_then(Value::as_array).map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
        };
        let upstream_mode = match v.get("upstream_mode") {
            None | Some(Value::Null) => None,
            Some(m) => Some(serde_json::from_value(m.clone()).map_err(|_| {
                "upstream_mode must be \"failover\", \"load-balance\", or \"parallel\"".to_string()
            })?),
        };
        Ok(Self::Apply(DnsOverrides {
            upstreams: str_list("upstreams"),
            upstream_mode,
            bootstrap: str_list("bootstrap"),
            cache_size: v.get("cache_size").and_then(Value::as_u64).map(|n| n as usize),
            override_min_ttl_secs: v.get("override_min_ttl_secs").and_then(Value::as_u64).map(|n| n as u32),
            override_max_ttl_secs: v.get("override_max_ttl_secs").and_then(Value::as_u64).map(|n| n as u32),
            ech_probe_domain: v
                .get("ech_probe_domain")
                .and_then(Value::as_str)
                .map(|s| s.trim().to_string()),
            ech_probe_mins: v.get("ech_probe_mins").and_then(Value::as_u64).map(|n| n as u32),
            strip_ech: v.get("strip_ech").and_then(Value::as_bool),
            log_ipv6: v.get("log_ipv6").and_then(Value::as_bool),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_commands_need_both_fields() {
        assert_eq!(
            RewriteCommand::parse(br#"{"domain": "app.example", "answer": "1.2.3.4"}"#).unwrap(),
            RewriteCommand::Add { domain: "app.example".into(), answer: "1.2.3.4".into() }
        );
        assert_eq!(
            RewriteCommand::parse(
                br#"{"domain": "app.example", "answer": "1.2.3.4", "delete": true}"#
            )
            .unwrap(),
            RewriteCommand::Delete { domain: "app.example".into(), answer: "1.2.3.4".into() }
        );
        assert!(RewriteCommand::parse(br#"{"domain": "app.example"}"#).is_err());
        assert!(RewriteCommand::parse(br#"{"answer": "1.2.3.4"}"#).is_err());
    }

    #[test]
    fn upstream_commands_need_a_name_a_host_and_a_known_transport() {
        assert_eq!(
            UpstreamCommand::parse(br#"{"name": " Cloudflare ", "host": " 1.1.1.1 ", "scheme": "tls"}"#)
                .unwrap(),
            UpstreamCommand::Add {
                name: "Cloudflare".into(),
                host: "1.1.1.1".into(),
                scheme: "tls",
            }
        );
        // DoH is the default transport.
        let UpstreamCommand::Add { scheme, .. } =
            UpstreamCommand::parse(br#"{"name": "n", "host": "h"}"#).unwrap()
        else {
            panic!("expected Add");
        };
        assert_eq!(scheme, "https");

        // A host pasted with its own transport keeps it, picker notwithstanding.
        assert_eq!(
            UpstreamCommand::parse(
                br#"{"name": "NextDNS", "host": "tls://dns.nextdns.io", "scheme": "https"}"#
            )
            .unwrap(),
            UpstreamCommand::Add {
                name: "NextDNS".into(),
                host: "dns.nextdns.io".into(),
                scheme: "tls",
            }
        );
        // A pasted DoH URL keeps its path.
        assert_eq!(
            UpstreamCommand::parse(
                br#"{"name": "n", "host": "https://dns.example/dns-query", "scheme": "udp"}"#
            )
            .unwrap(),
            UpstreamCommand::Add {
                name: "n".into(),
                host: "dns.example/dns-query".into(),
                scheme: "https",
            }
        );

        assert_eq!(
            UpstreamCommand::parse(br#"{"spec": "tls://1.1.1.1", "delete": true}"#).unwrap(),
            UpstreamCommand::Delete { spec: "tls://1.1.1.1".into() }
        );
        assert_eq!(
            UpstreamCommand::parse(br#"{"spec": "tls://1.1.1.1", "enabled": false}"#).unwrap(),
            UpstreamCommand::SetEnabled { spec: "tls://1.1.1.1".into(), enabled: false }
        );

        for (body, want) in [
            (&br#"{"host": "1.1.1.1"}"#[..], "name is required"),
            (br#"{"name": "a#b", "host": "1.1.1.1"}"#, "cannot contain '#'"),
            (br#"{"name": "n"}"#, "hostname or IPv4"),
            (br#"{"name": "n", "host": "h", "scheme": "quic"}"#, "unknown transport"),
            (br#"{"name": "n", "host": "quic://h"}"#, "unknown transport"),
            (br#"{"name": "n", "host": "tls://"}"#, "hostname or IPv4"),
            (br#"{"delete": true}"#, "expected 'spec'"),
        ] {
            let err = UpstreamCommand::parse(body).unwrap_err();
            assert!(err.contains(want), "{err} should mention {want}");
        }
    }

    #[test]
    fn dns_config_commands_reset_wins_and_bad_modes_are_named() {
        assert_eq!(
            DnsConfigCommand::parse(br#"{"reset": true, "cache_size": 5}"#).unwrap(),
            DnsConfigCommand::Reset
        );
        let DnsConfigCommand::Apply(upd) = DnsConfigCommand::parse(
            br#"{"upstreams": [" udp://1.1.1.1:53 ", ""], "override_min_ttl_secs": 30}"#,
        )
        .unwrap() else {
            panic!("expected Apply");
        };
        assert_eq!(upd.upstreams, Some(vec!["udp://1.1.1.1:53".to_string()]));
        assert_eq!(upd.override_min_ttl_secs, Some(30));
        assert_eq!(upd.override_max_ttl_secs, None);
        assert_eq!(upd.upstream_mode, None);
        assert_eq!(upd.ech_probe_domain, None);
        let DnsConfigCommand::Apply(upd) =
            DnsConfigCommand::parse(br#"{"ech_probe_domain": " example.com "}"#).unwrap()
        else {
            panic!("expected Apply");
        };
        assert_eq!(upd.ech_probe_domain, Some("example.com".to_string()));
        let DnsConfigCommand::Apply(upd) =
            DnsConfigCommand::parse(br#"{"ech_probe_domain": ""}"#).unwrap()
        else {
            panic!("expected Apply");
        };
        assert_eq!(upd.ech_probe_domain, Some(String::new()));
        let err = DnsConfigCommand::parse(br#"{"upstream_mode": "quantum"}"#).unwrap_err();
        assert!(err.contains("failover"), "err: {err}");
    }
}
