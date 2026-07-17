//! In-process name resolution API: typed lookups for clients inside this
//! process (the egress policy), going through the same filtering pipeline
//! as queries from the wire.

use std::net::IpAddr;

use hickory_proto::op::{Message, Query, ResponseCode};
use hickory_proto::rr::rdata::svcb::SvcParamValue;
use hickory_proto::rr::{Name, RData, RecordType};

use super::DnsService;

impl DnsService {
    /// Resolve `host` to IP addresses. Blocked or unresolvable names
    /// return an error saying why.
    pub async fn resolve(&self, host: &str, want_ipv6: bool) -> std::io::Result<Vec<IpAddr>> {
        let a_resp = self.lookup(host, RecordType::A).await.map_err(std::io::Error::other)?;
        let mut answers: Vec<IpAddr> = a_records(&a_resp).into_iter().map(IpAddr::V4).collect();

        if want_ipv6 {
            if let Ok(aaaa_resp) = self.lookup(host, RecordType::AAAA).await {
                answers.extend(aaaa_records(&aaaa_resp).into_iter().map(IpAddr::V6));
            }
        }

        let blocked = answers.iter().any(|ip| ip.is_unspecified());
        let addrs: Vec<IpAddr> =
            answers.into_iter().filter(|ip| !ip.is_unspecified()).collect();
        if addrs.is_empty() {
            return Err(std::io::Error::other(resolve_failure(host, &a_resp, blocked)));
        }
        Ok(addrs)
    }

    /// ECH config list from the host's HTTPS record, if it has one.
    pub async fn ech_config_list(&self, host: &str) -> Option<Vec<u8>> {
        let resp = self.lookup(host, RecordType::HTTPS).await.ok()?;
        ech_config(&resp)
    }

    async fn lookup(&self, host: &str, rtype: RecordType) -> Result<Message, String> {
        let name = Name::from_utf8(format!("{}.", host.trim_end_matches('.')))
            .map_err(|e| format!("bad host '{host}': {e}"))?;
        let mut msg = Message::query();
        msg.metadata.recursion_desired = true;
        msg.add_query(Query::query(name, rtype));
        Ok(self.handle_proxy(&msg).await)
    }
}

fn resolve_failure(host: &str, resp: &Message, blocked: bool) -> String {
    if blocked {
        return format!("'{host}' is blocked by the DNS filter (answered 0.0.0.0)");
    }
    match resp.metadata.response_code {
        ResponseCode::NoError => format!("no A record for '{host}'"),
        code => format!("'{host}' did not resolve: {code}"),
    }
}

fn a_records(resp: &Message) -> Vec<std::net::Ipv4Addr> {
    resp.answers
        .iter()
        .filter_map(|r| match &r.data {
            RData::A(a) => Some(a.0),
            _ => None,
        })
        .collect()
}

fn aaaa_records(resp: &Message) -> Vec<std::net::Ipv6Addr> {
    resp.answers
        .iter()
        .filter_map(|r| match &r.data {
            RData::AAAA(a) => Some(a.0),
            _ => None,
        })
        .collect()
}

fn ech_config(resp: &Message) -> Option<Vec<u8>> {
    resp.answers.iter().chain(resp.additionals.iter()).find_map(|r| {
        let params = match &r.data {
            RData::HTTPS(https) => &https.0.svc_params,
            RData::SVCB(svcb) => &svcb.svc_params,
            _ => return None,
        };
        params.iter().find_map(|(_, v)| match v {
            SvcParamValue::EchConfigList(list) => Some(list.0.clone()),
            _ => None,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_proto::rr::rdata::svcb::{SvcParamKey, SVCB};
    use hickory_proto::rr::rdata::{A, AAAA, HTTPS};
    use hickory_proto::rr::Record;

    #[test]
    fn aaaa_records_come_from_aaaa_answers_only() {
        let name = Name::from_utf8("example.com.").unwrap();
        let mut resp = Message::query();
        resp.add_answer(Record::from_rdata(
            name.clone(),
            300,
            RData::AAAA(AAAA::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
        ));
        resp.add_answer(Record::from_rdata(name, 300, RData::A(A::new(198, 51, 100, 9))));

        assert_eq!(aaaa_records(&resp), vec!["2001:db8::1".parse::<std::net::Ipv6Addr>().unwrap()]);
        assert!(aaaa_records(&Message::query()).is_empty());
    }

    #[test]
    fn a_records_ignore_other_types_and_ech_config_extracts_bytes() {
        use hickory_proto::rr::rdata::svcb::EchConfigList;
        let name = Name::from_utf8("example.com.").unwrap();
        let mut resp = Message::query();
        resp.add_answer(Record::from_rdata(name.clone(), 300, RData::A(A::new(192, 0, 2, 7))));
        resp.add_answer(Record::from_rdata(
            name.clone(),
            300,
            RData::HTTPS(HTTPS(SVCB::new(
                1,
                name,
                vec![(
                    SvcParamKey::EchConfigList,
                    SvcParamValue::EchConfigList(EchConfigList(vec![9, 9])),
                )],
            ))),
        ));
        assert_eq!(a_records(&resp), vec![std::net::Ipv4Addr::new(192, 0, 2, 7)]);
        assert_eq!(ech_config(&resp), Some(vec![9, 9]));
        assert_eq!(ech_config(&Message::query()), None);
    }
}
