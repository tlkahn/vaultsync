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

/// Inventory knobs threaded into [`crate::build_plan`] (issue 45,
/// D-plan-seam): the resolved `[inventory].mode`, the transfer concurrency
/// (bounds the cold path's heads via the store), and the vault root
/// (`Some` enables the S7 local cache; `None` until W243).
#[derive(Debug, Clone)]
pub struct InventoryOpts {
    pub mode: InventoryMode,
    pub concurrency: u32,
    pub vault_root: Option<std::path::PathBuf>,
}

impl InventoryOpts {
    /// Today's behavior: always live list+head, no manifest coupling, no
    /// cache. Explicit (not `Default`) so planner tests cannot accidentally
    /// couple warm (W235: prefer `ListHead` for pure planner tests that seed
    /// MemoryStore without manifests).
    pub fn list_head() -> Self {
        InventoryOpts {
            mode: InventoryMode::ListHead,
            concurrency: 1,
            vault_root: None,
        }
    }
}

impl Default for InventoryOpts {
    /// Product default (Q1): `auto`, concurrency 1 (callers override with
    /// their resolved settings), no cache until S7.
    fn default() -> Self {
        InventoryOpts {
            mode: InventoryMode::Auto,
            concurrency: 1,
            vault_root: None,
        }
    }
}

/// A successful remote mutation to fold into the manifest commit (issue 45,
/// D-commit-when / W237). Only successes are listed: a failed upload/delete
/// must never upsert/remove its key (bodies-first crash order, 7.5).
#[derive(Debug, Clone, PartialEq)]
pub enum CommitMutation {
    /// An upload that succeeded. The remote `Entity` (size/mtime/etag) comes
    /// from the `put_from` result so the manifest copies the true remote
    /// etag (I15 mtime identity preserved).
    Upload(Entity),
    /// A remote delete that succeeded (key only).
    DeleteRemote(String),
}

/// Outcome of a manifest commit (W238).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitOutcome {
    /// The manifest was written; `etag` is the new object etag (may be None
    /// on backends that do not report one), `entry_count` the committed
    /// entry count.
    Written {
        etag: Option<String>,
        entry_count: usize,
    },
    /// Zero successful remote mutations: nothing to commit (the manifest
    /// already matches the plan's view of remote for those keys).
    SkippedNoMutations,
    /// The conditional put lost its race (If-Match / If-None-Match failed):
    /// the manifest was NOT overwritten. Bodies from this run may still be
    /// live; the caller warns and suggests repair.
    PreconditionFailed,
}

/// Fold successful mutations into the base file set (W237, pure): uploads
/// upsert size/mtime/etag; deletes remove; anything not in `successes` keeps
/// its base entry. Output is sorted by key with no duplicates.
pub(crate) fn apply_commit_mutations(
    base_files: &[Entity],
    successes: &[CommitMutation],
) -> Vec<Entity> {
    let mut map: std::collections::BTreeMap<String, Entity> = base_files
        .iter()
        .map(|e| (e.key.clone(), e.clone()))
        .collect();
    for m in successes {
        match m {
            CommitMutation::Upload(e) => {
                map.insert(e.key.clone(), e.clone());
            }
            CommitMutation::DeleteRemote(k) => {
                map.remove(k);
            }
        }
    }
    map.into_values().collect()
}

/// Repair options (issue 45, D-repair / section 10).
#[derive(Debug, Clone)]
pub struct RepairOpts {
    /// Overwrite the manifest unconditionally (no If-Match; used after a
    /// bootstrap or when the current etag is unknown/skewed).
    pub force: bool,
    /// Compute the body and report the entry count but write nothing.
    pub dry_run: bool,
    /// Bounds the cold list+head path's heads (via the store).
    pub concurrency: u32,
}

/// Report of a repair run (W241).
#[derive(Debug, Clone)]
pub struct RepairReport {
    /// Number of file entities listed via live list+head (pre-ignore,
    /// post-reserved).
    pub listed: usize,
    /// Whether the manifest object was written this run.
    pub written: bool,
    /// True when `dry_run` was set (body computed, nothing written).
    pub dry_run: bool,
    /// The manifest object's etag after the run (None on dry run / backend
    /// without etags).
    pub etag: Option<String>,
    /// Advisory warnings from the cold listing (reserved drops, store
    /// warnings).
    pub warnings: Vec<String>,
}

