//! Pure routing logic for a request: where it goes. Nothing here reads what a
//! request is for — naming it is what filter rules match on, so Adblock does
//! it. Nothing here changes a request or a response either; the proxy forwards
//! what Adblock hands back.

use hyper::Request;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub(crate) struct RequestPlan {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub url: String,
    pub method: String,
}

pub(crate) fn plan_request<B>(
    req: &Request<B>,
    secure: bool,
) -> std::result::Result<RequestPlan, BoxError> {
    let t = target_of(req, secure)?;
    Ok(RequestPlan {
        url: t.url(),
        scheme: t.scheme,
        host: t.host,
        port: t.port,
        method: req.method().to_string(),
    })
}

fn target_of<B>(
    req: &Request<B>,
    secure: bool,
) -> std::result::Result<crate::proxy::target::HttpTarget, BoxError> {
    let uri = req.uri();
    let path = uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .to_string();
    if let Some(authority) = uri.authority() {
        let scheme = uri.scheme_str().unwrap_or("http").to_string();
        let host = authority.host().to_string();
        let port = authority
            .port_u16()
            .unwrap_or(crate::proxy::target::default_port(&scheme));
        Ok(crate::proxy::target::HttpTarget { scheme, host, port, path })
    } else {
        let host_header = req
            .headers()
            .get(hyper::header::HOST)
            .and_then(|v| v.to_str().ok())
            .ok_or("request without Host header")?;
        let scheme = if secure { "https" } else { "http" }.to_string();
        let (host, port) =
            crate::proxy::target::split_host_port(host_header, crate::proxy::target::default_port(&scheme));
        Ok(crate::proxy::target::HttpTarget { scheme, host, port, path })
    }
}

pub(crate) struct ConnectPlan {
    pub host: String,
    pub url: String,
    pub verdict: ConnectVerdict,
}

pub(crate) enum ConnectVerdict {
    Deny { blocked_by: String },
    BlindTunnel { excluded_by: String },
    Mitm,
}

impl ConnectPlan {
    pub fn record_label(&self) -> &'static str {
        match self.verdict {
            ConnectVerdict::BlindTunnel { .. } => "tunnel-blind",
            _ => "tunnel-mitm",
        }
    }

    pub fn record_tag(&self) -> String {
        match &self.verdict {
            ConnectVerdict::BlindTunnel { excluded_by } => format!("excluded: {excluded_by}"),
            _ => String::new(),
        }
    }
}

/// `check` answers with the rule that blocked the host, or `None` to let it
/// through. `url` is for the log and the record only — a tunnel has no URL of
/// its own, and the block decision is made from the host.
pub(crate) fn plan_connect(
    authority: &str,
    check: impl FnOnce(&str) -> Option<String>,
    exclusion: impl FnOnce(&str) -> Option<String>,
) -> ConnectPlan {
    let host = authority.split(':').next().unwrap_or("").to_string();
    let url = format!("https://{host}/");
    let verdict = if let Some(blocked_by) = check(&host) {
        ConnectVerdict::Deny { blocked_by }
    } else if let Some(excluded_by) = exclusion(&host) {
        ConnectVerdict::BlindTunnel { excluded_by }
    } else {
        ConnectVerdict::Mitm
    };
    ConnectPlan { host, url, verdict }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(uri: &str, headers: &[(&str, &str)]) -> Request<()> {
        let mut b = Request::builder().uri(uri);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(()).unwrap()
    }

    #[test]
    fn plan_absolute_form_request() {
        let r = req("http://example.com/ads.js", &[]);
        let p = plan_request(&r, false).unwrap();
        assert_eq!(p.url, "http://example.com/ads.js", "default port stays out of the URL");
        assert_eq!(p.host, "example.com");
        assert_eq!(p.port, 80);
    }

    #[test]
    fn plan_origin_form_recovers_host_header() {
        let r = req("/page", &[("host", "example.com:8443")]);
        let p = plan_request(&r, true).unwrap();
        assert_eq!(p.scheme, "https");
        assert_eq!(p.url, "https://example.com:8443/page");
        assert_eq!(p.port, 8443);
    }

    #[test]
    fn plan_origin_form_without_host_fails() {
        let r = req("/page", &[]);
        assert!(plan_request(&r, false).is_err());
    }

    #[test]
    fn connect_to_a_plain_host_is_mitm_with_an_untagged_record() {
        let plan = plan_connect(
            "example.com:443",
            |host| {
                assert_eq!(host, "example.com", "filters are asked about the bare host");
                None
            },
            |host| {
                assert_eq!(host, "example.com", "exclusions match on the bare host");
                None
            },
        );
        assert!(matches!(plan.verdict, ConnectVerdict::Mitm));
        assert_eq!(plan.host, "example.com");
        assert_eq!(plan.url, "https://example.com/");
        assert_eq!(plan.record_label(), "tunnel-mitm");
        assert_eq!(plan.record_tag(), "");
    }

    #[test]
    fn connect_to_a_blocked_host_is_denied_with_attribution() {
        let plan = plan_connect("ads.example:443", |_| Some("||ads.example^".to_string()), |_| None);
        match plan.verdict {
            ConnectVerdict::Deny { blocked_by } => assert_eq!(blocked_by, "||ads.example^"),
            _ => panic!("blocked host must be denied"),
        }
    }

    #[test]
    fn connect_to_an_excluded_host_tunnels_blind_and_names_the_rule() {
        let plan = plan_connect(
            "push.apple.com:443",
            |_| None,
            |_| Some("apple.com".to_string()),
        );
        assert!(matches!(&plan.verdict, ConnectVerdict::BlindTunnel { excluded_by } if excluded_by == "apple.com"));
        assert_eq!(plan.record_label(), "tunnel-blind");
        assert_eq!(plan.record_tag(), "excluded: apple.com");
    }

    #[test]
    fn connect_block_wins_over_exclusion_and_skips_the_match() {
        let plan = plan_connect(
            "ads.example:443",
            |_| Some("||ads.example^".to_string()),
            |_| panic!("exclusions must not be consulted for a blocked host"),
        );
        assert!(matches!(plan.verdict, ConnectVerdict::Deny { .. }));
    }
}
