//! Raw admin-command parsing for DNS-owned inputs. Callers (the web app)
//! hand bytes here and render the result; DNS decides what is valid.

use serde_json::Value;

use super::DnsOverrides;

#[derive(Debug, PartialEq)]
pub enum RewriteCommand {
    Add { domain: String, answer: String },
    Delete { domain: String, answer: String },
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
        Ok(if v.get("delete").and_then(Value::as_bool) == Some(true) {
            Self::Delete { domain, answer }
        } else {
            Self::Add { domain, answer }
        })
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
            min_ttl_secs: v.get("min_ttl_secs").and_then(Value::as_u64).map(|n| n as u32),
            max_ttl_secs: v.get("max_ttl_secs").and_then(Value::as_u64).map(|n| n as u32),
            ech_probe_domain: v
                .get("ech_probe_domain")
                .and_then(Value::as_str)
                .map(|s| s.trim().to_string()),
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
    fn dns_config_commands_reset_wins_and_bad_modes_are_named() {
        assert_eq!(
            DnsConfigCommand::parse(br#"{"reset": true, "cache_size": 5}"#).unwrap(),
            DnsConfigCommand::Reset
        );
        let DnsConfigCommand::Apply(upd) = DnsConfigCommand::parse(
            br#"{"upstreams": [" udp://1.1.1.1:53 ", ""], "min_ttl_secs": 30}"#,
        )
        .unwrap() else {
            panic!("expected Apply");
        };
        assert_eq!(upd.upstreams, Some(vec!["udp://1.1.1.1:53".to_string()]));
        assert_eq!(upd.min_ttl_secs, Some(30));
        assert_eq!(upd.max_ttl_secs, None);
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