/// Rebuild the remote manifest from a live list+head (W241, section 10.2):
/// I15 stays intact (cold path is authoritative), reserved keys are stripped,
/// and the body is written conditionally - If-Match the current manifest
/// etag when one exists (retry once on a lost race), If-None-Match: * when
/// creating, unconditional under `--force`. Never mutates file bodies
/// (10.4). `dry_run` computes the body but writes nothing.
pub fn repair_manifest(
    store: &dyn ObjectStore,
    opts: &RepairOpts,
    _cache: Option<&CachePaths>,
) -> Result<RepairReport, Error> {
    let inv = live_list_head(store)?;
    let files = &inv.base.file_entities;
    let created_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let manifest = crate::manifest::file_entities_to_manifest(
        files,
        created_ms,
        Some(format!("vaultsync {}", crate::version())),
        None,
    )?;
    let body = crate::manifest::serialize_manifest(&manifest)?;
    if opts.dry_run {
        return Ok(RepairReport {
            listed: files.len(),
            written: false,
            dry_run: true,
            etag: None,
            warnings: inv.warnings,
        });
    }
    let body_len = body.len() as u64;
    if opts.force {
        let mut cursor = std::io::Cursor::new(body);
        store.put_from_with(
            crate::local::MANIFEST_KEY,
            &mut cursor,
            body_len,
            crate::store::PutOpts::default(),
        )?;
    } else {
        match store.head(crate::local::MANIFEST_KEY) {
            Ok(cur) => {
                let etag = cur.etag;
                let mut cursor = std::io::Cursor::new(body.clone());
                match store.put_from_with(
                    crate::local::MANIFEST_KEY,
                    &mut cursor,
                    body_len,
                    crate::store::PutOpts {
                        if_match_etag: etag,
                        ..Default::default()
                    },
                ) {
                    Ok(_) => {}
                    // Lost the race: retry ONCE against a fresh head.
                    Err(Error::PreconditionFailed(_)) => {
                        let cur = store.head(crate::local::MANIFEST_KEY)?;
                        let mut cursor = std::io::Cursor::new(body);
                        store.put_from_with(
                            crate::local::MANIFEST_KEY,
                            &mut cursor,
                            body_len,
                            crate::store::PutOpts {
                                if_match_etag: cur.etag,
                                ..Default::default()
                            },
                        )?;
                    }
                    Err(e) => return Err(e),
                }
            }
            // No manifest yet: create (If-None-Match: *).
            Err(Error::NotFound(_)) => {
                let mut cursor = std::io::Cursor::new(body);
                store.put_from_with(
                    crate::local::MANIFEST_KEY,
                    &mut cursor,
                    body_len,
                    crate::store::PutOpts {
                        if_none_match_star: true,
                        ..Default::default()
                    },
                )?;
            }
            Err(e) => return Err(e),
        }
    }
    let etag = store
        .head(crate::local::MANIFEST_KEY)
        .ok()
        .and_then(|e| e.etag);
    Ok(RepairReport {
        listed: files.len(),
        written: true,
        dry_run: false,
        etag,
        warnings: inv.warnings,
    })
}

