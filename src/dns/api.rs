//! The DNS module's public interface.
//!
//! Every other module and the web app reach DNS only through the names
//! re-exported here. Everything else in the module is internal. The boundary
//! lint (`tests/boundaries.rs`) fails any cross-module path that does not go
//! through a module's `api`.

pub use super::commands::{DnsConfigCommand, RewriteCommand};
pub use super::control::DnsRuntime;
pub use super::error::{Error, Result};
pub use super::{BlockingMode, DnsConfig, DnsService, DnsStatus};
