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
/// `mode` selects warm vs cold; `cache` is `None` until S7. Cold-path head
/// concurrency is a store-construction concern (`S3Store::new(...,
/// settings.concurrency)`, `[transfer].concurrency`) - the facade does not
/// re-bind it through `dyn ObjectStore` (M3/F3, reviews 5472028291 +
/// 5472033449). Cold path: `store.list("")` + reserved partition
/// (second-line guard; S3 already pre-partitions) - I15 stays intact.
pub fn load_remote_inventory(
    store: &dyn ObjectStore,
    mode: InventoryMode,
    cache: Option<&CachePaths>,
) -> Result<RemoteInventory, Error> {
    load_remote_inventory_with_progress(store, mode, cache, &crate::progress::NoProgress)
}

/// Like [`load_remote_inventory`] with a plan-phase progress sink threaded
/// into the COLD path only (issue 42, I42-callers-no-sink): warm manifest
/// loads emit zero plan-phase events (I42-warm-events); a cold `ListHead`
/// (or an Auto fallback) brackets with `PlanStart`/`PlanEnd` and lets the
/// store's `list_with_progress` emit `ListPage`/head events through the same
/// sink. No-sink callers keep the old signature via the wrapper above.
pub fn load_remote_inventory_with_progress(
    store: &dyn ObjectStore,
    mode: InventoryMode,
    cache: Option<&CachePaths>,
    progress: &dyn crate::progress::Progress,
) -> Result<RemoteInventory, Error> {
    match mode {
        InventoryMode::ListHead => live_list_head(store, progress),
        InventoryMode::Auto => match try_load_manifest(store, cache) {
            Ok(ManifestWarm::Loaded(inv)) => Ok(inv),
            Ok(ManifestWarm::Missing) => {
                // No manifest object: auto falls back cold with a normative
                // warning (Q3: the read path stays side-effect free).
                let mut inv = live_list_head(store, progress)?;
                inv.warnings.push(
                    "inventory manifest missing; falling back to list+head (next push may ensure a manifest via bootstrap or commit, or run vaultsync repair)"
                        .to_string(),
                );
                Ok(inv)
            }
            Ok(ManifestWarm::Invalid(detail)) => {
                // Present but unusable (corrupt body / unknown schema / over
                // soft cap): same fallback, but the detail tells the
                // operator WHY and that push may heal it via the H1
                // head-then-If-Match commit (or run repair).
                let mut inv = live_list_head(store, progress)?;
                inv.warnings.push(format!(
                    "inventory manifest invalid: {detail}; falling back to list+head (push will try to replace a present corrupt object, or run vaultsync repair)"
                ));
                Ok(inv)
            }
            Err(e) => Err(e),
        },
        InventoryMode::Manifest => {
            // Strict mode (W234): a missing OR corrupt manifest is a hard
            // error suggesting repair - never a silent empty plan, never a
            // cold fallback (fail closed). A non-NotFound fetch error
            // propagates via `?` (fail closed on store trouble too). The
            // corrupt case carries the parse detail (F4/L6, review
            // 5472033449).
            match try_load_manifest(store, cache)? {
                ManifestWarm::Loaded(inv) => Ok(inv),
                ManifestWarm::Missing => Err(Error::Other(
                    "inventory.mode=manifest requires a valid remote manifest (none present); run vaultsync repair to create one".to_string(),
                )),
                ManifestWarm::Invalid(detail) => Err(Error::Other(format!(
                    "inventory.mode=manifest requires a valid remote manifest, but the object is invalid: {detail}; run vaultsync repair to rebuild it"
                ))),
            }
        }
    }
}

/// Outcome of a warm manifest attempt (F4/L6, reviews 5472028291 +
/// 5472033449): `Loaded` = valid manifest; `Missing` = absent object;
/// `Invalid` = present but unusable (corrupt body, unknown schema, or above
/// the soft cap), carrying the parse/remedy detail so operators can tell
/// "absent (next push creates)" from "present but corrupt (push may heal via
/// H1, or run repair)".
enum ManifestWarm {
    Loaded(RemoteInventory),
    Missing,
    Invalid(String),
}

/// Warm attempt: fetch + parse the remote manifest (issue 45, 6.1 steps 2-3
/// for the success path). A non-NotFound fetch error (e.g. Unavailable)
/// fails closed (never silently plan empty).
fn try_load_manifest(
    store: &dyn ObjectStore,
    cache: Option<&CachePaths>,
) -> Result<ManifestWarm, Error> {
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
/// M2 (review 5472028291): the fetch heads first and streams through a
/// capped writer, so a pathological/hostile object at MANIFEST_KEY cannot
/// force a full download + peak RSS before refusal. Absent => `Missing`;
/// over-cap or unparseable => `Invalid(detail)` (F4/L6: the caller decides
/// auto fallback vs strict error).
fn try_load_manifest_fresh(store: &dyn ObjectStore) -> Result<ManifestWarm, Error> {
    let (entity, buf) = match head_then_get_manifest(store, crate::manifest::MANIFEST_MAX_BYTES)? {
        WarmFetch::Body(entity, buf) => (entity, buf),
        WarmFetch::Absent => return Ok(ManifestWarm::Missing),
        WarmFetch::OverCap(msg) => return Ok(ManifestWarm::Invalid(msg)),
    };
    match crate::manifest::parse_manifest_bytes(&buf) {
        Ok(m) => manifest_inventory(&m, entity.etag).map(ManifestWarm::Loaded),
        Err(e) => Ok(ManifestWarm::Invalid(format!("{e}"))),
    }
}

/// Result of a warm manifest fetch attempt (M2, review 5472028291).
/// `Absent` = NotFound; `OverCap` = present but above the soft cap
/// (corrupt-like: Auto falls back, strict mode errors); `Body` = fetched
/// bytes within cap.
#[derive(Debug)]
enum WarmFetch {
    Absent,
    OverCap(String),
    Body(crate::entity::Entity, Vec<u8>),
}

/// Warm manifest fetch (M2): HEAD MANIFEST_KEY first - refuse when the
/// reported size already exceeds `cap` (cheap loud path, zero body bytes) -
/// then GET through a [`CappedWriter`] so a lying or changing object cannot
/// blow RSS. `Ok(Absent)` = NotFound; `Ok(OverCap)` = present but over the
/// cap (head size or mid-stream); a non-NotFound fetch error still fails
/// closed.
fn head_then_get_manifest(store: &dyn ObjectStore, cap: u64) -> Result<WarmFetch, Error> {
    let head = match store.head(crate::local::MANIFEST_KEY) {
        Ok(e) => e,
        Err(Error::NotFound(_)) => return Ok(WarmFetch::Absent),
        Err(e) => return Err(e),
    };
    if head.size > cap {
        return Ok(WarmFetch::OverCap(format!(
            "manifest object is {} bytes, above the {} byte soft cap; refusing to download (run vaultsync repair to rebuild it)",
            head.size, cap
        )));
    }
    let mut writer = CappedWriter::new(cap);
    match store.get_to(crate::local::MANIFEST_KEY, &mut writer) {
        Ok(entity) => Ok(WarmFetch::Body(entity, writer.buf)),
        Err(Error::NotFound(_)) => Ok(WarmFetch::Absent),
        // The writer tripped mid-stream: over-cap (belt-and-braces).
        Err(_) if writer.tripped => Ok(WarmFetch::OverCap(format!(
            "manifest body exceeded the {cap} byte soft cap while streaming; refusing to use it (run vaultsync repair to rebuild it)"
        ))),
        Err(e) => Err(e),
    }
}

/// Body writer for the warm manifest fetch (M2, review 5472028291): accepts
/// up to `cap` bytes, then sets `tripped` and answers an io error so a
/// hostile/lying object cannot force an unbounded buffer. Belt-and-braces
/// behind the head size check in [`head_then_get_manifest`].
struct CappedWriter {
    buf: Vec<u8>,
    cap: u64,
    tripped: bool,
}

impl CappedWriter {
    fn new(cap: u64) -> Self {
        CappedWriter {
            buf: Vec::new(),
            cap,
            tripped: false,
        }
    }
}

impl std::io::Write for CappedWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.buf.len() as u64 + data.len() as u64 > self.cap {
            self.tripped = true;
            return Err(std::io::Error::other("soft cap exceeded"));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
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
) -> Result<ManifestWarm, Error> {
    // Conditional attempt when we hold a remote etag.
    if let Some((meta, etag)) =
        read_cache_meta(cache).and_then(|m| m.remote_etag.clone().map(|etag| (m, etag)))
    {
        // M2 (review 5472028291): the conditional Body streams through a
        // capped writer too - no extra head here (the conditional GET
        // already round-tripped; a head would double every warm load's
        // RTT).
        let mut writer = CappedWriter::new(crate::manifest::MANIFEST_MAX_BYTES);
        match store.get_to_with(
            crate::local::MANIFEST_KEY,
            &mut writer,
            crate::store::GetOpts {
                if_none_match_etag: Some(etag.clone()),
            },
        ) {
            Ok(crate::store::GetOutcome::NotModified(_)) => {
                if let Some((cached, manifest)) = read_cache_body(cache) {
                    // W259 (N3, review 5472033449): the cached body must
                    // belong with THIS meta - a mismatched fingerprint
                    // (body/meta crash window, rot, tamper) is
                    // invalidated and refetched, never planned from.
                    // W257 (L3): the body was parsed exactly once inside
                    // read_cache_body.
                    if body_fingerprint(&cached) == meta.body_fnv {
                        return manifest_inventory(&manifest, Some(etag)).map(ManifestWarm::Loaded);
                    }
                    invalidate_cache(cache);
                }
                // Corrupt/missing/mismatched cache body invalidated
                // above: fall through to a fresh fetch below.
            }
            Ok(crate::store::GetOutcome::Body(entity)) => {
                if writer.tripped {
                    invalidate_cache(cache);
                    return Ok(ManifestWarm::Invalid(format!(
                        "manifest body exceeded the {} byte soft cap while streaming",
                        crate::manifest::MANIFEST_MAX_BYTES
                    )));
                }
                match crate::manifest::parse_manifest_bytes(&writer.buf) {
                    Ok(m) => {
                        let inv = manifest_inventory(&m, entity.etag.clone())?;
                        fill_cache(cache, &writer.buf, entity.etag);
                        return Ok(ManifestWarm::Loaded(inv));
                    }
                    Err(e) => {
                        invalidate_cache(cache);
                        return Ok(ManifestWarm::Invalid(format!("{e}")));
                    }
                }
            }
            Err(Error::NotFound(_)) => {
                invalidate_cache(cache);
                return Ok(ManifestWarm::Missing);
            }
            // W245: any non-NotFound failure fails closed - never plan
            // from the stale cache alone. The capped writer's own trip
            // is over-cap (corrupt-like), not a store failure.
            Err(_) if writer.tripped => {
                invalidate_cache(cache);
                return Ok(ManifestWarm::Invalid(format!(
                    "manifest body exceeded the {} byte soft cap while streaming",
                    crate::manifest::MANIFEST_MAX_BYTES
                )));
            }
            Err(e) => return Err(e),
        }
    }
    // No usable cache etag (or 304 with an invalidated body): unconditional
    // fetch (head-then-capped-GET, M2), then fill the cache with the fresh
    // body + etag.
    let (entity, buf) = match head_then_get_manifest(store, crate::manifest::MANIFEST_MAX_BYTES)? {
        WarmFetch::Body(entity, buf) => (entity, buf),
        WarmFetch::Absent => {
            invalidate_cache(cache);
            return Ok(ManifestWarm::Missing);
        }
        WarmFetch::OverCap(msg) => {
            invalidate_cache(cache);
            return Ok(ManifestWarm::Invalid(msg));
        }
    };
    match crate::manifest::parse_manifest_bytes(&buf) {
        Ok(m) => {
            let inv = manifest_inventory(&m, entity.etag.clone())?;
            fill_cache(cache, &buf, entity.etag);
            Ok(ManifestWarm::Loaded(inv))
        }
        Err(e) => {
            invalidate_cache(cache);
            Ok(ManifestWarm::Invalid(format!("{e}")))
        }
    }
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
        // W259 (N3): the fingerprint proves this body belongs with this meta
        // on the 304 path.
        body_fnv: body_fingerprint(body),
    };
    let _ = write_cache_files(body, &meta, cache);
}

