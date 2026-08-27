//! Object-store abstraction plus an in-memory mock.
//!
//! The store speaks the planner-facing descriptor type [`Entity`] so mock
//! list results plug straight into `plan()`. Methods are streaming from day
//! one (`get_to` / `put_from`).

use std::io::{Read, Write};

use crate::entity::Entity;
use crate::error::Error;

pub mod mock;

/// An object store holding a set of vault-relative keys.
///
/// Implementations must be usable through `&self` (interior mutability) so the
/// CLI can share one store object without `mut` gymnastics.
pub trait ObjectStore {
    /// List entities whose key starts with `prefix`. `""` lists everything.
    /// Folders are synthesized from key prefixes when no folder marker object
    /// exists. Results are sorted by key.
    fn list(&self, prefix: &str) -> Result<Vec<Entity>, Error>;
    /// Fetch metadata for a single object.
    fn head(&self, key: &str) -> Result<Entity, Error>;
    /// Stream object bytes into `w`, returning its metadata.
    fn get_to(&self, key: &str, w: &mut dyn Write) -> Result<Entity, Error>;
    /// Store exactly `size` bytes read from `r`.
    fn put_from(
        &self,
        key: &str,
        r: &mut dyn Read,
        size: u64,
        mtime_ms: Option<u64>,
    ) -> Result<Entity, Error>;
    /// Remove an object.
    fn delete(&self, key: &str) -> Result<(), Error>;
}
