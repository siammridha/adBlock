//! Outbound networking, shared by every module that dials out: the pooled
//! HTTP client, the egress policy, and host/port/URL parsing.

pub mod egress;
pub mod http_client;
pub(crate) mod target;
