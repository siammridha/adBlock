//! An ad/tracker-blocking HTTP/HTTPS proxy with a built-in filtering DNS server
//! and a web admin UI.

pub mod adblock;
mod config;
pub mod dns;
mod error;
pub mod proxy;
pub mod stats;
pub mod web;

pub use config::Config;
pub use error::{Error, Result};
