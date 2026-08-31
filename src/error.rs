//! Core error type shared by `store`, `local`, and orchestration code.

use std::fmt;

/// Errors surfaced across vaultsync's Phase 1 surface (no external crates).
#[derive(Debug)]
pub enum Error {
    /// A key (or store object) was not found.
    NotFound(String),
    /// A key failed vault-relative validation rules.
    InvalidKey(String),
    /// The provider rejected the credentials / denied access (403/401).
    Unauthorized(String),
    /// A conditional request failed its precondition (HTTP 412): the remote
    /// object changed or appeared under us (issue 45, D-error). Carries the
    /// key or a short message.
    PreconditionFailed(String),
    /// The request timed out (transient).
    Timeout(String),
    /// The provider is unavailable / throttling (5xx, service down).
    Unavailable(String),
    /// An underlying filesystem / IO error.
    Io(std::io::Error),
    /// Anything else, with a human-readable message.
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotFound(key) => write!(f, "not found: {key}"),
            Error::InvalidKey(key) => write!(f, "invalid key: {key}"),
            Error::Unauthorized(msg) => write!(f, "credentials or permissions rejected: {msg}"),
            Error::PreconditionFailed(msg) => write!(f, "precondition failed: {msg}"),
            Error::Timeout(msg) => write!(f, "request timed out: {msg}"),
            Error::Unavailable(msg) => write!(f, "store unavailable: {msg}"),
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
