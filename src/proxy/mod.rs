//! The HTTP/HTTPS proxy itself.

pub mod api;
pub(crate) mod blackhole;
pub mod ca;
pub(crate) mod capture;
pub mod certs;
pub mod config;
pub mod control;
pub mod egress;
pub mod error;
pub mod exclusions;
pub(crate) mod html;
pub mod http_client;
pub mod injection;
pub(crate) mod persist;
pub(crate) mod pipeline;
pub mod server;
pub(crate) mod target;

pub use server::Proxy;
