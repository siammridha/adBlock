//! An ad/tracker-blocking HTTP/HTTPS proxy with a built-in filtering DNS server
//! and a web admin UI.

// The crate is named `adBlock` (not snake_case) on purpose; silence the lint.
#![allow(non_snake_case)]

pub mod adblock;
pub mod dns;
mod error;
pub mod proxy;
pub mod stats;
pub mod tester;
pub mod web;

pub use error::{Error, Result};
