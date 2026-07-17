//! An ad/tracker-blocking HTTP/HTTPS proxy with a built-in filtering DNS server
//! and a web admin UI.

pub mod adblock;
pub mod dns;
pub mod net;
pub mod proxy;
pub mod stats;
pub mod support;
pub mod web;

pub use support::config::Config;
pub use support::error::{Error, Result};
