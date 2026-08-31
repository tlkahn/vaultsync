//! In-memory [`ObjectStore`] implementation for tests and Phase 1 CLI smoke.
//!
//! Key-validation latitude (trait N1): the trait lets read/delete paths answer
//! [`Error::NotFound`] for invalid keys, and this mock exercises that
//! latitude - `head`/`get_to`/`delete` do not validate. The S3 backend
//! differs: it validates before any outbound call.

use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Write};
use std::sync::Mutex;

use crate::entity::Entity;
use crate::error::Error;
use crate::store::{Listing, ObjectStore};

/// A stored object's payload and metadata.
struct MockObject {
    bytes: Vec<u8>,
    mtime_ms: Option<u64>,
    etag: String,
}

/// In-memory store. Files live in a map; folders are synthesized on `list`
/// from file-key parents, matching the "no folder objects" remote default.
///
/// Etags are content-derived (FNV-1a-64 over stored bytes, lowercase hex), so
/// they are comparable across store instances and processes: same content
/// yields the same etag, different content yields different etags. The planner
/// still treats etags as opaque (Phase 1 never compares them).
pub struct MemoryStore {
    objects: Mutex<HashMap<String, MockObject>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        MemoryStore {
            objects: Mutex::new(HashMap::new()),
        }
    }

    fn entity_for(&self, key: &str, obj: &MockObject) -> Entity {
        Entity {
            key: key.to_string(),
            size: obj.bytes.len() as u64,
            mtime_ms: obj.mtime_ms,
            etag: Some(obj.etag.clone()),
        }
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        MemoryStore::new()
    }
}

/// Ancestor folder keys (each trailing-`/` prefix) of a key. Shared
/// implementation (W230): see [`crate::manifest::parent_folders`].
fn parent_folders(key: &str) -> Vec<String> {
    crate::manifest::parent_folders(key)
}

/// Read exactly `n` bytes from `r` into a fresh `Vec`, or fail with
/// `UnexpectedEof` on a short read. Never preallocates caller-controlled
/// `n` bytes up front: `Read::take` bounds the read and the buffer grows as
/// data arrives, so a huge `n` with a short reader errors immediately
/// instead of attempting a `size`-driven allocation (P1r5-put-prealloc).
fn read_exact_n(r: &mut dyn Read, n: u64) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    r.take(n).read_to_end(&mut buf)?;
    if buf.len() as u64 != n {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "short read",
        ));
    }
    Ok(buf)
}

/// FNV-1a 64-bit hash over a byte slice (std-only, no crates). Also used by
/// the inventory cache body fingerprint (W259/N3).
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

impl ObjectStore for MemoryStore {
    fn list(&self, prefix: &str) -> Result<Listing, Error> {
        let guard = self.objects.lock().unwrap();
        let mut keys: BTreeSet<String> = guard.keys().cloned().collect();
        let folders: Vec<String> = keys.iter().flat_map(|k| parent_folders(k)).collect();
        for f in folders {
            keys.insert(f);
        }

        let mut entities = Vec::new();
        for key in keys {
            if !key.starts_with(prefix) {
                continue;
            }
            if key.ends_with('/') {
                entities.push(Entity {
                    key,
                    size: 0,
                    mtime_ms: None,
                    etag: None,
                });
            } else if let Some(obj) = guard.get(&key) {
                entities.push(self.entity_for(&key, obj));
            }
        }
        Ok(Listing {
            entities,
            warnings: Vec::new(),
        })
    }

    fn head(&self, key: &str) -> Result<Entity, Error> {
        let guard = self.objects.lock().unwrap();
        match guard.get(key) {
            Some(o) => Ok(self.entity_for(key, o)),
            None => Err(Error::NotFound(key.to_string())),
        }
    }

    fn get_to(&self, key: &str, w: &mut dyn Write) -> Result<Entity, Error> {
        // Clone the payload under the lock, then drop the guard before
        // touching the caller's writer: `w` may re-enter this store (head/
        // list) and must not deadlock against the same mutex.
        let (bytes, entity) = {
            let guard = self.objects.lock().unwrap();
            let obj = guard
                .get(key)
                .ok_or_else(|| Error::NotFound(key.to_string()))?;
            (obj.bytes.clone(), self.entity_for(key, obj))
        };
        w.write_all(&bytes)?;
        Ok(entity)
    }

