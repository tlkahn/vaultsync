//! vaultsync library core.
//!
//! Phase 1 modules: `entity`, `plan`, `local`, `store`.

pub mod cli;
pub mod config;
pub mod entity;
pub mod error;
pub mod exec;
pub mod local;
pub mod plan;
pub(crate) mod pool;
pub mod store;

use std::path::Path;

use crate::entity::Entity;
use crate::error::Error;
use crate::local::LocalFs;
use crate::plan::{ActionKind, Mode, Plan, PlanOpts};
use crate::store::ObjectStore;

/// A plan plus the advisory warnings surfaced while building it (dropped
/// remote keys from the store listing, reserved-namespace leftovers). The CLI
/// prints `warnings` (one `warning: ...` line each); library consumers may
/// inspect or ignore them. A struct, not a tuple, so Phase 3 fields extend
/// without another signature break (H1/W99).
#[derive(Debug, Clone)]
pub struct PlanReport {
    /// The computed plan.
    pub plan: Plan,
    /// Advisory warnings about the inputs, printed by the CLI layer.
    pub warnings: Vec<String>,
}

/// Build a [`Plan`] from a local walk + a store listing.
///
/// Remote list keys are validated with `ensure_valid_key` before planning
/// (fail closed): an escaping or control-char key from the store never becomes
/// a planned action. `plan()` itself stays pure (fixtures may feed it
/// anything).
pub fn build_plan(
    local: &LocalFs,
    store: &dyn ObjectStore,
    mode: Mode,
    opts: &PlanOpts,
) -> Result<PlanReport, Error> {
    let (local_entities, walk_report) = local.list_report()?;
    // H1 (W99): the store listing carries its own warnings (e.g. dropped
    // non-empty `*/` keys) in `Listing.warnings`; `build_plan` aggregates
    // them with its own into `PlanReport.warnings` for the CLI to print -
    // library code must not write to process stderr.
    let listing = store.list("")?;
    let mut warnings = listing.warnings;
    let remote_entities = listing.entities;
    // R4-M2: drop a remote empty key (the exact-prefix folder marker stripped
    // to `""`) before validation. W34 removes it at the S3 backend source, but
    // other backends could still surface one; an empty key is never a planned
    // action. Every *other* invalid key stays fail-closed (R5-L1).
    let remote_entities: Vec<_> = remote_entities
        .into_iter()
        .filter(|e| !e.key.is_empty())
        .collect();
    // W79/r9 L1: the reserved-namespace filter (W63/A-L3, R4-L4/W42 + W54/
    // A-L2) is factored into a pure, unit-testable partition so the dropped
    // keys can be counted and surfaced instead of vanishing silently. A
    // crashed `check` (SIGKILL between probe put and delete) can leave a
    // `.vaultsync-check-*` object remotely, and a tmp-sibling key
    // (`.name.vaultsync-tmp-*`) can reach the store out-of-band; neither must
    // ever plan a Download (which would materialize a reserved dotfile
    // locally). Users must not create such keys (object-store.md reserved
    // namespace), and now every run that encounters a leftover says so.
    let (remote_entities, reserved_dropped) = partition_reserved_remote_keys(remote_entities);
    if !reserved_dropped.is_empty() {
        warnings.push(reserved_drops_warning(&reserved_dropped));
    }
    for e in &remote_entities {
        crate::entity::ensure_valid_key(&e.key)?;
    }
    let mut p = plan::plan(&local_entities, &remote_entities, mode, opts);
    // 4c: case-only-collision preflight overrides the affected rows to
    // Conflict `case_collision` (never auto-paired with a differently-cased
    // sibling).
    let collided = plan::case_collision_keys(&local_entities, &remote_entities);
    if !collided.is_empty() {
        for a in &mut p.actions {
            // W31/N5: skip rows already diagnosed as a *path* collision - the
            // type collision (file vs folder) is the more precise reason and
            // folding an exact-case file/folder pair to the same value must
            // not relabel it `case_collision`.
            if collided.contains(&a.key) && a.reason != plan::reason::PATH_COLLISION {
                a.kind = plan::ActionKind::Conflict;
                a.reason = plan::reason::CASE_COLLISION;
            }
        }
        p.stats = compute_stats(&p.actions);
    }
    // R4-M1/W38 + W51 (A-M2/B-M1): --follow-symlinks is inventory-only in
    // v1. In mutating modes, a row whose key came from a followed *file*
    // symlink is overridden to Skip(followed_symlink) - the executor refuses
    // to open a symlink, so Push/Pull must never plan a transfer for it, and
    // a pull --delete must never unlink the link (the guarded delete refuses
    // symlink leaves as its fail-closed guard for *unplanned* swaps). Status
    // keeps the rows (inventory visible). Dir-symlink children are unaffected
    // - they transfer fine. DeleteRemote needs no arm: a key in
    // `followed_files` is local by construction, so it can never be a
    // remote-only delete.
    if mode != Mode::Status && !walk_report.followed_files.is_empty() {
        for a in &mut p.actions {
            if matches!(
                a.kind,
                plan::ActionKind::Upload
                    | plan::ActionKind::Download
                    | plan::ActionKind::DeleteLocal
            ) && walk_report.followed_files.contains(&a.key)
            {
                a.kind = plan::ActionKind::Skip;
                a.reason = plan::reason::FOLLOWED_SYMLINK;
            }
        }
        p.stats = compute_stats(&p.actions);
    }
    Ok(PlanReport { plan: p, warnings })
}

/// Split remote entities into `(kept, dropped)` by the reserved vaultsync
/// namespace filter (W63/A-L3 + W79/r9 L1): a tmp-sibling key
/// (`.name.vaultsync-tmp-*`) or a `.vaultsync-check-*` probe leftover on the
/// remote is never planned. Pure and unit-testable offline; the warning
/// side effect over the `dropped` list lives in [`reserved_drops_warning`]
/// (surfaced via `PlanReport.warnings`, not capture-tested, same precedent
/// as W70). Both output lists preserve the input order. `pub(crate)` so
/// `S3Store::list` partitions reserved keys out *before* issuing any head
/// (W118) - no wasted requests and no fail-closed scope creep over junk.
pub(crate) fn partition_reserved_remote_keys(entities: Vec<Entity>) -> (Vec<Entity>, Vec<Entity>) {
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for e in entities {
        // W109/L4: a folder-form key's reserved segment sits behind the
        // trailing `/` (`rsplit('/').next()` on `.vaultsync-check-1/` yields
        // the empty string); strip one trailing `/` first, same shape as
        // `fold_key`, so the filter's stated final-segment policy also holds
        // for folder-shaped keys.
        let reserved = e
            .key
            .strip_suffix('/')
            .unwrap_or(&e.key)
            .rsplit('/')
            .next()
            .is_some_and(crate::local::is_reserved_vaultsync_key_name);
        if reserved {
            dropped.push(e);
        } else {
            kept.push(e);
        }
    }
    (kept, dropped)
}

/// The W70-style one-line warning ("surface, don't hide") for a list of
/// reserved-namespace leftovers. Names bounded (first 5 + "and N more") so a
/// pathological namespace can't flood stderr. Single source of truth shared
/// by `build_plan` and `S3Store::list` (W118) so the text is identical
/// wherever it fires; `S3Store::list` emits it once (store side) so
/// `build_plan`'s partition no longer re-fires for S3.
pub(crate) fn reserved_drops_warning(dropped: &[Entity]) -> String {
    let names: Vec<String> = dropped.iter().map(|e| e.key.clone()).collect();
    let shown = names.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
    let more = names.len().saturating_sub(5);
    let suffix = if more > 0 {
        format!(" and {more} more")
    } else {
        String::new()
    };
    format!(
        "ignoring {} remote object(s) under the reserved vaultsync namespace: {shown}{suffix}",
        names.len()
    )
}

