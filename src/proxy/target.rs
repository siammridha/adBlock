//! Host/port parsing helpers for proxy targets.

pub(crate) fn default_port(scheme: &str) -> u16 {
    if scheme == "https" {
        443
    } else {
        80
    }
}

pub(crate) fn split_host_port(hostport: &str, default: u16) -> (String, u16) {
    if let Some(rest) = hostport.strip_prefix('[') {
        if let Some((host, after)) = rest.split_once(']') {
            let port = after
                .strip_prefix(':')
                .and_then(|p| p.parse().ok())
                .unwrap_or(default);
            return (format!("[{host}]"), port);
        }
    }
    match hostport.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => (h.to_string(), p.parse().unwrap_or(default)),
        _ => (hostport.to_string(), default),
    }
}

pub(crate) struct HttpTarget {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl HttpTarget {
    pub fn parse(url: &str) -> Result<Self, String> {
        let uri: hyper::Uri = url.parse().map_err(|e| format!("bad url: {e}"))?;
        let scheme = uri.scheme_str().unwrap_or("").to_string();
        if scheme != "http" && scheme != "https" {
            return Err(format!("unsupported scheme '{scheme}'"));
        }
        let host = uri.host().ok_or("url has no host")?.to_string();
        let port = uri.port_u16().unwrap_or(default_port(&scheme));
        let path = uri
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/")
            .to_string();
        Ok(Self {
            scheme,
            host,
            port,
            path,
        })
    }

    pub fn url(&self) -> String {
        if self.port == default_port(&self.scheme) {
            format!("{}://{}{}", self.scheme, self.host, self.path)
        } else {
            format!("{}://{}:{}{}", self.scheme, self.host, self.port, self.path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_port_splitting_defaults_and_keeps_ipv6_brackets() {
        assert_eq!(split_host_port("example.com", 80), ("example.com".into(), 80));
        assert_eq!(split_host_port("example.com:8443", 443), ("example.com".into(), 8443));
        assert_eq!(split_host_port("example.com:nope", 443), ("example.com".into(), 443));
        assert_eq!(split_host_port("[::1]", 443), ("[::1]".into(), 443));
        assert_eq!(split_host_port("[::1]:8443", 443), ("[::1]".into(), 8443));
        assert_eq!(split_host_port("::1", 443), ("::1".into(), 443));
    }

    #[test]
    fn http_target_parses_and_defaults_ports() {
        let t = HttpTarget::parse("https://easylist.to/easylist/easyprivacy.txt").unwrap();
        assert_eq!((t.scheme.as_str(), t.host.as_str(), t.port), ("https", "easylist.to", 443));
        assert_eq!(t.path, "/easylist/easyprivacy.txt");
        assert_eq!(t.url(), "https://easylist.to/easylist/easyprivacy.txt");

        let t = HttpTarget::parse("http://mirror.example:8080").unwrap();
        assert_eq!((t.port, t.path.as_str()), (8080, "/"));
        assert_eq!(t.url(), "http://mirror.example:8080/", "non-default port stays");

        assert!(HttpTarget::parse("ftp://example.com/list.txt").is_err());
        assert!(HttpTarget::parse("/no/host").is_err());
    }

}