    fn put_from(
        &self,
        key: &str,
        r: &mut dyn Read,
        size: u64,
        mtime_ms: Option<u64>,
    ) -> Result<Entity, Error> {
        crate::entity::ensure_valid_key(key)?;
        if key.ends_with('/') {
            // Folder markers are Phase 2+; puts are file keys only. Rejecting
            // here keeps list/head/get consistent (no split-brain folder put).
            return Err(Error::InvalidKey(format!(
                "put_from does not accept folder keys: {key:?}"
            )));
        }
        let bytes = read_exact_n(r, size)?;
        let etag = format!("{:016x}", fnv1a(&bytes));
        let obj = MockObject {
            bytes,
            mtime_ms,
            etag: etag.clone(),
        };
        let entity = Entity {
            key: key.to_string(),
            size: obj.bytes.len() as u64,
            mtime_ms,
            etag: Some(etag),
        };
        self.objects.lock().unwrap().insert(key.to_string(), obj);
        Ok(entity)
    }

    fn put_from_with(
        &self,
        key: &str,
        r: &mut dyn Read,
        size: u64,
        opts: crate::store::PutOpts,
    ) -> Result<Entity, Error> {
        // W223 (issue 45): real If-None-Match: * and If-Match against the
        // in-memory etag so mock race tests lock the manifest commit
        // semantics offline. The precondition is checked against the CURRENT
        // stored object before any body read; the mock is single-threaded in
        // practice (tests + status), so check-then-insert needs no extra
        // locking - the race-free variant is what the S3 backend provides via
        // its conditional headers.
        crate::entity::ensure_valid_key(key)?;
        if key.ends_with('/') {
            return Err(Error::InvalidKey(format!(
                "put_from does not accept folder keys: {key:?}"
            )));
        }
        {
            let guard = self.objects.lock().unwrap();
            if opts.if_none_match_star && guard.contains_key(key) {
                return Err(Error::PreconditionFailed(format!(
                    "key already exists: {key}"
                )));
            }
            if let Some(want) = &opts.if_match_etag {
                let current = guard.get(key);
                let ok = matches!(current, Some(obj) if &obj.etag == want);
                if !ok {
                    return Err(Error::PreconditionFailed(format!(
                        "etag mismatch for {key}"
                    )));
                }
            }
        }
        let bytes = read_exact_n(r, size)?;
        let etag = format!("{:016x}", fnv1a(&bytes));
        let obj = MockObject {
            bytes,
            mtime_ms: opts.mtime_ms,
            etag: etag.clone(),
        };
        let entity = Entity {
            key: key.to_string(),
            size: obj.bytes.len() as u64,
            mtime_ms: opts.mtime_ms,
            etag: Some(etag),
        };
        self.objects.lock().unwrap().insert(key.to_string(), obj);
        Ok(entity)
    }

    fn get_to_with(
        &self,
        key: &str,
        w: &mut dyn Write,
        opts: crate::store::GetOpts,
    ) -> Result<crate::store::GetOutcome, Error> {
        // W224 (issue 45): If-None-Match against the in-memory etag - a
        // matching etag answers NotModified with head-like metadata and the
        // writer is untouched (304 semantics, no body stream).
        if let Some(want) = &opts.if_none_match_etag {
            let entity = self.head(key)?;
            if entity.etag.as_deref() == Some(want.as_str()) {
                return Ok(crate::store::GetOutcome::NotModified(entity));
            }
        }
        self.get_to(key, w).map(crate::store::GetOutcome::Body)
    }

