//! Inventory facade (issue 45, A3): the single reader of the remote file set
//! for plan build (warm manifest or live list+head) and the only writer of
//! the remote manifest (commit/repair).
//!
//! Layering (normative): `plan()` never knows the inventory source; this
//! facade is the only reader path used by status/push/pull for remote
//! entities. The local cache (S7) is optional and derived; delete/equality
//! never trust the cache alone.

use crate::config::InventoryMode;
use crate::entity::Entity;
use crate::error::Error;
use crate::store::ObjectStore;

/// Where the remote file set came from (issue 45, authority table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InventorySource {
    /// A valid remote manifest was parsed and used as the planning
    /// authority. `remote_etag` is the manifest object's etag.
    Manifest { remote_etag: Option<String> },
    /// Live list+head (I15 path): the manifest was absent, corrupt, or the
    /// mode forced it.
    LiveListHead,
}

/// The planning base the commit path needs (issue 45, D-plan-seam / 7.2):
/// full remote file set (pre-ignore, post-reserved) plus source metadata.
#[derive(Debug, Clone)]
pub struct InventoryBase {
    pub source: InventorySource,
    /// Full remote FILE entities (no folder views), pre-ignore.
    pub file_entities: Vec<Entity>,
    /// Object etag of `.vaultsync/manifest/v1.json` when the source was
    /// Manifest (the commit's If-Match base).
    pub manifest_etag: Option<String>,
}

/// A loaded remote inventory in planner input shape.
#[derive(Debug, Clone)]
pub struct RemoteInventory {
    /// Files + synthesized folder views (planner input shape).
    pub entities: Vec<Entity>,
    /// The base snapshot for a later manifest commit.
    pub base: InventoryBase,
    /// Advisory warnings (store listing drops, reserved leftovers).
    pub warnings: Vec<String>,
}

/// Load the remote inventory for plan build (issue 45, section 6.1).
///
/// `mode` selects warm vs cold; `concurrency` bounds the cold path's heads
/// (via the store's own list enrichment on S3); `cache` is `None` until S7.
/// Cold path: `store.list("")` + reserved partition (second-line guard; S3
/// already pre-partitions) - I15 stays intact.
pub fn load_remote_inventory(
    store: &dyn ObjectStore,
    mode: InventoryMode,
    _concurrency: u32,
    _cache: Option<&crate::inventory::CachePaths>,
) -> Result<RemoteInventory, Error> {
    match mode {
        InventoryMode::ListHead => live_list_head(store),
        InventoryMode::Auto => match try_load_manifest(store) {
            Ok(Some(inv)) => Ok(inv),
            Ok(None) => {
                // Missing manifest: auto falls back cold with a normative
                // warning (Q3: the read path stays side-effect free).
                let mut inv = live_list_head(store)?;
                inv.warnings.push(
                    "inventory manifest missing or corrupt; falling back to list+head (run vaultsync repair to write one)".to_string(),
                );
                Ok(inv)
            }
            Err(e) => Err(e),
        },
        InventoryMode::Manifest => {
            // Strict mode (W234): a missing OR corrupt manifest is a hard
            // error suggesting repair - never a silent empty plan, never a
            // cold fallback (fail closed). A non-NotFound fetch error
            // propagates via `?` (fail closed on store trouble too).
            match try_load_manifest(store)? {
                Some(inv) => Ok(inv),
                None => Err(Error::Other(
                    "inventory.mode=manifest requires a valid remote manifest; run vaultsync repair to create one".to_string(),
                )),
            }
        }
    }
}

