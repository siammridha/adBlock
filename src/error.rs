//! Root error wiring: wraps each module's own error type so `main` can use
//! one `Result` for exit codes. Modules never use this type.

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Config(String),
    Adblock(crate::adblock::error::Error),
    Proxy(crate::proxy::error::Error),
    Dns(crate::dns::error::Error),
    Stats(crate::stats::error::Error),
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

impl From<crate::adblock::error::Error> for Error {
    fn from(e: crate::adblock::error::Error) -> Self {
        Error::Adblock(e)
    }
}

impl From<crate::proxy::error::Error> for Error {
    fn from(e: crate::proxy::error::Error) -> Self {
        Error::Proxy(e)
    }
}

impl From<crate::dns::error::Error> for Error {
    fn from(e: crate::dns::error::Error) -> Self {
        Error::Dns(e)
    }
}

impl From<crate::stats::error::Error> for Error {
    fn from(e: crate::stats::error::Error) -> Self {
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
