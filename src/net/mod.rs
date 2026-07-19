//! Outbound networking, shared by every module that dials out: the pooled
//! HTTP client and host/port/URL parsing.

pub mod http_client;
pub(crate) mod target;
