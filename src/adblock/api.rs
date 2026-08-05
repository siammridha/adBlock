//! The Adblock module's public interface.
//!
//! Every other module and the web app reach Adblock only through the names
//! re-exported here. Everything else in the module is internal. The boundary
//! lint (`tests/boundaries.rs`) fails any cross-module path that does not go
//! through a module's `api`.

pub use super::commands::{BlocklistCommand, DnsRuleTest, RuleTest};
pub use super::error::{Error, Result};
pub use super::fetch::HttpClient;
pub use super::maintenance::{
    event_list_change, event_scriptlets, spawn_blocklist_updater, BlocklistFetcher, Downloader,
    RefreshError,
};
pub use super::settings::DecisionSettings;
pub use super::updater::{ScriptletUpdater, UBO_TARBALL_PAGE};
pub use super::{
    from_config, with_store, AdBlocker, AdblockConfig, BlockAttribution, BlockDecision,
    ListCuration, ListEntry, MemoryListStore, Redirect, ResponseEdit,
};
