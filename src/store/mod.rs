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
/// Each object entity is replaced by a coherent head snapshot: `mtime_ms`,
/// `etag`, and `size` all come from the same `head()` result. Taking `size`
/// from head (not the raw list) aligns the W106 `CappedWriter` cap and the
/// head-before-delete size check with the mtime identity the planner just
/// trusted, and retires the stale-list-size chimera (a list-era size with a
/// head-era mtime). The residual race shrinks to "object changed between
/// enrich-head and get/delete-head", which the executor already handles.
///
/// Folder views are skipped (not objects; `head` on a folder key is NotFound
/// by contract - see the `ObjectStore::list` doc). A `NotFound` head drops the
/// row (a genuine concurrent-delete race; planning against a vanished object
/// would be worse) and is surfaced, not hidden, as one bounded warning in
/// `Listing.warnings` (W70/W79 surface-don't-hide; see W116). Note that
/// folder views synthesized before enrichment can survive their children -
/// all objects under a prefix deleted between LIST and HEAD leaves a stale
/// folder view - which is benign because folders plan as Skip and are never
/// delete targets. Any *other* head error fails the whole listing (I15-errors,
/// fail-closed - never plan against a knowingly-degraded remote view, matching
/// the W61 ethos and `pull --delete` safety). Head is a single attempt here:
/// retry/backoff/jitter for transient errors is owned by the backend's SDK
/// `RetryConfig` (I8, supersedes the retired W117 stopgap), so no per-object
/// retry loop lives at this boundary. Entity order (sorted) and `warnings`
/// are preserved verbatim.
pub(crate) fn enrich_with_head_mtimes<S: ObjectStore + ?Sized>(
    store: &S,
    listing: Listing,
) -> Result<Listing, Error> {
    let mut entities = Vec::new();
    let mut vanished: Vec<String> = Vec::new();
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
                e.size = h.size;
                entities.push(e);
            }
            Err(Error::NotFound(_)) => {
                // Concurrent-delete race between LIST and HEAD: drop the row,
                // and surface the drop (W70/W79 surface-don't-hide ethos) via
                // one bounded warning appended below.
                vanished.push(e.key);
            }
            Err(err) => return Err(err),
        }
    }
    let mut warnings = listing.warnings;
    if !vanished.is_empty() {
        const MAX: usize = 5;
        let total = vanished.len();
        let shown: Vec<&str> = vanished.iter().take(MAX).map(|s| s.as_str()).collect();
        let mut msg = format!(
            "{} listed key(s) vanished before head (deleted between LIST and HEAD); skipping: ",
            total
        );
        msg.push_str(&shown.join(", "));
        if total > MAX {
            msg.push_str(&format!(" and {} more", total - MAX));
        }
        warnings.push(msg);
    }
    Ok(Listing { entities, warnings })
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
    fn enrich_corrects_stale_listing_size_with_head_size() {
        let (store, mut listing) = degraded_listing();
        // Corrupt one file entity's listed `size` far from the real value.
        // The enrichment must replace it with the head's size (a coherent
        // head snapshot: size + mtime + etag from the same HeadObject), so
        // the W106 CappedWriter cap and the head-before-delete size check key
        // off the same identity the planner just trusted.
        let a = listing
            .entities
            .iter_mut()
            .find(|e| e.key == "a.md")
            .unwrap();
        a.size = 9_999;
        let enriched = enrich_with_head_mtimes(&store, listing).unwrap();
        let a = enriched.entities.iter().find(|e| e.key == "a.md").unwrap();
        assert_eq!(a.size, store.head("a.md").unwrap().size);
        assert_eq!(
            a.size, 1,
            "mock true size is 1; stale listed size must be corrected"
        );
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
    fn enrich_warns_bounded_when_rows_vanish_before_head() {
        let store = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"a".to_vec());
        store.put_from("a.md", &mut c, 1, Some(100)).unwrap();
        // Seven listed keys that no longer exist (deleted between LIST and
        // HEAD) plus one healthy key. The drop is surfaced as a single
        // bounded warning naming the first 5 + "and N more" (W79 ethos).
        let vanished: Vec<Entity> = (0..7)
            .map(|i| file(&format!("gone-{i}.md"), 5, Some(9_999_999)))
            .collect();
        let mut entities = Vec::new();
        entities.push(file("a.md", 1, Some(9_999_999)));
        entities.extend(vanished);
        let listing = Listing {
            entities,
            warnings: vec!["pre-existing".to_string()],
        };
        let enriched = enrich_with_head_mtimes(&store, listing).unwrap();
        // Healthy row kept; all vanished rows dropped.
        let keys: Vec<_> = enriched.entities.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["a.md"]);
        // Exactly one appended warning, bounded to 5 names + "and 2 more";
        // pre-existing warning preserved verbatim (append, not replace).
        assert_eq!(enriched.warnings.len(), 2);
        assert_eq!(enriched.warnings[0], "pre-existing");
        assert_eq!(
            enriched.warnings[1],
            "7 listed key(s) vanished before head (deleted between LIST and HEAD); skipping: \
             gone-0.md, gone-1.md, gone-2.md, gone-3.md, gone-4.md and 2 more"
        );
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
    struct HeadFailStore {
        calls: std::cell::Cell<usize>,
    }
    impl HeadFailStore {
        fn new() -> Self {
            Self {
                calls: std::cell::Cell::new(0),
            }
        }
    }
    impl ObjectStore for HeadFailStore {
        fn list(&self, _prefix: &str) -> Result<Listing, Error> {
            Ok(Listing::default())
        }
        fn head(&self, _key: &str) -> Result<Entity, Error> {
            self.calls.set(self.calls.get() + 1);
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

    /// Which transient/nontransient error a flaky head stub answers with.
    #[derive(Clone, Copy)]
    enum FlakyKind {
        Unavailable,
        Unauthorized,
    }

    /// Store whose `head` fails the first `fail_first` calls with `kind`,
    /// then answers from `inner`. Counts total head calls.
    struct FlakyHeadStore {
        inner: MemoryStore,
        fail_first: usize,
        kind: FlakyKind,
        calls: std::cell::Cell<usize>,
    }
    impl FlakyHeadStore {
        fn new(inner: MemoryStore, fail_first: usize, kind: FlakyKind) -> Self {
            Self {
                inner,
                fail_first,
                kind,
                calls: std::cell::Cell::new(0),
            }
        }
        fn err(&self) -> Error {
            match self.kind {
                FlakyKind::Unavailable => Error::Unavailable("throttled".to_string()),
                FlakyKind::Unauthorized => Error::Unauthorized("denied".to_string()),
            }
        }
    }
    impl ObjectStore for FlakyHeadStore {
        fn list(&self, _prefix: &str) -> Result<Listing, Error> {
            self.inner.list("")
        }
        fn head(&self, key: &str) -> Result<Entity, Error> {
            let n = self.calls.get();
            self.calls.set(n + 1);
            if n < self.fail_first {
                return Err(self.err());
            }
            self.inner.head(key)
        }
        fn get_to(&self, key: &str, w: &mut dyn std::io::Write) -> Result<Entity, Error> {
            self.inner.get_to(key, w)
        }
        fn put_from(
            &self,
            key: &str,
            r: &mut dyn std::io::Read,
            size: u64,
            mtime_ms: Option<u64>,
        ) -> Result<Entity, Error> {
            self.inner.put_from(key, r, size, mtime_ms)
        }
        fn delete(&self, key: &str) -> Result<(), Error> {
            self.inner.delete(key)
        }
    }

    #[test]
    fn enrich_fails_closed_on_first_transient_head_error() {
        // I8-retire: the SDK RetryConfig owns retry (cycle 4), so enrichment
        // calls head() exactly once per object - a transient error that the
        // old W117 stopgap would have retried (first head Unavailable, second
        // would succeed) now fails the listing closed on the first attempt,
        // with the attempt counter at 1. Was RED under W117 (retried); GREEN
        // under I8 (single attempt).
        let store = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"a".to_vec());
        store.put_from("a.md", &mut c, 1, Some(100)).unwrap();
        let flaky = FlakyHeadStore::new(store, 1, FlakyKind::Unavailable);
        let listing = Listing {
            entities: vec![file("a.md", 9_999, Some(9_999_999))],
            warnings: Vec::new(),
        };
        let err = enrich_with_head_mtimes(&flaky, listing).unwrap_err();
        assert!(
            matches!(err, Error::Unavailable(_)),
            "a transient head error must fail the listing on the first attempt, got {err:?}"
        );
        assert_eq!(flaky.calls.get(), 1, "must be a single head attempt");
    }

    #[test]
    fn enrich_transient_head_failure_is_single_attempt() {
        // I8-retire: an always-Unavailable store fails closed after exactly 1
        // head call - no sleeps, no retry loop (the SDK owns retry now).
        // Was RED under W117 (retried); GREEN under I8 (single attempt).
        let store = HeadFailStore::new();
        let listing = Listing {
            entities: vec![file("a.md", 1, Some(9_999_999))],
            warnings: Vec::new(),
        };
        let err = enrich_with_head_mtimes(&store, listing).unwrap_err();
        assert!(matches!(err, Error::Unavailable(_)));
        assert_eq!(store.calls.get(), 1, "must be a single head attempt");
    }

    #[test]
    fn enrich_does_not_retry_nontransient_errors() {
        // Unauthorized is nontransient: fail-closed after exactly 1 head call.
        let store = MemoryStore::new();
        let flaky = FlakyHeadStore::new(store, 1, FlakyKind::Unauthorized);
        let listing = Listing {
            entities: vec![file("a.md", 1, None)],
            warnings: Vec::new(),
        };
        let err = enrich_with_head_mtimes(&flaky, listing).unwrap_err();
        assert!(matches!(err, Error::Unauthorized(_)));
        assert_eq!(flaky.calls.get(), 1);
    }

    #[test]
    fn enrich_fails_closed_on_head_error() {
        let listing = Listing {
            entities: vec![file("a.md", 1, Some(9_999_999))],
            warnings: Vec::new(),
        };
        let err = enrich_with_head_mtimes(&HeadFailStore::new(), listing).unwrap_err();
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
