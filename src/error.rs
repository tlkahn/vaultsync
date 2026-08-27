//! Core error type shared by `store`, `local`, and orchestration code.

use std::fmt;

/// Errors surfaced across vaultsync's Phase 1 surface (no external crates).
#[derive(Debug)]
pub enum Error {
    /// A key (or store object) was not found.
    NotFound(String),
    /// A key failed vault-relative validation rules.
    InvalidKey(String),
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
