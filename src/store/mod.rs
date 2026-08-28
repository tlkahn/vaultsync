//! Object-store abstraction plus an in-memory mock.
//!
//! The store speaks the planner-facing descriptor type [`Entity`] so mock
//! list results plug straight into `plan()`. Methods are streaming from day
//! one (`get_to` / `put_from`).

use std::io::{Read, Write};

use crate::entity::Entity;
use crate::error::Error;

pub mod mock;
pub mod s3;

/// A listing result: the entities plus advisory warnings the backend wants
/// surfaced (e.g. keys dropped while listing). A struct, not a tuple, so
/// Phase 3 fields extend without another signature break. The CLI prints
/// `warnings` (one `warning: ...` line each); library consumers may inspect
/// or ignore them. Warnings never fail the listing - they describe what was
/// silently dropped so nothing vanishes without a trace (W70/A-N2, H1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    /// The listed entities (sorted by key).
    pub entities: Vec<Entity>,
    /// Advisory warnings about the listing, printed by the CLI layer.
    pub warnings: Vec<String>,
}

/// An object store holding a set of vault-relative keys.
///
/// Implementations must be usable through `&self` (interior mutability) so the
/// CLI can share one store object without `mut` gymnastics.
///
/// Key validation contract (N1): every key passed to `head` / `get_to` /
/// `put_from` / `delete` is expected to satisfy `ensure_valid_key`.
/// `put_from` validates and rejects invalid keys with [`Error::InvalidKey`];
/// read/delete paths may answer [`Error::NotFound`] for invalid keys (mock
/// behavior). Phase 2 backends must never forward an unvalidated key to a
/// provider (e.g. S3) - validate before any outbound call.
pub trait ObjectStore {
    /// List entities whose key starts with `prefix`. `""` lists everything.
    /// Folders are synthesized from key prefixes when no folder marker object
    /// exists. Results are sorted by key.
    ///
    /// Contract: `list` may return folder keys that exist only as prefixes
    /// (folder *views*, trailing `/`, `Entity::is_folder()` true). Such keys
    /// are not objects: `head`/`get_to`/`delete` operate on object keys only
    /// and return [`Error::NotFound`] for folder views. Callers must branch
    /// on `Entity::is_folder()` (or filter) before object ops.
    ///
    /// `prefix` is a raw string prefix, **not** a delimiter-aligned path
    /// segment. Callers that want only the contents of a folder must pass a
    /// trailing `/` (e.g. `notes/`); passing `notes` will also match `notes.md`
    /// and any sibling whose key merely starts with `notes`.
    fn list(&self, prefix: &str) -> Result<Listing, Error>;
    /// Fetch metadata for a single object.
    fn head(&self, key: &str) -> Result<Entity, Error>;
    /// Stream object bytes into `w`, returning its metadata.
    ///
    /// Contract (W30/N6/L2): the returned entity's `size` must be the true
    /// number of bytes written to `w`. Backends must not report a placeholder
    /// (e.g. 0) when the body length is not known up front; the executor's
    /// size check relies on this.
    fn get_to(&self, key: &str, w: &mut dyn Write) -> Result<Entity, Error>;
    /// Store exactly `size` bytes read from `r`. File keys only: a trailing
    /// `/` (folder marker) is rejected with [`Error::InvalidKey`].
    ///
    /// Contract: exactly `size` bytes are consumed and stored. A reader that
    /// ends early errors loudly (`UnexpectedEof`); bytes beyond `size` in the
    /// reader are ignored (normal `Read` semantics - the reader is read up to
    /// `size`, no more). Backends must not grow this into silent partial puts:
    /// the Phase 2 executor re-verifies transferred size at read time
    /// (checklist R3.3), and mock behavior matches the contract.
    fn put_from(
        &self,
        key: &str,
        r: &mut dyn Read,
        size: u64,
        mtime_ms: Option<u64>,
    ) -> Result<Entity, Error>;
    /// Remove an object.
    ///
    /// Delete is idempotent-friendly (W10/A-M3/B-L6): deleting an already-
    /// absent key MAY return `Ok` (S3 is idempotent) **or** [`Error::NotFound`]
    /// (the mock and `LocalFs::delete_file` do). Callers must treat both as
    /// reaching the goal state; the executor normalizes `NotFound` to success.
    fn delete(&self, key: &str) -> Result<(), Error>;
}