/// The cold path: today's `store.list("")` (list + head enrichment on S3),
/// with the empty-key drop (R4-M2) and the reserved partition (W63/A-L3 +
/// issue 45 control-plane). Folder views stay in `entities`; `file_entities`
/// carries files only (the commit base). I15-errors: a non-NotFound head
/// error still fails the whole listing (fail closed).
///
/// Issue 42 (I42 emission rules): the cold path emits `PlanStart` before the
/// list and `PlanEnd` after the success path (warm paths never call this, so
/// they stay event-free, I42-warm-events). The store's `list_with_progress`
/// carries `ListPage`/head events through the same sink (W343/W346).
/// Mid-cold failure returns WITHOUT `PlanEnd` - the CLI finalizes the bar
/// defensively via the renderer's `finish_plan` (I42-finalize).
fn live_list_head(
    store: &dyn ObjectStore,
    progress: &dyn crate::progress::Progress,
) -> Result<RemoteInventory, Error> {
    progress.event(crate::progress::ProgressEvent::PlanStart);
    let result = (|| -> Result<RemoteInventory, Error> {
        let listing = store.list_with_progress("", progress)?;
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
    })();
    if result.is_ok() {
        progress.event(crate::progress::ProgressEvent::PlanEnd);
    }
    result
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
/// fetched under (used for the conditional GET), when it was fetched, the
/// source key, and the body's fingerprint (W259/N3, review 5472033449) so
/// the 304 path can prove a cached body belongs with its meta. Stored at
/// `manifest-v1.meta.json`. The fingerprint field is REQUIRED: a meta
/// written before W259 fails to deserialize and is treated as absent
/// (one-time refetch).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheMeta {
    pub remote_etag: Option<String>,
    pub fetched_at_ms: u64,
    pub source_key: String,
    /// FNV-1a 64-bit of the cached body bytes, hex (16 chars).
    pub body_fnv: String,
}

/// FNV-1a 64-bit hex fingerprint of a cache body (std-only; the same
/// primitive the mock uses for etags, reused per W259).
fn body_fingerprint(body: &[u8]) -> String {
    format!("{:016x}", crate::store::mock::fnv1a(body))
}

/// Write the cache body + meta atomically (temp+rename, owner-only 0o600 on
/// Unix; W243). A crash between the two renames leaves a body/meta pair that
/// may disagree - the reader validates the PAIR: a meta that does not
/// deserialize is treated as absent, a body that fails to parse is
/// invalidated (W245), and a body whose fingerprint does not match its meta
/// is invalidated and refetched (W259/N3). No single crash window can serve
/// a wrong body.
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

/// Read the cached manifest body and parse it exactly ONCE; `None` when
/// unreadable OR unparseable. A corrupt cached body is invalidated (removed,
/// best-effort - W245) so the next load refetches instead of re-serving
/// garbage. W257 (L3, review 5472028291): the caller reuses the returned
/// parsed manifest - the 304 path never parses the same bytes twice, and a
/// second failure is a fall-through to fresh fetch, not a hard error.
fn read_cache_body(cache: &CachePaths) -> Option<(Vec<u8>, crate::manifest::ManifestV1)> {
    let bytes = std::fs::read(&cache.body).ok()?;
    match crate::manifest::parse_manifest_bytes(&bytes) {
        Ok(manifest) => Some((bytes, manifest)),
        Err(_) => {
            invalidate_cache(cache);
            None
        }
    }
}

/// Best-effort removal of both cache files (W245 invalidate-on-corrupt).
fn invalidate_cache(cache: &CachePaths) {
    let _ = std::fs::remove_file(&cache.body);
    let _ = std::fs::remove_file(&cache.meta);
}

/// Inventory knobs threaded into [`crate::build_plan`] (issue 45,
/// D-plan-seam): the resolved `[inventory].mode` and the vault root (`Some`
/// enables the S7 local cache; `None` until W243). Cold-head concurrency is
/// owned by store construction (M3/F3, reviews 5472028291 + 5472033449) -
/// not re-bound here.
#[derive(Debug, Clone)]
pub struct InventoryOpts {
    pub mode: InventoryMode,
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
            vault_root: None,
        }
    }
}

