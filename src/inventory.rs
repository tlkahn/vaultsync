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
use serde::{Deserialize, Serialize};
use std::io::Write;

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
    cache: Option<&CachePaths>,
) -> Result<RemoteInventory, Error> {
    match mode {
        InventoryMode::ListHead => live_list_head(store),
        InventoryMode::Auto => match try_load_manifest(store, cache) {
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
            match try_load_manifest(store, cache)? {
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
fn try_load_manifest(
    store: &dyn ObjectStore,
    cache: Option<&CachePaths>,
) -> Result<Option<RemoteInventory>, Error> {
    match cache {
        Some(cache) => try_load_manifest_cached(store, cache),
        None => try_load_manifest_fresh(store),
    }
}

/// Build the planner-input inventory from a parsed manifest (shared by the
/// fresh and cached warm paths, W233/W244).
fn manifest_inventory(
    manifest: &crate::manifest::ManifestV1,
    remote_etag: Option<String>,
) -> Result<RemoteInventory, Error> {
    let file_entities = crate::manifest::manifest_to_file_entities(manifest)?;
    let folders = crate::manifest::synthesize_folders(&file_entities);
    let mut entities = file_entities.clone();
    entities.extend(folders);
    entities.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(RemoteInventory {
        base: InventoryBase {
            source: InventorySource::Manifest {
                remote_etag: remote_etag.clone(),
            },
            file_entities,
            manifest_etag: remote_etag,
        },
        entities,
        warnings: Vec::new(),
    })
}

/// Unconditional warm fetch (no cache): GET the manifest, parse, and serve.
fn try_load_manifest_fresh(store: &dyn ObjectStore) -> Result<Option<RemoteInventory>, Error> {
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
    manifest_inventory(&manifest, entity.etag).map(Some)
}

/// Cache-aware warm fetch (W244, section 6.1 step 2a): with a cached remote
/// etag, issue a conditional GET (If-None-Match). A 304 plans from the
/// cached body (no re-download) and the cache is refreshed on a Body. A
/// NotFound invalidates the cache and means "no manifest". A non-NotFound
/// fetch error FAILS CLOSED - the cache is never authority (W245); a corrupt
/// cached body is invalidated (removed) so the next load refetches. Cache
/// writes are best-effort (a cache IO error never fails the load; Q3 read
/// path stays side-effect free for the remote).
fn try_load_manifest_cached(
    store: &dyn ObjectStore,
    cache: &CachePaths,
) -> Result<Option<RemoteInventory>, Error> {
    // Conditional attempt when we hold a remote etag.
    if let Some(etag) = read_cache_meta(cache).and_then(|m| m.remote_etag) {
        let mut buf = Vec::new();
        match store.get_to_with(
            crate::local::MANIFEST_KEY,
            &mut buf,
            crate::store::GetOpts {
                if_none_match_etag: Some(etag.clone()),
            },
        ) {
            Ok(crate::store::GetOutcome::NotModified(_)) => {
                if let Some(cached) = read_cache_body(cache) {
                    // Cache valid: plan from it without re-download.
                    let manifest = crate::manifest::parse_manifest_bytes(&cached)?;
                    return manifest_inventory(&manifest, Some(etag)).map(Some);
                }
                // Corrupt/missing cache body was invalidated above: fall
                // through to a fresh fetch below.
            }
            Ok(crate::store::GetOutcome::Body(entity)) => {
                let manifest = match crate::manifest::parse_manifest_bytes(&buf) {
                    Ok(m) => m,
                    Err(_) => {
                        invalidate_cache(cache);
                        return Ok(None);
                    }
                };
                let inv = manifest_inventory(&manifest, entity.etag.clone())?;
                fill_cache(cache, &buf, entity.etag);
                return Ok(Some(inv));
            }
            Err(Error::NotFound(_)) => {
                invalidate_cache(cache);
                return Ok(None);
            }
            // W245: any non-NotFound failure fails closed - never plan from
            // the stale cache alone.
            Err(e) => return Err(e),
        }
    }
    // No usable cache etag (or 304 with an invalidated body): unconditional
    // fetch, then fill the cache with the fresh body + etag.
    let mut buf = Vec::new();
    let entity = match store.get_to(crate::local::MANIFEST_KEY, &mut buf) {
        Ok(e) => e,
        Err(Error::NotFound(_)) => {
            invalidate_cache(cache);
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    let manifest = match crate::manifest::parse_manifest_bytes(&buf) {
        Ok(m) => m,
        Err(_) => {
            invalidate_cache(cache);
            return Ok(None);
        }
    };
    let inv = manifest_inventory(&manifest, entity.etag.clone())?;
    fill_cache(cache, &buf, entity.etag);
    Ok(Some(inv))
}

/// Best-effort cache fill after a valid remote fetch (W244, Q3): write the
/// body + meta (etag, fetched time, source key). Errors are swallowed - the
/// cache is optional and never fails the load.
fn fill_cache(cache: &CachePaths, body: &[u8], remote_etag: Option<String>) {
    let meta = CacheMeta {
        remote_etag,
        fetched_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        source_key: crate::local::MANIFEST_KEY.to_string(),
    };
    let _ = write_cache_files(body, &meta, cache);
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

impl CachePaths {
    /// The S7 cache files under `<vault_root>/.vaultsync/cache/` (issue 45,
    /// D-cache / 4.3).
    pub fn new(vault_root: &std::path::Path) -> Self {
        let dir = vault_root.join(".vaultsync/cache");
        CachePaths {
            body: dir.join("manifest-v1.json"),
            meta: dir.join("manifest-v1.meta.json"),
        }
    }
}

/// Cache metadata (issue 45, 4.3): the remote etag the cached body was
/// fetched under (used for the conditional GET), when it was fetched, and
/// the source key. Stored at `manifest-v1.meta.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheMeta {
    pub remote_etag: Option<String>,
    pub fetched_at_ms: u64,
    pub source_key: String,
}

/// Write the cache body + meta atomically (temp+rename, owner-only 0o600 on
/// Unix; W243). A crash between the two renames leaves a body/meta pair that
/// may disagree - the reader treats a meta that does not validate as absent
/// (and a body that fails to parse is invalidated, W245).
fn write_cache_files(body: &[u8], meta: &CacheMeta, cache: &CachePaths) -> Result<(), Error> {
    if let Some(parent) = cache.body.parent() {
        std::fs::create_dir_all(parent).map_err(Error::Io)?;
    }
    let body_tmp = temp_sibling(&cache.body);
    let meta_tmp = temp_sibling(&cache.meta);
    let write_one = |path: &std::path::Path, data: &[u8]| -> Result<(), Error> {
        let mut f = crate::local::create_new_owner_only(path)?;
        f.write_all(data)?;
        f.sync_all()?;
        Ok(())
    };
    let result = (|| -> Result<(), Error> {
        write_one(&body_tmp, body)?;
        write_one(
            &meta_tmp,
            &serde_json::to_vec(meta)
                .map_err(|e| Error::Other(format!("cache meta serialize: {e}")))?,
        )?;
        std::fs::rename(&body_tmp, &cache.body).map_err(Error::Io)?;
        std::fs::rename(&meta_tmp, &cache.meta).map_err(Error::Io)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&body_tmp);
        let _ = std::fs::remove_file(&meta_tmp);
    }
    result
}

/// A unique temp sibling path of `final_path` (`.name.vaultsync-cache-tmp-<pid>-<n>`).
/// The writer creates it exclusively (owner-only) via `create_new_owner_only`.
fn temp_sibling(final_path: &std::path::Path) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = format!(
        ".{}.vaultsync-cache-tmp-{}-{n}",
        final_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "cache".to_string()),
        std::process::id()
    );
    final_path.with_file_name(name)
}

/// Read + validate the cache meta; `None` on any failure (missing file,
/// unreadable, or unparseable - a corrupt meta is treated as absent, W245).
fn read_cache_meta(cache: &CachePaths) -> Option<CacheMeta> {
    let bytes = std::fs::read(&cache.meta).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Read the cached manifest body; `None` when unreadable OR unparseable. A
/// corrupt cached body is invalidated (removed, best-effort - W245) so the
/// next load refetches instead of re-serving garbage.
fn read_cache_body(cache: &CachePaths) -> Option<Vec<u8>> {
    let bytes = std::fs::read(&cache.body).ok()?;
    if crate::manifest::parse_manifest_bytes(&bytes).is_err() {
        invalidate_cache(cache);
        return None;
    }
    Some(bytes)
}

/// Best-effort removal of both cache files (W245 invalidate-on-corrupt).
fn invalidate_cache(cache: &CachePaths) {
    let _ = std::fs::remove_file(&cache.body);
    let _ = std::fs::remove_file(&cache.meta);
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
    cache: Option<&CachePaths>,
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
    // W246: keep a clone for the cache refresh (the write paths move `body`
    // into their cursors).
    let cache_body = body.clone();
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
    // W246: repair refreshes the local cache mirror (best-effort).
    if let Some(cache) = cache {
        fill_cache(cache, &cache_body, etag.clone());
    }
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
/// LAST (D-commit-order) with a conditional put. A WARM base (read a
/// manifest) uses If-Match on the base etag. A COLD base (`manifest_etag:
/// None` - list+head, missing, or forced mode) resolves the condition
/// against the LIVE object at commit time (H1, review 5472028291): a
/// present object is overwritten via If-Match on its live etag (heals
/// corrupt bodies / `list_head`-created objects), an absent object creates
/// via If-None-Match: *, and a present object whose head has no etag
/// (etag-less backend) degrades to an unconditional put - multi-writer
/// safety is lost there (N5, docs). A lost race answers
/// [`CommitOutcome::PreconditionFailed`] and never clobbers the other
/// writer's manifest. Zero successes skip the write entirely
/// (D-commit-when).
pub fn commit_manifest(
    store: &dyn ObjectStore,
    base: &InventoryBase,
    successes: &[CommitMutation],
    cache: Option<&CachePaths>,
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
    // H1 (W250): a cold base resolves its condition via a live head at
    // commit time - present => If-Match on the live etag (overwrite),
    // absent => If-None-Match: * create. A warm base keeps If-Match on the
    // base etag (no extra head).
    let (if_match_etag, if_none_match_star) = match &base.manifest_etag {
        Some(e) => (Some(e.clone()), false),
        None => match store.head(crate::local::MANIFEST_KEY) {
            // Etag-less present object: unconditional put on the cold
            // resolve path only (R46-h1-etagless); If-None-Match: * would
            // fail forever on a present object.
            Ok(ent) => (ent.etag, false),
            Err(Error::NotFound(_)) => (None, true),
            Err(e) => return Err(e),
        },
    };
    let opts = crate::store::PutOpts {
        mtime_ms: None,
        if_match_etag,
        if_none_match_star,
    };
    match store.put_from_with(crate::local::MANIFEST_KEY, &mut cursor, body_len, opts) {
        Ok(entity) => {
            // W246: a successful commit refreshes the local cache mirror.
            if let Some(cache) = cache {
                fill_cache(cache, &cursor.into_inner(), entity.etag.clone());
            }
            Ok(CommitOutcome::Written {
                etag: entity.etag,
                entry_count: manifest.entry_count,
            })
        }
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
    fn commit_manifest_cold_base_overwrites_existing_corrupt_body() {
        // W249 (H1, review 5472028291): a COLD base (`manifest_etag: None`)
        // must resolve the commit condition against the LIVE object: a
        // present-but-corrupt body is overwritten via If-Match on the live
        // etag, not rejected by create-only If-None-Match: *. Today
        // (pre-fix) this answers PreconditionFailed and push can never heal
        // the corrupt object.
        let store = MemoryStore::new();
        // Seed MANIFEST_KEY with garbage that is not a valid manifest.
        let mut c = std::io::Cursor::new(b"this is not a manifest".to_vec());
        store
            .put_from(crate::local::MANIFEST_KEY, &mut c, 21, None)
            .unwrap();
        let cold = InventoryBase {
            source: InventorySource::LiveListHead,
            file_entities: vec![crate::entity::file("a.md", 5, Some(5))],
            manifest_etag: None,
        };
        let out = commit_manifest(
            &store,
            &cold,
            &[CommitMutation::Upload(crate::entity::file(
                "a.md",
                5,
                Some(5),
            ))],
            None,
        )
        .unwrap();
        let (etag, count) = match out {
            CommitOutcome::Written { etag, entry_count } => (etag, entry_count),
            other => panic!("expected Written, got {other:?}"),
        };
        assert_eq!(count, 1);
        assert!(etag.is_some(), "mock returns etags");
        // The overwritten body is a valid manifest containing a.md.
        let mut buf = Vec::new();
        store.get_to(crate::local::MANIFEST_KEY, &mut buf).unwrap();
        let m = crate::manifest::parse_manifest_bytes(&buf).unwrap();
        assert_eq!(m.entry_count, 1);
    }

    #[test]
    fn commit_manifest_cold_base_creates_when_absent() {
        // W249 (H1 control): a cold base with NO object at MANIFEST_KEY still
        // creates via If-None-Match: *. GREEN on arrival (characterization);
        // mutation-checked by forcing a present object without the H1 fix
        // -> PreconditionFailed.
        let store = MemoryStore::new();
        let cold = InventoryBase {
            source: InventorySource::LiveListHead,
            file_entities: Vec::new(),
            manifest_etag: None,
        };
        let out = commit_manifest(
            &store,
            &cold,
            &[CommitMutation::Upload(crate::entity::file(
                "a.md",
                5,
                Some(5),
            ))],
            None,
        )
        .unwrap();
        match out {
            CommitOutcome::Written { etag, entry_count } => {
                assert!(etag.is_some(), "mock returns etags");
                assert_eq!(entry_count, 1);
            }
            other => panic!("expected Written, got {other:?}"),
        }
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

    #[test]
    fn cache_write_read_round_trip_and_owner_only() {
        // W243 (issue 45): the cache body + meta write atomically (temp +
        // rename) under `<vault_root>/.vaultsync/cache/`; the meta carries
        // the remote etag, fetched time, and source key; reading back yields
        // the same values. On Unix the files are owner-only (0o600).
        let dir = crate::testutil::TempDir::new("vaultsync-cache-test");
        let cache = CachePaths::new(dir.path());
        let meta = CacheMeta {
            remote_etag: Some("\"abc\"".to_string()),
            fetched_at_ms: 1234,
            source_key: crate::local::MANIFEST_KEY.to_string(),
        };
        let body =
            br#"{"schema":"vaultsync.manifest.v1","created_ms":0,"entry_count":0,"entries":[]}"#;
        write_cache_files(body, &meta, &cache).unwrap();
        // No temp leftovers.
        assert!(
            std::fs::read_dir(cache.body.parent().unwrap())
                .unwrap()
                .all(|e| !e.unwrap().file_name().to_string_lossy().contains("tmp")),
            "temp files must be renamed away"
        );
        // Body parses; meta round-trips.
        let cached_body = read_cache_body(&cache).expect("cached body");
        assert_eq!(cached_body, body);
        let cached_meta = read_cache_meta(&cache).expect("cached meta");
        assert_eq!(cached_meta, meta);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&cache.body).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "cache body must be owner-only");
            let mode = std::fs::metadata(&cache.meta).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "cache meta must be owner-only");
        }
    }

    /// Counting store: wraps MemoryStore, counts `get_to_with` bytes fetched
    /// (the warm-path body re-download gauge, W244) and `list` calls.
    struct CountingGetStore {
        inner: MemoryStore,
        bytes_fetched: std::sync::atomic::AtomicU64,
        list_calls: std::sync::atomic::AtomicUsize,
    }
    impl CountingGetStore {
        fn new() -> Self {
            CountingGetStore {
                inner: MemoryStore::new(),
                bytes_fetched: std::sync::atomic::AtomicU64::new(0),
                list_calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn bytes_fetched(&self) -> u64 {
            self.bytes_fetched
                .load(std::sync::atomic::Ordering::Relaxed)
        }
        fn list_calls(&self) -> usize {
            self.list_calls.load(std::sync::atomic::Ordering::Relaxed)
        }
    }
    impl ObjectStore for CountingGetStore {
        fn list(&self, prefix: &str) -> Result<crate::store::Listing, Error> {
            self.list_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.list(prefix)
        }
        fn head(&self, key: &str) -> Result<Entity, Error> {
            self.inner.head(key)
        }
        fn get_to(&self, key: &str, w: &mut dyn std::io::Write) -> Result<Entity, Error> {
            let mut buf = Vec::new();
            let e = self.inner.get_to(key, &mut buf)?;
            self.bytes_fetched
                .fetch_add(buf.len() as u64, std::sync::atomic::Ordering::Relaxed);
            w.write_all(&buf)?;
            Ok(e)
        }
        fn get_to_with(
            &self,
            key: &str,
            w: &mut dyn std::io::Write,
            opts: crate::store::GetOpts,
        ) -> Result<crate::store::GetOutcome, Error> {
            use crate::store::GetOutcome;
            // 304: MemoryStore's real conditional get; count only real body
            // fetches (the mock's Body path streams through get_to).
            if let Some(want) = &opts.if_none_match_etag {
                let h = self.inner.head(key)?;
                if h.etag.as_deref() == Some(want.as_str()) {
                    return Ok(GetOutcome::NotModified(h));
                }
            }
            self.get_to(key, w).map(GetOutcome::Body)
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
        fn put_from_with(
            &self,
            key: &str,
            r: &mut dyn std::io::Read,
            size: u64,
            opts: crate::store::PutOpts,
        ) -> Result<Entity, Error> {
            self.inner.put_from_with(key, r, size, opts)
        }
        fn delete(&self, key: &str) -> Result<(), Error> {
            self.inner.delete(key)
        }
    }

    #[test]
    fn cache_serves_second_load_via_304_without_redownload() {
        // W244 (issue 45): with a vault_root cache, the first load fetches
        // the manifest body (and fills the cache); the second load with the
        // same remote etag issues a conditional GET, receives NotModified,
        // and plans from the cached body - NO full body re-download (the
        // byte counter stays flat) and no list. Source stays Manifest.
        let dir = crate::testutil::TempDir::new("vaultsync-cache-test");
        let cache = CachePaths::new(dir.path());
        let store = CountingGetStore::new();
        let body = manifest_body(&[("notes/a.md", 3, Some(100)), ("b.md", 1, None)]);
        let body_len = body.len() as u64;
        let mut c = std::io::Cursor::new(body);
        let put = store
            .inner
            .put_from(crate::local::MANIFEST_KEY, &mut c, body_len, None)
            .unwrap();
        let etag = put.etag.clone().unwrap();

        let inv1 = load_remote_inventory(&store, InventoryMode::Auto, 1, Some(&cache)).unwrap();
        assert_eq!(
            inv1.base.source,
            InventorySource::Manifest {
                remote_etag: Some(etag.clone())
            }
        );
        assert!(store.bytes_fetched() > 0, "first load must fetch the body");
        assert_eq!(store.list_calls(), 0, "warm path must not list");
        let fetched1 = store.bytes_fetched();

        // Second load: 304 via the cache etag; no re-download.
        let inv2 = load_remote_inventory(&store, InventoryMode::Auto, 1, Some(&cache)).unwrap();
        assert_eq!(
            inv2.base.source,
            InventorySource::Manifest {
                remote_etag: Some(etag.clone())
            }
        );
        assert_eq!(
            store.bytes_fetched(),
            fetched1,
            "second load must not re-download the body (304 + cache)"
        );
        let file_keys: Vec<&str> = inv2
            .base
            .file_entities
            .iter()
            .map(|e| e.key.as_str())
            .collect();
        assert_eq!(file_keys, vec!["b.md", "notes/a.md"]);
    }

    #[test]
    fn cache_is_never_authority_on_remote_failure() {
        // W245 (issue 45): a warm cache present does NOT let the facade plan
        // from stale data when the remote conditional GET fails with a
        // non-NotFound error - the load fails CLOSED (never silently plan
        // from cache alone). A corrupt cached body is invalidated (removed)
        // best-effort so the next load refetches.
        let dir = crate::testutil::TempDir::new("vaultsync-cache-test");
        let cache = CachePaths::new(dir.path());
        let store = MemoryStore::new();
        let body = manifest_body(&[("a.md", 3, Some(100))]);
        let body_len = body.len() as u64;
        let mut c = std::io::Cursor::new(body.clone());
        store
            .put_from(crate::local::MANIFEST_KEY, &mut c, body_len, None)
            .unwrap();
        // Fill the cache with a valid body+meta.
        let meta = CacheMeta {
            remote_etag: Some("\"abc\"".to_string()),
            fetched_at_ms: 1,
            source_key: crate::local::MANIFEST_KEY.to_string(),
        };
        write_cache_files(&body, &meta, &cache).unwrap();

        // Corrupt the cached body: read_cache_body returns None and the file
        // is removed (invalidate), so a 304 path can never parse garbage.
        std::fs::write(&cache.body, b"not json").unwrap();
        assert!(read_cache_body(&cache).is_none());
        assert!(
            !cache.body.exists(),
            "corrupt cache body must be invalidated (removed)"
        );
        // A fresh valid cache body again, then a failing remote GET: the
        // load must fail closed, not serve the cache.
        write_cache_files(&body, &meta, &cache).unwrap();
        let failing = crate::testutil::FailGetStore {
            inner: MemoryStore::new(),
        };
        let err =
            load_remote_inventory(&failing, InventoryMode::Auto, 1, Some(&cache)).unwrap_err();
        assert!(
            matches!(err, Error::Unavailable(_)),
            "non-NotFound remote failure must fail closed (got {err:?})"
        );
    }

    #[test]
    fn commit_and_repair_refresh_local_cache() {
        // W246 (issue 45): after a successful manifest commit (and after
        // repair), the local cache mirrors the new manifest body + etag -
        // the next load is a 304 path (no re-download) against the new
        // version.
        let dir = crate::testutil::TempDir::new("vaultsync-cache-test");
        let cache = CachePaths::new(dir.path());
        let store = MemoryStore::new();
        let base = InventoryBase {
            source: InventorySource::LiveListHead,
            file_entities: Vec::new(),
            manifest_etag: None,
        };
        let up = CommitMutation::Upload(crate::entity::file("a.md", 5, Some(5)));
        let out = commit_manifest(&store, &base, &[up], Some(&cache)).unwrap();
        let (etag, count) = match out {
            CommitOutcome::Written { etag, entry_count } => (etag, entry_count),
            other => panic!("expected Written, got {other:?}"),
        };
        assert_eq!(count, 1);
        assert_eq!(etag, read_cache_meta(&cache).unwrap().remote_etag);
        let cached = read_cache_body(&cache).expect("cache body written");
        let m = crate::manifest::parse_manifest_bytes(&cached).unwrap();
        assert_eq!(m.entry_count, 1);
        assert_eq!(m.entries[0].key, "a.md");

        // Repair also refreshes the cache (add bodies as the executor would,
        // then repair sees both files).
        let mut c = std::io::Cursor::new(b"aaa".to_vec());
        store.put_from("a.md", &mut c, 3, Some(5)).unwrap();
        let mut c = std::io::Cursor::new(b"bbb".to_vec());
        store.put_from("b.md", &mut c, 3, Some(2)).unwrap();
        let rep = repair_manifest(
            &store,
            &RepairOpts {
                force: false,
                dry_run: false,
                concurrency: 1,
            },
            Some(&cache),
        )
        .unwrap();
        assert!(rep.written);
        assert_eq!(
            rep.etag,
            read_cache_meta(&cache).unwrap().remote_etag,
            "repair must refresh the cache etag"
        );
        let cached = read_cache_body(&cache).unwrap();
        let m = crate::manifest::parse_manifest_bytes(&cached).unwrap();
        assert_eq!(m.entry_count, 2);
    }
}