fn compute_stats(actions: &[plan::Action]) -> plan::PlanStats {
    use plan::ActionKind::*;
    let mut s = plan::PlanStats::default();
    for a in actions {
        match a.kind {
            Upload => s.upload += 1,
            Download => s.download += 1,
            DeleteLocal => s.delete_local += 1,
            DeleteRemote => s.delete_remote += 1,
            Skip => s.skip += 1,
            Conflict => s.conflict += 1,
        }
    }
    s
}

/// Build a [`Plan`] against a store for a real vault directory (Status mode).
pub fn status_with_store(
    vault: &Path,
    store: &dyn ObjectStore,
    opts: &PlanOpts,
) -> Result<Plan, Error> {
    let local = LocalFs::new(vault);
    build_plan(&local, store, Mode::Status, opts).map(|report| report.plan)
}

/// Connectivity probe: write a tiny probe object, read it back, delete it.
/// Succeeds only if put + get + delete all round-trip (lock: probe failure is
/// a failure in Slice 8 - no head-bucket-only fallback).
pub fn check_store(store: &dyn ObjectStore) -> Result<(), Error> {
    let key = probe_key();
    let body: &[u8] = b"vaultsync-connectivity-probe";
    let mut writer = std::io::Cursor::new(body.to_vec());
    let _ = store.put_from(&key, &mut writer, body.len() as u64, None)?;
    // W24/M2: after a successful put, every exit path attempts to delete the
    // probe, so a failed/partial check never leaves `.vaultsync-check-*`
    // litter (which would otherwise list as a remote_only row later). A delete
    // failure on an already-erroring path is secondary context, never masking
    // the primary error; a delete failure on the success path remains the
    // returned error.
    match (|| -> Result<(), Error> {
        let mut buf = Vec::new();
        let ent = store.get_to(&key, &mut buf)?;
        if buf != body || ent.size != body.len() as u64 {
            return Err(Error::Other("check: read-back mismatch".to_string()));
        }
        Ok(())
    })() {
        Ok(()) => {
            store.delete(&key)?;
            Ok(())
        }
        Err(primary) => {
            if let Err(de) = store.delete(&key) {
                return Err(Error::Other(format!(
                    "{primary} (also failed to remove probe {key}: {de})"
                )));
            }
            Err(primary)
        }
    }
}

/// The vault-relative probe key used by `check`. Predictable names (pid-only)
/// could clobber a user object; a per-process counter + sub-second nano
/// component makes each probe key unique (W11/A-M7). The `.vaultsync-check-`
/// prefix is documented as reserved (object-store.md, W19).
pub fn probe_key() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    format!(".vaultsync-check-{}-{}-{}", std::process::id(), n, nanos)
}

/// Format a plan as human-readable text (Phase 1 subset of [cli.md]).
pub fn format_plan_human(plan: &Plan) -> String {
    format_plan_human_verbose(plan, 0)
}

/// Format a plan; skip (S) rows are hidden unless `verbosity > 0` (R3 low).
pub fn format_plan_human_verbose(plan: &Plan, verbosity: u8) -> String {
    let s = &plan.stats;
    let mut out = String::new();
    out.push_str(&format!(
        "plan: {} upload, {} download, {} delete_local, {} delete_remote, {} skip, {} conflict\n",
        s.upload, s.download, s.delete_local, s.delete_remote, s.skip, s.conflict
    ));
    let show_skips = verbosity > 0;
    for a in &plan.actions {
        if a.kind == ActionKind::Skip && !show_skips {
            continue;
        }
        let prefix = match a.kind {
            ActionKind::Upload => "U  ",
            ActionKind::Download => "D  ",
            ActionKind::DeleteLocal => "DL ",
            ActionKind::DeleteRemote => "DR ",
            ActionKind::Skip => "S  ",
            ActionKind::Conflict => "*  ",
        };
        match a.kind {
            // W71/A-N3: Conflict AND Skip rows (the latter only visible under
            // -v) carry their planner reason - the diagnosis a debugging user
            // looks for, same shape on both row kinds.
            ActionKind::Conflict | ActionKind::Skip => {
                out.push_str(&format!("{prefix}{}    {}\n", a.key, a.reason));
            }
            _ => {
                out.push_str(&format!("{prefix}{}\n", a.key));
            }
        }
    }
    out
}