impl Default for InventoryOpts {
    /// Product default (Q1): `auto`, no cache until S7.
    fn default() -> Self {
        InventoryOpts {
            mode: InventoryMode::Auto,
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

/// Conditional shape for the shared single-attempt manifest put (issue 48,
/// W263 / D-write-helper / F8). The lowest shared layer: one conditional put,
/// no retry, no HEAD, no H1-V GET, no dry-run interpretation - those live in
/// the callers that wrap this helper (repair keeps its retry/force/dry-run;
/// commit and B1 keep their H1-V resolve).
#[derive(Debug, Clone)]
pub(crate) enum WriteCond {
    /// If-Match: succeed only if the current object etag equals this.
    IfMatch(String),
    /// If-None-Match: *: succeed only if the key does not already exist.
    IfNoneMatchStar,
    /// Unconditional put (N5 etag-less backends / repair --force).
    Force,
}

/// Outcome of [`write_manifest_body`]: a written object (with its etag) or a
/// lost conditional race. `PreconditionFailed` is a value, not an error -
/// callers decide policy (commit Q2 warns; B1 push aborts; B1 status exits).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WriteBodyOutcome {
    Written { etag: Option<String> },
    PreconditionFailed,
}

/// Shared lowest-layer manifest writer (issue 48, W263 / D-write-helper / F8):
/// serde is assumed done by the caller - this owns ONLY the single conditional
/// put (via `cond`) plus the optional cache fill on success, so repair, commit,
/// and B1 cannot diverge on conditional-put/cache-fill plumbing.
///
/// Deliberately narrow: NO retry loop, NO HEAD, NO H1-V GET, NO dry-run. The
/// plan's W263 pin: moving a retry loop INTO this helper would make the
/// bare-helper path retry, which the repair-lost-race retry pin forbids - the
/// helper answers at most one put.
pub(crate) fn write_manifest_body(
    store: &dyn ObjectStore,
    body: &[u8],
    cond: WriteCond,
    cache: Option<&CachePaths>,
) -> Result<WriteBodyOutcome, Error> {
    let body_len = body.len() as u64;
    // W304 (PR50-r1 F2, review 5476323432): `Cursor::new(&[u8])` reads in
    // place - no forced `body.to_vec()` clone. All in-tree stores consume the
    // reader synchronously within this call.
    let mut cursor = std::io::Cursor::new(body);
    let opts = match cond {
        WriteCond::IfMatch(etag) => crate::store::PutOpts {
            if_match_etag: Some(etag),
            ..Default::default()
        },
        WriteCond::IfNoneMatchStar => crate::store::PutOpts {
            if_none_match_star: true,
            ..Default::default()
        },
        WriteCond::Force => crate::store::PutOpts::default(),
    };
    match store.put_from_with(crate::local::MANIFEST_KEY, &mut cursor, body_len, opts) {
        Ok(entity) => {
            // W246 contract: a successful write refreshes the local mirror.
            if let Some(cache) = cache {
                fill_cache(cache, body, entity.etag.clone());
            }
            Ok(WriteBodyOutcome::Written { etag: entity.etag })
        }
        Err(Error::PreconditionFailed(_)) => Ok(WriteBodyOutcome::PreconditionFailed),
        Err(e) => Err(e),
    }
}

/// Map an optional live object etag to a write condition (issue 48, W263):
/// a known etag => If-Match; an etag-less present object (N5 backend) =>
/// unconditional Force put (If-None-Match: * would fail forever on a present
/// object; If-Match needs an etag). Used by repair's present branch to
/// preserve its pre-extract semantics through the shared helper.
fn cond_from_etag(etag: Option<String>) -> WriteCond {
    match etag {
        Some(e) => WriteCond::IfMatch(e),
        None => WriteCond::Force,
    }
}
/// Repair options (issue 45, D-repair / section 10). Cold list+head head
/// concurrency is a store-construction concern, not re-bound here (M3/F3,
/// reviews 5472028291 + 5472033449).
#[derive(Debug, Clone)]
pub struct RepairOpts {
    /// Overwrite the manifest unconditionally (no If-Match; used after a
    /// bootstrap or when the current etag is unknown/skewed).
    pub force: bool,
    /// Compute the body and report the entry count but write nothing.
    pub dry_run: bool,
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
    // I42-callers-no-sink: the no-sink wrapper behaves byte-identically to
    // today (NoProgress emits nothing on the cold list).
    repair_manifest_with_progress(store, opts, cache, &crate::progress::NoProgress)
}

/// Like [`repair_manifest`] with a plan-phase progress sink threaded into the
/// cold list+head (issue 42, W349): brackets with `PlanStart`/`PlanEnd` (and
/// `ListPage`/head events from the store's `list_with_progress`), including
/// dry-run (which still lists cold before skipping the write). No executor
/// `PassKind` events on this path (W350).
pub fn repair_manifest_with_progress(
    store: &dyn ObjectStore,
    opts: &RepairOpts,
    cache: Option<&CachePaths>,
    progress: &dyn crate::progress::Progress,
) -> Result<RepairReport, Error> {
    let inv = live_list_head(store, progress)?;
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
    // W263 (issue 48, D-write-helper / F8): repair routes its bytes through
    // the shared single-attempt `write_manifest_body`, but KEEPS its own
    // retry/force/dry-run policy in this wrapper. The helper has no loop;
    // the lost-race retry-once lives HERE (the plan's W263 pin).
    let written_etag: Option<String> = if opts.force {
        // N2 (review 5472033449): force mode never needs a pre-put head; the
        // put result carries the etag (no trailing head).
        match write_manifest_body(store, &body, WriteCond::Force, cache)? {
            WriteBodyOutcome::Written { etag } => etag,
            // N2 (PR50-r1, review 5476323432): a store that answers
            // PreconditionFailed on an unconditional put is surfaced as Err -
            // never `unreachable!`.
            WriteBodyOutcome::PreconditionFailed => {
                return Err(Error::PreconditionFailed(format!(
                    "unexpected precondition failure on force put of {}",
                    crate::local::MANIFEST_KEY
                )));
            }
        }
    } else {
        match store.head(crate::local::MANIFEST_KEY) {
            Ok(cur) => {
                match write_manifest_body(store, &body, cond_from_etag(cur.etag), cache)? {
                    WriteBodyOutcome::Written { etag } => etag,
                    // Lost the race: retry ONCE against a fresh head. The
                    // retry is repair's policy, not the helper's (W263).
                    WriteBodyOutcome::PreconditionFailed => {
                        let cur = store.head(crate::local::MANIFEST_KEY)?;
                        match write_manifest_body(store, &body, cond_from_etag(cur.etag), cache)? {
                            WriteBodyOutcome::Written { etag } => etag,
                            WriteBodyOutcome::PreconditionFailed => {
                                return Err(Error::PreconditionFailed(format!(
                                    "lost race twice on {} (repair retried once)",
                                    crate::local::MANIFEST_KEY
                                )));
                            }
                        }
                    }
                }
            }
            // No manifest yet: create (If-None-Match: *).
            Err(Error::NotFound(_)) => {
                match write_manifest_body(store, &body, WriteCond::IfNoneMatchStar, cache)? {
                    WriteBodyOutcome::Written { etag } => etag,
                    WriteBodyOutcome::PreconditionFailed => {
                        return Err(Error::PreconditionFailed(format!(
                            "lost race creating {} via repair",
                            crate::local::MANIFEST_KEY
                        )));
                    }
                }
            }
            Err(e) => return Err(e),
        }
    };
    Ok(RepairReport {
        listed: files.len(),
        written: true,
        dry_run: false,
        etag: written_etag,
        warnings: inv.warnings,
    })
}

/// The resolved write plan for a COLD manifest commit/ensure (issue 48,
/// S2 W293 / H1-V D-h1v / D-cond): decide create vs overwrite vs adopt by
/// first HEADing the live object, and on a present+etag object VALIDATE it
/// (GET + parse with load rules) before ever If-Match overwriting it -
/// never a blind H1 clobber of a concurrent-valid manifest.
#[derive(Debug, Clone)]
pub(crate) enum ColdPlan {
    /// Absent: create with If-None-Match: *.
    Create { files: Vec<Entity> },
    /// Present with no etag (N5 etag-less backend): unconditional put.
    ForcePut { files: Vec<Entity> },
    /// Present + etag: If-Match the live etag, with this final file set.
    Overwrite { files: Vec<Entity>, etag: String },
    /// Present + etag + VALID body: adopt without writing (B1 only; commit
    /// with successes never returns this). No cache fill on Adopt. Carries
    /// the parsed winner file set + entry count so a warm refresh installs a
    /// COMPLETE base (PR50-r1 H1, review 5476323432). `etag` is `String`
    /// because this branch is present+etag only (N1).
    Adopt {
        etag: String,
        files: Vec<Entity>,
        entry_count: usize,
    },
}

/// Shared cold H1-V resolve (issue 48, W295 / D-h1v / D-write-helper): takes
/// the base file set plus optional successful mutations and returns what to
/// do with the LIVE object at MANIFEST_KEY.
///
/// - `successes_opt: None` (B1): write paths use `base_files` verbatim; a
///   concurrent-valid manifest is ADOPTED (no put, no clobber).
/// - `successes_opt: Some(successes)` (commit): absent/etag-less/invalid fold
///   `apply(base_files, successes)`; a concurrent-valid manifest folds
///   `apply(their.file_entities, successes)` so the winner's untouched keys
///   survive and our successes win on touched keys (F1 / W291).
///
/// A GET/parse failure on the validate probe is an Err (fail closed): the
/// caller decides policy (B1 push warns-and-continues; commit warns as today;
/// B1 status exits 1).
pub(crate) fn resolve_cold_put_plan(
    store: &dyn ObjectStore,
    base_files: &[Entity],
    successes_opt: Option<&[CommitMutation]>,
) -> Result<ColdPlan, Error> {
    // The base fold: for commit, apply base+successes; for B1, keep base
    // (successes is None there so this is an identity copy).
    let apply_base = |base: &[Entity]| -> Vec<Entity> {
        match successes_opt {
            Some(s) => apply_commit_mutations(base, s),
            None => base.to_vec(),
        }
    };
    match store.head(crate::local::MANIFEST_KEY) {
        Err(Error::NotFound(_)) => Ok(ColdPlan::Create {
            files: apply_base(base_files),
        }),
        // Present etag-less (N5): no If-Match to protect with; degrade to an
        // unconditional put (residual multi-writer hole, documented).
        Ok(ent) if ent.etag.is_none() => Ok(ColdPlan::ForcePut {
            files: apply_base(base_files),
        }),
        Ok(ent) => {
            let live_etag = ent
                .etag
                .as_deref()
                .expect("present+etag branch")
                .to_string();
            // F1 (PR50-r1, review 5476323432): a present object whose HEAD
            // size already exceeds the soft cap is unusable - heal via If-Match
            // WITHOUT GETting the (potentially huge) body. Cheap and loud on
            // the head size alone (same outcome family as the tripped writer).
            if ent.size > crate::manifest::MANIFEST_MAX_BYTES {
                return Ok(ColdPlan::Overwrite {
                    files: apply_base(base_files),
                    etag: live_etag,
                });
            }
            // H1-V validate probe: GET the small JSON with the same capped
            // writer the warm fetch uses, then parse with load rules.
            let mut writer = CappedWriter::new(crate::manifest::MANIFEST_MAX_BYTES);
            match store.get_to(crate::local::MANIFEST_KEY, &mut writer) {
                Ok(ent) => {
                    if writer.tripped {
                        // Present but above the soft cap: unusable -> heal.
                        // Body is unusable, so If-Match the HEAD etag (the
                        // live token for replacing the junk).
                        return Ok(ColdPlan::Overwrite {
                            files: apply_base(base_files),
                            etag: live_etag,
                        });
                    }
                    // M2 (PR50-r2, review 5476798257): prefer the GET
                    // entity etag - the generation we actually validated -
                    // when it is present (matches the warm load which trusts
                    // the GET etag). A HEAD etag that DIFFERS from the GET
                    // etag means the object changed between HEAD and GET
                    // (another writer): fail closed, never Adopt/If-Match a
                    // mixed pair (caller F2 policy). A GET etag-less backend
                    // (N5 residual) falls back to the HEAD etag.
                    let cond_etag = match (ent.etag.as_deref(), live_etag.as_str()) {
                        (Some(g), h) if g != h => {
                            return Err(Error::Other(format!(
                                "manifest HEAD/GET etag mismatch during conditional resolve ({h} vs {g}); please retry"
                            )));
                        }
                        (Some(g), _) => g.to_string(),
                        (None, h) => h.to_string(),
                    };
                    match crate::manifest::parse_manifest_bytes(&writer.buf) {
                        Ok(their) => match successes_opt {
                            // B1: a concurrent-valid manifest is adopted.
                            None => {
                                let files =
                                    crate::manifest::manifest_to_file_entities(&their)
                                        .expect("parsed manifest maps cleanly");
                                // N4: the carried files/count stay lock-step.
                                debug_assert_eq!(files.len(), their.entries.len());
                                Ok(ColdPlan::Adopt {
                                    etag: cond_etag,
                                    files,
                                    entry_count: their.entries.len(),
                                })
                            }
                            // Commit: fold successes onto THEIR entries so
                            // their untouched keys survive (F1 / W291).
                            Some(successes) => {
                                let their_files =
                                    crate::manifest::manifest_to_file_entities(&their)
                                        .expect("parsed manifest maps cleanly");
                                let files = apply_commit_mutations(&their_files, successes);
                                Ok(ColdPlan::Overwrite {
                                    files,
                                    etag: cond_etag,
                                })
                            }
                        },
                        // Present but corrupt: heal via If-Match the etag of
                        // the validated-per-HEAD generation.
                        Err(_) => Ok(ColdPlan::Overwrite {
                            files: apply_base(base_files),
                            etag: cond_etag,
                        }),
                    }
                }
                // The object vanished between HEAD and GET (concurrent
                // delete): fail closed - callers choose policy.
                Err(Error::NotFound(_)) => Err(Error::Other(
                    "manifest vanished between HEAD and GET during conditional resolve (another writer removed it); please retry".to_string(),
                )),
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
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
    // Decide the final file set + write condition. WARM base => If-Match on
    // the base etag over apply(base, successes) (no HEAD, no H1-V - walready
    // trusted the manifest we read). COLD base => H1-V resolve (W293):
    // absent create, etag-less force, present+valid fold THEIR+successes,
    // present+corrupt heal base+successes - never a blind clobber.
    let (files, cond) = match &base.manifest_etag {
        Some(e) => {
            let files = apply_commit_mutations(&base.file_entities, successes);
            (files, WriteCond::IfMatch(e.clone()))
        }
        None => match resolve_cold_put_plan(store, &base.file_entities, Some(successes))? {
            ColdPlan::Create { files } => (files, WriteCond::IfNoneMatchStar),
            ColdPlan::ForcePut { files } => (files, WriteCond::Force),
            ColdPlan::Overwrite { files, etag } => (files, WriteCond::IfMatch(etag)),
            ColdPlan::Adopt { .. } => {
                return Err(Error::Other(
                    "internal: commit cold resolve returned Adopt; please retry".to_string(),
                ));
            }
        },
    };
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
    match write_manifest_body(store, &body, cond, cache)? {
        WriteBodyOutcome::Written { etag } => Ok(CommitOutcome::Written {
            etag,
            entry_count: manifest.entry_count,
        }),
        WriteBodyOutcome::PreconditionFailed => Ok(CommitOutcome::PreconditionFailed),
    }
}

/// Outcome of a push/status-time inventory bootstrap (issue 48, IQ-api /
/// D-ensure-outcome / IQ-refresh): what B1 did to the remote manifest.
/// `Written` = we published a new snapshot and filled the local cache;
/// `Adopted` = a concurrent-valid manifest already lived there (no put, no
/// cache fill) so we refresh warm; `PreconditionFailed` = another writer won
/// (never clobbered - the caller decides: push aborts, status exits 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnsureOutcome {
    Written {
        etag: Option<String>,
        entry_count: usize,
    },
    Adopted {
        /// Live manifest etag: this branch is present+etag-only, so the
        /// adopted generation is a bare `String` (N1, PR50-r2) not an
        /// `Option`.
        etag: String,
        /// Parsed winner FILE set, carried so a warm refresh installs a
        /// complete base (PR50-r1 H1). `len() == entry_count` after
        /// `manifest_to_file_entities` (debug_asserted, N4 PR50-r2).
        files: Vec<Entity>,
        entry_count: usize,
    },
    PreconditionFailed,
}

/// Wall-clock ms (diagnostic `created_ms` for written manifests).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Publish (or adopt) the remote manifest from a COLD inventory base (issue
/// 48, B1 / IQ-api). The caller enforces policy - this runs only when B1 is
/// eligible (push: auto + push-ensure + LiveListHead; status: the explicit
/// `--write-manifest` flag + auto + cold). It never re-lists and never claims
/// in-flight uploads: it publishes the pre-transfer snapshot
/// (`base.file_entities`) via If-None-Match-create / H1-V heal, or ADOPTS a
/// concurrent-valid live manifest without writing (D-h1v / F1).
///
/// Cache fill: on `Written` the local mirror is refreshed (D-cache); on
/// `Adopted` no cache write is done in v1 (the next warm load fetches/304s).
pub fn ensure_remote_manifest(
    store: &dyn ObjectStore,
    base: &InventoryBase,
    cache: Option<&CachePaths>,
) -> Result<EnsureOutcome, Error> {
    let created_ms = now_ms();
    let generator = format!("vaultsync {}", crate::version());
    match resolve_cold_put_plan(store, &base.file_entities, None)? {
        // Concurrent-valid manifest already present: adopt, never write.
        // The parsed file set rides along for a complete warm refresh (H1).
        ColdPlan::Adopt {
            etag,
            files,
            entry_count,
        } => Ok(EnsureOutcome::Adopted {
            etag,
            files,
            entry_count,
        }),
        ColdPlan::Create { files } => write_bootstrap(
            store,
            &files,
            created_ms,
            &generator,
            WriteCond::IfNoneMatchStar,
            cache,
        ),
        ColdPlan::ForcePut { files } => write_bootstrap(
            store,
            &files,
            created_ms,
            &generator,
            WriteCond::Force,
            cache,
        ),
        ColdPlan::Overwrite { files, etag } => write_bootstrap(
            store,
            &files,
            created_ms,
            &generator,
            WriteCond::IfMatch(etag),
            cache,
        ),
    }
}

/// What a successful B1 refresh applied to an inventory base (PR50-r1, W301):
/// the caller prints the locked stderr line; the library never writes stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnsureApply {
    Written { entry_count: usize },
    Adopted { entry_count: usize },
}

/// Install a successful B1 outcome into an inventory base (PR50-r1 H1/F3,
/// review 5476323432): `Written` warms etag/source only (`file_entities`
/// already match the body WE wrote), `Adopted` ALSO replaces `file_entities`
/// with the parsed winner snapshot so the final commit's warm fold preserves
/// their untouched keys instead of clobbering them (H1). Takes `outcome` by
/// value so `Adopted`'s files are moved, not cloned. Returns `None` for
/// `PreconditionFailed` - that is a non-apply the caller decides policy for.
pub(crate) fn apply_ensure_outcome(
    base: &mut InventoryBase,
    outcome: EnsureOutcome,
) -> Option<EnsureApply> {
    match outcome {
        EnsureOutcome::Written { etag, entry_count } => {
            base.source = InventorySource::Manifest {
                remote_etag: etag.clone(),
            };
            base.manifest_etag = etag;
            Some(EnsureApply::Written { entry_count })
        }
        EnsureOutcome::Adopted {
            etag,
            files,
            entry_count,
        } => {
            // H1: install the COMPLETE adopted snapshot, not the stale cold
            // list view. N4: the carried files/count must stay in lock-step.
            debug_assert_eq!(files.len(), entry_count);
            base.file_entities = files;
            base.source = InventorySource::Manifest {
                remote_etag: Some(etag.clone()),
            };
            base.manifest_etag = Some(etag);
            Some(EnsureApply::Adopted { entry_count })
        }
        EnsureOutcome::PreconditionFailed => None,
    }
}

/// Shared B1 write arm (issue 48, S3): serialize `files` from the cold base
/// (pre-ignore, post-reserved - D-body) and publish via the shared single
/// attempt helper, mapping the outcome to `EnsureOutcome`. `entry_count` is
/// the real file count (D-b1-ok-msg / A23).
fn write_bootstrap(
    store: &dyn ObjectStore,
    files: &[Entity],
    created_ms: u64,
    generator: &str,
    cond: WriteCond,
    cache: Option<&CachePaths>,
) -> Result<EnsureOutcome, Error> {
    let manifest = crate::manifest::file_entities_to_manifest(
        files,
        created_ms,
        Some(generator.into()),
        None,
    )?;
    let body = crate::manifest::serialize_manifest(&manifest)?;
    match write_manifest_body(store, &body, cond, cache)? {
        WriteBodyOutcome::Written { etag } => Ok(EnsureOutcome::Written {
            etag,
            entry_count: files.len(),
        }),
        WriteBodyOutcome::PreconditionFailed => Ok(EnsureOutcome::PreconditionFailed),
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

    // I42 (W346): live_list_head routes through `list_with_progress`, not
    // bare `list` - a counter store shows list_with_progress hit once and
    // list hit zero on the cold path (the S3 page emission can only fire
    // when the facade calls the progress-carrying method).
    #[test]
    fn cold_path_uses_list_with_progress() {
        struct CountingListStore {
            inner: MemoryStore,
            list: std::sync::atomic::AtomicUsize,
            list_with: std::sync::atomic::AtomicUsize,
        }
        impl ObjectStore for CountingListStore {
            fn list(&self, prefix: &str) -> Result<crate::store::Listing, Error> {
                self.list.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.inner.list(prefix)
            }
            fn list_with_progress(
                &self,
                prefix: &str,
                progress: &dyn crate::progress::Progress,
            ) -> Result<crate::store::Listing, Error> {
                self.list_with
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.inner.list_with_progress(prefix, progress)
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
        let store = CountingListStore {
            inner: MemoryStore::new(),
            list: std::sync::atomic::AtomicUsize::new(0),
            list_with: std::sync::atomic::AtomicUsize::new(0),
        };
        let prog = crate::testutil::RecordingProgress::new();
        let _inv =
            load_remote_inventory_with_progress(&store, InventoryMode::ListHead, None, &prog)
                .unwrap();
        assert_eq!(
            store.list_with.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "cold path must call list_with_progress exactly once"
        );
        assert_eq!(
            store.list.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "cold path must never call bare list"
        );
    }

    // I42 facade (W345): Auto fallback (missing / corrupt manifest) is COLD -
    // emits PlanStart..PlanEnd and keeps the existing warning text verbatim;
    // progress must not alter warning strings.
    #[test]
    fn auto_cold_fallback_emits_plan_progress_and_keeps_warnings() {
        use crate::progress::ProgressEvent;
        // missing manifest
        let store = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"aaa".to_vec());
        store
            .put_from("a.md", &mut c, 3, Some(1_600_000_000_000))
            .unwrap();
        let prog = crate::testutil::RecordingProgress::new();
        let inv =
            load_remote_inventory_with_progress(&store, InventoryMode::Auto, None, &prog).unwrap();
        assert_eq!(inv.base.source, InventorySource::LiveListHead);
        let events = prog.events();
        assert!(matches!(events.first(), Some(ProgressEvent::PlanStart)));
        assert!(matches!(events.last(), Some(ProgressEvent::PlanEnd)));
        let warn = inv.warnings.join(" ");
        assert!(warn.contains("falling back"), "warnings: {warn}");
        assert!(warn.contains("list+head"), "warnings: {warn}");

        // corrupt manifest body
        let store2 = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"not json at all".to_vec());
        store2
            .put_from(
                crate::local::MANIFEST_KEY,
                &mut c,
                "not json at all".len() as u64,
                None,
            )
            .unwrap();
        let mut c = std::io::Cursor::new(b"aaa".to_vec());
        store2
            .put_from("a.md", &mut c, 3, Some(1_600_000_000_000))
            .unwrap();
        let prog2 = crate::testutil::RecordingProgress::new();
        let inv2 = load_remote_inventory_with_progress(&store2, InventoryMode::Auto, None, &prog2)
            .unwrap();
        assert_eq!(inv2.base.source, InventorySource::LiveListHead);
        let events2 = prog2.events();
        assert!(matches!(events2.first(), Some(ProgressEvent::PlanStart)));
        assert!(matches!(events2.last(), Some(ProgressEvent::PlanEnd)));
        let warn2 = inv2.warnings.join(" ");
        assert!(warn2.contains("falling back"), "warnings: {warn2}");
        assert!(warn2.contains("invalid"), "warnings: {warn2}");
    }

    // I42-warm (W344): warm Auto/Manifest loads emit ZERO plan-phase events
    // (I42-warm-events) - no PlanStart/PlanEnd, no ListPage, no heads; the
    // source stays Manifest and entities match.
    #[test]
    fn warm_load_emits_no_plan_progress() {
        let store = MemoryStore::new();
        let body = manifest_body(&[("notes/a.md", 3, Some(100)), ("b.md", 1, None)]);
        let body_len = body.len() as u64;
        let mut c = std::io::Cursor::new(body);
        store
            .put_from(crate::local::MANIFEST_KEY, &mut c, body_len, None)
            .unwrap();
        for mode in [InventoryMode::Auto, InventoryMode::Manifest] {
            let prog = crate::testutil::RecordingProgress::new();
            let inv = load_remote_inventory_with_progress(&store, mode, None, &prog).unwrap();
            assert!(
                prog.events().is_empty(),
                "warm load must be event-free: {:?}",
                prog.events()
            );
            assert!(matches!(inv.base.source, InventorySource::Manifest { .. }));
            assert_eq!(inv.base.file_entities.len(), 2);
        }
    }

    // I42 facade (W343): load_remote_inventory_with_progress brackets the
    // cold ListHead path with PlanStart..PlanEnd; FakePagingStore adds
    // ListPage between the brackets; the no-sink wrapper returns the same
    // entities/source/warnings as the with-progress call under NoProgress.
    #[test]
    fn load_remote_inventory_with_progress_cold_brackets() {
        use crate::progress::ProgressEvent;
        // MemoryStore: PlanStart/PlanEnd only (no page loop, no enrich).
        let store = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"aaa".to_vec());
        store
            .put_from("a.md", &mut c, 3, Some(1_600_000_000_000))
            .unwrap();
        let prog = crate::testutil::RecordingProgress::new();
        let inv = load_remote_inventory_with_progress(&store, InventoryMode::ListHead, None, &prog)
            .unwrap();
        let events = prog.events();
        assert!(
            matches!(events.first(), Some(ProgressEvent::PlanStart)),
            "first event must be PlanStart: {events:?}"
        );
        assert!(
            matches!(events.last(), Some(ProgressEvent::PlanEnd)),
            "last event must be PlanEnd: {events:?}"
        );
        assert_eq!(
            events.len(),
            2,
            "MemoryStore cold emits only the brackets: {events:?}"
        );
        assert_eq!(inv.base.source, InventorySource::LiveListHead);
        let keys: Vec<&str> = inv.entities.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["a.md"]);

        // FakePagingStore: ListPage events between the brackets.
        let paged = crate::testutil::FakePagingStore::new(1);
        let mut c = std::io::Cursor::new(b"x".to_vec());
        paged.inner.put_from("a.md", &mut c, 1, Some(1)).unwrap();
        let mut c = std::io::Cursor::new(b"y".to_vec());
        paged.inner.put_from("b.md", &mut c, 1, Some(2)).unwrap();
        let prog2 = crate::testutil::RecordingProgress::new();
        let inv2 =
            load_remote_inventory_with_progress(&paged, InventoryMode::ListHead, None, &prog2)
                .unwrap();
        let events2 = prog2.events();
        assert!(matches!(events2.first(), Some(ProgressEvent::PlanStart)));
        assert!(matches!(events2.last(), Some(ProgressEvent::PlanEnd)));
        assert!(
            events2
                .iter()
                .any(|e| matches!(e, ProgressEvent::ListPage { .. })),
            "FakePagingStore must emit ListPage between the brackets: {events2:?}"
        );
        assert_eq!(inv2.base.file_entities.len(), 2);

        // No-sink wrapper: identical result to NoProgress threading.
        let plain = load_remote_inventory(&store, InventoryMode::ListHead, None).unwrap();
        assert_eq!(inv.base.source, plain.base.source);
        assert_eq!(inv.base.file_entities, plain.base.file_entities);
        assert_eq!(inv.base.manifest_etag, plain.base.manifest_etag);
        assert_eq!(inv.entities, plain.entities);
        assert_eq!(inv.warnings, plain.warnings);
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
        let inv = load_remote_inventory(&store, InventoryMode::Auto, None).unwrap();
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
        let inv = load_remote_inventory(&store, InventoryMode::Auto, None).unwrap();
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
        let inv = load_remote_inventory(&store, InventoryMode::Auto, None).unwrap();
        assert_eq!(inv.base.source, InventorySource::LiveListHead);
        let warn = inv.warnings.join(" ");
        assert!(warn.contains("falling back"), "warnings: {warn}");
        assert!(warn.contains("list+head"), "warnings: {warn}");
    }

    #[test]
    fn auto_missing_manifest_warning_names_absent() {
        // W255 (F4/L6, reviews 5472028291 + 5472033449) + W319 (PR50-r2
        // L2, review 5476798257): the auto fallback warning must let an
        // operator tell ABSENT from CORRUPT - the missing case names
        // absence and that a future push may ENSURE a manifest (bootstrap
        // or commit) or repair can create it, and must NOT claim "corrupt"
        // or promise "will create one". The unconditional old promise is
        // wrong post-48: bootstrap=never may write nothing on zero mutation.
        let store = MemoryStore::new();
        let inv = load_remote_inventory(&store, InventoryMode::Auto, None).unwrap();
        assert_eq!(inv.base.source, InventorySource::LiveListHead);
        let warn = inv.warnings.join(" ");
        assert!(warn.contains("missing"), "warnings: {warn}");
        assert!(
            !warn.contains("corrupt"),
            "missing must not be called corrupt: {warn}"
        );
        assert!(
            !warn.contains("invalid"),
            "missing must not be called invalid: {warn}"
        );
        assert!(warn.contains("push"), "warnings: {warn}");
        assert!(
            warn.contains("ensure") || warn.contains("bootstrap"),
            "must mention post-48 ensure/bootstrap: {warn}"
        );
        assert!(
            !warn.contains("will create one"),
            "must not promise an unconditional create: {warn}"
        );
    }

    #[test]
    fn auto_corrupt_manifest_warning_includes_parse_detail() {
        // W255 (F4/L6): a corrupt body's auto warning carries a stable
        // fragment of the parse error so the operator sees WHY (here the
        // locked "JSON parse failed" wrapper), plus the push-may-heal
        // guidance. RED today: the shared generic string has no detail.
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
        let inv = load_remote_inventory(&store, InventoryMode::Auto, None).unwrap();
        assert_eq!(inv.base.source, InventorySource::LiveListHead);
        let warn = inv.warnings.join(" ");
        assert!(
            warn.contains("JSON parse failed"),
            "parse detail must surface: {warn}"
        );
        assert!(warn.contains("falling back"), "warnings: {warn}");
        assert!(warn.contains("push"), "warnings: {warn}");
    }

    #[test]
    fn strict_manifest_mode_surfaces_parse_detail() {
        // W255 (F4/L6): strict Manifest mode errors on a corrupt body with
        // the parse detail + a repair hint (never the generic missing
        // message). RED today: the strict error carries no parse text.
        let store = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"not json".to_vec());
        store
            .put_from(
                crate::local::MANIFEST_KEY,
                &mut c,
                "not json".len() as u64,
                None,
            )
            .unwrap();
        let err = load_remote_inventory(&store, InventoryMode::Manifest, None).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("JSON parse failed"),
            "parse detail must surface in strict error: {msg}"
        );
        assert!(msg.contains("repair"), "error must suggest repair: {msg}");
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
        let err = load_remote_inventory(&store, InventoryMode::Manifest, None).unwrap_err();
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
        let err = load_remote_inventory(&store2, InventoryMode::Manifest, None).unwrap_err();
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
            fn head(&self, _key: &str) -> Result<Entity, Error> {
                // A down store fails head too (M2 head-first fetch): the
                // load must still fail closed, never silently plan empty.
                Err(Error::Unavailable("store down".to_string()))
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
        let err =
            load_remote_inventory(&FailGetStore(MemoryStore::new()), InventoryMode::Auto, None)
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
        let inv = load_remote_inventory(&store, InventoryMode::ListHead, None).unwrap();
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
    fn commit_manifest_cold_keeps_concurrent_valid_keys() {
        // W291 (issue 48, F1 / D-h1v): a COLD commit must NOT clobber a
        // concurrent-valid manifest via a blind H1 If-Match overwrite. The
        // live object is valid, so the commit folds successes onto THEIR
        // entries (their untouched keys survive) and If-Matchs their etag.
        // A stale `base ∪ successes` fold (a.md only) would clobber b.md -
        // this RED pins the H1-V fold.
        let store = MemoryStore::new();
        // Their valid manifest: only b.md.
        let their_body = manifest_body(&[("b.md", 1, None)]);
        let blen = their_body.len() as u64;
        let mut c = std::io::Cursor::new(their_body);
        let their_put = store
            .put_from(crate::local::MANIFEST_KEY, &mut c, blen, None)
            .unwrap();
        let their_etag = their_put.etag.clone();
        // Cold base is STALE: has a.md, lacks b.md.
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
        match out {
            CommitOutcome::Written { etag, entry_count } => {
                assert!(etag.is_some());
                assert_eq!(entry_count, 2);
            }
            other => panic!("expected Written, got {other:?}"),
        }
        // Live body has BOTH a.md (our success) and b.md (their, untouched).
        let mut buf = Vec::new();
        let put = store.get_to(crate::local::MANIFEST_KEY, &mut buf).unwrap();
        assert_ne!(put.etag, their_etag, "we wrote a new manifest object");
        let m = crate::manifest::parse_manifest_bytes(&buf).unwrap();
        let keys: Vec<&str> = m.entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["a.md", "b.md"], "their key must survive");
    }

    #[test]
    fn commit_manifest_cold_base_overwrites_existing_corrupt_body() {
        // W249 (H1, review 5472028291) + W292 (issue 48, H1-V invalid
        // branch): a COLD base (`manifest_etag: None`) must resolve the
        // commit condition against the LIVE object: a present-but-corrupt
        // body is healed via If-Match on the live etag, not rejected by
        // create-only If-None-Match: *. Under H1-V (W293), a corrupt body
        // (GET+parse fails) falls to the invalid branch => Overwrite with
        // apply(base, successes) - this test pins exactly that. Today
        // (pre-fix) this answered PreconditionFailed and push could never
        // heal the corrupt object.
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

    /// Double whose `head` reports an over-cap manifest size WITHOUT any
    /// body, and whose `get_to` PANICS: the M2 head-size refuse must prevent
    /// any GET (zero body bytes is the contract; a panic is the loudest
    /// proof).
    struct NoGetOverCapStore {
        inner: MemoryStore,
    }
    impl ObjectStore for NoGetOverCapStore {
        fn list(&self, prefix: &str) -> Result<crate::store::Listing, Error> {
            self.inner.list(prefix)
        }
        fn head(&self, key: &str) -> Result<Entity, Error> {
            if key == crate::local::MANIFEST_KEY {
                return Ok(Entity {
                    key: key.to_string(),
                    size: crate::manifest::MANIFEST_MAX_BYTES + 1,
                    mtime_ms: None,
                    etag: Some("fake-etag".to_string()),
                });
            }
            self.inner.head(key)
        }
        fn get_to(&self, _key: &str, _w: &mut dyn std::io::Write) -> Result<Entity, Error> {
            panic!("M2: get_to must not be called when head already refuses (over cap)");
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

    /// Double whose `head` LIES about the size (reports small) while `get_to`
    /// streams a body above the cap: the capped writer must trip and refuse
    /// (belt-and-braces behind the head check, M2).
    struct LyingSizeStore {
        inner: MemoryStore,
    }
    impl ObjectStore for LyingSizeStore {
        fn list(&self, prefix: &str) -> Result<crate::store::Listing, Error> {
            self.inner.list(prefix)
        }
        fn head(&self, key: &str) -> Result<Entity, Error> {
            if key == crate::local::MANIFEST_KEY {
                return Ok(Entity {
                    key: key.to_string(),
                    size: 4,
                    mtime_ms: None,
                    etag: Some("fake-etag".to_string()),
                });
            }
            self.inner.head(key)
        }
        fn get_to(&self, key: &str, w: &mut dyn std::io::Write) -> Result<Entity, Error> {
            if key == crate::local::MANIFEST_KEY {
                w.write_all(b"0123456789abcdef")?;
                return Ok(Entity {
                    key: key.to_string(),
                    size: 16,
                    mtime_ms: None,
                    etag: Some("real-etag".to_string()),
                });
            }
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
    fn head_then_get_manifest_refuses_over_cap_without_get() {
        // W253 (M2, review 5472028291): the warm fetch heads MANIFEST_KEY
        // first and refuses when the reported size exceeds the soft cap -
        // `get_to` is never called (the double panics).
        let store = NoGetOverCapStore {
            inner: MemoryStore::new(),
        };
        match head_then_get_manifest(&store, 8) {
            Ok(WarmFetch::OverCap(msg)) => {
                assert!(msg.contains("soft cap"), "msg: {msg}");
            }
            other => panic!("expected OverCap, got {other:?}"),
        }
    }

    #[test]
    fn head_then_get_manifest_trips_on_lying_size() {
        // W253 (M2, belt-and-braces): a store whose head lies about the size
        // still cannot force an unbounded buffer - the capped writer trips
        // mid-stream and the fetch refuses.
        let store = LyingSizeStore {
            inner: MemoryStore::new(),
        };
        match head_then_get_manifest(&store, 8) {
            Ok(WarmFetch::OverCap(msg)) => {
                assert!(msg.contains("soft cap"), "msg: {msg}");
            }
            other => panic!("expected OverCap, got {other:?}"),
        }
    }

    #[test]
    fn warm_auto_falls_back_when_head_over_cap() {
        // W253 (M2, mode mapping): under Auto an over-cap manifest object is
        // Invalid (present but unusable) - the load falls back cold with a
        // warning naming the soft cap instead of failing the plan (W255:
        // the warning is "invalid", not "missing").
        let store = NoGetOverCapStore {
            inner: MemoryStore::new(),
        };
        let inv = load_remote_inventory(&store, InventoryMode::Auto, None).unwrap();
        assert_eq!(inv.base.source, InventorySource::LiveListHead);
        let warn = inv.warnings.join(" ");
        assert!(warn.contains("invalid"), "warnings: {warn}");
        assert!(warn.contains("soft cap"), "warnings: {warn}");
    }

    #[test]
    fn strict_manifest_mode_over_cap_is_invalid() {
        // W253 (M2, mode mapping): under strict Manifest mode an over-cap
        // object is Invalid - a hard error suggesting repair, never a cold
        // fallback.
        let store = NoGetOverCapStore {
            inner: MemoryStore::new(),
        };
        let err = load_remote_inventory(&store, InventoryMode::Manifest, None).unwrap_err();
        assert!(
            err.to_string().contains("requires a valid remote manifest"),
            "err: {err}"
        );
    }

    // I42 repair (W349): repair_manifest_with_progress brackets the cold
    // list with PlanStart..PlanEnd; dry-run still emits them (it lists cold
    // before skipping the write); the no-sink wrapper equals the NoProgress
    // result on every report field.
    #[test]
    fn repair_manifest_with_progress_emits_cold_brackets() {
        use crate::progress::ProgressEvent;
        let store = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"aaa".to_vec());
        store
            .put_from("a.md", &mut c, 3, Some(1_600_000_000_000))
            .unwrap();
        for opts in [
            RepairOpts {
                force: false,
                dry_run: true,
            },
            RepairOpts {
                force: false,
                dry_run: false,
            },
        ] {
            let prog = crate::testutil::RecordingProgress::new();
            let rep = repair_manifest_with_progress(&store, &opts, None, &prog).unwrap();
            let events = prog.events();
            assert!(
                matches!(events.first(), Some(ProgressEvent::PlanStart)),
                "dry_run={} first event: {events:?}",
                opts.dry_run
            );
            assert!(
                matches!(events.last(), Some(ProgressEvent::PlanEnd)),
                "dry_run={} last event: {events:?}",
                opts.dry_run
            );
            assert_eq!(rep.listed, 1);
        }
        // no-sink wrapper equals the NoProgress result field-for-field.
        let plain = repair_manifest(
            &store,
            &RepairOpts {
                force: false,
                dry_run: false,
            },
            None,
        )
        .unwrap();
        let prog2 = crate::testutil::RecordingProgress::new();
        let with = repair_manifest_with_progress(
            &store,
            &RepairOpts {
                force: false,
                dry_run: false,
            },
            None,
            &prog2,
        )
        .unwrap();
        assert_eq!(plain.listed, with.listed);
        assert_eq!(plain.written, with.written);
        assert_eq!(plain.dry_run, with.dry_run);
        assert_eq!(plain.etag, with.etag);
        assert_eq!(plain.warnings, with.warnings);
    }

    // I42 repair (W350): repair progress is plan-phase only - no executor
    // PassKind events on the recording.
    #[test]
    fn repair_progress_is_plan_phase_only() {
        let store = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"aaa".to_vec());
        store
            .put_from("a.md", &mut c, 3, Some(1_600_000_000_000))
            .unwrap();
        let prog = crate::testutil::RecordingProgress::new();
        repair_manifest_with_progress(
            &store,
            &RepairOpts {
                force: false,
                dry_run: false,
            },
            None,
            &prog,
        )
        .unwrap();
        let events = prog.events();
        assert!(
            !events.iter().any(|e| matches!(
                e,
                crate::progress::ProgressEvent::PassStart { .. }
                    | crate::progress::ProgressEvent::KeyDone { .. }
            )),
            "repair must emit plan-phase events only: {events:?}"
        );
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
    fn repair_force_uses_put_etag_without_trailing_head() {
        // W258 (N2, review 5472033449): repair must carry the etag from the
        // PUT result - no extra trailing head, and a head that fails after a
        // successful write must not degrade the report (today the trailing
        // head's error is swallowed into etag: None). Force mode never needs
        // a pre-put head, so a double whose head always fails on MANIFEST_KEY
        // proves the trailing head is gone.
        struct HeadFailStore {
            inner: MemoryStore,
        }
        impl ObjectStore for HeadFailStore {
            fn list(&self, prefix: &str) -> Result<crate::store::Listing, Error> {
                self.inner.list(prefix)
            }
            fn head(&self, _key: &str) -> Result<Entity, Error> {
                Err(Error::Unavailable("head disabled".to_string()))
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
        let store = HeadFailStore {
            inner: MemoryStore::new(),
        };
        let mut c = std::io::Cursor::new(b"hi".to_vec());
        store.put_from("a.md", &mut c, 2, Some(1)).unwrap();
        let rep = repair_manifest(
            &store,
            &RepairOpts {
                force: true,
                dry_run: false,
            },
            None,
        )
        .unwrap();
        assert!(rep.written);
        assert!(
            rep.etag.is_some(),
            "etag must come from the put result, not a trailing head"
        );
    }

    #[test]
    fn repair_force_put_precondition_failed_returns_err_no_panic() {
        // W305/N2 (PR50-r1, review 5476323432): a force repair against a
        // store that answers PreconditionFailed on the unconditional put must
        // return `Err(Error::PreconditionFailed(_))`, never panic on the old
        // `unreachable!`.
        struct AlwaysFailStore {
            inner: MemoryStore,
        }
        impl ObjectStore for AlwaysFailStore {
            fn list(&self, prefix: &str) -> Result<crate::store::Listing, Error> {
                self.inner.list(prefix)
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
            fn put_from_with(
                &self,
                _key: &str,
                _r: &mut dyn std::io::Read,
                _size: u64,
                _opts: crate::store::PutOpts,
            ) -> Result<Entity, Error> {
                Err(Error::PreconditionFailed(
                    "simulated unconditional-put precondition failure".to_string(),
                ))
            }
            fn delete(&self, key: &str) -> Result<(), Error> {
                self.inner.delete(key)
            }
        }
        let store = AlwaysFailStore {
            inner: MemoryStore::new(),
        };
        let mut c = std::io::Cursor::new(b"hi".to_vec());
        store.put_from("a.md", &mut c, 2, Some(1)).unwrap();
        let err = repair_manifest(
            &store,
            &RepairOpts {
                force: true,
                dry_run: false,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::PreconditionFailed(_)),
            "got {err:?} - must be Err, not panic"
        );
    }

    #[test]
    fn cache_write_read_round_trip_and_owner_only() {
        // W243 (issue 45): the cache body + meta write atomically (temp +
        // rename) under `<vault_root>/.vaultsync/cache/`; the meta carries
        // the remote etag, fetched time, and source key; reading back yields
        // the same values. On Unix the files are owner-only (0o600).
        let dir = crate::testutil::TempDir::new("vaultsync-cache-test");
        let cache = CachePaths::new(dir.path());
        let body =
            br#"{"schema":"vaultsync.manifest.v1","created_ms":0,"entry_count":0,"entries":[]}"#;
        let meta = CacheMeta {
            remote_etag: Some("\"abc\"".to_string()),
            fetched_at_ms: 1234,
            source_key: crate::local::MANIFEST_KEY.to_string(),
            body_fnv: body_fingerprint(body),
        };
        write_cache_files(body, &meta, &cache).unwrap();
        // No temp leftovers.
        assert!(
            std::fs::read_dir(cache.body.parent().unwrap())
                .unwrap()
                .all(|e| !e.unwrap().file_name().to_string_lossy().contains("tmp")),
            "temp files must be renamed away"
        );
        // Body parses; meta round-trips.
        let (cached_body, _manifest) = read_cache_body(&cache).expect("cached body");
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

        let inv1 = load_remote_inventory(&store, InventoryMode::Auto, Some(&cache)).unwrap();
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
        let inv2 = load_remote_inventory(&store, InventoryMode::Auto, Some(&cache)).unwrap();
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
    fn cached_garbage_body_304_path_refetches_fresh() {
        // W257 (L3, review 5472028291): the 304 path parses the cached body
        // exactly ONCE (no double parse) and a second failure (body rotted
        // after the meta was written) invalidates and fresh-fetches - never
        // a hard error from the 304 arm. Here the remote etag matches the
        // meta (304) while the cached body is garbage: the load succeeds
        // from a fresh fetch and heals the cache.
        //
        // Characterization: GREEN on arrival today (read_cache_body
        // invalidates garbage and the arm falls through); mutation-checked
        // by re-adding a `?` second parse on raw bytes -> hard Err from the
        // 304 arm -> pin re-fails.
        let dir = crate::testutil::TempDir::new("vaultsync-cache-test");
        let cache = CachePaths::new(dir.path());
        let store = CountingGetStore::new();
        let body = manifest_body(&[("a.md", 3, Some(100))]);
        let body_len = body.len() as u64;
        let mut c = std::io::Cursor::new(body);
        let put = store
            .inner
            .put_from(crate::local::MANIFEST_KEY, &mut c, body_len, None)
            .unwrap();
        let etag = put.etag.clone().unwrap();
        // Meta claims the CURRENT etag; the cached BODY is garbage (rot).
        let meta = CacheMeta {
            remote_etag: Some(etag.clone()),
            fetched_at_ms: 1,
            source_key: crate::local::MANIFEST_KEY.to_string(),
            body_fnv: body_fingerprint(b"garbage body"),
        };
        write_cache_files(b"garbage body", &meta, &cache).unwrap();
        let inv = load_remote_inventory(&store, InventoryMode::Auto, Some(&cache)).unwrap();
        assert_eq!(
            inv.base.source,
            InventorySource::Manifest {
                remote_etag: Some(etag.clone())
            }
        );
        // The garbage cached body was invalidated and the cache re-filled
        // with the fresh (valid) body.
        let (cached_body, _m) = read_cache_body(&cache).expect("cache healed");
        let m = crate::manifest::parse_manifest_bytes(&cached_body).unwrap();
        assert_eq!(m.entry_count, 1);
        assert_eq!(m.entries[0].key, "a.md");
    }

    #[test]
    fn cache_body_mismatched_fingerprint_is_invalidated_not_served() {
        // W259 (N3, review 5472033449): the 304 path must prove a cached
        // body belongs with its meta - a body whose fingerprint does not
        // match (body/meta crash window, rot, tamper) is never planned from;
        // the load invalidates and fresh-fetches, then heals the pair. RED
        // today: no fingerprint field, the stale body is served as if it were
        // the current manifest.
        let dir = crate::testutil::TempDir::new("vaultsync-cache-test");
        let cache = CachePaths::new(dir.path());
        let store = CountingGetStore::new();
        let current = manifest_body(&[("a.md", 3, Some(100))]);
        let body_len = current.len() as u64;
        let mut c = std::io::Cursor::new(current.clone());
        let put = store
            .inner
            .put_from(crate::local::MANIFEST_KEY, &mut c, body_len, None)
            .unwrap();
        let etag = put.etag.clone().unwrap();
        // Manual mismatch: the meta claims the CURRENT etag + the CURRENT
        // body's fingerprint, but the body FILE holds a STALE (different,
        // valid) manifest - a pair that only a partial write could produce.
        let stale = manifest_body(&[("stale.md", 1, None)]);
        let meta = CacheMeta {
            remote_etag: Some(etag.clone()),
            fetched_at_ms: 1,
            source_key: crate::local::MANIFEST_KEY.to_string(),
            body_fnv: body_fingerprint(&current),
        };
        std::fs::create_dir_all(cache.body.parent().unwrap()).unwrap();
        std::fs::write(&cache.body, &stale).unwrap();
        std::fs::write(&cache.meta, serde_json::to_vec(&meta).unwrap()).unwrap();
        let inv = load_remote_inventory(&store, InventoryMode::Auto, Some(&cache)).unwrap();
        assert_eq!(
            inv.base.source,
            InventorySource::Manifest {
                remote_etag: Some(etag.clone())
            }
        );
        // Planned from the CURRENT body (a.md), never the stale one.
        let keys: Vec<&str> = inv
            .base
            .file_entities
            .iter()
            .map(|e| e.key.as_str())
            .collect();
        assert_eq!(keys, vec!["a.md"], "stale body must not be served");
        // Cache healed: body + meta now agree (fingerprint of the current
        // body), so the NEXT load is a plain 304 no-re-download.
        let (cached, _m) = read_cache_body(&cache).expect("cache healed");
        let m = crate::manifest::parse_manifest_bytes(&cached).unwrap();
        assert_eq!(m.entries[0].key, "a.md");
        assert_eq!(
            body_fingerprint(&cached),
            read_cache_meta(&cache).unwrap().body_fnv,
            "cache pair must be consistent after healing"
        );
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
            body_fnv: body_fingerprint(&body),
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
        let err = load_remote_inventory(&failing, InventoryMode::Auto, Some(&cache)).unwrap_err();
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
        assert_eq!(cached.1.entry_count, 1);
        assert_eq!(cached.1.entries[0].key, "a.md");

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
        assert_eq!(cached.1.entry_count, 2);
    }

    /// Thin store counting `put_from_with` calls per key (issue 48 W268 /
    /// W290 put-counter pin: assert B1 did/did not put MANIFEST_KEY).
    struct PutCounterStore {
        inner: MemoryStore,
        puts: std::sync::atomic::AtomicUsize,
    }
    impl PutCounterStore {
        fn new() -> Self {
            Self {
                inner: MemoryStore::new(),
                puts: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn manifest_puts(&self) -> usize {
            self.puts.load(std::sync::atomic::Ordering::Relaxed)
        }
    }
    impl ObjectStore for PutCounterStore {
        fn list(&self, prefix: &str) -> Result<crate::store::Listing, Error> {
            self.inner.list(prefix)
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
            if key == crate::local::MANIFEST_KEY {
                self.puts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            self.inner.put_from(key, r, size, mtime_ms)
        }
        fn put_from_with(
            &self,
            key: &str,
            r: &mut dyn std::io::Read,
            size: u64,
            opts: crate::store::PutOpts,
        ) -> Result<Entity, Error> {
            if key == crate::local::MANIFEST_KEY {
                self.puts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            self.inner.put_from_with(key, r, size, opts)
        }
        fn delete(&self, key: &str) -> Result<(), Error> {
            self.inner.delete(key)
        }
    }

    fn cold_base(files: Vec<Entity>) -> InventoryBase {
        InventoryBase {
            source: InventorySource::LiveListHead,
            file_entities: files,
            manifest_etag: None,
        }
    }

    /// W303 mini store: MANIFEST_KEY head answers an oversized present object
    /// (etag Some, size above the soft cap); `get_to` on it is COUNTED so a
    /// test can prove the H1-V validate path never streams an over-cap body.
    struct OversizeHeadStore {
        inner: MemoryStore,
        gets: std::sync::atomic::AtomicUsize,
    }
    impl OversizeHeadStore {
        fn new() -> Self {
            Self {
                inner: MemoryStore::new(),
                gets: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn gets(&self) -> usize {
            self.gets.load(std::sync::atomic::Ordering::Relaxed)
        }
    }
    impl ObjectStore for OversizeHeadStore {
        fn list(&self, prefix: &str) -> Result<crate::store::Listing, Error> {
            self.inner.list(prefix)
        }
        fn head(&self, key: &str) -> Result<Entity, Error> {
            if key == crate::local::MANIFEST_KEY {
                return Ok(Entity {
                    key: crate::local::MANIFEST_KEY.to_string(),
                    size: crate::manifest::MANIFEST_MAX_BYTES + 1,
                    mtime_ms: None,
                    etag: Some("oversize-etag".to_string()),
                });
            }
            self.inner.head(key)
        }
        fn get_to(&self, key: &str, w: &mut dyn std::io::Write) -> Result<Entity, Error> {
            if key == crate::local::MANIFEST_KEY {
                self.gets.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
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
    fn ensure_written_empty_create() {
        // W264 (issue 48, S3): a cold EMPTY base publishes a 0-entry
        // manifest via If-None-Match: * (warm empty baseline, IQ-empty); the
        // live body parses, entry_count == 0, and the outcome carries an
        // etag (mock backends report one).
        let store = PutCounterStore::new();
        let base = cold_base(Vec::new());
        let out = ensure_remote_manifest(&store, &base, None).unwrap();
        let (etag, count) = match out {
            EnsureOutcome::Written { etag, entry_count } => (etag, entry_count),
            other => panic!("expected Written, got {other:?}"),
        };
        assert!(etag.is_some());
        assert_eq!(count, 0);
        let mut buf = Vec::new();
        store.get_to(crate::local::MANIFEST_KEY, &mut buf).unwrap();
        let m = crate::manifest::parse_manifest_bytes(&buf).unwrap();
        assert_eq!(m.entry_count, 0);
        assert!(m.entries.is_empty());
        assert_eq!(store.manifest_puts(), 1);
    }

    #[test]
    fn ensure_written_non_empty_entry_count() {
        // W294 (issue 48, F5 / A23): the Written line's N must equal the
        // real file count of the base (non-empty).
        let store = PutCounterStore::new();
        let base = cold_base(vec![
            crate::entity::file("a.md", 1, None),
            crate::entity::file("b.md", 2, None),
        ]);
        let out = ensure_remote_manifest(&store, &base, None).unwrap();
        let (etag, count) = match out {
            EnsureOutcome::Written { etag, entry_count } => (etag, entry_count),
            other => panic!("expected Written, got {other:?}"),
        };
        assert!(etag.is_some());
        assert_eq!(count, 2);
        let mut buf = Vec::new();
        store.get_to(crate::local::MANIFEST_KEY, &mut buf).unwrap();
        let m = crate::manifest::parse_manifest_bytes(&buf).unwrap();
        assert_eq!(m.entry_count, 2);
        let keys: Vec<&str> = m.entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["a.md", "b.md"]);
    }

    #[test]
    fn ensure_heals_corrupt_present_and_next_load_warm() {
        // W265 (issue 48, H1-V invalid branch): a present-but-corrupt body
        // under a cold base is healed via If-Match overwrite with the base's
        // file set; afterwards the load is WARM (a NoListStore no longer
        // lists - the healed manifest is planning authority). W265 also
        // mutation-checks: returning Adopted on a corrupt body would leave
        // load failing.
        let store = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"not a manifest".to_vec());
        store
            .put_from(
                crate::local::MANIFEST_KEY,
                &mut c,
                "not a manifest".len() as u64,
                None,
            )
            .unwrap();
        let base = cold_base(vec![crate::entity::file("a.md", 3, Some(1))]);
        let out = ensure_remote_manifest(&store, &base, None).unwrap();
        let (etag, count) = match out {
            EnsureOutcome::Written { etag, entry_count } => (etag, entry_count),
            other => panic!("expected Written, got {other:?}"),
        };
        assert!(etag.is_some());
        assert_eq!(count, 1);
        let mut buf = Vec::new();
        store.get_to(crate::local::MANIFEST_KEY, &mut buf).unwrap();
        let m = crate::manifest::parse_manifest_bytes(&buf).unwrap();
        assert_eq!(m.entry_count, 1);
        assert_eq!(m.entries[0].key, "a.md");
        // Next load is warm on a no-list store (healed manifest is authority).
        let nolist = NoListStore {
            inner: MemoryStore::new(),
        };
        // copy the healed body into the nolist's inner store
        let mut c = std::io::Cursor::new(buf.clone());
        nolist
            .inner
            .put_from(crate::local::MANIFEST_KEY, &mut c, buf.len() as u64, None)
            .unwrap();
        let inv = load_remote_inventory(&nolist, InventoryMode::Auto, None).unwrap();
        assert!(
            matches!(inv.base.source, InventorySource::Manifest { .. }),
            "healed manifest must be warm authority, got {:?}",
            inv.base.source
        );
    }

    #[test]
    fn ensure_adopts_concurrent_valid_with_zero_puts() {
        // W268 (issue 48, F1 / D-h1v valid branch): a concurrent-valid live
        // manifest is ADOPTED - no put (counter 0), the live body unchanged,
        // and the outcome carries the live etag + live entry count. Mutation-
        // check: if B1 still overwrote a valid present, puts would be 1 and
        // b.md would be lost.
        let store = PutCounterStore::new();
        let their_body = manifest_body(&[("b.md", 1, None)]);
        let blen = their_body.len() as u64;
        let mut c = std::io::Cursor::new(their_body);
        let live_etag = store
            .inner
            .put_from(crate::local::MANIFEST_KEY, &mut c, blen, None)
            .unwrap()
            .etag;
        // Cold base is STALE (has a, lacks b).
        let base = cold_base(vec![crate::entity::file("a.md", 5, Some(5))]);
        let out = ensure_remote_manifest(&store, &base, None).unwrap();
        let (etag, files, count) = match out {
            EnsureOutcome::Adopted {
                etag,
                files,
                entry_count,
            } => (etag, files, entry_count),
            other => panic!("expected Adopted, got {other:?}"),
        };
        assert_eq!(etag, live_etag.unwrap());
        assert_eq!(count, 1);
        let adopted_keys: Vec<&str> = files.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(
            adopted_keys,
            vec!["b.md"],
            "adopt carries the winner file set"
        );
        assert_eq!(store.manifest_puts(), 0, "adopt must not put");
        // Live body unchanged: still only b.md.
        let mut buf = Vec::new();
        store.get_to(crate::local::MANIFEST_KEY, &mut buf).unwrap();
        let m = crate::manifest::parse_manifest_bytes(&buf).unwrap();
        let keys: Vec<&str> = m.entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["b.md"]);
    }

    #[test]
    fn commit_after_adopted_refresh_keeps_their_untouched_keys() {
        // W299/W300 (PR50-r1 H1, review 5476323432): after B1 ADOPTS a
        // concurrent-valid manifest, the warm refresh must install the
        // ADOPTED file set so the final commit's warm fold preserves the
        // winner's untouched keys (c.md) instead of clobbering them with the
        // stale cold list snapshot. RED: today the Adopted outcome carries no
        // file set and refresh only warms etag/source, so apply([a],
        // [Upload(a')]) writes {a.md} and c.md is lost.
        let store = MemoryStore::new();
        // Live manifest authored by a concurrent winner: {a.md, c.md}. c.md
        // exists ONLY in the manifest (never surfaces in list+head).
        let their_body = manifest_body(&[("a.md", 1, None), ("c.md", 2, None)]);
        let blen = their_body.len() as u64;
        let mut c = std::io::Cursor::new(their_body);
        let their_etag = store
            .put_from(crate::local::MANIFEST_KEY, &mut c, blen, None)
            .unwrap()
            .etag;
        // Cold base is STALE: list+head saw only a.md (c.md is manifest-only).
        let mut base = cold_base(vec![crate::entity::file("a.md", 5, Some(5))]);
        // B1 adopts the concurrent-valid manifest (W268); the outcome must
        // carry the parsed winner file set for a complete warm refresh.
        let out = ensure_remote_manifest(&store, &base, None).unwrap();
        let entry_count = match &out {
            EnsureOutcome::Adopted { entry_count, .. } => *entry_count,
            other => panic!("expected Adopted, got {other:?}"),
        };
        assert_eq!(entry_count, 2);
        // W313 (M1): route the library pin through the PRODUCTION
        // apply_ensure_outcome (by value, no hand-rolled source/etag/files
        // install) so this pin cannot drift from the real warm refresh that
        // installs the complete adopted snapshot.
        let apply = apply_ensure_outcome(&mut base, out).unwrap();
        assert_eq!(apply, EnsureApply::Adopted { entry_count: 2 });
        assert_eq!(base.manifest_etag, their_etag);
        // Push uploads a newer a.md; c.md is untouched by this run.
        let upload = crate::entity::file("a.md", 6, Some(6));
        let out = commit_manifest(&store, &base, &[CommitMutation::Upload(upload)], None).unwrap();
        let (_, count) = match out {
            CommitOutcome::Written { etag, entry_count } => (etag, entry_count),
            other => panic!("expected Written, got {other:?}"),
        };
        assert_eq!(count, 2);
        // The winner's untouched c.md must SURVIVE the final commit.
        let mut buf = Vec::new();
        store.get_to(crate::local::MANIFEST_KEY, &mut buf).unwrap();
        let m = crate::manifest::parse_manifest_bytes(&buf).unwrap();
        let keys: Vec<&str> = m.entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["a.md", "c.md"]);
        // W313 (M1): the fold must ALSO assert our Upload won on the touched
        // key - not just that the winner's untouched key survived. a.md's
        // size/mtime come from the Upload entity (size 6, mtime 6), not the
        // winner's stale size 1.
        let a = m.entries.iter().find(|e| e.key == "a.md").unwrap();
        assert_eq!(a.size, 6, "our Upload must win on the touched key (size)");
        assert_eq!(
            a.mtime_ms,
            Some(6),
            "our Upload must win on the touched key (mtime)"
        );
    }

    #[test]
    fn resolve_head_get_etag_mismatch_fails_closed() {
        // W316 (PR50-r2 M2, review 5476798257): H1-V must condition on the
        // GET entity etag when the validate probe returns one, and a HEAD
        // etag that DIFFERS from the GET etag means the live object changed
        // between HEAD and GET (another writer) - fail CLOSED (Err), never
        // Adopt/If-Match a mixed pair. RED: today the code discards the GET
        // etag and Adopts with the HEAD etag, so head="etag-head"/
        // get="etag-get" returns Ok(Adopt) instead of Err.
        struct MixedEtagStore {
            inner: MemoryStore,
        }
        impl ObjectStore for MixedEtagStore {
            fn list(&self, prefix: &str) -> Result<crate::store::Listing, Error> {
                self.inner.list(prefix)
            }
            fn head(&self, key: &str) -> Result<crate::entity::Entity, Error> {
                if key == crate::local::MANIFEST_KEY {
                    return Ok(crate::entity::Entity {
                        key: key.to_string(),
                        size: 100,
                        mtime_ms: None,
                        etag: Some("etag-head".to_string()),
                    });
                }
                self.inner.head(key)
            }
            fn get_to(
                &self,
                key: &str,
                w: &mut dyn std::io::Write,
            ) -> Result<crate::entity::Entity, Error> {
                if key == crate::local::MANIFEST_KEY {
                    let body = manifest_body(&[("a.md", 1, None)]);
                    std::io::Write::write_all(w, &body).unwrap();
                    return Ok(crate::entity::Entity {
                        key: key.to_string(),
                        size: body.len() as u64,
                        mtime_ms: None,
                        etag: Some("etag-get".to_string()),
                    });
                }
                self.inner.get_to(key, w)
            }
            fn put_from(
                &self,
                key: &str,
                r: &mut dyn std::io::Read,
                size: u64,
                mtime_ms: Option<u64>,
            ) -> Result<crate::entity::Entity, Error> {
                self.inner.put_from(key, r, size, mtime_ms)
            }
            fn delete(&self, key: &str) -> Result<(), Error> {
                self.inner.delete(key)
            }
        }

        let store = MixedEtagStore {
            inner: MemoryStore::new(),
        };
        let base_files = vec![crate::entity::file("a.md", 1, None)];
        let err = resolve_cold_put_plan(&store, &base_files, None).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("etag mismatch"),
            "HEAD/GET etag mismatch must fail closed: {msg}"
        );
    }

    #[test]
    fn resolve_uses_get_etag_when_present() {
        // W317 (PR50-r2 M2): when the HEAD and GET etags agree, H1-V
        // conditions on the GET entity etag (the generation we validated),
        // not the stale HEAD token. Equal pair => Adopt carries the shared
        // etag. characterization: GREEN on arrival (HEAD==GET here).
        struct EqualEtagStore {
            inner: MemoryStore,
        }
        impl ObjectStore for EqualEtagStore {
            fn list(&self, prefix: &str) -> Result<crate::store::Listing, Error> {
                self.inner.list(prefix)
            }
            fn head(&self, key: &str) -> Result<crate::entity::Entity, Error> {
                if key == crate::local::MANIFEST_KEY {
                    return Ok(crate::entity::Entity {
                        key: key.to_string(),
                        size: 100,
                        mtime_ms: None,
                        etag: Some("etag-x".to_string()),
                    });
                }
                self.inner.head(key)
            }
            fn get_to(
                &self,
                key: &str,
                w: &mut dyn std::io::Write,
            ) -> Result<crate::entity::Entity, Error> {
                if key == crate::local::MANIFEST_KEY {
                    let body = manifest_body(&[("a.md", 1, None)]);
                    std::io::Write::write_all(w, &body).unwrap();
                    return Ok(crate::entity::Entity {
                        key: key.to_string(),
                        size: body.len() as u64,
                        mtime_ms: None,
                        etag: Some("etag-x".to_string()),
                    });
                }
                self.inner.get_to(key, w)
            }
            fn put_from(
                &self,
                key: &str,
                r: &mut dyn std::io::Read,
                size: u64,
                mtime_ms: Option<u64>,
            ) -> Result<crate::entity::Entity, Error> {
                self.inner.put_from(key, r, size, mtime_ms)
            }
            fn delete(&self, key: &str) -> Result<(), Error> {
                self.inner.delete(key)
            }
        }

        let store = EqualEtagStore {
            inner: MemoryStore::new(),
        };
        let base_files = vec![crate::entity::file("a.md", 1, None)];
        match resolve_cold_put_plan(&store, &base_files, None).unwrap() {
            ColdPlan::Adopt {
                etag, entry_count, ..
            } => {
                assert_eq!(etag, "etag-x");
                assert_eq!(entry_count, 1);
            }
            other => panic!("expected Adopt with GET etag, got {other:?}"),
        }
    }

    #[test]
    fn resolve_or_ensure_oversize_present_skips_body_get() {
        // W303 (PR50-r1 F1, review 5476323432): a present object whose HEAD
        // size already exceeds the soft cap is unusable - the H1-V resolve
        // must plan an Overwrite heal via If-Match WITHOUT GETting the
        // (potentially huge) body. RED: today the present+etag branch always
        // GETs (get counter > 0) and then fails on the vanished manifest.
        let store = OversizeHeadStore::new();
        let base_files = vec![crate::entity::file("a.md", 3, Some(1))];
        let plan = resolve_cold_put_plan(&store, &base_files, None).unwrap();
        assert_eq!(store.gets(), 0, "must not stream an over-cap body");
        match plan {
            ColdPlan::Overwrite { files, etag } => {
                // Heal with the base file set, If-Match the live etag.
                assert_eq!(etag, "oversize-etag");
                let keys: Vec<&str> = files.iter().map(|e| e.key.as_str()).collect();
                assert_eq!(keys, vec!["a.md"]);
            }
            other => panic!("expected Overwrite heal, got {other:?}"),
        }
    }

    #[test]
    fn ensure_precondition_failed_is_race_as_data() {
        // W266 (issue 48, F5 honesty note): a lost conditional race answers
        // EnsureOutcome::PreconditionFailed (a value, not Err), clobbers
        // nothing (pre-seeded body unchanged), and fills NO cache even when
        // Some(cache) is passed. Single-store simulation, not two tasks
        // (comment pin).
        struct FailCondStore {
            inner: MemoryStore,
        }
        impl ObjectStore for FailCondStore {
            fn list(&self, prefix: &str) -> Result<crate::store::Listing, Error> {
                self.inner.list(prefix)
            }
            fn head(&self, key: &str) -> Result<Entity, Error> {
                // W266 simulated race: B1's HEAD sees ABSENT (so resolve
                // chooses Create), while a concurrent writer's body ALREADY
                // sits behind it - the later If-None-Match: * create must
                // lose the race (return PreconditionFailed) and clobber
                // nothing.
                if key == crate::local::MANIFEST_KEY {
                    return Err(Error::NotFound(key.to_string()));
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
            fn put_from_with(
                &self,
                _key: &str,
                _r: &mut dyn std::io::Read,
                _size: u64,
                opts: crate::store::PutOpts,
            ) -> Result<Entity, Error> {
                if opts.if_none_match_star || opts.if_match_etag.is_some() {
                    return Err(Error::PreconditionFailed("lost race".to_string()));
                }
                self.inner.put_from(_key, _r, _size, opts.mtime_ms)
            }
            fn delete(&self, key: &str) -> Result<(), Error> {
                self.inner.delete(key)
            }
        }
        // W266 simulated race (not two ensure_remote_manifest tasks).
        let dir = crate::testutil::TempDir::new("vaultsync-cache-test");
        let cache = CachePaths::new(dir.path());
        let store = FailCondStore {
            inner: MemoryStore::new(),
        };
        // Pre-seed an existing manifest so the create path loses the
        // If-None-Match: * race.
        let seed = manifest_body(&[("x.md", 1, None)]);
        let slen = seed.len() as u64;
        let mut c = std::io::Cursor::new(seed);
        store
            .inner
            .put_from(crate::local::MANIFEST_KEY, &mut c, slen, None)
            .unwrap();
        let base = cold_base(Vec::new());
        let out = ensure_remote_manifest(&store, &base, Some(&cache)).unwrap();
        assert_eq!(out, EnsureOutcome::PreconditionFailed);
        // Pre-seeded body unchanged (no clobber).
        let mut buf = Vec::new();
        store.get_to(crate::local::MANIFEST_KEY, &mut buf).unwrap();
        let m = crate::manifest::parse_manifest_bytes(&buf).unwrap();
        assert_eq!(m.entry_count, 1);
        assert_eq!(m.entries[0].key, "x.md");
        // No cache written on the lost race.
        assert!(!cache.body.exists(), "no cache body on PreconditionFailed");
    }

    #[test]
    fn ensure_fills_cache_on_written_not_adopted() {
        // W267 (issue 48, D-cache): on `Written` the local mirror is filled;
        // on `Adopted` no cache files are written (deferred in v1; the next
        // warm load fetches/304s).
        let dir = crate::testutil::TempDir::new("vaultsync-cache-test");
        let cache = CachePaths::new(dir.path());
        let store = MemoryStore::new();
        // Written: warm empty base published + cache filled.
        let base = cold_base(vec![crate::entity::file("a.md", 2, Some(1))]);
        let out = ensure_remote_manifest(&store, &base, Some(&cache)).unwrap();
        assert!(matches!(out, EnsureOutcome::Written { .. }));
        assert!(cache.body.exists(), "cache body written on Written");
        assert_eq!(
            read_cache_meta(&cache).unwrap().remote_etag,
            match out {
                EnsureOutcome::Written { etag, .. } => etag,
                _ => unreachable!(),
            }
        );
        // Adopted: clear the cache dir, then ensure a concurrent-valid
        // manifest is adopted - no NEW cache files appear.
        std::fs::remove_file(&cache.body).unwrap();
        std::fs::remove_file(&cache.meta).unwrap();
        let their_body = manifest_body(&[("b.md", 1, None)]);
        let blen = their_body.len() as u64;
        let mut c = std::io::Cursor::new(their_body);
        store
            .put_from(crate::local::MANIFEST_KEY, &mut c, blen, None)
            .unwrap();
        let base2 = cold_base(vec![crate::entity::file("c.md", 3, Some(3))]);
        let out = ensure_remote_manifest(&store, &base2, Some(&cache)).unwrap();
        assert!(matches!(out, EnsureOutcome::Adopted { .. }));
        assert!(
            !cache.body.exists(),
            "no cache body on Adopted (D-cache: no fill on adopt)"
        );
    }

    #[test]
    fn apply_ensure_outcome_adopted_replaces_file_entities() {
        // W301 (PR50-r1 H1/F3, review 5476323432): after an Adopted refresh,
        // the base's file set is REPLACED by the winner snapshot (complete),
        // not left stale, and source/etag warm. Characterization pin.
        let mut base = cold_base(vec![crate::entity::file("a.md", 1, None)]);
        let outcome = EnsureOutcome::Adopted {
            etag: "etag-x".to_string(),
            files: vec![
                crate::entity::file("a.md", 1, None),
                crate::entity::file("c.md", 2, None),
            ],
            entry_count: 2,
        };
        let apply = apply_ensure_outcome(&mut base, outcome).unwrap();
        assert_eq!(apply, EnsureApply::Adopted { entry_count: 2 });
        let keys: Vec<&str> = base.file_entities.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["a.md", "c.md"], "adopted snapshot installed");
        assert_eq!(base.manifest_etag.as_deref(), Some("etag-x"));
        assert_eq!(
            base.source,
            InventorySource::Manifest {
                remote_etag: Some("etag-x".to_string())
            }
        );
    }

    #[test]
    fn apply_ensure_outcome_written_preserves_file_entities() {
        // W301 (PR50-r1 H1/F3): after a Written refresh the base set is
        // UNCHANGED (it already equals the body we wrote); only source/etag
        // warm. Characterization pin.
        let mut base = cold_base(vec![
            crate::entity::file("a.md", 1, None),
            crate::entity::file("b.md", 2, None),
        ]);
        let outcome = EnsureOutcome::Written {
            etag: Some("etag-y".to_string()),
            entry_count: 2,
        };
        let apply = apply_ensure_outcome(&mut base, outcome).unwrap();
        assert_eq!(apply, EnsureApply::Written { entry_count: 2 });
        let keys: Vec<&str> = base.file_entities.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["a.md", "b.md"],
            "written keeps existing file set"
        );
        assert_eq!(base.manifest_etag.as_deref(), Some("etag-y"));
    }

    #[test]
    fn apply_ensure_outcome_precondition_failed_is_none() {
        // W301 (PR50-r1 H1/F3): PreconditionFailed is a non-apply - the
        // helper returns None and leaves the cold base untouched (callers
        // decide policy: push aborts, status exits 1).
        let mut base = cold_base(vec![crate::entity::file("a.md", 1, None)]);
        assert!(apply_ensure_outcome(&mut base, EnsureOutcome::PreconditionFailed).is_none());
        assert_eq!(base.source, InventorySource::LiveListHead);
        assert!(base.manifest_etag.is_none());
    }
}