    fn delete(&self, key: &str) -> Result<(), Error> {
        let mut guard = self.objects.lock().unwrap();
        match guard.remove(key) {
            Some(_) => Ok(()),
            None => Err(Error::NotFound(key.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_is_send_sync() {
        // I20-traits: `MemoryStore` is `Mutex`-backed (already `Send +
        // Sync`); the pin locks the property so the pool can share it across
        // worker threads.
        fn assert_ss<T: ?Sized + Send + Sync>() {}
        assert_ss::<MemoryStore>();
    }

    fn new_store() -> MemoryStore {
        MemoryStore::new()
    }

    fn put_str(
        store: &MemoryStore,
        key: &str,
        body: &str,
        mtime: Option<u64>,
    ) -> Result<Entity, Error> {
        let mut cursor = std::io::Cursor::new(body.as_bytes().to_vec());
        store.put_from(key, &mut cursor, body.len() as u64, mtime)
    }

    fn get_str(store: &MemoryStore, key: &str) -> Result<String, Error> {
        let mut buf = Vec::new();
        store.get_to(key, &mut buf)?;
        Ok(String::from_utf8(buf).unwrap())
    }

    #[test]
    fn mock_put_from_large_size_short_reader_errors() {
        // A caller-declared `size` far larger than the actual reader payload
        // must produce a short-read error, not a silent truncation. The
        // 50 MB figure is big enough to catch a size-driven prealloc without
        // depending on OOM behavior in CI (H2).
        let store = new_store();
        let mut cursor = std::io::Cursor::new(b"x".to_vec());
        let err = store
            .put_from("a.md", &mut cursor, 50_000_000, None)
            .unwrap_err();
        assert!(
            matches!(err, Error::Io(ref e) if e.kind() == std::io::ErrorKind::UnexpectedEof),
            "expected UnexpectedEof, got {err:?}"
        );
        // nothing must be stored
        assert_eq!(store.list("").unwrap().entities, Vec::<Entity>::new());
    }

    #[test]
    fn mock_put_from_huge_size_short_reader_errors() {
        // `u64::MAX` size with a 1-byte reader: must error (`UnexpectedEof`)
        // without ever attempting a `size`-driven allocation. Running this
        // against the old `vec![0u8; n]` code path aborts the process
        // (handle_alloc_error), which is exactly the defect this locks (H2).
        let store = new_store();
        let mut cursor = std::io::Cursor::new(b"x".to_vec());
        let err = store
            .put_from("a.md", &mut cursor, u64::MAX, None)
            .unwrap_err();
        assert!(
            matches!(err, Error::Io(ref e) if e.kind() == std::io::ErrorKind::UnexpectedEof),
            "expected UnexpectedEof, got {err:?}"
        );
        assert_eq!(store.list("").unwrap().entities, Vec::<Entity>::new());
    }

    #[test]
    fn mock_put_rejects_control_char_key() {
        // `put_from` inherits the full `ensure_valid_key` rule set, including
        // control chars (B1); nothing must be stored on rejection.
        let store = new_store();
        let mut cursor = std::io::Cursor::new(b"x".to_vec());
        let err = store.put_from("a/\nb", &mut cursor, 1, None).unwrap_err();
        assert!(matches!(err, Error::InvalidKey(_)));
        assert_eq!(store.list("").unwrap().entities, Vec::<Entity>::new());
    }

    #[test]
    fn mock_put_rejects_folder_key() {
        let store = new_store();
        let mut cursor = std::io::Cursor::new(b"x".to_vec());
        let err = store.put_from("notes/", &mut cursor, 1, None).unwrap_err();
        assert!(matches!(err, Error::InvalidKey(_)));
        // nothing must be stored: list stays empty, head is NotFound
        assert_eq!(store.list("").unwrap().entities, Vec::<Entity>::new());
        assert!(matches!(
            store.head("notes/").unwrap_err(),
            Error::NotFound(_)
        ));
    }

    #[test]
    fn mock_put_rejects_invalid_key_dotdot() {
        let store = new_store();
        let mut cursor = std::io::Cursor::new(b"x".to_vec());
        let err = store.put_from("../x", &mut cursor, 1, None).unwrap_err();
        assert!(matches!(err, Error::InvalidKey(_)));
        // key must not have been inserted
        assert_eq!(store.list("").unwrap().entities, Vec::<Entity>::new());
    }

    #[test]
    fn mock_put_rejects_leading_slash() {
        let store = new_store();
        let mut cursor = std::io::Cursor::new(b"x".to_vec());
        let err = store.put_from("/a.md", &mut cursor, 1, None).unwrap_err();
        assert!(matches!(err, Error::InvalidKey(_)));
    }

    #[test]
    fn mock_put_get_roundtrip() {
        let store = new_store();
        put_str(&store, "a.md", "hello world", Some(1000)).unwrap();
        assert_eq!(get_str(&store, "a.md").unwrap(), "hello world");
        let h = store.head("a.md").unwrap();
        assert_eq!(h.size, 11);
        assert_eq!(h.mtime_ms, Some(1000));
        assert!(h.etag.is_some());
    }

    /// A writer that re-enters the store from inside `write`. `get_to` must
    /// not hold the map lock across the caller's writer, or this deadlocks
    /// on the non-reentrant mutex (regression lock for R2.4).
    struct ReentrantWrite<'a> {
        store: &'a MemoryStore,
        key: String,
        buf: Vec<u8>,
    }
    impl Write for ReentrantWrite<'_> {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            // Re-enters the store: would deadlock if the map guard were held.
            let _ = self.store.head(&self.key);
            let _ = self.store.list("");
            self.buf.extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn mock_get_to_does_not_require_reentrant_store() {
        let store = new_store();
        put_str(&store, "a.md", "hello", None).unwrap();
        let mut w = ReentrantWrite {
            store: &store,
            key: "a.md".to_string(),
            buf: Vec::new(),
        };
        let e = store.get_to("a.md", &mut w).unwrap();
        assert_eq!(w.buf, b"hello");
        assert_eq!(e.size, 5);
    }

    #[test]
    fn mock_get_missing_not_found() {
        let store = new_store();
        let mut buf = Vec::new();
        let err = store.get_to("nope", &mut buf).unwrap_err();
        assert!(matches!(err, Error::NotFound(_)));
    }

    #[test]
    fn mock_delete_removes() {
        let store = new_store();
        put_str(&store, "a.md", "x", None).unwrap();
        store.delete("a.md").unwrap();
        let mut buf = Vec::new();
        assert!(matches!(
            store.get_to("a.md", &mut buf).unwrap_err(),
            Error::NotFound(_)
        ));
    }

    #[test]
    fn mock_delete_missing() {
        let store = new_store();
        assert!(matches!(
            store.delete("missing").unwrap_err(),
            Error::NotFound(_)
        ));
    }

    #[test]
    fn mock_list_all_and_prefix() {
        let store = new_store();
        put_str(&store, "a.md", "a", None).unwrap();
        put_str(&store, "notes/b.md", "b", None).unwrap();
        let all = store.list("").unwrap().entities;
        let keys: Vec<_> = all.iter().map(|e| e.key.clone()).collect();
        assert!(keys.iter().any(|k| k == "a.md"));
        assert!(keys.iter().any(|k| k == "notes/b.md"));

        let notes = store.list("notes/").unwrap().entities;
        let nkeys: Vec<_> = notes.iter().map(|e| e.key.clone()).collect();
        assert!(nkeys.iter().any(|k| k == "notes/b.md"));
        assert!(!nkeys.iter().any(|k| k == "a.md"));
    }

    #[test]
    fn mock_list_prefix_is_raw_starts_with() {
        // Locks the documented behavior: prefix is a raw string prefix, not a
        // path-segment boundary. `note` matches `note.md`, the synthesized
        // `notes/` folder, and `notes/b.md` alike.
        let store = new_store();
        put_str(&store, "note.md", "a", None).unwrap();
        put_str(&store, "notes/b.md", "b", None).unwrap();
        let got = store.list("note").unwrap().entities;
        let keys: Vec<_> = got.iter().map(|e| e.key.clone()).collect();
        assert!(keys.iter().any(|k| k == "note.md"));
        assert!(keys.iter().any(|k| k == "notes/"));
        assert!(keys.iter().any(|k| k == "notes/b.md"));
        // delimiter-aligned prefix still narrows correctly
        let skinny: Vec<_> = store
            .list("notes/")
            .unwrap()
            .entities
            .iter()
            .map(|e| e.key.clone())
            .collect();
        assert!(skinny.iter().any(|k| k == "notes/b.md"));
        assert!(!skinny.iter().any(|k| k == "note.md"));
    }

    #[test]
    fn mock_list_synthesizes_folder_prefixes() {
        let store = new_store();
        put_str(&store, "notes/b.md", "b", None).unwrap();
        let all = store.list("").unwrap().entities;
        let folder = all.iter().find(|e| e.key == "notes/").expect("folder");
        assert_eq!(folder.size, 0);
        assert!(folder.is_folder());
    }

    #[test]
    fn mock_etag_differs_for_different_content_across_stores() {
        // Two fresh store instances, different content: etags must differ
        // (content-derived, not a per-instance counter).
        let s1 = new_store();
        let s2 = new_store();
        let e1 = put_str(&s1, "f.md", "foo", None).unwrap();
        let e2 = put_str(&s2, "f.md", "bar", None).unwrap();
        assert_ne!(e1.etag, e2.etag);
    }

    #[test]
    fn mock_etag_stable_for_same_content() {
        // Re-putting identical content must yield the same etag (content-derived).
        let store = new_store();
        let first = put_str(&store, "g.md", "same", None).unwrap();
        let second = put_str(&store, "g.md", "same", None).unwrap();
        assert_eq!(first.etag, second.etag);
    }

    #[test]
    fn mock_etag_equal_for_same_content_across_stores() {
        // Same content on two fresh stores: equal etags (property Phase 2
        // cross-store fixtures rely on).
        let s1 = new_store();
        let s2 = new_store();
        let e1 = put_str(&s1, "h.md", "same", None).unwrap();
        let e2 = put_str(&s2, "h.md", "same", None).unwrap();
        assert_eq!(e1.etag, e2.etag);
    }

    #[test]
    fn mock_folder_keys_are_not_object_targets() {
        // `list` synthesizes folder views; folder keys are not objects.
        // head/get_to/delete must answer NotFound for them (locks the
        // A-low-1/B3 contract; callers branch on Entity::is_folder()).
        let store = new_store();
        put_str(&store, "notes/a.md", "a", None).unwrap();
        let all = store.list("").unwrap().entities;
        assert!(all.iter().any(|e| e.key == "notes/"));
        assert!(matches!(
            store.head("notes/").unwrap_err(),
            Error::NotFound(_)
        ));
        let mut buf = Vec::new();
        assert!(matches!(
            store.get_to("notes/", &mut buf).unwrap_err(),
            Error::NotFound(_)
        ));
        assert!(matches!(
            store.delete("notes/").unwrap_err(),
            Error::NotFound(_)
        ));
    }

    #[test]
    fn mock_conditional_put_create_and_race() {
        use crate::store::{GetOpts, GetOutcome, PutOpts};
        // W223 (issue 45): MemoryStore implements real If-None-Match: * and
        // If-Match conditionals so mock race tests lock the manifest commit
        // semantics offline. `if_none_match_star` creates on an absent key,
        // fails with `PreconditionFailed` (never clobbering) when the key
        // already exists; `if_match_etag` succeeds on a matching etag and
        // fails without changing the body on a mismatch.
        let store = new_store();
        // create: absent key + If-None-Match * => Ok.
        let mut c = std::io::Cursor::new(b"first".to_vec());
        let created = store
            .put_from_with(
                "m.json",
                &mut c,
                5,
                PutOpts {
                    if_none_match_star: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let etag = created.etag.clone().expect("etag");
        // second create: key exists => PreconditionFailed, body unchanged.
        let mut c = std::io::Cursor::new(b"clobber".to_vec());
        let err = store
            .put_from_with(
                "m.json",
                &mut c,
                7,
                PutOpts {
                    if_none_match_star: true,
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(matches!(err, Error::PreconditionFailed(_)), "got {err:?}");
        assert_eq!(get_str(&store, "m.json").unwrap(), "first");
        // If-Match with the correct etag => Ok.
        let mut c = std::io::Cursor::new(b"second".to_vec());
        let ok = store
            .put_from_with(
                "m.json",
                &mut c,
                6,
                PutOpts {
                    if_match_etag: Some(etag.clone()),
                    ..Default::default()
                },
            )
            .unwrap();
        // Body replaced; the content-derived etag therefore changed.
        assert!(ok.etag.is_some());
        assert_ne!(ok.etag.as_deref(), Some(etag.as_str()));
        // If-Match with a stale/wrong etag => PreconditionFailed, body
        // unchanged (no clobber).
        let mut c = std::io::Cursor::new(b"clobber".to_vec());
        let err = store
            .put_from_with(
                "m.json",
                &mut c,
                7,
                PutOpts {
                    if_match_etag: Some("\"stale\"".to_string()),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(matches!(err, Error::PreconditionFailed(_)), "got {err:?}");
        assert_eq!(get_str(&store, "m.json").unwrap(), "second");
        // Conditional put still validates keys and rejects folder keys.
        let mut c = std::io::Cursor::new(b"x".to_vec());
        let err = store
            .put_from_with(
                "../bad",
                &mut c,
                1,
                PutOpts {
                    if_none_match_star: true,
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(matches!(err, Error::InvalidKey(_)), "got {err:?}");
        // 304 shape available for later W224 tests (GetOutcome import sanity).
        let mut buf = Vec::new();
        let out = store
            .get_to_with(
                "m.json",
                &mut buf,
                GetOpts {
                    if_none_match_etag: Some("\"nope\"".to_string()),
                },
            )
            .unwrap();
        assert!(matches!(out, GetOutcome::Body(_)));
    }

    #[test]
    fn mock_conditional_get_304_not_modified() {
        use crate::store::{GetOpts, GetOutcome, PutOpts};
        // W224 (issue 45): If-None-Match with the CURRENT etag answers
        // NotModified - the writer is untouched (0 bytes written), and the
        // outcome carries head-like metadata. A mismatched etag streams the
        // body (Body). The 304 path must never hit the writer's `write`.
        let store = new_store();
        let mut c = std::io::Cursor::new(b"body".to_vec());
        let e = store
            .put_from_with(
                "m.json",
                &mut c,
                4,
                PutOpts {
                    mtime_ms: Some(123),
                    ..Default::default()
                },
            )
            .unwrap();
        let etag = e.etag.clone().unwrap();

        // Matching etag: NotModified, writer untouched.
        let mut buf = Vec::new();
        let out = store
            .get_to_with(
                "m.json",
                &mut buf,
                GetOpts {
                    if_none_match_etag: Some(etag.clone()),
                },
            )
            .unwrap();
        match out {
            GetOutcome::NotModified(m) => {
                assert_eq!(m.key, "m.json");
                assert_eq!(m.size, 4);
                assert_eq!(m.mtime_ms, Some(123));
            }
            other => panic!("expected NotModified, got {other:?}"),
        }
        assert!(buf.is_empty(), "304 must not write to the writer: {buf:?}");

        // Mismatched etag: Body streamed.
        let mut buf = Vec::new();
        let out = store
            .get_to_with(
                "m.json",
                &mut buf,
                GetOpts {
                    if_none_match_etag: Some("\"stale\"".to_string()),
                },
            )
            .unwrap();
        assert!(matches!(out, GetOutcome::Body(_)));
        assert_eq!(buf, b"body");

        // Missing key: NotFound propagates (no silent 304).
        let mut buf = Vec::new();
        let err = store
            .get_to_with(
                "nope",
                &mut buf,
                GetOpts {
                    if_none_match_etag: Some(etag.clone()),
                },
            )
            .unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "got {err:?}");
    }

    #[test]
    fn mock_overwrite_put() {
        let store = new_store();
        let first = put_str(&store, "a.md", "one", Some(10)).unwrap();
        let second = put_str(&store, "a.md", "twolong", Some(20)).unwrap();
        assert_eq!(get_str(&store, "a.md").unwrap(), "twolong");
        let h = store.head("a.md").unwrap();
        assert_eq!(h.size, 7);
        assert_eq!(h.mtime_ms, Some(20));
        assert!(first.etag != second.etag);
    }
}
