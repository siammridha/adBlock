//! Root error wiring: wraps each module's own error type so `main` can use
//! one `Result` for exit codes. Modules never use this type.

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Config(String),
    Adblock(crate::adblock::api::Error),
    Proxy(crate::proxy::api::Error),
    Dns(crate::dns::api::Error),
    Stats(crate::stats::api::Error),
    Web(crate::web::Error),
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Config(m) => write!(f, "configuration error: {m}"),
            Error::Adblock(e) => e.fmt(f),
            Error::Proxy(e) => e.fmt(f),
            Error::Dns(e) => e.fmt(f),
            Error::Stats(e) => e.fmt(f),
            Error::Web(e) => e.fmt(f),
            Error::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<crate::adblock::api::Error> for Error {
    fn from(e: crate::adblock::api::Error) -> Self {
        Error::Adblock(e)
    }
}

impl From<crate::proxy::api::Error> for Error {
    fn from(e: crate::proxy::api::Error) -> Self {
        Error::Proxy(e)
    }
}

impl From<crate::dns::api::Error> for Error {
    fn from(e: crate::dns::api::Error) -> Self {
        Error::Dns(e)
    }
}

impl From<crate::stats::api::Error> for Error {
    fn from(e: crate::stats::api::Error) -> Self {
        Error::Stats(e)
    }
}

impl From<crate::web::Error> for Error {
    fn from(e: crate::web::Error) -> Self {
        Error::Web(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