/// Warm attempt: fetch + parse the remote manifest (issue 45, 6.1 steps 2-3
/// for the success path). `Ok(None)` means "no valid manifest to use"
/// (absent object OR corrupt body - the caller decides auto-fallback vs
/// strict error); a non-NotFound fetch error (e.g. Unavailable) fails closed
/// (never silently plan empty).
fn try_load_manifest(store: &dyn ObjectStore) -> Result<Option<RemoteInventory>, Error> {
    let mut buf = Vec::new();
    let entity = match store.get_to(crate::local::MANIFEST_KEY, &mut buf) {
        Ok(e) => e,
        Err(Error::NotFound(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    let manifest = match crate::manifest::parse_manifest_bytes(&buf) {
        Ok(m) => m,
        Err(_) => return Ok(None), // corrupt: caller falls back / errors per mode
    };
    let file_entities = crate::manifest::manifest_to_file_entities(&manifest)?;
    let folders = crate::manifest::synthesize_folders(&file_entities);
    let mut entities = file_entities.clone();
    entities.extend(folders);
    entities.sort_by(|a, b| a.key.cmp(&b.key));
    let remote_etag = entity.etag.clone();
    Ok(Some(RemoteInventory {
        base: InventoryBase {
            source: InventorySource::Manifest {
                remote_etag: remote_etag.clone(),
            },
            file_entities,
            manifest_etag: remote_etag,
        },
        entities,
        warnings: Vec::new(),
    }))
}

/// The cold path: today's `store.list("")` (list + head enrichment on S3),
/// with the empty-key drop (R4-M2) and the reserved partition (W63/A-L3 +
/// issue 45 control-plane). Folder views stay in `entities`; `file_entities`
/// carries files only (the commit base). I15-errors: a non-NotFound head
/// error still fails the whole listing (fail closed).
fn live_list_head(store: &dyn ObjectStore) -> Result<RemoteInventory, Error> {
    let listing = store.list("")?;
    let mut warnings = listing.warnings;
    // R4-M2: drop a remote empty key (the exact-prefix folder marker) before
    // validation. Every *other* invalid key stays fail-closed (R5-L1) at the
    // build_plan boundary.
    let entities: Vec<Entity> = listing
        .entities
        .into_iter()
        .filter(|e| !e.key.is_empty())
        .collect();
    // W79/r9 L1 + W219 (issue 45): reserved partition (control-plane +
    // final-segment probe/tmp). S3 already pre-partitions (W118) so this is
    // a second-line guard for other backends - the warning fires once.
    let (entities, reserved_dropped) = crate::partition_reserved_remote_keys(entities);
    if !reserved_dropped.is_empty() {
        warnings.push(crate::reserved_drops_warning(&reserved_dropped));
    }
    let file_entities: Vec<Entity> = entities
        .iter()
        .filter(|e| !e.is_folder())
        .cloned()
        .collect();
    Ok(RemoteInventory {
        entities,
        base: InventoryBase {
            source: InventorySource::LiveListHead,
            file_entities,
            manifest_etag: None,
        },
        warnings,
    })
}

/// Local cache paths (S7). Declared here so the facade signature is stable
/// from S4; populated in W243-W246.
#[derive(Debug, Clone)]
pub struct CachePaths {
    pub body: std::path::PathBuf,
    pub meta: std::path::PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::mock::MemoryStore;

    /// Double whose `list` is forbidden (panics if called): the warm path
    /// must never list - it serves the manifest via `get_to` only (W233 pin:
    /// warm planning does not issue per-object heads/list).
    struct NoListStore {
        inner: MemoryStore,
    }
    impl ObjectStore for NoListStore {
        fn list(&self, _prefix: &str) -> Result<crate::store::Listing, Error> {
            panic!("warm path must not call list");
        }
        fn head(&self, key: &str) -> Result<Entity, Error> {
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

    fn manifest_body(entries: &[(&str, u64, Option<u64>)]) -> Vec<u8> {
        let m = crate::manifest::file_entities_to_manifest(
            &entries
                .iter()
                .map(|(k, s, m)| crate::entity::file(k, *s, *m))
                .collect::<Vec<_>>(),
            42,
            None,
            None,
        )
        .unwrap();
        crate::manifest::serialize_manifest(&m).unwrap()
    }

    #[test]
    fn warm_path_reads_manifest_without_list() {
        // W233 (issue 45): with a valid remote manifest, mode Auto plans from
        // it - `list` is never called (a double that panics on `list` proves
        // it), the source is Manifest with the manifest etag, and the
        // entities are files + synthesized folders (no per-object heads).
        let store = NoListStore {
            inner: MemoryStore::new(),
        };
        let body = manifest_body(&[("notes/a.md", 3, Some(100)), ("b.md", 1, None)]);
        let body_len = body.len() as u64;
        let mut c = std::io::Cursor::new(body);
        let put = store
            .inner
            .put_from(crate::local::MANIFEST_KEY, &mut c, body_len, None)
            .unwrap();
        let inv = load_remote_inventory(&store, InventoryMode::Auto, 1, None).unwrap();
        assert_eq!(
            inv.base.source,
            InventorySource::Manifest {
                remote_etag: put.etag.clone()
            }
        );
        assert_eq!(inv.base.manifest_etag, put.etag);
        let file_keys: Vec<&str> = inv
            .base
            .file_entities
            .iter()
            .map(|e| e.key.as_str())
            .collect();
        assert_eq!(file_keys, vec!["b.md", "notes/a.md"]);
        let keys: Vec<&str> = inv.entities.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["b.md", "notes/", "notes/a.md"],
            "files + synthesized folder view"
        );
        assert!(inv.warnings.is_empty(), "{:?}", inv.warnings);
    }

    #[test]
    fn auto_falls_back_cold_when_manifest_missing() {
        // W233 (issue 45): no manifest object => mode Auto warns and falls
        // back to live list+head (source LiveListHead); the warning pins the
        // normative substring "falling back" + "list+head".
        let store = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"aaa".to_vec());
        store
            .put_from("a.md", &mut c, 3, Some(1_600_000_000_000))
            .unwrap();
        let inv = load_remote_inventory(&store, InventoryMode::Auto, 1, None).unwrap();
        assert_eq!(inv.base.source, InventorySource::LiveListHead);
        let file_keys: Vec<&str> = inv
            .base
            .file_entities
            .iter()
            .map(|e| e.key.as_str())
            .collect();
        assert_eq!(file_keys, vec!["a.md"]);
        let warn = inv.warnings.join(" ");
        assert!(warn.contains("falling back"), "warnings: {warn}");
        assert!(warn.contains("list+head"), "warnings: {warn}");
    }

    #[test]
    fn auto_falls_back_cold_when_manifest_corrupt() {
        // W233 (issue 45): a corrupt manifest body (bad JSON) in mode Auto
        // warns and falls back cold - never plans against a degraded
        // manifest, never errors the whole run.
        let store = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"not json at all".to_vec());
        store
            .put_from(
                crate::local::MANIFEST_KEY,
                &mut c,
                "not json at all".len() as u64,
                None,
            )
            .unwrap();
        let mut c = std::io::Cursor::new(b"aaa".to_vec());
        store
            .put_from("a.md", &mut c, 3, Some(1_600_000_000_000))
            .unwrap();
        let inv = load_remote_inventory(&store, InventoryMode::Auto, 1, None).unwrap();
        assert_eq!(inv.base.source, InventorySource::LiveListHead);
        let warn = inv.warnings.join(" ");
        assert!(warn.contains("falling back"), "warnings: {warn}");
        assert!(warn.contains("list+head"), "warnings: {warn}");
    }

    #[test]
    fn strict_mode_requires_valid_manifest() {
        // W234 (issue 45): mode `manifest` fails CLOSED when the manifest is
        // missing or corrupt - never a silent empty plan, never a cold
        // fallback. The error suggests `repair`.
        let store = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"aaa".to_vec());
        store
            .put_from("a.md", &mut c, 3, Some(1_600_000_000_000))
            .unwrap();
        let err = load_remote_inventory(&store, InventoryMode::Manifest, 1, None).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("repair"), "error must suggest repair: {msg}");

        let store2 = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"not json".to_vec());
        store2
            .put_from(
                crate::local::MANIFEST_KEY,
                &mut c,
                "not json".len() as u64,
                None,
            )
            .unwrap();
        let err = load_remote_inventory(&store2, InventoryMode::Manifest, 1, None).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("repair"), "error must suggest repair: {msg}");
    }

    #[test]
    fn warm_fetch_non_not_found_fails_closed() {
        // W233/W234 (issue 45, 6.1): a NON-NotFound fetch error (e.g. the
        // store is unavailable) fails the whole load closed - the facade
        // must never silently plan empty or pretend a missing manifest is a
        // healthy empty remote.
        struct FailGetStore(MemoryStore);
        impl ObjectStore for FailGetStore {
            fn list(&self, prefix: &str) -> Result<crate::store::Listing, Error> {
                self.0.list(prefix)
            }
            fn head(&self, key: &str) -> Result<Entity, Error> {
                self.0.head(key)
            }
            fn get_to(&self, _key: &str, _w: &mut dyn std::io::Write) -> Result<Entity, Error> {
                Err(Error::Unavailable("store down".to_string()))
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
        let err = load_remote_inventory(
            &FailGetStore(MemoryStore::new()),
            InventoryMode::Auto,
            1,
            None,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Unavailable(_)), "got {err:?}");
    }

    #[test]
    fn cold_path_uses_list_and_splits_files_folders() {
        // W232 (issue 45): mode `list_head` always hits `store.list("")` and
        // returns the planner input shape: file entities + synthesized folder
        // views in `entities`, files-only in `base.file_entities`, source
        // LiveListHead. Reserved control-plane keys are dropped (and named by
        // the shared warning); a healthy file survives.
        let store = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"aaa".to_vec());
        store
            .put_from("notes/a.md", &mut c, 3, Some(1_600_000_000_000))
            .unwrap();
        let mut c = std::io::Cursor::new(b"j".to_vec());
        store
            .put_from(
                crate::local::MANIFEST_KEY,
                &mut c,
                1,
                Some(1_600_000_000_000),
            )
            .unwrap();
        let inv = load_remote_inventory(&store, InventoryMode::ListHead, 1, None).unwrap();
        let keys: Vec<&str> = inv.entities.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["notes/", "notes/a.md"],
            "planner input: file + folder view, no control-plane: {keys:?}"
        );
        let file_keys: Vec<&str> = inv
            .base
            .file_entities
            .iter()
            .map(|e| e.key.as_str())
            .collect();
        assert_eq!(file_keys, vec!["notes/a.md"], "files-only base");
        assert_eq!(inv.base.source, InventorySource::LiveListHead);
        assert_eq!(inv.base.manifest_etag, None);
        assert!(
            inv.warnings
                .iter()
                .any(|w| w.contains("reserved vaultsync namespace")),
            "control-plane drop must be surfaced: {:?}",
            inv.warnings
        );
    }
}