/// Library version string (mirrors the package version).
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::ops::Deref;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Unique temp dir per instance, removed on drop (std-only; no `tempfile`).
    /// Derefs to `Path` so `dir.join("x")` and `&dir -> &Path` coercion work.
    pub(crate) struct TempDir(PathBuf);

    impl TempDir {
        pub(crate) fn new(prefix: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!("{prefix}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Deref for TempDir {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Offline test double replicating issue #15's S3 behavior (W111, I15).
    ///
    /// Wraps a [`crate::store::mock::MemoryStore`]. `list` delegates to the
    /// mock but rewrites every OBJECT entity's mtime to a fixed later "upload
    /// time" and drops its etag (simulating the ListObjectsV2 `LastModified`
    /// fallback - user metadata is invisible to a listing), then runs the
    /// production [`crate::store::enrich_with_head_mtimes`] - mirroring the
    /// W113 S3 wiring so lib-level convergence tests exercise the real
    /// production path. `head`/`get_to`/`put_from`/`delete` delegate unchanged
    /// (the mock `head` reports the metadata mtime, exactly like S3).
    pub(crate) struct S3LikeListStore {
        inner: crate::store::mock::MemoryStore,
        /// Listed upload-time override applied to every object mtime (picked
        /// bigger than any real mtime so the degraded frame is unambiguous).
        upload_time_ms: u64,
        /// Every key the double's `head` delegate has served, in order.
        head_log: std::sync::Mutex<Vec<String>>,
        /// Keys whose `head` should answer `Unavailable` (W118 fail-closed
        /// scope-creep probe).
        fail_head_keys: Vec<String>,
    }

    impl S3LikeListStore {
        pub(crate) fn new() -> Self {
            S3LikeListStore {
                inner: crate::store::mock::MemoryStore::new(),
                upload_time_ms: 9_999_999_999,
                head_log: std::sync::Mutex::new(Vec::new()),
                fail_head_keys: Vec::new(),
            }
        }
        pub(crate) fn inner(&self) -> &crate::store::mock::MemoryStore {
            &self.inner
        }
        /// Keys whose `head` should answer `Unavailable` (throttle).
        pub(crate) fn fail_head(&mut self, key: &str) -> &mut Self {
            self.fail_head_keys.push(key.to_string());
            self
        }
        /// Snapshot of every key the double's `head` has served, in order.
        pub(crate) fn head_log(&self) -> Vec<String> {
            self.head_log.lock().unwrap().clone()
        }
    }

    impl crate::store::ObjectStore for S3LikeListStore {
        fn list(&self, prefix: &str) -> Result<crate::store::Listing, crate::error::Error> {
            let mut listing = self.inner.list(prefix)?;
            for e in listing.entities.iter_mut() {
                if !e.is_folder() {
                    e.mtime_ms = Some(self.upload_time_ms);
                    e.etag = None;
                }
            }
            // W118: partition reserved-namespace leftovers out before any
            // head is issued and surface the shared bounded warning, mirroring
            // `S3Store::list`'s wiring order (degrade -> partition -> enrich).
            let (entities, reserved_dropped) =
                crate::partition_reserved_remote_keys(listing.entities);
            if !reserved_dropped.is_empty() {
                listing
                    .warnings
                    .push(crate::reserved_drops_warning(&reserved_dropped));
            }
            listing.entities = entities;
            crate::store::enrich_with_head_mtimes(self, listing)
        }
        fn head(&self, key: &str) -> Result<crate::entity::Entity, crate::error::Error> {
            self.head_log.lock().unwrap().push(key.to_string());
            if self.fail_head_keys.iter().any(|k| k == key) {
                return Err(crate::error::Error::Unavailable(format!(
                    "throttled: {key}"
                )));
            }
            self.inner.head(key)
        }
        fn get_to(
            &self,
            key: &str,
            w: &mut dyn std::io::Write,
        ) -> Result<crate::entity::Entity, crate::error::Error> {
            self.inner.get_to(key, w)
        }
        fn put_from(
            &self,
            key: &str,
            r: &mut dyn std::io::Read,
            size: u64,
            mtime_ms: Option<u64>,
        ) -> Result<crate::entity::Entity, crate::error::Error> {
            self.inner.put_from(key, r, size, mtime_ms)
        }
        fn delete(&self, key: &str) -> Result<(), crate::error::Error> {
            self.inner.delete(key)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::Entity;
    use crate::plan::{ActionKind, PlanOpts};
    use crate::store::Listing;
    use crate::store::ObjectStore;
    use crate::store::mock::MemoryStore;
    use crate::testutil::TempDir;

    /// Minimal store stub whose `list` returns seeded entities; object ops
    /// always `NotFound`. Used to exercise `build_plan`'s remote ingest
    /// validation without pulling in a real store.
    struct StubStore {
        listed: Vec<Entity>,
    }

    impl ObjectStore for StubStore {
        fn list(&self, _prefix: &str) -> Result<Listing, Error> {
            Ok(Listing {
                entities: self.listed.clone(),
                warnings: Vec::new(),
            })
        }
        fn head(&self, key: &str) -> Result<Entity, Error> {
            Err(Error::NotFound(key.to_string()))
        }
        fn get_to(&self, key: &str, _w: &mut dyn std::io::Write) -> Result<Entity, Error> {
            Err(Error::NotFound(key.to_string()))
        }
        fn put_from(
            &self,
            _key: &str,
            _r: &mut dyn std::io::Read,
            _size: u64,
            _mtime_ms: Option<u64>,
        ) -> Result<Entity, Error> {
            Err(Error::Other("stub".to_string()))
        }
        fn delete(&self, key: &str) -> Result<(), Error> {
            Err(Error::NotFound(key.to_string()))
        }
    }

    #[test]
    fn partition_reserved_drops_trailing_slash_reserved_folder() {
        // W109/L4: the reserved-namespace filter is a final-segment policy, so
        // a folder-form key whose reserved segment is hidden behind the
        // trailing `/` must still be dropped. `rsplit('/').next()` on
        // `.vaultsync-check-1/` yields the empty string after the slash, so
        // the reserved segment was never seen and the folder stayed in the
        // plan (as a Skip row plus its synthesized parents). Fails today:
        // both reserved folders are kept.
        let all = vec![
            crate::entity::folder(".vaultsync-check-1"),
            crate::entity::folder("a/.name.vaultsync-tmp-1-2"),
            crate::entity::folder("notes"),
        ];
        let (kept, dropped) = partition_reserved_remote_keys(all);
        let kept_keys: Vec<&str> = kept.iter().map(|e| e.key.as_str()).collect();
        let dropped_keys: Vec<&str> = dropped.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(kept_keys, vec!["notes/"], "kept wrong: {kept_keys:?}");
        assert_eq!(
            dropped_keys,
            vec![".vaultsync-check-1/", "a/.name.vaultsync-tmp-1-2/"],
            "dropped wrong: {dropped_keys:?}"
        );
    }

    #[test]
    fn partition_reserved_remote_keys_splits_and_preserves_order() {
        // W79/r9 L1: the pure partition splits tmp-sibling, check-probe, and
        // nested reserved keys from normal keys, preserving order in both
        // output lists (compile-RED on the helper). The `eprintln!` warning
        // side effect itself is not capture-tested (same precedent as W70).
        let all = vec![
            crate::entity::file("a.md", 1, Some(1)),
            crate::entity::file(".a.md.vaultsync-tmp-1-2", 2, Some(2)),
            crate::entity::file("notes/.vaultsync-check-1-2-3", 3, Some(3)),
            crate::entity::file("b.md", 4, Some(4)),
            crate::entity::file("notes/.b.md.vaultsync-tmp-5-6", 5, Some(5)),
        ];
        let (kept, dropped) = partition_reserved_remote_keys(all);
        let kept_keys: Vec<&str> = kept.iter().map(|e| e.key.as_str()).collect();
        let dropped_keys: Vec<&str> = dropped.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(kept_keys, vec!["a.md", "b.md"], "kept order wrong");
        assert_eq!(
            dropped_keys,
            vec![
                ".a.md.vaultsync-tmp-1-2",
                "notes/.vaultsync-check-1-2-3",
                "notes/.b.md.vaultsync-tmp-5-6"
            ],
            "dropped order wrong"
        );
    }

    #[test]
    fn build_plan_drops_reserved_remote_keys_unchanged() {
        // W79/r9 L1 behavior lock: with the counting + stderr warning added,
        // reserved remote keys still produce NO plan rows (the W63 invariant
        // pinned while the visibility is added).
        let dir = TempDir::new("vaultsync-lib-test");
        let local = LocalFs::new(dir.path());
        let store = StubStore {
            listed: vec![
                crate::entity::file(".a.md.vaultsync-tmp-1-2", 25, Some(100)),
                crate::entity::file("notes/.vaultsync-check-1-2-3", 25, Some(100)),
                crate::entity::file("ok.md", 25, Some(100)),
            ],
        };
        let p = build_plan(&local, &store, Mode::Pull, &PlanOpts::default())
            .unwrap()
            .plan;
        assert!(
            !p.actions.iter().any(|a| {
                a.key == ".a.md.vaultsync-tmp-1-2" || a.key == "notes/.vaultsync-check-1-2-3"
            }),
            "reserved keys planned: {:?}",
            p.actions
        );
        assert!(p.actions.iter().any(|a| a.key == "ok.md"));
    }

    #[test]
    fn build_plan_surfaces_reserved_namespace_warning_in_report() {
        // H1 (W99): the reserved-namespace warning must be carried in
        // `PlanReport.warnings` (same W79/r9-L1 text as today's eprintln,
        // minus the CLI "warning: " prefix) so the CLI layer prints it -
        // library code must not write to process stderr. RED: `PlanReport`
        // does not exist (compile failure).
        let dir = TempDir::new("vaultsync-lib-test");
        let local = LocalFs::new(dir.path());
        let store = MemoryStore::new();
        store
            .put_from(
                ".vaultsync-check-1-2-3",
                &mut std::io::Cursor::new(b"x".to_vec()),
                1,
                None,
            )
            .unwrap();
        let report = build_plan(&local, &store, Mode::Pull, &PlanOpts::default()).unwrap();
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("reserved vaultsync namespace")
                    && w.contains(".vaultsync-check-1-2-3")),
            "reserved-namespace warning missing: {:?}",
            report.warnings
        );
        assert!(
            !report
                .plan
                .actions
                .iter()
                .any(|a| a.key.starts_with(".vaultsync-check-")),
            "reserved key planned: {:?}",
            report.plan.actions
        );
    }

    #[test]
    fn list_filters_reserved_keys_before_head_enrichment() {
        use crate::testutil::S3LikeListStore;
        // W118/R2-2: reserved-namespace leftovers are partitioned out of the
        // S3 listing *before* any head is issued, so a junk key appears in no
        // plan row, the shared W79 warning fires exactly once (store side,
        // not re-fired by build_plan for S3), and the head log shows only the
        // healthy key - never the reserved one.
        let dir = TempDir::new("vaultsync-lib-test");
        let store = S3LikeListStore::new();
        store
            .inner()
            .put_from(
                ".vaultsync-check-9",
                &mut std::io::Cursor::new(b"junk".to_vec()),
                4,
                Some(1_600_000_000_000),
            )
            .unwrap();
        store
            .inner()
            .put_from(
                "notes/a.md",
                &mut std::io::Cursor::new(b"aaa".to_vec()),
                3,
                Some(1_600_000_000_000),
            )
            .unwrap();
        let local = LocalFs::new(dir.path());
        let report = build_plan(&local, &store, Mode::Status, &PlanOpts::default()).unwrap();
        // reserved key never planned.
        assert!(
            !report
                .plan
                .actions
                .iter()
                .any(|a| a.key.starts_with(".vaultsync-check-")),
            "reserved key planned: {:?}",
            report.plan.actions
        );
        // store-side warning present exactly once with the W79 text.
        let matching: Vec<&String> = report
            .warnings
            .iter()
            .filter(|w| w.contains("reserved vaultsync namespace"))
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "expected exactly one warning: {:?}",
            report.warnings
        );
        assert!(
            matching[0].contains(".vaultsync-check-9"),
            "warning must name the junk key: {}",
            matching[0]
        );
        // head log during list: healthy key served, reserved key never.
        let head_log = store.head_log();
        assert!(
            head_log.iter().any(|k| k == "notes/a.md"),
            "healthy key not headed: {head_log:?}"
        );
        assert!(
            !head_log.iter().any(|k| k == ".vaultsync-check-9"),
            "reserved key was headed (should be pre-filtered): {head_log:?}"
        );
    }

    #[test]
    fn reserved_key_head_failure_does_not_fail_listing() {
        use crate::testutil::S3LikeListStore;
        // W118/R2-2 fail-closed scope creep: a transient head error (throttle)
        // on a reserved leftover must NOT abort the whole listing/plan - the
        // junk key is filtered before any head, so the healthy key still
        // plans.
        let dir = TempDir::new("vaultsync-lib-test");
        let mut store = S3LikeListStore::new();
        store.fail_head(".vaultsync-check-9");
        store
            .inner()
            .put_from(
                ".vaultsync-check-9",
                &mut std::io::Cursor::new(b"junk".to_vec()),
                4,
                Some(1_600_000_000_000),
            )
            .unwrap();
        store
            .inner()
            .put_from(
                "notes/a.md",
                &mut std::io::Cursor::new(b"aaa".to_vec()),
                3,
                Some(1_600_000_000_000),
            )
            .unwrap();
        let local = LocalFs::new(dir.path());
        let report = build_plan(&local, &store, Mode::Status, &PlanOpts::default()).unwrap();
        assert!(
            report.plan.actions.iter().any(|a| a.key == "notes/a.md"),
            "healthy key dropped: {:?}",
            report.plan.actions
        );
    }

    #[test]
    fn build_plan_ignores_remote_check_probe_leftovers() {
        // R4-L4/W42: a crashed `check` can leave a `.vaultsync-check-*` probe
        // object remotely; it must NEVER plan as a `remote_only` -> Download
        // (which would materialize a stray dotfile). Such keys are dropped
        // from the remote ingest before planning.
        let dir = TempDir::new("vaultsync-lib-test");
        let local = LocalFs::new(dir.path());
        let store = StubStore {
            listed: vec![crate::entity::file(".vaultsync-check-1-2-3", 25, Some(100))],
        };
        let p = build_plan(&local, &store, Mode::Pull, &PlanOpts::default())
            .unwrap()
            .plan;
        assert!(
            !p.actions
                .iter()
                .any(|a| a.key.starts_with(".vaultsync-check-")),
            "probe leftover planned: {:?}",
            p.actions
        );
    }

    #[test]
    fn build_plan_drops_nested_remote_check_probe_key() {
        // W54/A-L2: the remote `.vaultsync-check-*` filter must match the
        // local walker's final-segment policy - a nested
        // `notes/.vaultsync-check-*` key (valid per `ensure_valid_key`) must
        // never plan a Download. The old full-key `starts_with` filter missed
        // nested keys: the local walker skips by *file name* in any
        // directory (is_reserved_vaultsync_name), so a nested probe leftover
        // was skipped locally but planned as a download remotely.
        let dir = TempDir::new("vaultsync-lib-test");
        let local = LocalFs::new(dir.path());
        let store = StubStore {
            listed: vec![crate::entity::file(
                "notes/.vaultsync-check-1-2-3",
                25,
                Some(100),
            )],
        };
        let p = build_plan(&local, &store, Mode::Pull, &PlanOpts::default())
            .unwrap()
            .plan;
        assert!(
            !p.actions
                .iter()
                .any(|a| a.key == "notes/.vaultsync-check-1-2-3"),
            "nested probe leftover planned: {:?}",
            p.actions
        );
    }

    #[test]
    fn build_plan_remote_tmp_sibling_never_downloads() {
        // W63/A-L3: a tmp-sibling key (`.name.vaultsync-tmp-*`) that reached
        // the store out-of-band must never plan a Download - the walker
        // treats that namespace as never-syncable, so a materialized
        // download would write a reserved dotfile locally. Same final-
        // segment policy as the walker, both at the root and nested. Fails
        // today: plans Download (only the check-probe namespace is
        // filtered).
        let dir = TempDir::new("vaultsync-lib-test");
        let local = LocalFs::new(dir.path());
        let store = StubStore {
            listed: vec![
                crate::entity::file(".a.md.vaultsync-tmp-1-2", 25, Some(100)),
                crate::entity::file("notes/.a.md.vaultsync-tmp-3-4", 25, Some(100)),
            ],
        };
        let p = build_plan(&local, &store, Mode::Pull, &PlanOpts::default())
            .unwrap()
            .plan;
        assert!(
            !p.actions.iter().any(|a| {
                a.key == ".a.md.vaultsync-tmp-1-2" || a.key == "notes/.a.md.vaultsync-tmp-3-4"
            }),
            "tmp-sibling key planned: {:?}",
            p.actions
        );
    }

    #[cfg(unix)]
    #[test]
    fn build_plan_push_skips_followed_symlink_files() {
        // R4-M1/W38: push plans local_only rows for the vault above; the
        // followed *file* symlink `link.md` must become Skip(followed_symlink)
        // (transfers refuse to open a symlink), while `real.md` is a normal
        // Upload and the dir-symlink child `linkdir/child.md` is a normal
        // Upload (it transfers fine).
        let dir = TempDir::new("vaultsync-lib-test");
        std::fs::write(dir.join("real.md"), "r").unwrap();
        std::fs::create_dir_all(dir.join("realdir")).unwrap();
        std::fs::write(dir.join("realdir/child.md"), "c").unwrap();
        std::os::unix::fs::symlink("real.md", dir.join("link.md")).unwrap();
        std::os::unix::fs::symlink("realdir", dir.join("linkdir")).unwrap();
        let local = LocalFs::with_follow(dir.path(), true);
        let store = MemoryStore::new();
        let p = build_plan(&local, &store, Mode::Push, &PlanOpts::default())
            .unwrap()
            .plan;
        let link = p.actions.iter().find(|a| a.key == "link.md").unwrap();
        assert_eq!(link.kind, ActionKind::Skip, "{:?}", link);
        assert_eq!(link.reason, "followed_symlink");
        let real = p.actions.iter().find(|a| a.key == "real.md").unwrap();
        assert_eq!(real.kind, ActionKind::Upload, "{:?}", real);
        let child = p
            .actions
            .iter()
            .find(|a| a.key == "linkdir/child.md")
            .unwrap();
        assert_eq!(child.kind, ActionKind::Upload, "dir child must transfer");
    }

    #[cfg(unix)]
    #[test]
    fn build_plan_pull_skips_followed_symlink_downloads() {
        // R4-M1/W38: pull with a remote `link.md` newer than the followed
        // local entity must plan Skip(followed_symlink), NOT a Download (the
        // pull write through a symlink stays fail-closed in the executor).
        let dir = TempDir::new("vaultsync-lib-test");
        std::fs::write(dir.join("real.md"), "r").unwrap();
        std::os::unix::fs::symlink("real.md", dir.join("link.md")).unwrap();
        // pin the followed target's mtime so the remote is strictly newer
        let base = 1_700_000_000_000u64;
        {
            let f = std::fs::File::open(dir.join("real.md")).unwrap();
            let t = std::time::UNIX_EPOCH + std::time::Duration::from_millis(base);
            f.set_times(std::fs::FileTimes::new().set_modified(t))
                .unwrap();
        }
        let local = LocalFs::with_follow(dir.path(), true);
        let store = MemoryStore::new();
        // remote link.md much newer than the target's (remote_newer -> Download)
        put_str(&store, "link.md", "remote-new", base + 1_000_000);
        put_str(&store, "real.md", "r", 1);
        let p = build_plan(&local, &store, Mode::Pull, &PlanOpts::default())
            .unwrap()
            .plan;
        let link = p
            .actions
            .iter()
            .find(|a| a.key == "link.md")
            .expect("link.md action");
        assert_eq!(link.kind, ActionKind::Skip, "{:?}", link);
        assert_eq!(link.reason, "followed_symlink");
    }

    #[cfg(unix)]
    #[test]
    fn build_plan_pull_delete_skips_followed_symlink_files() {
        // W51 (A-M2/B-M1): `pull --delete --follow-symlinks` must plan a
        // local-only followed *file* symlink row as Skip(followed_symlink),
        // NOT DeleteLocal - the delete arm of the W38 override. The link is
        // never removed in v1; the guard delete would otherwise refuse it as
        // a symlink and fail the whole run per-key.
        let dir = TempDir::new("vaultsync-lib-test");
        std::fs::write(dir.join("real.md"), "r").unwrap();
        std::os::unix::fs::symlink("real.md", dir.join("link.md")).unwrap();
        let local = LocalFs::with_follow(dir.path(), true);
        let store = MemoryStore::new();
        let opts = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let p = build_plan(&local, &store, Mode::Pull, &opts).unwrap().plan;
        let link = p.actions.iter().find(|a| a.key == "link.md").unwrap();
        assert_eq!(link.kind, ActionKind::Skip, "{:?}", link);
        assert_eq!(link.reason, "followed_symlink");
    }

    #[cfg(unix)]
    #[test]
    fn build_plan_status_keeps_followed_symlink_rows() {
        // R4-M1/W38: --follow-symlinks is inventory-only in v1 - Status mode
        // leaves followed-symlink rows un-overridden so inventory stays
        // visible (here an Upload, since the remote lacks it).
        let dir = TempDir::new("vaultsync-lib-test");
        std::fs::write(dir.join("real.md"), "r").unwrap();
        std::os::unix::fs::symlink("real.md", dir.join("link.md")).unwrap();
        let local = LocalFs::with_follow(dir.path(), true);
        let store = MemoryStore::new();
        let p = build_plan(&local, &store, Mode::Status, &PlanOpts::default())
            .unwrap()
            .plan;
        let link = p.actions.iter().find(|a| a.key == "link.md").unwrap();
        assert_eq!(
            link.kind,
            ActionKind::Upload,
            "status must keep inventory row: {:?}",
            link
        );
    }

    #[test]
    fn build_plan_ignores_exact_prefix_marker_object() {
        // R4-M2: a remote listing that yields the exact-prefix folder marker
        // as an empty relative key (`""`) must be dropped by build_plan's
        // remote ingest, not abort the plan with `InvalidKey("key must not be
        // empty")`. (W34 removes it at the S3 backend source; this is the
        // defense-in-depth lock for other backends.)
        let dir = TempDir::new("vaultsync-lib-test");
        let local = LocalFs::new(dir.path());
        let store = StubStore {
            listed: vec![Entity {
                key: "".to_string(),
                size: 0,
                mtime_ms: Some(123),
                etag: None,
            }],
        };
        let p = build_plan(&local, &store, Mode::Status, &PlanOpts::default())
            .unwrap()
            .plan;
        assert!(
            !p.actions.iter().any(|a| a.key.is_empty()),
            "empty key planned: {:?}",
            p.actions
        );
    }

    #[test]
    fn build_plan_rejects_invalid_remote_key() {
        // Remote list keys must pass `ensure_valid_key` before any plan is
        // built (fail closed): an escaping or control-char key must never
        // become a planned action.
        let dir = TempDir::new("vaultsync-lib-test");
        let local = LocalFs::new(dir.path());
        for bad in ["../evil.md", "a/\nb.md"] {
            let store = StubStore {
                listed: vec![crate::entity::file(bad, 1, Some(1))],
            };
            let err = build_plan(&local, &store, Mode::Status, &PlanOpts::default()).unwrap_err();
            assert!(matches!(err, Error::InvalidKey(_)), "key {bad:?}: {err}");
        }
    }

    #[test]
    fn build_plan_accepts_remote_folder_entities() {
        // Folder views (trailing `/`) from a remote listing must still pass
        // validation and plan as Skip `folder`.
        let dir = TempDir::new("vaultsync-lib-test");
        let local = LocalFs::new(dir.path());
        let store = StubStore {
            listed: vec![
                crate::entity::folder("notes"),
                crate::entity::file("notes/a.md", 1, Some(1)),
            ],
        };
        let p = build_plan(&local, &store, Mode::Status, &PlanOpts::default())
            .unwrap()
            .plan;
        let folder_act = p
            .actions
            .iter()
            .find(|a| a.key == "notes/")
            .expect("folder entity present");
        assert_eq!(folder_act.kind, ActionKind::Skip);
        assert_eq!(folder_act.reason, "folder");
        let file_act = p
            .actions
            .iter()
            .find(|a| a.key == "notes/a.md")
            .expect("file entity present");
        assert_eq!(file_act.kind, ActionKind::Download);
    }

    fn put_str(store: &MemoryStore, key: &str, body: &str, mtime: u64) {
        let mut cursor = std::io::Cursor::new(body.as_bytes().to_vec());
        store
            .put_from(key, &mut cursor, body.len() as u64, Some(mtime))
            .unwrap();
    }

    #[test]
    fn version_matches_package_version() {
        assert_eq!(version(), "0.1.0");
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn status_with_store_local_only() {
        let dir = TempDir::new("vaultsync-lib-test");
        std::fs::write(dir.join("a.md"), "hi").unwrap();
        let store = MemoryStore::new();
        let p = status_with_store(&dir, &store, &PlanOpts::default()).unwrap();
        assert_eq!(p.actions.len(), 1);
        assert_eq!(p.actions[0].key, "a.md");
        assert_eq!(p.actions[0].kind, ActionKind::Upload);
    }

    #[test]
    fn status_with_store_matches_seeded_remote() {
        let dir = TempDir::new("vaultsync-lib-test");
        std::fs::write(dir.join("a.md"), "same").unwrap();
        let mt = std::fs::metadata(dir.join("a.md"))
            .unwrap()
            .modified()
            .unwrap();
        let ms = mt
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let store = MemoryStore::new();
        put_str(&store, "a.md", "same", ms);
        let p = status_with_store(&dir, &store, &PlanOpts::default()).unwrap();
        assert_eq!(p.actions.len(), 1);
        assert_eq!(p.actions[0].kind, ActionKind::Skip);
        assert_eq!(p.stats.skip, 1);
    }

    #[test]
    fn status_with_store_remote_only_download() {
        let dir = TempDir::new("vaultsync-lib-test");
        let store = MemoryStore::new();
        put_str(&store, "b.md", "x", 1000);
        let p = status_with_store(&dir, &store, &PlanOpts::default()).unwrap();
        assert!(
            p.actions
                .iter()
                .any(|a| a.key == "b.md" && a.kind == ActionKind::Download)
        );
    }

    #[test]
    fn format_plan_human_contains_stats_line() {
        let store = MemoryStore::new();
        put_str(&store, "b.md", "x", 1000);
        let dir = TempDir::new("vaultsync-lib-test");
        std::fs::write(dir.join("a.md"), "hi").unwrap();
        let p = status_with_store(&dir, &store, &PlanOpts::default()).unwrap();
        let txt = format_plan_human(&p);
        assert!(txt.contains("plan:"));
        assert!(txt.contains("1 upload"));
        assert!(txt.contains("1 download"));
    }

    #[test]
    fn format_plan_human_marks_actions() {
        let dir = TempDir::new("vaultsync-lib-test");
        std::fs::write(dir.join("a.md"), "hi").unwrap();
        std::fs::write(dir.join("c.md"), "x").unwrap();
        // seed remote with c.md at the local file's mtime but different size -> conflict
        let mt = std::fs::metadata(dir.join("c.md"))
            .unwrap()
            .modified()
            .unwrap();
        let ms = mt
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let store = MemoryStore::new();
        put_str(&store, "b.md", "x", 1000);
        put_str(&store, "c.md", "xx", ms);
        let p = status_with_store(&dir, &store, &PlanOpts::default()).unwrap();
        let txt = format_plan_human(&p);
        assert!(txt.lines().any(|l| l.starts_with("U  a.md")));
        assert!(txt.lines().any(|l| l.starts_with("D  b.md")));
        assert!(txt.lines().any(|l| l.starts_with("*  c.md")));
    }

    #[test]
    fn folder_mtime_asymmetry_is_intentional() {
        // Local folder entities carry real mtimes; mock/remote synthesized
        // folders use `None`. Asymmetry is intentional (decision row
        // P1r4-folder-mtime): Phase 2 must not compare folder mtimes across
        // sides.
        let dir = TempDir::new("vaultsync-lib-test");
        std::fs::create_dir_all(dir.join("notes")).unwrap();
        let local_ents = LocalFs::new(dir.path()).list().unwrap();
        let local_folder = local_ents.iter().find(|e| e.key == "notes/").unwrap();
        assert!(local_folder.mtime_ms.is_some());

        let store = MemoryStore::new();
        put_str(&store, "notes/a.md", "x", 1000);
        let remote_ents = store.list("").unwrap().entities;
        let remote_folder = remote_ents.iter().find(|e| e.key == "notes/").unwrap();
        assert_eq!(remote_folder.mtime_ms, None);
    }

    /// Mirrors the roadmap exit sentence: status against a mock store in a
    /// temp vault produces a correct plan including folder handling.
    #[test]
    fn phase1_exit_status_against_mock_in_temp_vault() {
        let dir = TempDir::new("vaultsync-lib-test");
        std::fs::create_dir_all(dir.join("notes")).unwrap();
        std::fs::write(dir.join("notes/hello.md"), "hi").unwrap();

        let store = MemoryStore::new(); // seeded with nothing
        let p = status_with_store(&dir, &store, &PlanOpts::default()).unwrap();

        let upload = p
            .actions
            .iter()
            .find(|a| a.key == "notes/hello.md")
            .expect("file upload planned");
        assert_eq!(upload.kind, ActionKind::Upload);
        assert_eq!(upload.reason, "local_only");

        // local-only folder entity -> Skip `folder` (does not round-trip to S3)
        let folder_act = p
            .actions
            .iter()
            .find(|a| a.key == "notes/")
            .expect("folder entity present");
        assert_eq!(folder_act.kind, ActionKind::Skip);
        assert_eq!(folder_act.reason, "folder");

        assert_eq!(p.stats.upload, 1);
    }
    #[test]
    fn build_plan_exact_case_file_folder_keeps_path_collision() {
        // W31/N5: a local file `notes` vs a remote folder `notes/` (identical
        // case) is a *path* collision; the 4c case-collision override must NOT
        // relabel it to `case_collision`. Only a true case *variant* (e.g.
        // `Notes` vs `notes/`) is a case collision.
        let dir = TempDir::new("vaultsync-lib-test");
        std::fs::write(dir.join("notes"), "file-notes").unwrap();
        let local = LocalFs::new(dir.path());
        let store = StubStore {
            listed: vec![
                crate::entity::folder("notes"),
                crate::entity::file("notes/x", 1, Some(1)),
            ],
        };
        let p = build_plan(&local, &store, Mode::Status, &PlanOpts::default())
            .unwrap()
            .plan;
        let file_act = p
            .actions
            .iter()
            .find(|a| a.key == "notes")
            .expect("notes file");
        assert_eq!(file_act.kind, ActionKind::Conflict);
        assert_eq!(
            file_act.reason, "path_collision",
            "file row mislabeled: {}",
            file_act.reason
        );
        let folder_act = p
            .actions
            .iter()
            .find(|a| a.key == "notes/")
            .expect("notes/");
        assert_eq!(folder_act.kind, ActionKind::Conflict);
        assert_eq!(
            folder_act.reason, "path_collision",
            "folder row mislabeled: {}",
            folder_act.reason
        );
    }

    #[test]
    fn build_plan_case_collision_cross_side_conflicts() {
        // 4c: local `Note.md` vs remote `note.md` (different size/mtime) ->
        // both rows Conflict `case_collision`; they are never auto-paired as
        // Equal. (Cross-side form: platform-safe, unlike two same-side files
        // differing only by case on a case-insensitive FS.)
        let dir = TempDir::new("vaultsync-lib-test");
        std::fs::write(dir.join("Note.md"), "local-case").unwrap();
        let local = LocalFs::new(dir.path());
        let store = StubStore {
            listed: vec![crate::entity::file("note.md", 5, Some(1000))],
        };
        let p = build_plan(&local, &store, Mode::Status, &PlanOpts::default())
            .unwrap()
            .plan;
        let note = p
            .actions
            .iter()
            .find(|a| a.key == "Note.md")
            .expect("Note.md");
        let note_lower = p
            .actions
            .iter()
            .find(|a| a.key == "note.md")
            .expect("note.md");
        assert_eq!(note.kind, ActionKind::Conflict);
        assert_eq!(note.reason, "case_collision");
        assert_eq!(note_lower.kind, ActionKind::Conflict);
        assert_eq!(note_lower.reason, "case_collision");
        assert_eq!(p.stats.conflict, 2);
    }

    #[test]
    fn build_plan_case_collision_file_vs_folder() {
        // W4/A-H3: local file `Notes` vs remote folder `notes/` (case variant)
        // -> both rows Conflict `case_collision` (was missed because the fold
        // kept the trailing slash `notes/`).
        let dir = TempDir::new("vaultsync-lib-test");
        std::fs::write(dir.join("Notes"), "local").unwrap();
        let local = LocalFs::new(dir.path());
        let store = StubStore {
            listed: vec![
                crate::entity::folder("notes"),
                crate::entity::file("notes/x", 1, Some(1)),
            ],
        };
        let p = build_plan(&local, &store, Mode::Status, &PlanOpts::default())
            .unwrap()
            .plan;
        let notes = p.actions.iter().find(|a| a.key == "Notes").expect("Notes");
        let notes_folder = p
            .actions
            .iter()
            .find(|a| a.key == "notes/")
            .expect("notes/");
        assert_eq!(notes.kind, ActionKind::Conflict);
        assert_eq!(notes.reason, "case_collision");
        assert_eq!(notes_folder.kind, ActionKind::Conflict);
        assert_eq!(notes_folder.reason, "case_collision");
    }

    #[cfg(unix)]
    #[test]
    fn build_plan_case_collision_conflict_survives_follow_override() {
        // W72 (locks the REFUTED round-7 B nit): a key that is BOTH a case
        // collision AND a followed file symlink keeps its `case_collision`
        // Conflict in mutating modes. The case-collision preflight runs
        // first (row -> Conflict), and the W38/W51 followed-symlink override
        // matches only Upload|Download|DeleteLocal - a Conflict row is not in
        // its match arms. Cross-side construction (local followed symlink
        // `link.md` vs remote `LINK.md`) keeps the test platform-safe: on a
        // case-insensitive filesystem two same-side case variants cannot
        // coexist.
        let dir = TempDir::new("vaultsync-lib-test");
        std::fs::write(dir.join("target.md"), "real-target").unwrap();
        std::os::unix::fs::symlink("target.md", dir.join("link.md")).unwrap();
        let local = LocalFs::with_follow(dir.path(), true);
        let store = StubStore {
            listed: vec![crate::entity::file("LINK.md", 11, Some(100))],
        };
        let p = build_plan(&local, &store, Mode::Push, &PlanOpts::default())
            .unwrap()
            .plan;
        let link = p
            .actions
            .iter()
            .find(|a| a.key == "link.md")
            .expect("link.md row");
        assert_eq!(
            link.kind,
            ActionKind::Conflict,
            "case_collision Conflict overridden: {link:?}"
        );
        assert_eq!(
            link.reason, "case_collision",
            "case_collision reason dropped: {link:?}"
        );
    }

    #[test]
    fn build_plan_ignores_tmp_leftover() {
        // W23/M1: a planted vaultsync temp sibling produces no Upload row and
        // no stray key; only the real file is planned.
        let dir = TempDir::new("vaultsync-lib-test");
        std::fs::write(dir.join("note.md"), "real").unwrap();
        std::fs::write(dir.join(".note.md.vaultsync-tmp-123-4"), "leftover").unwrap();
        let local = LocalFs::new(dir.path());
        let store = MemoryStore::new();
        let p = build_plan(&local, &store, Mode::Push, &PlanOpts::default())
            .unwrap()
            .plan;
        assert_eq!(p.stats.upload, 1);
        assert!(p.actions.iter().any(|a| a.key == "note.md"));
        assert!(
            !p.actions
                .iter()
                .any(|a| a.key.starts_with('.') && a.key.contains("vaultsync-tmp")),
            "tmp sibling planned"
        );
    }

    #[test]
    fn check_probe_key_under_prefix() {
        // The probe key is a valid vault-relative key under a dot-prefix.
        let k = probe_key();
        assert!(k.starts_with(".vaultsync-check-"));
        assert!(
            crate::entity::ensure_valid_key(&k).is_ok(),
            "invalid probe key {k:?}"
        );
    }

    #[test]
    fn probe_key_unique_and_valid() {
        // W11/A-M7: consecutive probe keys differ (not pid-only, so they cannot
        // clobber a user object), keep the reserved prefix, and stay valid.
        let a = probe_key();
        let b = probe_key();
        assert_ne!(a, b, "probe keys must be unique");
        for k in [&a, &b] {
            assert!(k.starts_with(".vaultsync-check-"), "key {k:?}");
            assert!(
                crate::entity::ensure_valid_key(k).is_ok(),
                "invalid probe key {k:?}"
            );
        }
    }

    #[test]
    fn check_store_succeeds_on_mock() {
        let store = MemoryStore::new();
        crate::check_store(&store).unwrap();
        // probe object removed after the check
        assert!(store.list("").unwrap().entities.is_empty());
    }

    /// A store whose `get_to` always errors after a successful put, to inject
    /// a probe read failure (W24/M2).
    struct GetFailStore {
        inner: MemoryStore,
    }
    impl ObjectStore for GetFailStore {
        fn list(&self, prefix: &str) -> Result<Listing, Error> {
            self.inner.list(prefix)
        }
        fn head(&self, key: &str) -> Result<Entity, Error> {
            self.inner.head(key)
        }
        fn get_to(&self, _key: &str, _w: &mut dyn std::io::Write) -> Result<Entity, Error> {
            Err(Error::Other("injected get failure".to_string()))
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

    /// A store whose `get_to` returns a corrupted size for the probe (bytes
    /// round-trip, header disagrees) - the read-back mismatch path (W24/M2).
    struct SizeCorruptStore {
        inner: MemoryStore,
    }
    impl ObjectStore for SizeCorruptStore {
        fn list(&self, prefix: &str) -> Result<Listing, Error> {
            self.inner.list(prefix)
        }
        fn head(&self, key: &str) -> Result<Entity, Error> {
            self.inner.head(key)
        }
        fn get_to(&self, key: &str, w: &mut dyn std::io::Write) -> Result<Entity, Error> {
            let ent = self.inner.get_to(key, w)?;
            Ok(Entity {
                size: ent.size + 999,
                ..ent
            })
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

    fn assert_no_check_probe(store: &MemoryStore) {
        let keys: Vec<String> = store
            .list("")
            .unwrap()
            .entities
            .into_iter()
            .map(|e| e.key)
            .collect();
        assert!(
            !keys.iter().any(|k| k.starts_with(".vaultsync-check-")),
            "probe leaked after failed check: {keys:?}"
        );
    }

    #[test]
    fn check_store_cleans_probe_when_get_fails() {
        // W24/M2: even when `get_to` fails after a successful put, `check_store`
        // errors AND removes the probe so `.vaultsync-check-*` never lists as a
        // remote_only row later.
        let store = GetFailStore {
            inner: MemoryStore::new(),
        };
        assert!(crate::check_store(&store).is_err());
        assert_no_check_probe(&store.inner);
    }

    #[test]
    fn check_store_cleans_probe_on_readback_mismatch() {
        // W24/M2: a read-back mismatch (corrupted size) is an error that must
        // still delete the probe.
        let store = SizeCorruptStore {
            inner: MemoryStore::new(),
        };
        assert!(crate::check_store(&store).is_err());
        assert_no_check_probe(&store.inner);
    }

    #[test]
    fn format_plan_verbose_shows_skip_reasons() {
        // W71/A-N3: under -v the Skip rows must show their planner reason
        // (local_only under pull without --delete) - the diagnosis a
        // debugging user looks for, same shape as the Conflict row. Fails
        // today: S rows print the key only.
        let dir = TempDir::new("vaultsync-lib-test");
        std::fs::write(dir.join("local-only.md"), "x").unwrap();
        let local = LocalFs::new(dir.path());
        let store = MemoryStore::new();
        let p = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default())
            .unwrap()
            .plan;
        let verbose = format_plan_human_verbose(&p, 1);
        let s_line = verbose
            .lines()
            .find(|l| l.starts_with("S  ") && l.contains("local-only.md"))
            .unwrap_or_else(|| panic!("no S line for local-only.md: {verbose}"));
        assert!(
            s_line.contains("local_only"),
            "S line lacks reason: {s_line:?}"
        );
    }

    #[test]
    fn format_plan_human_hides_skips_by_default() {
        // R3 low: S rows hidden by default (stats line still counts them);
        // -v shows them.
        let dir = TempDir::new("vaultsync-lib-test");
        std::fs::create_dir_all(dir.join("notes")).unwrap();
        std::fs::write(dir.join("notes/a.md"), "x").unwrap();
        std::fs::write(dir.join("a.md"), "same").unwrap();
        let store = MemoryStore::new();
        put_str(&store, "a.md", "same", 1); // placeholder, mtime replaced below
        // seed equal a.md so it skips
        let mt = std::fs::metadata(dir.join("a.md"))
            .unwrap()
            .modified()
            .unwrap();
        let ms = mt
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        {
            let mut c = std::io::Cursor::new(b"same".to_vec());
            store.put_from("a.md", &mut c, 4, Some(ms)).unwrap();
        }
        let p = status_with_store(&dir, &store, &PlanOpts::default()).unwrap();
        let default = format_plan_human(&p);
        assert!(
            !default.lines().any(|l| l.starts_with("S  ")),
            "skips leaked: {default}"
        );
        let verbose = format_plan_human_verbose(&p, 1);
        assert!(
            verbose.lines().any(|l| l.starts_with("S  ")),
            "skips hidden with -v: {verbose}"
        );
    }

    /// Pin a file's mtime to a fixed ms value (deterministic convergence tests).
    fn set_mtime(p: &std::path::Path, ms: u64) {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms);
        let times = std::fs::FileTimes::new().set_modified(t);
        std::fs::File::open(p).unwrap().set_times(times).unwrap();
    }

    fn mtime_ms(p: &std::path::Path) -> u64 {
        std::fs::metadata(p)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    /// Pre-fix sanity (documented, issue #15): planning against this double's
    /// RAW degraded listing (every object mtime rewritten to a later upload
    /// time, as S3's ListObjectsV2 `LastModified` fallback does) classifies
    /// every key `RemoteNewer` => Download-everything - reproduced here with
    /// no socket. The `enrich_with_head_mtimes` call inside the double's
    /// `list` (mirroring the W113 S3 wiring) is what turns that into a no-op.
    #[test]
    fn build_plan_status_converges_after_push_with_s3like_listing() {
        use crate::testutil::S3LikeListStore;
        let dir = TempDir::new("vaultsync-lib-test");
        std::fs::create_dir_all(dir.join("notes")).unwrap();
        let files = [
            ("a.md", b"aaa".to_vec()),
            ("notes/b.md", b"bbbb".to_vec()),
            ("c.md", b"cc".to_vec()),
        ];
        let fixed = 1_600_000_000_123u64;
        for (rel, bytes) in &files {
            let p = dir.join(rel);
            std::fs::write(&p, bytes).unwrap();
            set_mtime(&p, fixed);
        }
        let store = S3LikeListStore::new();
        let local = LocalFs::new(dir.path());
        // push (the S3LikeListStore list enriches via head, so this converges)
        let plan = crate::build_plan(&local, &store, Mode::Push, &PlanOpts::default())
            .unwrap()
            .plan;
        // W120/R1-M3: assert the push plan actually planned the seeded
        // uploads (a vacuous pass on an empty push would hide regressions).
        assert_eq!(
            plan.stats.upload,
            files.len() as u32,
            "push plan must plan the seeded uploads: {:?}",
            plan.actions
        );
        let rep =
            crate::exec::execute_plan(&local, &store, &plan, Mode::Push, &PlanOpts::default(), 1);
        assert!(rep.failed.is_empty(), "push failures: {:?}", rep.failed);
        assert_eq!(
            rep.executed,
            files.len() as u32,
            "push must execute exactly the seeded uploads: {:?}",
            rep
        );
        // per-key size sanity: each upload landed with the true byte count.
        for (rel, bytes) in &files {
            let h = store.head(rel).unwrap();
            assert_eq!(
                h.size,
                bytes.len() as u64,
                "uploaded {rel} size wrong: {:?}",
                h
            );
        }
        // status after push must plan 0 mutating actions (issue acceptance 1)
        let status = crate::build_plan(&local, &store, Mode::Status, &PlanOpts::default())
            .unwrap()
            .plan;
        assert_eq!(status.stats.upload, 0, "uploads: {:?}", status.actions);
        assert_eq!(status.stats.download, 0, "downloads: {:?}", status.actions);
        assert_eq!(status.stats.conflict, 0, "conflicts: {:?}", status.actions);
        // every non-folder row is a visible Skip (folders may Skip as well)
        for a in &status.actions {
            if !a.key.ends_with('/') {
                assert_eq!(a.kind, ActionKind::Skip, "non-converged row: {:?}", a);
            }
        }
    }

    /// Maps onto the issue's second acceptance bullet: pull into a fresh dir
    /// restores byte-identical files with exact mtimes, and a following
    /// `status` there plans 0 mutating actions (download-direction
    /// incrementality exists).
    #[test]
    fn pull_into_fresh_dir_then_status_converges_with_s3like_listing() {
        use crate::testutil::S3LikeListStore;
        let store = S3LikeListStore::new();
        let fixed = 1_600_000_000_123u64;
        let files = [("a.md", b"aaa".to_vec()), ("notes/b.md", b"bbbb".to_vec())];
        for (rel, bytes) in &files {
            let mut c = std::io::Cursor::new(bytes.clone());
            store
                .inner()
                .put_from(rel, &mut c, bytes.len() as u64, Some(fixed))
                .unwrap();
        }
        let dst = TempDir::new("vaultsync-lib-test");
        let ldst = LocalFs::new(dst.path());
        let plan = crate::build_plan(&ldst, &store, Mode::Pull, &PlanOpts::default())
            .unwrap()
            .plan;
        let rep =
            crate::exec::execute_plan(&ldst, &store, &plan, Mode::Pull, &PlanOpts::default(), 1);
        assert!(rep.failed.is_empty(), "pull failures: {:?}", rep.failed);
        // byte-identical + exact mtime restored (existing feature must hold)
        for (rel, bytes) in &files {
            assert_eq!(std::fs::read(dst.join(rel)).unwrap(), *bytes, "{rel} bytes");
            let gm = mtime_ms(&dst.join(rel));
            assert!(gm.abs_diff(fixed) < 2000, "{rel} mtime {gm} != {fixed}");
        }
        // status in the fresh dir plans 0 mutating actions (issue acceptance 2)
        let status = crate::build_plan(&ldst, &store, Mode::Status, &PlanOpts::default())
            .unwrap()
            .plan;
        assert_eq!(status.stats.upload, 0, "uploads: {:?}", status.actions);
        assert_eq!(status.stats.download, 0, "downloads: {:?}", status.actions);
        assert_eq!(status.stats.conflict, 0, "conflicts: {:?}", status.actions);
    }
}
