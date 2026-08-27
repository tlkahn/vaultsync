//! vaultsync library core.
//!
//! Phase 1 will grow modules: `entity`, `plan`, `local`, `store`.

/// Library version string (mirrors the package version).
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
