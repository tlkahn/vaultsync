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

/// Options for a conditional put (issue 45, D-trait-default). Absent fields
/// mean "no condition", exactly like the plain `put_from` call.
#[derive(Debug, Clone, Default)]
pub struct PutOpts {
    /// Client-visible mtime in ms since epoch, stored as user metadata.
    pub mtime_ms: Option<u64>,
    /// When `Some`, put succeeds only if the object's current ETag equals
    /// this value (If-Match). Used by the manifest commit to detect a lost
    /// race against another writer.
    pub if_match_etag: Option<String>,
    /// When true, put succeeds only if the key does not already exist
    /// (If-None-Match: *). Used for the manifest create path.
    pub if_none_match_star: bool,
}

/// Options for a conditional get (issue 45, D-trait-default).
#[derive(Debug, Clone, Default)]
pub struct GetOpts {
    /// When `Some`, a matching current ETag answers
    /// [`GetOutcome::NotModified`] (HTTP 304) with head-like metadata and no
    /// body; a mismatch answers [`GetOutcome::Body`].
    pub if_none_match_etag: Option<String>,
}

/// Outcome of a conditional get (issue 45, D-get-outcome): 304 is a value,
/// not a hard error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GetOutcome {
    /// The body was streamed to the writer; `Entity` carries its metadata.
    Body(Entity),
    /// Conditional GET satisfied (HTTP 304). The object was not fetched;
    /// `Entity` carries head-like metadata when the backend provides it
    /// (size/mtime may be best-effort).
    NotModified(Entity),
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
///
/// I20-traits: `Send + Sync` are supertraits so a shared `&dyn ObjectStore`
/// can be handed to the scoped worker pool (each worker thread calls the
/// store through `&self`). Implementations must be `Send + Sync`;
/// interior-mutable state must use `Mutex`/`Atomic*` (the `Cell`/`RefCell`
/// test doubles migrated in issue 20, cycle 1).
pub trait ObjectStore: Send + Sync {
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

    /// Conditional put (issue 45, D-trait-default): like [`put_from`] with
    /// [`PutOpts`] precondition support. The default body keeps every
    /// existing impl compiling: no precondition set -> delegate to
    /// [`put_from`]; any precondition -> a loud unsupported error. Providers
    /// with real conditionals (mock, S3) override this.
    fn put_from_with(
        &self,
        key: &str,
        r: &mut dyn Read,
        size: u64,
        opts: PutOpts,
    ) -> Result<Entity, Error> {
        if opts.if_match_etag.is_some() || opts.if_none_match_star {
            return Err(Error::Other(format!(
                "conditional put not supported for key {key}"
            )));
        }
        self.put_from(key, r, size, opts.mtime_ms)
    }

