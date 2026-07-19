//! Raw admin-command parsing for adblock-owned inputs. Callers (the web app)
//! hand bytes here and render the result; adblock decides what is valid.

use serde_json::Value;

use super::RulesUpdate;

#[derive(Debug, PartialEq)]
pub enum BlocklistCommand {
    Delete { name: String },
    AddUrl { url: String },
    ApplyRules {
        name: Option<String>,
        rules: String,
        update: RulesUpdate,
    },
}

impl BlocklistCommand {
    pub fn parse(body: &[u8]) -> Result<Self, String> {
        let v: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        if v.get("delete").and_then(Value::as_bool) == Some(true) {
            let name = v
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.trim().is_empty())
                .ok_or("delete needs 'name'")?
                .to_string();
            return Ok(BlocklistCommand::Delete { name });
        }
        if let Some(url) = v.get("url").and_then(Value::as_str) {
            return Ok(BlocklistCommand::AddUrl {
                url: url.trim().to_string(),
            });
        }
        let Some(rules) = v.get("rules").and_then(Value::as_str) else {
            return Err("expected {\"url\": …} or {\"rules\": …}".into());
        };
        Ok(BlocklistCommand::ApplyRules {
            name: v.get("name").and_then(Value::as_str).map(str::to_string),
            rules: rules.to_string(),
            update: if v.get("replace").and_then(Value::as_bool) == Some(true) {
                RulesUpdate::Replace
            } else {
                RulesUpdate::Append
            },
        })
    }
}

/// A rule-tester request: which URL to check, as what request type, from
/// which source page. The URL is normalized the same way the tester UI
/// expects (scheme added, whitespace trimmed).
#[derive(Debug, PartialEq)]
pub struct RuleTest {
    pub url: String,
    pub req_type: String,
    pub source: String,
}

impl RuleTest {
    pub fn parse(body: &[u8]) -> Result<Self, String> {
        let v: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        let url = match v.get("url").and_then(Value::as_str) {
            Some(u) if !u.trim().is_empty() => super::normalize_test_url(u),
            _ => return Err("missing 'url'".into()),
        };
        Ok(RuleTest {
            url,
            req_type: v.get("type").and_then(Value::as_str).unwrap_or("other").to_string(),
            source: v.get("source").and_then(Value::as_str).unwrap_or("").to_string(),
        })
    }
}

/// A DNS rule-tester request: the domain to check, normalized the way DNS
/// matching normalizes (trimmed, trailing dot removed, lowercased).
#[derive(Debug, PartialEq)]
pub struct DnsRuleTest {
    pub domain: String,
}

impl DnsRuleTest {
    pub fn parse(body: &[u8]) -> Result<Self, String> {
        let v: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        let domain = v
            .get("domain")
            .and_then(Value::as_str)
            .map(|d| d.trim().trim_end_matches('.').to_ascii_lowercase())
            .unwrap_or_default();
        if domain.is_empty() {
            return Err("missing 'domain'".into());
        }
        Ok(DnsRuleTest { domain })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> BlocklistCommand {
        BlocklistCommand::parse(body.as_bytes()).unwrap()
    }

    #[test]
    fn pasted_rules_default_to_append_on_the_custom_list() {
        assert_eq!(
            parse(r#"{"rules": "||ads.example^"}"#),
            BlocklistCommand::ApplyRules {
                name: None,
                rules: "||ads.example^".into(),
                update: RulesUpdate::Append,
            }
        );
    }

    #[test]
    fn replace_is_opt_in_and_names_are_kept() {
        assert_eq!(
            parse(r#"{"name": "mine", "rules": "||a^", "replace": true}"#),
            BlocklistCommand::ApplyRules {
                name: Some("mine".into()),
                rules: "||a^".into(),
                update: RulesUpdate::Replace,
            }
        );
        assert!(matches!(
            parse(r#"{"rules": "||a^", "replace": false}"#),
            BlocklistCommand::ApplyRules { update: RulesUpdate::Append, .. }
        ));
    }

    #[test]
    fn url_wins_over_rules_and_is_trimmed() {
        assert_eq!(
            parse(r#"{"url": "  https://x/l.txt  "}"#),
            BlocklistCommand::AddUrl { url: "https://x/l.txt".into() }
        );
    }

    #[test]
    fn delete_takes_precedence_and_needs_a_name() {
        assert_eq!(
            parse(r#"{"name": "easylist", "delete": true}"#),
            BlocklistCommand::Delete { name: "easylist".into() }
        );
        assert!(BlocklistCommand::parse(br#"{"delete": true}"#).is_err());
        assert!(BlocklistCommand::parse(br#"{"name": "  ", "delete": true}"#).is_err());
        assert!(BlocklistCommand::parse(br#"{"delete": false}"#).is_err());
    }

    #[test]
    fn a_body_with_no_recognized_action_is_rejected() {
        assert!(BlocklistCommand::parse(br#"{"nonsense": 1}"#).is_err());
        assert!(BlocklistCommand::parse(b"not json").is_err());
    }

    #[test]
    fn rule_tests_normalize_the_url_and_default_the_rest() {
        let t = RuleTest::parse(br#"{"url": "ads.example.com/x.js", "type": "script"}"#).unwrap();
        assert_eq!(t.url, "https://ads.example.com/x.js");
        assert_eq!(t.req_type, "script");
        assert_eq!(t.source, "");
        assert_eq!(
            RuleTest::parse(br#"{"type": "script"}"#).unwrap_err(),
            "missing 'url'"
        );
        assert_eq!(RuleTest::parse(br#"{"url": "  "}"#).unwrap_err(), "missing 'url'");
        assert!(RuleTest::parse(b"not json").is_err());
    }

    #[test]
    fn dns_rule_tests_normalize_the_domain() {
        assert_eq!(
            DnsRuleTest::parse(br#"{"domain": " Sub.ADS.example.com. "}"#).unwrap(),
            DnsRuleTest { domain: "sub.ads.example.com".into() }
        );
        assert_eq!(DnsRuleTest::parse(br#"{}"#).unwrap_err(), "missing 'domain'");
        assert!(DnsRuleTest::parse(b"not json").is_err());
    }
}
