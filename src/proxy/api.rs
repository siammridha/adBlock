//! The Proxy module's public interface.
//!
//! Every other module and the web app reach Proxy only through the names
//! re-exported here. Everything else in the module is internal. The boundary
//! lint (`tests/boundaries.rs`) fails any cross-module path that does not go
//! through a module's `api`.

pub use super::ca::CertAuthority;
pub use super::certs::{CertCommand, CertStore};
pub use super::config::{PerformanceConfig, ProxyBaseConfig, ServerConfig, TlsConfig};
pub use super::control::ProxyRuntime;
pub use super::egress::{EgressOverrides, EgressPolicy};
pub use super::error::{Error, Result};
pub use super::exclusions::{ExclusionCommand, ExclusionStore};
pub use super::http_client::HttpClient;
pub use super::Proxy;