    /// Conditional get (issue 45, D-trait-default): like [`get_to`] with
    /// [`GetOpts`] If-None-Match support; a matching etag answers
    /// [`GetOutcome::NotModified`] without streaming the body. The default
    /// body keeps every existing impl compiling: no precondition -> delegate
    /// to [`get_to`]; a precondition -> a loud unsupported error. Providers
    /// with real conditionals (mock, S3) override this.
    fn get_to_with(
        &self,
        key: &str,
        w: &mut dyn Write,
        opts: GetOpts,
    ) -> Result<GetOutcome, Error> {
        if opts.if_none_match_etag.is_some() {
            return Err(Error::Other(format!(
                "conditional get not supported for key {key}"
            )));
        }
        self.get_to(key, w).map(GetOutcome::Body)
    }
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
///
/// I20-heads: `concurrency` bounds how many heads run in flight (same knob as
/// the transfer passes - workers = `min(concurrency, rows)`). The sequential
/// path (`concurrency <= 1`) is the literal pre-issue-20 loop, restored in
/// I20-r1/F1: it stops at the FIRST non-NotFound head error (immediate
/// `return Err`), so the "1 = byte-for-byte pre-issue-20" claim holds on the
/// error path too (N failing heads no longer burn the shared SDK retry quota
/// against a perma-failing backend). Heads fan out through
/// [`crate::pool::run_bounded`] only at `concurrency > 1`, and results are
/// merged by index, so entity order and the vanish-warning text are
/// deterministic regardless of completion order. Error selection is locked:
/// with several non-NotFound head failures in one listing, the returned error
/// is the first one in **listing order** (not completion order); a hard error
/// still fails the whole listing - no partial entities, no vanished warning
/// (the warning is built on the success path only). In the POOLED path
/// in-flight heads are not cancelled when a sibling fails: extra completed
/// requests are accepted (documented, no-cancellation behavior).
pub(crate) fn enrich_with_head_mtimes<S: ObjectStore + ?Sized>(
    store: &S,
    listing: Listing,
    concurrency: u32,
    progress: &dyn crate::progress::Progress,
) -> Result<Listing, Error> {
    let mut warnings = listing.warnings;
    let mut entities = Vec::new();
    let mut vanished: Vec<String> = Vec::new();
    // I42-heads (W335): one `HeadsStart` before the fan-out (object rows
    // only - folder views are never headed, never counted), then one
    // `HeadDone` per completed object head (success or NotFound-vanish).
    // Skipped entirely when there are zero object rows (I42 emission rules).
    let object_rows_total = listing
        .entities
        .iter()
        .filter(|e| !e.is_folder())
        .count() as u32;
    if object_rows_total > 0 {
        progress.event(crate::progress::ProgressEvent::HeadsStart {
            total_keys: object_rows_total,
        });
    }
    let mut heads_done: u32 = 0;
    if concurrency <= 1 {
        // Sequential path (I20-r1/F1): the pre-issue-20 loop verbatim
        // (recovered from a2fca0a) - folder passthrough, NotFound -> vanished
        // row, any other head error fails the listing IMMEDIATELY (no further
        // heads issued). The vanished-warning tail is shared with the pooled
        // path via `vanished_warning` so nothing is duplicated.
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
                    // Concurrent-delete race between LIST and HEAD: drop the
                    // row, and surface the drop (W70/W79 surface-don't-hide
                    // ethos) via one bounded warning appended below.
                    vanished.push(e.key);
                }
                Err(err) => return Err(err),
            }
            // I42-heads: NotFound-vanish still advances the count (W336); a
            // hard error returns before this line (partial HeadDone ok).
            heads_done += 1;
            progress.event(crate::progress::ProgressEvent::HeadDone {
                done: heads_done,
                total_keys: object_rows_total,
            });
        }
    } else {
        // I20-heads: fan the object-row heads out through the bounded pool;
        // folder views pass through untouched (never headed). Results come
        // back in listing order, so walking `listing.entities` in order and
        // returning the first non-NotFound error yields the listing-order
        // error lock. In-flight heads are not cancelled when a sibling fails
        // (extra completed requests accepted - documented pooled behavior).
        let object_rows: Vec<&Entity> =
            listing.entities.iter().filter(|e| !e.is_folder()).collect();
        let results = crate::pool::run_bounded(concurrency, &object_rows, |e| store.head(&e.key));
        let mut results = results.into_iter();
        for e in listing.entities {
            if e.is_folder() {
                entities.push(e);
                continue;
            }
            match results.next().expect("one head result per object row") {
                Ok(h) => {
                    let mut e = e;
                    e.mtime_ms = h.mtime_ms;
                    e.etag = h.etag;
                    e.size = h.size;
                    entities.push(e);
                }
                Err(Error::NotFound(_)) => {
                    // Concurrent-delete race between LIST and HEAD: drop the
                    // row, and surface the drop (W70/W79 surface-don't-hide
                    // ethos) via one bounded warning appended below.
                    vanished.push(e.key);
                }
                Err(err) => return Err(err),
            }
            heads_done += 1;
            progress.event(crate::progress::ProgressEvent::HeadDone {
                done: heads_done,
                total_keys: object_rows_total,
            });
        }
    }
    if let Some(msg) = vanished_warning(&vanished) {
        warnings.push(msg);
    }
    Ok(Listing { entities, warnings })
}

