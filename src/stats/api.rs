//! The Stats module's public interface.
//!
//! Every other module and the web app reach Stats only through the names
//! re-exported here. Everything else in the module is internal. The boundary
//! lint (`tests/boundaries.rs`) fails any cross-module path that does not go
//! through a module's `api`.

pub use super::error::{Error, Result};
pub use super::history::Metric;
pub use super::{
    BodyDecode, CaptureSlot, DnsOutcome, DnsRecord, EventKind, Exchange, LoggingConfig,
    RequestFacts, RequestKind, SharedState, StaticInfo, StatsExclusionCommand, StatsExclusions,
    StatsOverrides, UiMsg,
};
