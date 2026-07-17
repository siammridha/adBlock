//! Crate-wide plumbing shared by every module: the config file schema, the
//! error type, and small persistence helpers.

pub mod config;
pub mod error;
pub(crate) mod persist;