/// Commit a new manifest after a mutating push (W238, D-commit-cond):
/// bodies are already live (transfers ran first); this writes the manifest
/// LAST (D-commit-order) with a conditional put - If-Match on the base etag
/// when the plan read a manifest, If-None-Match: * when creating (base was
/// list+head/missing). A lost race answers [`CommitOutcome::PreconditionFailed`]
/// and never clobbers the other writer's manifest. Zero successes skip the
/// write entirely (D-commit-when).
pub fn commit_manifest(
    store: &dyn ObjectStore,
    base: &InventoryBase,
    successes: &[CommitMutation],
    _cache: Option<&CachePaths>,
) -> Result<CommitOutcome, Error> {
    if successes.is_empty() {
        return Ok(CommitOutcome::SkippedNoMutations);
    }
    let files = apply_commit_mutations(&base.file_entities, successes);
    let created_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let manifest = crate::manifest::file_entities_to_manifest(
        &files,
        created_ms,
        Some(format!("vaultsync {}", crate::version())),
        None,
    )?;
    let body = crate::manifest::serialize_manifest(&manifest)?;
    let body_len = body.len() as u64;
    let mut cursor = std::io::Cursor::new(body);
    let opts = crate::store::PutOpts {
        mtime_ms: None,
        if_match_etag: base.manifest_etag.clone(),
        if_none_match_star: base.manifest_etag.is_none(),
    };
    match store.put_from_with(crate::local::MANIFEST_KEY, &mut cursor, body_len, opts) {
        Ok(entity) => Ok(CommitOutcome::Written {
            etag: entity.etag,
            entry_count: manifest.entry_count,
        }),
        Err(Error::PreconditionFailed(_)) => Ok(CommitOutcome::PreconditionFailed),
        Err(e) => Err(e),
    }
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

    #[test]
    fn commit_apply_upserts_deletes_and_keeps_failed() {
        // W237 (issue 45): the pure apply folds successful uploads (size/
        // mtime/etag upsert), removes successful deletes, and leaves
        // anything NOT in the success list at its base value (a failed
        // upload/delete must never be claimed). Output is sorted by key with
        // no duplicates.
        let base = vec![
            crate::entity::file("a.md", 1, Some(1)),
            crate::entity::file("b.md", 2, Some(2)),
            crate::entity::file("gone.md", 3, Some(3)),
        ];
        let successes = vec![
            CommitMutation::Upload(Entity {
                key: "b.md".to_string(),
                size: 20,
                mtime_ms: Some(200),
                etag: Some("\"new-etag\"".to_string()),
            }),
            CommitMutation::DeleteRemote("gone.md".to_string()),
        ];
        // `a.md` has no mutation: failed/absent -> base entry survives.
        let out = apply_commit_mutations(&base, &successes);
        let keys: Vec<&str> = out.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["a.md", "b.md"], "sorted unique: {keys:?}");
        let b = out.iter().find(|e| e.key == "b.md").unwrap();
        assert_eq!(b.size, 20);
        assert_eq!(b.mtime_ms, Some(200));
        assert_eq!(b.etag.as_deref(), Some("\"new-etag\""));
        let a = out.iter().find(|e| e.key == "a.md").unwrap();
        assert_eq!(a.size, 1, "failed/absent key keeps base entry");
        assert!(!out.iter().any(|e| e.key == "gone.md"));
        // An upload for a brand-new key adds it.
        let out = apply_commit_mutations(
            &base,
            &[CommitMutation::Upload(crate::entity::file(
                "c.md",
                4,
                Some(4),
            ))],
        );
        let keys: Vec<&str> = out.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["a.md", "b.md", "c.md", "gone.md"]);
    }

    #[test]
    fn commit_manifest_create_then_if_match_race() {
        use crate::store::{GetOpts, GetOutcome, PutOpts};
        // W238 (issue 45, D-commit-cond): the first commit creates with
        // If-None-Match: * (base was list+head); a second commit with a
        // STALE base etag answers PreconditionFailed and does NOT clobber;
        // with the CURRENT etag it succeeds (If-Match). Zero successes skip
        // the write entirely (SkippedNoMutations).
        let store = MemoryStore::new();
        let empty = InventoryBase {
            source: InventorySource::LiveListHead,
            file_entities: Vec::new(),
            manifest_etag: None,
        };
        // No successes: nothing written.
        let out = commit_manifest(&store, &empty, &[], None).unwrap();
        assert_eq!(out, CommitOutcome::SkippedNoMutations);
        assert!(matches!(
            store.head(crate::local::MANIFEST_KEY).unwrap_err(),
            Error::NotFound(_)
        ));
        // First commit: create (If-None-Match: *).
        let up = CommitMutation::Upload(crate::entity::file("a.md", 5, Some(5)));
        let out = commit_manifest(&store, &empty, std::slice::from_ref(&up), None).unwrap();
        let (etag1, count1) = match out {
            CommitOutcome::Written { etag, entry_count } => (etag, entry_count),
            other => panic!("expected Written, got {other:?}"),
        };
        assert_eq!(count1, 1);
        let etag1 = etag1.expect("mock returns etags");
        // Second commit with a STALE base etag: PreconditionFailed, no
        // clobber.
        let stale = InventoryBase {
            source: InventorySource::Manifest {
                remote_etag: Some("\"stale\"".to_string()),
            },
            file_entities: vec![crate::entity::file("a.md", 5, Some(5))],
            manifest_etag: Some("\"stale\"".to_string()),
        };
        let out = commit_manifest(&store, &stale, std::slice::from_ref(&up), None).unwrap();
        assert_eq!(out, CommitOutcome::PreconditionFailed);
        // Manifest body unchanged (no clobber) - still the first version.
        let mut buf = Vec::new();
        store.get_to(crate::local::MANIFEST_KEY, &mut buf).unwrap();
        let m = crate::manifest::parse_manifest_bytes(&buf).unwrap();
        assert_eq!(m.entry_count, 1);
        // Third commit with the CURRENT etag: If-Match succeeds.
        let current = InventoryBase {
            source: InventorySource::Manifest {
                remote_etag: Some(etag1.clone()),
            },
            file_entities: vec![crate::entity::file("a.md", 5, Some(5))],
            manifest_etag: Some(etag1.clone()),
        };
        let out = commit_manifest(
            &store,
            &current,
            &[CommitMutation::Upload(crate::entity::file(
                "b.md",
                7,
                Some(7),
            ))],
            None,
        )
        .unwrap();
        let (etag2, count2) = match out {
            CommitOutcome::Written { etag, entry_count } => (etag, entry_count),
            other => panic!("expected Written, got {other:?}"),
        };
        assert_eq!(count2, 2);
        assert_ne!(etag2, Some(etag1));
        // Sanity: conditional get surface still works (GetOpts unused here).
        let _ = PutOpts::default();
        let mut buf = Vec::new();
        let out = store
            .get_to_with(
                crate::local::MANIFEST_KEY,
                &mut buf,
                GetOpts {
                    if_none_match_etag: Some(etag2.clone().unwrap()),
                },
            )
            .unwrap();
        assert!(matches!(out, GetOutcome::NotModified(_)));
    }

    #[test]
    fn repair_writes_valid_manifest_from_live_list() {
        // W241 (issue 45): repair rebuilds the manifest from live list+head
        // (I15-authoritative), matching the live files. With no manifest
        // present it creates via If-None-Match: *; reserved control-plane
        // keys never become entries.
        let store = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"aaa".to_vec());
        store
            .put_from("notes/a.md", &mut c, 3, Some(1_600_000_000_000))
            .unwrap();
        let mut c = std::io::Cursor::new(b"j".to_vec());
        store
            .put_from("b.md", &mut c, 1, Some(1_600_000_000_000))
            .unwrap();
        // A stray control-plane object must never become a manifest entry
        // (reserved strip happens before the body is built).
        let mut c = std::io::Cursor::new(b"stray".to_vec());
        store
            .put_from(
                ".vaultsync/cache/stale.json",
                &mut c,
                "stray".len() as u64,
                None,
            )
            .unwrap();
        let rep = repair_manifest(
            &store,
            &RepairOpts {
                force: false,
                dry_run: false,
                concurrency: 1,
            },
            None,
        )
        .unwrap();
        assert_eq!(rep.listed, 2);
        assert!(rep.written);
        assert!(!rep.dry_run);
        assert!(rep.etag.is_some());
        let mut buf = Vec::new();
        store.get_to(crate::local::MANIFEST_KEY, &mut buf).unwrap();
        let m = crate::manifest::parse_manifest_bytes(&buf).unwrap();
        let keys: Vec<&str> = m.entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["b.md", "notes/a.md"]);
        assert_eq!(m.entries[0].size, 1);
        assert_eq!(m.entries[1].mtime_ms, Some(1_600_000_000_000));
    }

    #[test]
    fn repair_dry_run_writes_nothing() {
        // W241 (issue 45): `--dry-run` computes the body (entry count) but
        // writes nothing - no manifest object appears.
        let store = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"aaa".to_vec());
        store
            .put_from("a.md", &mut c, 3, Some(1_600_000_000_000))
            .unwrap();
        let rep = repair_manifest(
            &store,
            &RepairOpts {
                force: false,
                dry_run: true,
                concurrency: 1,
            },
            None,
        )
        .unwrap();
        assert_eq!(rep.listed, 1);
        assert!(rep.dry_run);
        assert!(!rep.written);
        assert!(matches!(
            store.head(crate::local::MANIFEST_KEY).unwrap_err(),
            Error::NotFound(_)
        ));
    }

    #[test]
    fn repair_force_overwrites_existing_manifest() {
        // W241 (issue 45): a manifest that exists is replaced by repair
        // (conditional If-Match when the etag is current; the mock answers
        // correctly). `--force` also overwrites without any condition.
        let store = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"aaa".to_vec());
        store
            .put_from("a.md", &mut c, 3, Some(1_600_000_000_000))
            .unwrap();
        // Stale/corrupt manifest body on the remote.
        let mut c = std::io::Cursor::new(b"corrupt".to_vec());
        store
            .put_from(
                crate::local::MANIFEST_KEY,
                &mut c,
                "corrupt".len() as u64,
                None,
            )
            .unwrap();
        // Without force: conditional If-Match on the CURRENT etag succeeds.
        let rep = repair_manifest(
            &store,
            &RepairOpts {
                force: false,
                dry_run: false,
                concurrency: 1,
            },
            None,
        )
        .unwrap();
        assert!(rep.written);
        let mut buf = Vec::new();
        store.get_to(crate::local::MANIFEST_KEY, &mut buf).unwrap();
        let m = crate::manifest::parse_manifest_bytes(&buf).unwrap();
        assert_eq!(m.entry_count, 1);
        assert_eq!(m.entries[0].key, "a.md");
        // Force overwrites a manifest whose etag we don't hold (no
        // condition at all): simulate by putting a fresh manifest first.
        let mut c = std::io::Cursor::new(b"zzz".to_vec());
        store.put_from("z.md", &mut c, 3, Some(2)).unwrap();
        let rep = repair_manifest(
            &store,
            &RepairOpts {
                force: true,
                dry_run: false,
                concurrency: 1,
            },
            None,
        )
        .unwrap();
        assert!(rep.written);
        let mut buf = Vec::new();
        store.get_to(crate::local::MANIFEST_KEY, &mut buf).unwrap();
        let m = crate::manifest::parse_manifest_bytes(&buf).unwrap();
        let keys: Vec<&str> = m.entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["a.md", "z.md"]);
    }
}
