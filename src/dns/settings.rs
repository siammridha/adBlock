//! DNS settings that can be changed at runtime, persisted as overrides on top
//! of the config file.

use serde::{Deserialize, Serialize};

use super::config::{DnsConfig, UpstreamMode};

macro_rules! dns_tunables {
    ($( $field:ident : $ty:ty ),+ $(,)?) => {
        #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
        #[serde(default)]
        pub struct DnsOverrides {
            $( pub $field: Option<$ty>, )+
        }

        impl DnsOverrides {
            #[must_use]
            pub fn merged_with(self, upd: &DnsOverrides) -> Self {
                Self { $( $field: upd.$field.clone().or(self.$field), )+ }
            }

            /// A fully-populated override built from effective settings. Used to
            /// seed the settings file with full default values on first run.
            #[must_use]
            pub fn from_effective(e: &EffectiveDnsSettings) -> Self {
                Self { $( $field: Some(e.$field.clone()), )+ }
            }
        }

        #[derive(Clone, Debug, PartialEq)]
        pub struct EffectiveDnsSettings {
            $( pub $field: $ty, )+
        }

        impl EffectiveDnsSettings {
            pub fn from_config(cfg: &DnsConfig) -> Self {
                Self { $( $field: cfg.$field.clone(), )+ }.normalized()
            }

            #[must_use]
            pub fn with(mut self, o: &DnsOverrides) -> Self {
                $( if let Some(v) = o.$field.clone() { self.$field = v; } )+
                self.normalized()
            }
        }
    };
}

dns_tunables! {
    upstreams: Vec<String>,
    upstream_mode: UpstreamMode,
    bootstrap: Vec<String>,
    cache_size: usize,
    override_min_ttl_secs: u32,
    override_max_ttl_secs: u32,
    ech_probe_domain: String,
    ech_probe_mins: u32,
    strip_ech: bool,
    log_ipv6: bool,
}

pub type SettingsStore = crate::dns::persist::OverrideStore<DnsOverrides>;

impl EffectiveDnsSettings {

    pub fn validate(&self) -> Result<(), String> {
        // 0 disables a bound, so only a conflict between two active bounds is invalid.
        if self.override_min_ttl_secs > 0 && self.override_max_ttl_secs > 0 && self.override_min_ttl_secs > self.override_max_ttl_secs {
            return Err(format!(
                "override_min_ttl_secs ({}) exceeds override_max_ttl_secs ({})",
                self.override_min_ttl_secs, self.override_max_ttl_secs
            ));
        }
        Ok(())
    }

    fn normalized(mut self) -> Self {
        for s in self.upstreams.iter_mut().chain(self.bootstrap.iter_mut()) {
            *s = s.trim().to_string();
        }
        self.ech_probe_domain = self.ech_probe_domain.trim().to_string();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_lets_the_update_win_and_keeps_the_rest() {
        let saved = DnsOverrides {
            upstreams: Some(vec!["udp://10.0.0.1:53".into()]),
            cache_size: Some(512),
            ..Default::default()
        };
        let upd = DnsOverrides {
            upstreams: Some(vec!["tls://1.1.1.1".into()]),
            override_min_ttl_secs: Some(30),
            ..Default::default()
        };
        let merged = saved.merged_with(&upd);
        assert_eq!(merged.upstreams, Some(vec!["tls://1.1.1.1".to_string()]));
        assert_eq!(merged.override_min_ttl_secs, Some(30));
        assert_eq!(merged.cache_size, Some(512));
        assert_eq!(merged.override_max_ttl_secs, None);
        assert_eq!(merged.upstream_mode, None);

        let saved = DnsOverrides { cache_size: Some(9), ..Default::default() };
        assert_eq!(saved.clone().merged_with(&DnsOverrides::default()), saved);
    }

    #[test]
    fn effective_settings_fold_normalize_and_validate() {
        let cfg = DnsConfig { ech_probe_domain: " probe.example ".into(), ..DnsConfig::default() };
        let eff = EffectiveDnsSettings::from_config(&cfg);
        assert_eq!(eff.ech_probe_domain, "probe.example");

        let upd = DnsOverrides {
            upstreams: Some(vec![" tls://1.1.1.1 ".into()]),
            log_ipv6: Some(true),
            ..Default::default()
        };
        let folded = eff.clone().with(&upd);
        assert_eq!(folded.upstreams, vec!["tls://1.1.1.1".to_string()]);
        assert!(folded.log_ipv6);
        assert_eq!(folded.cache_size, eff.cache_size);

        assert!(folded.validate().is_ok());
        let bad = folded.clone().with(&DnsOverrides {
            override_min_ttl_secs: Some(100),
            override_max_ttl_secs: Some(50),
            ..Default::default()
        });
        assert!(bad.validate().unwrap_err().contains("override_min_ttl_secs"));
        // A disabled max (0) never conflicts with an active min.
        let disabled_max = folded.clone().with(&DnsOverrides {
            override_min_ttl_secs: Some(100),
            override_max_ttl_secs: Some(0),
            ..Default::default()
        });
        assert!(disabled_max.validate().is_ok());
        // Empty upstreams is allowed now: DNS may run with none configured.
        let empty = folded.with(&DnsOverrides { upstreams: Some(vec![]), ..Default::default() });
        assert!(empty.validate().is_ok());
    }

    #[test]
    fn roundtrip_and_reset() {
        let path = std::env::temp_dir().join("proxy-dns-settings-test.json");
        let _ = std::fs::remove_file(&path);
        let store = SettingsStore::new(path);

        assert_eq!(store.load(), DnsOverrides::default());
        let o = DnsOverrides {
            upstreams: Some(vec!["tls://1.1.1.1".into()]),
            upstream_mode: Some(UpstreamMode::Parallel),
            cache_size: Some(1024),
            log_ipv6: Some(true),
            ..Default::default()
        };
        store.save(&o).unwrap();
        assert_eq!(store.load(), o);
        assert_eq!(store.load().override_min_ttl_secs, None);
        store.reset().unwrap();
        store.reset().unwrap();
        assert_eq!(store.load(), DnsOverrides::default());
    }
}
