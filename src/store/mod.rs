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

/// Enrich a listing's object entities with per-object `head()` results so
/// plans compare client mtimes, not upload times (issue #15, I15-approach).
///
/// Folder views are skipped (not objects; `head` on a folder key is NotFound
/// by contract - see the `ObjectStore::list` doc). A `NotFound` head drops the
/// row (a genuine concurrent-delete race; planning against a vanished object
/// would be worse). Any *other* head error fails the whole listing (I15-errors,
/// fail-closed - never plan against a knowingly-degraded remote view, matching
/// the W61 ethos and `pull --delete` safety). Entity order (sorted) and
/// `warnings` are preserved verbatim; only `mtime_ms` and `etag` are
/// overridden from the head result (`size` stays as listed - a mid-list
/// rewrite race is out of scope, and `plan()` tolerates either value).
pub(crate) fn enrich_with_head_mtimes<S: ObjectStore + ?Sized>(
    store: &S,
    listing: Listing,
) -> Result<Listing, Error> {
    let mut entities = Vec::new();
    for e in listing.entities {
        if e.is_folder() {
            entities.push(e);
            continue;
        }
        match store.head(&e.key) {
            Ok(h) => {
                let mut e = e;
                e.mtime_ms = h.mtime_ms;
                e.etag = h.etag;
                entities.push(e);
            }
            Err(Error::NotFound(_)) => {
                // Concurrent-delete race between LIST and HEAD: drop the row.
            }
            Err(err) => return Err(err),
        }
    }
    Ok(Listing {
        entities,
        warnings: listing.warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{file, folder};
    use crate::store::mock::MemoryStore;

    /// Build a store with two files (true metadata mtimes 100/200) and return
    /// a listing whose FILE entities carry later "upload-time" mtimes and no
    /// etag - exactly the degraded input real S3 `list` produces today
    /// (ListObjectsV2 `LastModified` fallback; issue #15).
    fn degraded_listing() -> (MemoryStore, Listing) {
        let store = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"a".to_vec());
        store.put_from("a.md", &mut c, 1, Some(100)).unwrap();
        let mut c = std::io::Cursor::new(b"b".to_vec());
        store.put_from("notes/b.md", &mut c, 1, Some(200)).unwrap();
        let mut listing = store.list("").unwrap();
        for e in listing.entities.iter_mut() {
            if !e.is_folder() {
                e.mtime_ms = Some(9_999_999);
                e.etag = None;
            }
        }
        listing.warnings = vec!["warn-1".to_string()];
        // sanity: this is the exact input today's planner would classify
        // RemoteNewer on every key (the issue's Download-everything shape).
        assert!(
            listing
                .entities
                .iter()
                .all(|e| e.is_folder() || e.mtime_ms == Some(9_999_999))
        );
        (store, listing)
    }

    #[test]
    fn enrich_overrides_listing_mtime_with_head_mtime() {
        let (store, listing) = degraded_listing();
        let enriched = enrich_with_head_mtimes(&store, listing).unwrap();
        // head reports the true (earlier) metadata mtimes and the real etag.
        let a = enriched.entities.iter().find(|e| e.key == "a.md").unwrap();
        assert_eq!(a.mtime_ms, Some(100));
        assert_eq!(a.etag, store.head("a.md").unwrap().etag);
        let b = enriched
            .entities
            .iter()
            .find(|e| e.key == "notes/b.md")
            .unwrap();
        assert_eq!(b.mtime_ms, Some(200));
        // folder entity passes through untouched (mtime stays None).
        let notes = enriched
            .entities
            .iter()
            .find(|e| e.key == "notes/")
            .unwrap();
        assert_eq!(notes.mtime_ms, None);
        // order stays sorted; warnings preserved verbatim.
        let keys: Vec<_> = enriched.entities.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["a.md", "notes/", "notes/b.md"]);
        assert_eq!(enriched.warnings, vec!["warn-1".to_string()]);
    }

    #[test]
    fn enrich_drops_row_when_head_not_found() {
        let (store, mut listing) = degraded_listing();
        // A listed key that vanishes between LIST and HEAD (concurrent-delete
        // race): head answers NotFound, so the row is dropped, siblings kept.
        listing.entities.push(file("gone.md", 5, Some(9_999_999)));
        let enriched = enrich_with_head_mtimes(&store, listing).unwrap();
        let keys: Vec<_> = enriched.entities.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["a.md", "notes/", "notes/b.md"]);
    }

    /// Store whose `head` fails with a non-NotFound error (throttling class).
    struct HeadFailStore;
    impl ObjectStore for HeadFailStore {
        fn list(&self, _prefix: &str) -> Result<Listing, Error> {
            Ok(Listing::default())
        }
        fn head(&self, _key: &str) -> Result<Entity, Error> {
            Err(Error::Unavailable("throttled".to_string()))
        }
        fn get_to(&self, _key: &str, _w: &mut dyn std::io::Write) -> Result<Entity, Error> {
            Err(Error::Unavailable("throttled".to_string()))
        }
        fn put_from(
            &self,
            _key: &str,
            _r: &mut dyn std::io::Read,
            _size: u64,
            _mtime_ms: Option<u64>,
        ) -> Result<Entity, Error> {
            Err(Error::Unavailable("throttled".to_string()))
        }
        fn delete(&self, _key: &str) -> Result<(), Error> {
            Err(Error::Unavailable("throttled".to_string()))
        }
    }

    #[test]
    fn enrich_fails_closed_on_head_error() {
        let listing = Listing {
            entities: vec![file("a.md", 1, Some(9_999_999))],
            warnings: Vec::new(),
        };
        let err = enrich_with_head_mtimes(&HeadFailStore, listing).unwrap_err();
        assert!(
            matches!(err, Error::Unavailable(_)),
            "non-NotFound head error must fail the listing, got {err:?}"
        );
    }

    #[test]
    fn enrich_handles_empty_and_folder_only_listings() {
        let store = MemoryStore::new();
        // empty listing: no heads, unchanged.
        let empty = enrich_with_head_mtimes(
            &store,
            Listing {
                entities: Vec::new(),
                warnings: vec!["w".to_string()],
            },
        )
        .unwrap();
        assert!(empty.entities.is_empty());
        assert_eq!(empty.warnings, vec!["w".to_string()]);
        // folder-only listing: no heads attempted (head on a folder view is
        // NotFound by contract), folders pass through.
        let folder_only = enrich_with_head_mtimes(
            &store,
            Listing {
                entities: vec![folder("notes")],
                warnings: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(folder_only.entities.len(), 1);
        assert_eq!(folder_only.entities[0].key, "notes/");
    }
}