/// The shared vanished-row warning tail (MAX = 5 bounded message) used by
/// both enrichment paths (sequential and pooled) so the text cannot drift
/// (I20-r1/F1). `None` when nothing vanished.
fn vanished_warning(vanished: &[String]) -> Option<String> {
    if vanished.is_empty() {
        return None;
    }
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
    Some(msg)
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
    fn object_store_is_send_sync() {
        // I20-traits: `ObjectStore` must be shareable across the scoped
        // worker pool (each worker holds a `&dyn ObjectStore` on its own
        // thread), so the supertraits are a compile-time contract. This test
        // is the pin: it fails to compile if the trait ever loses `Send +
        // Sync`. RED today (trait lacks the supertraits).
        fn assert_ss<T: ?Sized + Send + Sync>() {}
        assert_ss::<dyn ObjectStore>();
    }

    /// Stub wrapping MemoryStore WITHOUT overriding `put_from_with` /
    /// `get_to_with`, so the trait's DEFAULT bodies are exercised (W222).
    /// Every existing impl has this shape: the 5 required methods only.
    struct DefaultWithStore(MemoryStore);
    impl ObjectStore for DefaultWithStore {
        fn list(&self, prefix: &str) -> Result<Listing, Error> {
            self.0.list(prefix)
        }
        fn head(&self, key: &str) -> Result<Entity, Error> {
            self.0.head(key)
        }
        fn get_to(&self, key: &str, w: &mut dyn std::io::Write) -> Result<Entity, Error> {
            self.0.get_to(key, w)
        }
        fn put_from(
            &self,
            key: &str,
            r: &mut dyn std::io::Read,
            size: u64,
            mtime_ms: Option<u64>,
        ) -> Result<Entity, Error> {
            self.0.put_from(key, r, size, mtime_ms)
        }
        fn delete(&self, key: &str) -> Result<(), Error> {
            self.0.delete(key)
        }
    }

    #[test]
    fn put_from_with_default_methods_delegate_or_reject() {
        // W222 (issue 45): the trait extension's default bodies keep every
        // existing impl compiling. No precondition -> delegate to
        // `put_from`/`get_to`; any precondition -> a loud unsupported error.
        // `DefaultWithStore` does not override `_with`, so this test
        // exercises the DEFAULTS (MemoryStore overrides them since W223).
        let store = DefaultWithStore(MemoryStore::new());
        let mut c = std::io::Cursor::new(b"x".to_vec());
        let e = store
            .put_from_with("a.md", &mut c, 1, PutOpts::default())
            .unwrap();
        assert_eq!(e.key, "a.md");
        assert_eq!(e.size, 1);

        let mut c = std::io::Cursor::new(b"y".to_vec());
        let err = store
            .put_from_with(
                "b.md",
                &mut c,
                1,
                PutOpts {
                    if_match_etag: Some("\"etag\"".to_string()),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(
            format!("{err}").contains("conditional put not supported"),
            "unexpected: {err}"
        );

        let mut c = std::io::Cursor::new(b"z".to_vec());
        let err = store
            .put_from_with(
                "c.md",
                &mut c,
                1,
                PutOpts {
                    if_none_match_star: true,
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(
            format!("{err}").contains("conditional put not supported"),
            "unexpected: {err}"
        );

        let mut buf = Vec::new();
        let out = store
            .get_to_with("a.md", &mut buf, GetOpts::default())
            .unwrap();
        assert!(matches!(out, GetOutcome::Body(e) if e.key == "a.md"));
        assert_eq!(buf, b"x");

        let mut buf = Vec::new();
        let err = store
            .get_to_with(
                "a.md",
                &mut buf,
                GetOpts {
                    if_none_match_etag: Some("\"etag\"".to_string()),
                },
            )
            .unwrap_err();
        assert!(
            format!("{err}").contains("conditional get not supported"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn precondition_failed_error_displays() {
        // W222 (issue 45, D-error): the real `PreconditionFailed` variant
        // keeps mock race tests matching cleanly (never stuffed into Other).
        let err = Error::PreconditionFailed("\"abc\"".to_string());
        assert_eq!(format!("{err}"), "precondition failed: \"abc\"");
    }

    // I42-heads (W335): enrich_with_head_mtimes emits HeadsStart then one
    // HeadDone per object head; folder views are never headed and never
    // counted in the total. Under concurrency 1 the HeadDone order is the
    // listing order. I15 behavior (head mtimes win) is unchanged.
    // I42-heads (W337): at concurrency > 1 the pooled enrichment still emits
    // HeadsStart{20} + exactly 20 HeadDone (totals pin; order may interleave
    // in general - here listing-order merge keeps done 1..=20 exactly once).
    #[test]
    fn enrich_head_progress_under_concurrency() {
        use crate::progress::ProgressEvent;
        let store = MemoryStore::new();
        for i in 0..20 {
            let mut c = std::io::Cursor::new(b"x".to_vec());
            store
                .put_from(&format!("k{i:02}.md"), &mut c, 1, Some(1000 + i))
                .unwrap();
        }
        let listing = store.list("").unwrap();
        let prog = crate::testutil::RecordingProgress::new();
        let enriched = enrich_with_head_mtimes(&store, listing, 4, &prog).unwrap();
        let events = prog.events();
        assert!(matches!(
            events[0],
            ProgressEvent::HeadsStart { total_keys: 20 }
        ));
        let done: Vec<u32> = events[1..]
            .iter()
            .map(|e| match e {
                ProgressEvent::HeadDone { done, total_keys } => {
                    assert_eq!(*total_keys, 20);
                    *done
                }
                other => panic!("expected HeadDone, got {other:?}"),
            })
            .collect();
        assert_eq!(done.len(), 20, "exactly 20 HeadDone");
        assert_eq!(done.iter().max(), Some(&20), "done reaches the total");
        let mut sorted = done.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 20, "done values 1..=20 each once: {done:?}");
        assert_eq!(enriched.entities.len(), 20, "no vanishes");
    }

    // I42-heads (W336): a NotFound vanish still advances `HeadDone` (the
    // vanish warning is unchanged); a hard head error fails closed with
    // partial `HeadDone` emissions allowed (no PlanEnd requirement here).
    #[test]
    fn enrich_head_progress_on_vanish_and_error() {
        use crate::progress::ProgressEvent;
        let store = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"a".to_vec());
        store.put_from("a.md", &mut c, 1, Some(100)).unwrap();
        // one healthy + two listed keys that no longer exist (vanish race)
        let listing = Listing {
            entities: vec![
                file("a.md", 1, Some(9_999_999)),
                file("gone-0.md", 5, Some(9_999_999)),
                file("gone-1.md", 5, Some(9_999_999)),
            ],
            warnings: Vec::new(),
        };
        let prog = crate::testutil::RecordingProgress::new();
        let enriched = enrich_with_head_mtimes(&store, listing, 1, &prog).unwrap();
        let events = prog.events();
        assert!(matches!(
            events[0],
            ProgressEvent::HeadsStart { total_keys: 3 }
        ));
        let done: Vec<u32> = events[1..]
            .iter()
            .map(|e| match e {
                ProgressEvent::HeadDone { done, total_keys } => {
                    assert_eq!(*total_keys, 3);
                    *done
                }
                other => panic!("expected HeadDone, got {other:?}"),
            })
            .collect();
        assert_eq!(done, vec![1, 2, 3], "vanish still advances done");
        assert_eq!(enriched.entities.len(), 1, "vanished rows dropped");
        assert!(
            enriched.warnings.iter().any(|w| w.contains("vanished")),
            "vanish warning unchanged: {:?}",
            enriched.warnings
        );

        // hard head error: fails closed; partial HeadDone allowed
        let mut fail = KeyFailStore::new(MemoryStore::new());
        let mut c = std::io::Cursor::new(b"a".to_vec());
        fail.inner.put_from("a.md", &mut c, 1, Some(1)).unwrap();
        fail.fail("a.md", "boom");
        let listing2 = Listing {
            entities: vec![file("a.md", 1, Some(1))],
            warnings: Vec::new(),
        };
        let prog2 = crate::testutil::RecordingProgress::new();
        let err = enrich_with_head_mtimes(&fail, listing2, 1, &prog2).unwrap_err();
        assert!(format!("{err}").contains("boom"));
        let events2 = prog2.events();
        assert!(
            matches!(events2.first(), Some(ProgressEvent::HeadsStart { total_keys: 1 })),
            "HeadsStart may precede the fail-closed error: {events2:?}"
        );
    }

    #[test]
    fn enrich_emits_head_progress() {
        use crate::progress::ProgressEvent;
        let store = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"a".to_vec());
        store.put_from("a.md", &mut c, 1, Some(100)).unwrap();
        let mut c = std::io::Cursor::new(b"b".to_vec());
        store.put_from("b.md", &mut c, 1, Some(200)).unwrap();
        let mut c = std::io::Cursor::new(b"c".to_vec());
        store.put_from("n/c.md", &mut c, 1, Some(300)).unwrap();
        let listing = store.list("").unwrap();
        // sanity: 3 objects + 1 synthesized folder view (n/)
        assert_eq!(listing.entities.len(), 4);
        let prog = crate::testutil::RecordingProgress::new();
        let enriched = enrich_with_head_mtimes(&store, listing, 1, &prog).unwrap();
        let events = prog.events();
        match &events[0] {
            ProgressEvent::HeadsStart { total_keys } => assert_eq!(*total_keys, 3),
            other => panic!("expected HeadsStart first, got {other:?}"),
        }
        let done: Vec<u32> = events[1..]
            .iter()
            .map(|e| match e {
                ProgressEvent::HeadDone { done, total_keys } => {
                    assert_eq!(*total_keys, 3);
                    *done
                }
                other => panic!("expected HeadDone, got {other:?}"),
            })
            .collect();
        assert_eq!(done, vec![1, 2, 3], "concurrency 1: listing order");
        assert_eq!(events.len(), 4, "HeadsStart + 3 HeadDone");
        // I15 unchanged: head mtimes win; folder view passes through unheaded.
        let a = enriched.entities.iter().find(|e| e.key == "a.md").unwrap();
        assert_eq!(a.mtime_ms, Some(100));
        let folders: Vec<&Entity> =
            enriched.entities.iter().filter(|e| e.is_folder()).collect();
        assert_eq!(folders.len(), 1, "folder view survives unheaded");
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
        let enriched = enrich_with_head_mtimes(&store, listing, 1, &crate::progress::NoProgress).unwrap();
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
        let enriched = enrich_with_head_mtimes(&store, listing, 1, &crate::progress::NoProgress).unwrap();
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
        let enriched = enrich_with_head_mtimes(&store, listing, 1, &crate::progress::NoProgress).unwrap();
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
        let enriched = enrich_with_head_mtimes(&store, listing, 1, &crate::progress::NoProgress).unwrap();
        let keys: Vec<_> = enriched.entities.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["a.md", "notes/", "notes/b.md"]);
    }

    /// Store wrapper that gauges the max number of concurrent `head` calls
    /// (I20 cycle 5 / I17-gauges: Condvar rendezvous, never wall-clock /
    /// never `yield_now`). I17-r1/F1: `OverlapRendezvous` latches
    /// `released` for the life of the wrapper (single gauge pass per
    /// instance) - do NOT reuse the wrapper across a sequential baseline
    /// leg: the conc-1 pass deadlocks (`target=2` never reached;
    /// `n_workers` was sized for the N leg). Comparison legs must run
    /// against `store.inner` (see `enrich_heads_bounded_parallel`; same
    /// trap documented on `enrich_parallel_vanished_warning_order_stable`).
    struct GaugedHeadStore {
        inner: MemoryStore,
        rendezvous: crate::testutil::OverlapRendezvous,
    }
    impl GaugedHeadStore {
        /// `n_workers` is the concurrency under test (pool size); the
        /// rendezvous target is 2 (any real overlap).
        fn new(n_workers: usize) -> Self {
            GaugedHeadStore {
                inner: MemoryStore::new(),
                rendezvous: crate::testutil::OverlapRendezvous::new(
                    2,
                    n_workers,
                    std::time::Duration::from_secs(5),
                ),
            }
        }
        fn max_in_flight(&self) -> usize {
            self.rendezvous.max_in_flight()
        }
    }
    impl ObjectStore for GaugedHeadStore {
        fn list(&self, prefix: &str) -> Result<Listing, Error> {
            self.inner.list(prefix)
        }
        fn head(&self, key: &str) -> Result<Entity, Error> {
            self.rendezvous.enter();
            let r = self.inner.head(key);
            self.rendezvous.leave();
            r
        }
        fn get_to(&self, key: &str, w: &mut dyn std::io::Write) -> Result<Entity, Error> {
            self.inner.get_to(key, w)
        }
        fn put_from(
            &self,
            key: &str,
            r: &mut dyn std::io::Read,
            size: u64,
            mtime: Option<u64>,
        ) -> Result<Entity, Error> {
            self.inner.put_from(key, r, size, mtime)
        }
        fn delete(&self, key: &str) -> Result<(), Error> {
            self.inner.delete(key)
        }
    }

    /// Store whose `head` fails specific keys with distinct messages (I20
    /// cycle 5 listing-order error-selection probe).
    struct KeyFailStore {
        inner: MemoryStore,
        fails: std::collections::HashMap<String, String>,
    }
    impl KeyFailStore {
        fn new(inner: MemoryStore) -> Self {
            KeyFailStore {
                inner,
                fails: std::collections::HashMap::new(),
            }
        }
        fn fail(&mut self, key: &str, msg: &str) {
            self.fails.insert(key.to_string(), msg.to_string());
        }
    }
    impl ObjectStore for KeyFailStore {
        fn list(&self, prefix: &str) -> Result<Listing, Error> {
            self.inner.list(prefix)
        }
        fn head(&self, key: &str) -> Result<Entity, Error> {
            if let Some(msg) = self.fails.get(key) {
                return Err(Error::Other(format!("{key}:{msg}")));
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
            mtime: Option<u64>,
        ) -> Result<Entity, Error> {
            self.inner.put_from(key, r, size, mtime)
        }
        fn delete(&self, key: &str) -> Result<(), Error> {
            self.inner.delete(key)
        }
    }

    #[test]
    fn enrich_heads_bounded_parallel() {
        // I20 cycle 5 / I17-gauges (W157): a 32-object enrichment at
        // concurrency 4 fans out - the max-in-flight `head` gauge must exceed
        // 1 (real overlap via Condvar rendezvous, not yield_now), stay <= 4,
        // and the enriched listing (order included) must equal the
        // concurrency-1 result exactly.
        let store = GaugedHeadStore::new(4);
        for i in 0..32 {
            let mut c = std::io::Cursor::new(b"x".to_vec());
            store
                .inner
                .put_from(&format!("k{i:02}.md"), &mut c, 1, Some(1000 + i))
                .unwrap();
        }
        let listing = store.inner.list("").unwrap();
        // I17-r1/F1 (W161): the concurrency-1 leg is an EQUALITY pin, not a
        // gauge, so it runs through `store.inner` (bare MemoryStore) - the
        // Condvar rendezvous latches `released` for the life of the wrapper,
        // so a conc-1 pass through the gauge deadlocks (target=2 never
        // reached; `n_workers` was sized for the N leg). Same trap as
        // `enrich_parallel_vanished_warning_order_stable`.
        let enriched1 = enrich_with_head_mtimes(&store.inner, listing.clone(), 1, &crate::progress::NoProgress).unwrap();
        let enriched4 = enrich_with_head_mtimes(&store, listing, 4, &crate::progress::NoProgress).unwrap();
        assert!(
            store.max_in_flight() > 1,
            "heads must overlap at concurrency 4 (max in-flight {})",
            store.max_in_flight()
        );
        assert!(store.max_in_flight() <= 4);
        assert_eq!(enriched4, enriched1, "enriched listings must be identical");
    }

    #[test]
    fn enrich_parallel_vanished_warning_order_stable() {
        // I20 cycle 5: vanished keys interleaved with healthy ones - the
        // bounded warning names them in listing order, identical across runs
        // and identical to the concurrency-1 result. Uses MemoryStore (not
        // GaugedHeadStore): this test is an order/determinism pin, not an
        // overlap gauge, and the Condvar rendezvous would deadlock on the
        // concurrency-1 leg (single-threaded enter never hits target=2).
        let store = MemoryStore::new();
        let mut entities = Vec::new();
        let mut c = std::io::Cursor::new(b"a".to_vec());
        store.put_from("a.md", &mut c, 1, Some(100)).unwrap();
        entities.push(file("a.md", 1, Some(9_999_999)));
        for i in 0..7 {
            entities.push(file(&format!("gone-{i}.md"), 5, Some(9_999_999)));
        }
        let mut c = std::io::Cursor::new(b"z".to_vec());
        store.put_from("z.md", &mut c, 1, Some(200)).unwrap();
        entities.push(file("z.md", 1, Some(9_999_999)));
        let listing = Listing {
            entities,
            warnings: vec!["pre-existing".to_string()],
        };
        let r1 = enrich_with_head_mtimes(&store, listing.clone(), 1, &crate::progress::NoProgress).unwrap();
        let r4 = enrich_with_head_mtimes(&store, listing, 4, &crate::progress::NoProgress).unwrap();
        assert_eq!(r1, r4);
        assert_eq!(
            r4.warnings[1],
            "7 listed key(s) vanished before head (deleted between LIST and HEAD); skipping: \
             gone-0.md, gone-1.md, gone-2.md, gone-3.md, gone-4.md and 2 more"
        );
    }

    #[test]
    fn enrich_parallel_fails_closed() {
        // I20 cycle 5 / I20-heads: a non-NotFound head error fails the whole
        // listing with that error, exactly like the sequential path. With two
        // non-NotFound errors on different keys the returned error is the one
        // of the key EARLIER in listing order (deterministic, independent of
        // completion order); the failed listing carries no partial entities
        // and no vanished warning (the warning is built on the success path
        // only). In-flight heads are not cancelled - extra completed requests
        // are accepted (documented behavior).
        let mut store = KeyFailStore::new(MemoryStore::new());
        let mut c = std::io::Cursor::new(b"a".to_vec());
        store.inner.put_from("a.md", &mut c, 1, Some(1)).unwrap();
        let mut c = std::io::Cursor::new(b"b".to_vec());
        store.inner.put_from("b.md", &mut c, 1, Some(2)).unwrap();
        let mut c = std::io::Cursor::new(b"c".to_vec());
        store.inner.put_from("c.md", &mut c, 1, Some(3)).unwrap();
        store.fail("a.md", "boom-a");
        store.fail("c.md", "boom-c");
        let listing = Listing {
            entities: vec![
                file("a.md", 1, Some(1)),
                file("b.md", 1, Some(2)),
                file("c.md", 1, Some(3)),
            ],
            warnings: Vec::new(),
        };
        let err = enrich_with_head_mtimes(&store, listing, 4, &crate::progress::NoProgress).unwrap_err();
        assert_eq!(
            format!("{err}"),
            "a.md:boom-a",
            "the error of the listing-earliest failing key must win"
        );
    }

    /// Store recording the order of `head` attempts (I20 cycle 5 order lock).
    struct HeadLogStore {
        inner: MemoryStore,
        log: std::sync::Mutex<Vec<String>>,
    }
    impl HeadLogStore {
        fn new(inner: MemoryStore) -> Self {
            HeadLogStore {
                inner,
                log: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }
    impl ObjectStore for HeadLogStore {
        fn list(&self, prefix: &str) -> Result<Listing, Error> {
            self.inner.list(prefix)
        }
        fn head(&self, key: &str) -> Result<Entity, Error> {
            self.log.lock().unwrap().push(key.to_string());
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
            mtime: Option<u64>,
        ) -> Result<Entity, Error> {
            self.inner.put_from(key, r, size, mtime)
        }
        fn delete(&self, key: &str) -> Result<(), Error> {
            self.inner.delete(key)
        }
    }

    /// Store whose `head` logs every attempt and fails a specific key with a
    /// non-NotFound error (I20-r1/F1 short-circuit probe). Combines the
    /// `HeadLogStore` attempt log with the `KeyFailStore` error injection.
    struct UnauthorizedHeadLogStore {
        inner: MemoryStore,
        log: std::sync::Mutex<Vec<String>>,
        fail: std::sync::Mutex<std::collections::HashSet<String>>,
    }
    impl UnauthorizedHeadLogStore {
        fn new(inner: MemoryStore) -> Self {
            Self {
                inner,
                log: std::sync::Mutex::new(Vec::new()),
                fail: std::sync::Mutex::new(std::collections::HashSet::new()),
            }
        }
        fn fail(&self, key: &str) {
            self.fail.lock().unwrap().insert(key.to_string());
        }
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }
    impl ObjectStore for UnauthorizedHeadLogStore {
        fn list(&self, prefix: &str) -> Result<Listing, Error> {
            self.inner.list(prefix)
        }
        fn head(&self, key: &str) -> Result<Entity, Error> {
            self.log.lock().unwrap().push(key.to_string());
            if self.fail.lock().unwrap().contains(key) {
                return Err(Error::Unauthorized(format!("denied:{key}")));
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
    fn enrich_concurrency_1_stops_at_first_hard_error() {
        // I20-r1/F1: at concurrency 1 the sequential path must short-circuit
        // on the FIRST non-NotFound head error (pre-I20 loop shape), instead
        // of issuing all N heads and then reporting the listing-earliest
        // error. `a.md`'s head fails with Unauthorized and every attempt is
        // logged, so the log is the ground truth for how many heads were
        // issued. RED today: the pooled path issues all heads even at
        // concurrency 1 (log == [a, b, c]).
        let store = UnauthorizedHeadLogStore::new(MemoryStore::new());
        for (key, body, mtime) in [("a.md", "a", 1), ("b.md", "b", 2), ("c.md", "c", 3)] {
            let mut c = std::io::Cursor::new(body.as_bytes().to_vec());
            store
                .inner
                .put_from(key, &mut c, body.len() as u64, Some(mtime))
                .unwrap();
        }
        store.fail("a.md");
        let listing = Listing {
            entities: vec![
                file("a.md", 1, Some(1)),
                file("b.md", 1, Some(2)),
                file("c.md", 1, Some(3)),
            ],
            warnings: Vec::new(),
        };
        let err = enrich_with_head_mtimes(&store, listing, 1, &crate::progress::NoProgress).unwrap_err();
        assert!(
            matches!(err, Error::Unauthorized(_)),
            "a.md's Unauthorized must fail the listing, got {err:?}"
        );
        assert_eq!(
            store.log(),
            vec!["a.md".to_string()],
            "sequential path must stop at the first hard error (attempt log: {:?})",
            store.log()
        );
    }

    #[test]
    fn enrich_parallel_issues_all_heads_on_error() {
        // I20-r1/F1 pin: the pooled path deliberately does NOT cancel
        // in-flight heads when a sibling fails - with a.md failing, all
        // three heads are still issued (documented, accepted behavior), and
        // the returned error is still the listing-earliest one (I20-heads
        // error lock). GREEN on arrival; guards against a "fix" that adds
        // cancellation to the pooled path.
        let store = UnauthorizedHeadLogStore::new(MemoryStore::new());
        for (key, body, mtime) in [("a.md", "a", 1), ("b.md", "b", 2), ("c.md", "c", 3)] {
            let mut c = std::io::Cursor::new(body.as_bytes().to_vec());
            store
                .inner
                .put_from(key, &mut c, body.len() as u64, Some(mtime))
                .unwrap();
        }
        store.fail("a.md");
        let listing = Listing {
            entities: vec![
                file("a.md", 1, Some(1)),
                file("b.md", 1, Some(2)),
                file("c.md", 1, Some(3)),
            ],
            warnings: Vec::new(),
        };
        let err = enrich_with_head_mtimes(&store, listing, 4, &crate::progress::NoProgress).unwrap_err();
        assert!(
            matches!(err, Error::Unauthorized(_)),
            "listing-earliest error must win, got {err:?}"
        );
        let mut attempted = store.log();
        attempted.sort();
        assert_eq!(
            attempted,
            vec!["a.md".to_string(), "b.md".to_string(), "c.md".to_string()],
            "pooled path must issue all heads even when one fails"
        );
    }

    #[test]
    fn enrich_concurrency_1_unchanged() {
        // I20-one: at concurrency 1 enrichment produces exactly today's
        // entities/warnings, and head attempts happen in listing order.
        let store = HeadLogStore::new(MemoryStore::new());
        for i in 0..4 {
            let mut c = std::io::Cursor::new(b"x".to_vec());
            store
                .inner
                .put_from(&format!("k{i}.md"), &mut c, 1, Some(1000 + i))
                .unwrap();
        }
        let listing = store.inner.list("").unwrap();
        let expected: Vec<String> = listing
            .entities
            .iter()
            .filter(|e| !e.is_folder())
            .map(|e| e.key.clone())
            .collect();
        let enriched = enrich_with_head_mtimes(&store, listing.clone(), 1, &crate::progress::NoProgress).unwrap();
        assert_eq!(
            store.log(),
            expected,
            "head attempts must run in listing order at concurrency 1"
        );
        assert_eq!(enriched.entities, listing.entities);
        assert_eq!(enriched.warnings, listing.warnings);
    }

    /// Store whose `head` fails with a non-NotFound error (throttling class).
    struct HeadFailStore {
        calls: std::sync::atomic::AtomicUsize,
    }
    impl HeadFailStore {
        fn new() -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }
    impl ObjectStore for HeadFailStore {
        fn list(&self, _prefix: &str) -> Result<Listing, Error> {
            Ok(Listing::default())
        }
        fn head(&self, _key: &str) -> Result<Entity, Error> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        calls: std::sync::atomic::AtomicUsize,
    }
    impl FlakyHeadStore {
        fn new(inner: MemoryStore, fail_first: usize, kind: FlakyKind) -> Self {
            Self {
                inner,
                fail_first,
                kind,
                calls: std::sync::atomic::AtomicUsize::new(0),
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
            let n = self
                .calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        let err = enrich_with_head_mtimes(&flaky, listing, 1, &crate::progress::NoProgress).unwrap_err();
        assert!(
            matches!(err, Error::Unavailable(_)),
            "a transient head error must fail the listing on the first attempt, got {err:?}"
        );
        assert_eq!(
            flaky.calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "must be a single head attempt"
        );
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
        let err = enrich_with_head_mtimes(&store, listing, 1, &crate::progress::NoProgress).unwrap_err();
        assert!(matches!(err, Error::Unavailable(_)));
        assert_eq!(store.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
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
        let err = enrich_with_head_mtimes(&flaky, listing, 1, &crate::progress::NoProgress).unwrap_err();
        assert!(matches!(err, Error::Unauthorized(_)));
        assert_eq!(flaky.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn enrich_fails_closed_on_head_error() {
        let listing = Listing {
            entities: vec![file("a.md", 1, Some(9_999_999))],
            warnings: Vec::new(),
        };
        let err = enrich_with_head_mtimes(&HeadFailStore::new(), listing, 1, &crate::progress::NoProgress).unwrap_err();
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
            1,
            &crate::progress::NoProgress,
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
            1,
            &crate::progress::NoProgress,
        )
        .unwrap();
        assert_eq!(folder_only.entities.len(), 1);
        assert_eq!(folder_only.entities[0].key, "notes/");
    }
}
