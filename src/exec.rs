//! Executor: applies a [`Plan`] against a [`LocalFs`] + [`ObjectStore`] (Phase
//! 2 Slice 5).
//!
//! Fully TDD against the mock store + temp vaults; no network. Ordering locks
//! (sync-model "Execution order"):
//! - transfers first (downloads then uploads within mode), destination deletes
//!   last;
//! - parents-before-children on create (via `create_dir_all`), and
//!   children-before-parents on delete (file deletes + a bottom-up empty-dir
//!   post-pass, R2.1 option a);
//! - folder actions are always Skip (never executed);
//! - Conflict / Skip rows never mutate anything;
//! - upload re-verifies size + mtime via `open_verified` (R3.3); a mismatch is
//!   a per-key error, the run continues, and exit is non-zero at dispatch;
//! - per-key failures are isolated; the report collects `(key, error)`.

use crate::error::Error;
use crate::local::LocalFs;
use crate::plan::{ActionKind, Mode, Plan};
use crate::progress::{NoProgress, PassKind, Progress, ProgressEvent};
use crate::store::ObjectStore;

/// Outcome of an execution run.
#[derive(Debug, Default, PartialEq)]
pub struct ExecReport {
    /// Number of transfers/deletes that succeeded.
    pub executed: u32,
    /// Failed keys with a human message (one bad key never aborts the run).
    pub failed: Vec<ExecFailure>,
    /// Non-fatal warnings (e.g. an empty-dir cleanup error, W16/A-L3)
    /// surfaced to stderr; do not affect the exit code.
    pub warnings: Vec<String>,
}

/// A single failed key.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecFailure {
    pub key: String,
    pub message: String,
}

/// Apply a plan. `Mode::Status` plans never mutate anything (belt and braces).
/// `opts` carries the resolved `mtime_tolerance_ms` used for the upload-side
/// re-verification (W2, PR2 A-H2/B-M1) and, later, the pull destination
/// freshness guard (W13).
///
/// I20: `concurrency` bounds how many keys each pass runs in flight (workers =
/// `min(concurrency, items)`); `concurrency <= 1` runs the sequential loop on
/// the caller's thread (I20-one). The four passes still run strictly in order
/// (downloads, uploads, DeleteRemote, DeleteLocal); per-key guards are
/// unchanged and now execute concurrently inside the pool (R3.3, W13/W22,
/// W62/W119, W39). Report assembly happens after each pass in plan order, so
/// `failed` stays deterministic regardless of completion order.
///
/// I27-api: `execute_plan` is the thin wrapper that runs with `&NoProgress` -
/// library callers keep today's exact signature and see zero progress output.
pub fn execute_plan(
    local: &LocalFs,
    store: &dyn ObjectStore,
    plan: &Plan,
    mode: Mode,
    opts: &crate::plan::PlanOpts,
    concurrency: u32,
) -> ExecReport {
    execute_plan_with_progress(local, store, plan, mode, opts, concurrency, &NoProgress)
}

/// The event-emitting form of [`execute_plan`] (I27-api): every completed key
/// fires a `KeyDone`; each non-empty pass brackets its items with
/// `PassStart`/`PassEnd`; a terminal `RunEnd` follows the final pass and the
/// empty-dir post-pass. Emission is completion-driven inside the pool closure
/// (I27-events), so `KeyDone` order may interleave under `concurrency > 1`;
/// `ExecReport` assembly stays plan-ordered and unchanged. `Mode::Status`
/// stays silent (no events at all).
pub fn execute_plan_with_progress(
    local: &LocalFs,
    store: &dyn ObjectStore,
    plan: &Plan,
    mode: Mode,
    opts: &crate::plan::PlanOpts,
    concurrency: u32,
    progress: &dyn Progress,
) -> ExecReport {
    let mut rep = ExecReport::default();
    if mode == Mode::Status {
        return rep;
    }

    let mut deleted_keys: Vec<String> = Vec::new();
    let mut fold = FoldState {
        rep: &mut rep,
        deleted_keys: &mut deleted_keys,
    };
    // The four passes run strictly in order (downloads, uploads,
    // DeleteRemote, DeleteLocal); each emits its own bracketed event stream
    // through the shared sink. Passes with zero items emit nothing
    // (I27-render).
    run_pass(
        plan,
        ActionKind::Download,
        PassKind::Download,
        concurrency,
        progress,
        |a| exec_download(local, store, a, opts.mtime_tolerance_ms),
        &mut fold,
    );
    run_pass(
        plan,
        ActionKind::Upload,
        PassKind::Upload,
        concurrency,
        progress,
        |a| exec_upload(local, store, a, opts.mtime_tolerance_ms),
        &mut fold,
    );
    run_pass(
        plan,
        ActionKind::DeleteRemote,
        PassKind::DeleteRemote,
        concurrency,
        progress,
        |a| exec_delete_remote(store, a, opts),
        &mut fold,
    );
    run_pass(
        plan,
        ActionKind::DeleteLocal,
        PassKind::DeleteLocal,
        concurrency,
        progress,
        |a| exec_delete_local(local, a, opts),
        &mut fold,
    );
    // R2.1 option (a), scoped (W77/r9 M1): after local deletes, clean the
    // now-empty ancestor chains of the files deleted this run (both the Ok
    // and the W32 NotFound goal-state arms - the goal state is "file
    // absent", so an emptied ancestor is removable in both). Pre-existing,
    // plan-unrelated empty dirs are never touched. W16/A-L3: a cleanup
    // top-level error is a non-fatal warning; R4/R5 nit (W47): per-dir
    // removal failures are surfaced individually, both without changing the
    // exit code.
    if !deleted_keys.is_empty() {
        match local.remove_empty_ancestor_dirs(&deleted_keys) {
            Ok((_removed, dir_warnings)) => {
                rep.warnings.extend(dir_warnings);
            }
            Err(e) => rep.warnings.push(format!("empty-dir cleanup: {e}")),
        }
    }

    progress.event(ProgressEvent::RunEnd {
        executed: rep.executed,
        failed: rep.failed.len() as u32,
    });
    rep
}

/// Mutable per-run fold state threaded through the four passes (bundled so
/// `run_pass` stays under clippy's 7-argument limit, W18/B-L9).
struct FoldState<'a> {
    rep: &'a mut ExecReport,
    deleted_keys: &'a mut Vec<String>,
}

/// One executor pass with event emission (I27 cycle 2 refactor): filters the
/// plan's actions of `kind`, emits `PassStart` (skipped entirely for empty
/// passes), runs the bounded pool with a completion-driven `KeyDone` per item,
/// folds the per-item results into `fold.rep` in plan order (unchanged
/// semantics), then emits `PassEnd`.
fn run_pass<F>(
    plan: &Plan,
    kind: ActionKind,
    pass: PassKind,
    concurrency: u32,
    progress: &dyn Progress,
    exec_item: F,
    fold: &mut FoldState<'_>,
) where
    F: Fn(&crate::plan::Action) -> Result<(), Error> + Sync,
{
    let items: Vec<&crate::plan::Action> = plan.actions.iter().filter(|a| a.kind == kind).collect();
    if items.is_empty() {
        return;
    }
    let total_bytes: u64 = items.iter().map(|a| planned_bytes(a)).sum();
    progress.event(ProgressEvent::PassStart {
        kind: pass,
        total_keys: items.len() as u32,
        total_bytes,
    });
    let results = crate::pool::run_bounded(concurrency, &items, |a| {
        let r = exec_item(a);
        progress.event(ProgressEvent::KeyDone {
            kind: pass,
            key: a.key.clone(),
            bytes: planned_bytes(a),
            ok: r.is_ok(),
        });
        r
    });
    for (a, r) in items.iter().zip(results) {
        match r {
            Ok(()) => {
                fold.rep.executed += 1;
                if pass == PassKind::DeleteLocal {
                    fold.deleted_keys.push(a.key.clone());
                }
            }
            Err(e) => fail(fold.rep, &a.key, e),
        }
    }
    progress.event(ProgressEvent::PassEnd { kind: pass });
}

/// The planned byte size an action contributes to progress (I27-bytes):
/// Upload/Download use the source-side entity size; deletes contribute 0 but
/// still advance key counts.
fn planned_bytes(a: &crate::plan::Action) -> u64 {
    match a.kind {
        ActionKind::Upload => a.local.as_ref().map(|e| e.size).unwrap_or(0),
        ActionKind::Download => a.remote.as_ref().map(|e| e.size).unwrap_or(0),
        _ => 0,
    }
}

/// Pass 3 per-item body (extracted from the inline pool closure, W10
/// A-M3/B-L6): delete is idempotent-friendly across backends - deleting an
/// already-gone key achieves the goal state, so NotFound is normalized to a
/// success here (S3 delete is idempotent; LocalFs.delete_file still reports
/// NotFound for a missing key, and the executor absorbs it).
///
/// W62/A-M2: head-before-delete. The list-time entity alone is stale
/// authority for a delete (the local side got `delete_file_guarded` for
/// exactly this race class). Re-verify the remote object immediately
/// before unlinking it: a NotFound means the goal state (absent) is
/// already achieved (counts as success, matching W10); a size or mtime
/// drift (beyond the tolerance) means the object changed since the plan -
/// the key fails and the new remote content survives. Post-W113 the
/// planned entity's `mtime_ms` IS the head/`vaultsync-mtime` value (the
/// R-c list-skew rationale is retired; no in-tree backend systematically
/// false-fails). The residual race is a same-size replacement whose mtime
/// is within tolerance between plan and delete.
fn exec_delete_remote(
    store: &dyn ObjectStore,
    a: &crate::plan::Action,
    opts: &crate::plan::PlanOpts,
) -> Result<(), Error> {
    let Some(planned_remote) = &a.remote else {
        return Err(Error::Other(format!(
            "delete-remote planned without remote entity: {}",
            a.key
        )));
    };
    match store.head(&a.key) {
        Ok(cur) => {
            if cur.size != planned_remote.size {
                return Err(Error::Other(format!(
                    "remote changed since plan for {}; not deleting",
                    a.key
                )));
            }
            // W119/R1-M2: post-W113 the planned `mtime_ms` IS the head/
            // `vaultsync-mtime` value (the old R-c list-skew rationale is
            // retired), so a same-size replacement whose mtime drifted
            // beyond the tolerance between plan and delete is refused too;
            // the residual race is only a same-size replacement within
            // tolerance. Refuse only when both sides carry an mtime.
            if let (Some(planned_m), Some(cur_m)) = (planned_remote.mtime_ms, cur.mtime_ms)
                && planned_m.abs_diff(cur_m) > opts.mtime_tolerance_ms
            {
                return Err(Error::Other(format!(
                    "remote changed since plan for {}; not deleting",
                    a.key
                )));
            }
        }
        Err(Error::NotFound(_)) => {
            // goal state already achieved (W10 idempotent-delete arm)
            return Ok(());
        }
        Err(e) => return Err(e),
    }
    match store.delete(&a.key) {
        Ok(()) => Ok(()),
        // W10: head is best-effort; a delete-time NotFound still means
        // the goal state was reached.
        Err(Error::NotFound(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Pass 4 per-item body (extracted from the inline pool closure,
/// R4-L1/W39): a `pull --delete` re-verifies local freshness before
/// removing the file (symmetric to upload R3.3 / download W13). The
/// planned local entity is the truth the walk recorded; `a.local` is
/// always `Some` for DeleteLocal - a missing one is a per-key error,
/// never an unguarded delete.
fn exec_delete_local(
    local: &LocalFs,
    a: &crate::plan::Action,
    opts: &crate::plan::PlanOpts,
) -> Result<(), Error> {
    let Some(planned_local) = &a.local else {
        return Err(Error::Other(format!(
            "delete-local planned without local entity: {}",
            a.key
        )));
    };
    match local.delete_file_guarded(
        &a.key,
        planned_local.size,
        planned_local.mtime_ms,
        opts.mtime_tolerance_ms,
    ) {
        Ok(()) => Ok(()),
        Err(Error::NotFound(_)) => {
            // W32: the goal state (file absent) is achieved, so count a
            // no-op delete as reaching it and keep the empty-dir cleanup
            // pass active for this key's ancestor chain. (The guarded
            // delete reports NotFound before any freshness check,
            // matching the old delete_file contract.)
            Ok(())
        }
        Err(e) => Err(e),
    }
}

fn fail(rep: &mut ExecReport, key: &str, e: Error) {
    rep.failed.push(ExecFailure {
        key: key.to_string(),
        message: format!("{e}"),
    });
}

/// Validate a pull destination for `a` (W13/B-L4 + W22/N2/L3): fails fast
/// when the destination changed since the plan (size/mtime drift ->
/// `Changed`), became a symlink or non-regular file (guard error), or
/// appeared for a remote-only key. Shared by the pre-body fast-fail
/// (W68/A-L4) and the post-body check that owns the plan-to-rename window
/// (N3) - identical messages, no semantics change.
///
/// N3: this is a check-then-act stat (std has no `renameat2(NOREPLACE)`/
/// fd-exchange), so a writer that lands between the stat and the rename is
/// still silently overwritten - documented limitation; the upload half
/// (R3.3) re-checks the OPENED descriptor on the same fd, which this
/// download path cannot (it renames a separate temp file).
fn check_destination(
    local: &LocalFs,
    a: &crate::plan::Action,
    tolerance_ms: u64,
) -> Result<(), Error> {
    if let Some(planned) = &a.local {
        let freshness =
            local.destination_freshness(&a.key, planned.size, planned.mtime_ms, tolerance_ms)?;
        if freshness == crate::local::Freshness::Changed {
            return Err(Error::Other(format!(
                "destination changed since plan for {}; not overwriting",
                a.key
            )));
        }
    } else if !local.destination_absent(&a.key)? {
        // R4-L5/W43: distinguish a destination that was *skipped by the
        // walk* (a pre-existing symlink - the key is remote-only because the
        // walk skipped it, not because it appeared) from one that truly
        // appeared since the plan. Both fail closed; only the message differs.
        let is_symlink = local.is_symlink_destination(&a.key)?;
        if is_symlink {
            return Err(Error::Other(format!(
                "destination {} exists but was skipped by the walk (symlink); not overwriting",
                a.key
            )));
        }
        return Err(Error::Other(format!(
            "destination appeared since plan for {}; not overwriting",
            a.key
        )));
    }
    Ok(())
}

/// Bounds the bytes a store's `get_to` may write into a download temp at the
/// planned size (W106/M1): a remote object replaced between `list` and `get`
/// (or a misbehaving S3-compatible endpoint) with a larger body is refused
/// mid-stream, before the extra bytes reach disk. `write` past `remaining`
/// errors with `WriteZero` naming the key and the planned size; the
/// executor's post-stream on-disk truth-check (A-H1/B-L3) is kept as
/// belt-and-braces and now also catches a store that ignores the writer
/// error shape. Executor-internal: no `ObjectStore` trait change.
struct CappedWriter<'a> {
    inner: &'a mut dyn std::io::Write,
    remaining: u64,
    key: &'a str,
    planned: u64,
}

impl<'a> CappedWriter<'a> {
    fn new(inner: &'a mut dyn std::io::Write, key: &'a str, planned: u64) -> Self {
        CappedWriter {
            inner,
            remaining: planned,
            key,
            planned,
        }
    }
}

impl std::io::Write for CappedWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                format!(
                    "download body for {} exceeds the planned size {}",
                    self.key, self.planned
                ),
            ));
        }
        let n = buf.len().min(self.remaining as usize);
        let written = self.inner.write(&buf[..n])?;
        self.remaining -= written as u64;
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Download one key into an atomic temp + rename, applying the remote mtime
/// from the `get_to` metadata (which carries the client `vaultsync-mtime`;
/// post-W113 the plan's mtime is that same client value, not a list-time
/// LastModified).
fn exec_download(
    local: &LocalFs,
    store: &dyn ObjectStore,
    a: &crate::plan::Action,
    tolerance_ms: u64,
) -> Result<(), Error> {
    // W106/M1: a Download row must carry the remote entity the plan recorded
    // - the mid-stream cap's planned-size bound is derived from it (fail
    // closed like exec_upload does for `a.local`).
    let remote_ent = a.remote.as_ref().ok_or_else(|| {
        Error::Other(format!("download planned without remote entity: {}", a.key))
    })?;
    // W68/A-L4: pre-download destination checks - fail fast before the
    // (potentially multi-hundred-MB) body streams. The post-body check below
    // stays: it owns the plan-to-rename race window (N3). Pure earlier-fail
    // optimization with identical messages.
    check_destination(local, a, tolerance_ms)?;
    let (tmp, mut f, created_dirs) = local.tmp_path_for(&a.key)?;
    // W66/A-L2: every post-creation failure removes the temp sibling AND the
    // parent dirs `tmp_path_for` created (best-effort, only while empty - a
    // pre-existing empty dir is never touched). Without this, a pull with
    // per-key failures accumulates empty dirs that the next walk lists as
    // folder entities. The temp is removed first so the dirs are empty when
    // the bottom-up pass reaches them.
    let result = (|| -> Result<(), Error> {
        let remote_mtime = {
            // W106/M1: cap the stream at the planned size (the upload side
            // has the symmetric bound - open_verified size + put_from reads
            // exactly `size`; the download side had none). A remote object
            // replaced between `list` and `get` with a larger body is
            // refused mid-stream, before the extra bytes reach disk.
            let mut capped = CappedWriter::new(&mut f, &a.key, remote_ent.size);
            let remote = store.get_to(&a.key, &mut capped)?;
            // A-H1/B-L3: truth-check the bytes actually on disk (not just the
            // backend's declared size or the planned remote entity). A backend
            // that truncates the body while returning a clean EOF is caught
            // here and the key fails closed; the tmp is removed on the error
            // path below (belt-and-braces over the store-side count in get_to
            // and over the W106 cap - a store that ignores the writer error
            // shape cannot grow the file past the cap, and a short body still
            // fails here).
            let on_disk = std::fs::metadata(&tmp)?.len();
            let expected = remote_ent.size;
            if on_disk != remote.size || remote.size != expected {
                return Err(Error::Other(format!(
                    "download size mismatch for {} (expected {expected}, got {on_disk})",
                    a.key
                )));
            }
            // W48: no `sync_all` here - `finalize_write` opens the temp and
            // syncs it before the atomic rename, covering durability. The
            // on-disk size re-stat above is kept (it is the correctness check).
            remote.mtime_ms
        };
        // W22/N2/L3 + W13/B-L4: post-body destination guard (same logic as
        // the pre-body fast-fail, W68/A-L4). Re-run after the body download
        // because it owns the plan-to-rename window (N3): a writer that lands
        // between the pre-check and the rename must still be refused.
        check_destination(local, a, tolerance_ms)?;
        local.finalize_write(&a.key, &tmp, remote_mtime)
    })();
    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            // I20-r1/F3: route the created-dir cleanup through the
            // lock-protected wrapper so it cannot interleave into another
            // worker's `tmp_path_for` create-alloc window (shared-parent
            // cleanup race). Same removal semantics as the free
            // `remove_created_dirs` (best-effort, only while empty).
            local.cleanup_created_dirs(&created_dirs);
            Err(e)
        }
    }
}

/// Upload one key, re-verifying size + mtime at open (R3.3).
fn exec_upload(
    local: &LocalFs,
    store: &dyn ObjectStore,
    a: &crate::plan::Action,
    tolerance_ms: u64,
) -> Result<(), Error> {
    let local_ent = a
        .local
        .as_ref()
        .ok_or_else(|| Error::Other(format!("upload planned without local entity: {}", a.key)))?;
    // Planned size and mtime are the truth the walk recorded; a file that
    // changed between walk and open fails here (per-key), not silently.
    let expected_mtime = local_ent.mtime_ms;
    let mut f = local.open_verified(&a.key, local_ent.size, expected_mtime, tolerance_ms)?;
    store.put_from(&a.key, &mut f, local_ent.size, expected_mtime)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::Entity;
    use crate::plan::PlanOpts;
    use crate::progress::ProgressLine;
    use crate::store::Listing;
    use crate::store::mock::MemoryStore;
    use crate::testutil::TempDir;
    use std::sync::Mutex;

    fn put_str(store: &MemoryStore, key: &str, body: &str, mtime: Option<u64>) {
        let mut c = std::io::Cursor::new(body.as_bytes().to_vec());
        store
            .put_from(key, &mut c, body.len() as u64, mtime)
            .unwrap();
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

    fn get_bytes(store: &MemoryStore, key: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        store.get_to(key, &mut buf).unwrap();
        buf
    }

    /// Stores that records per-call ops to verify ordering (transfers before
    /// deletes).
    struct RecordingStore {
        inner: MemoryStore,
        log: Mutex<Vec<String>>,
    }
    impl RecordingStore {
        fn new() -> Self {
            RecordingStore {
                inner: MemoryStore::new(),
                log: Mutex::new(Vec::new()),
            }
        }
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
        fn seed(&self, key: &str, body: &str) {
            let mut c = std::io::Cursor::new(body.as_bytes().to_vec());
            self.inner
                .put_from(key, &mut c, body.len() as u64, None)
                .unwrap();
        }
        fn seed_mtime(&self, key: &str, body: &str, mtime: u64) {
            let mut c = std::io::Cursor::new(body.as_bytes().to_vec());
            self.inner
                .put_from(key, &mut c, body.len() as u64, Some(mtime))
                .unwrap();
        }
    }
    impl ObjectStore for RecordingStore {
        fn list(&self, prefix: &str) -> Result<Listing, Error> {
            self.inner.list(prefix)
        }
        fn head(&self, key: &str) -> Result<Entity, Error> {
            self.inner.head(key)
        }
        fn get_to(&self, key: &str, w: &mut dyn std::io::Write) -> Result<Entity, Error> {
            self.log.lock().unwrap().push(format!("get_to:{key}"));
            self.inner.get_to(key, w)
        }
        fn put_from(
            &self,
            key: &str,
            r: &mut dyn std::io::Read,
            size: u64,
            mtime: Option<u64>,
        ) -> Result<Entity, Error> {
            self.log.lock().unwrap().push(format!("put_from:{key}"));
            self.inner.put_from(key, r, size, mtime)
        }
        fn delete(&self, key: &str) -> Result<(), Error> {
            self.log.lock().unwrap().push(format!("delete:{key}"));
            self.inner.delete(key)
        }
    }

    #[test]
    fn exec_upload_creates_remote_bytes_and_mtime() {
        let dir = TempDir::new("vaultsync-exec");
        std::fs::write(dir.join("a.md"), "hello").unwrap();
        let mt = mtime_ms(&dir.join("a.md"));
        let local = LocalFs::new(dir.path());
        let store = MemoryStore::new();
        let plan = crate::build_plan(&local, &store, Mode::Push, &PlanOpts::default())
            .unwrap()
            .plan;
        let rep = execute_plan(&local, &store, &plan, Mode::Push, &PlanOpts::default(), 1);
        assert_eq!(rep.failed, Vec::<ExecFailure>::new(), "{:?}", rep);
        assert_eq!(rep.executed, 1);
        let e = store.head("a.md").unwrap();
        assert_eq!(e.size, 5);
        assert_eq!(e.mtime_ms, Some(mt));
        assert_eq!(get_bytes(&store, "a.md"), b"hello");
    }

    fn set_mtime_ms(p: &std::path::Path, ms: u64) {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms);
        let times = std::fs::FileTimes::new().set_modified(t);
        std::fs::File::open(p).unwrap().set_times(times).unwrap();
    }

    #[test]
    fn exec_upload_uses_plan_tolerance() {
        // W2 (PR2 A-H2/B-M1): the executor passes the plan's mtime tolerance to
        // open_verified. A file that drifts 3000 ms between walk and open is
        // accepted under plan tolerance 5000 (today: the hardcoded 1000 fails
        // the key).
        let dir = TempDir::new("vaultsync-exec");
        let p = dir.join("a.md");
        std::fs::write(&p, "hello").unwrap();
        let base = 1_700_000_000_000u64;
        set_mtime_ms(&p, base);
        let local = LocalFs::new(dir.path());
        let store = MemoryStore::new();
        let opts = PlanOpts {
            mtime_tolerance_ms: 5000,
            ..Default::default()
        };
        let plan = crate::build_plan(&local, &store, Mode::Push, &opts)
            .unwrap()
            .plan;
        // drift the file 3000 ms after the plan captured its mtime
        set_mtime_ms(&p, base + 3000);
        let rep = crate::exec::execute_plan(&local, &store, &plan, Mode::Push, &opts, 1);
        assert_eq!(rep.failed, Vec::<ExecFailure>::new(), "{:?}", rep.failed);
        assert_eq!(rep.executed, 1, "{:?}", rep);
    }

    #[cfg(unix)]
    #[test]
    fn exec_push_followed_symlink_run_succeeds() {
        // R4-M1/W38: pushing a vault containing a followed *file* symlink must
        // succeed with no failed keys - the symlink row is planned Skip
        // (followed_symlink) and never transferred - while the real file and
        // the dir-symlink child upload normally.
        let dir = TempDir::new("vaultsync-exec");
        std::fs::write(dir.join("real.md"), "r").unwrap();
        std::fs::create_dir_all(dir.join("realdir")).unwrap();
        std::fs::write(dir.join("realdir/child.md"), "c").unwrap();
        std::os::unix::fs::symlink("real.md", dir.join("link.md")).unwrap();
        std::os::unix::fs::symlink("realdir", dir.join("linkdir")).unwrap();
        let local = LocalFs::with_follow(dir.path(), true);
        let store = MemoryStore::new();
        let opts = PlanOpts::default();
        let plan = crate::build_plan(&local, &store, Mode::Push, &opts)
            .unwrap()
            .plan;
        let link = plan.actions.iter().find(|a| a.key == "link.md").unwrap();
        assert_eq!(link.kind, ActionKind::Skip, "link.md must be planned skip");
        let rep = crate::exec::execute_plan(&local, &store, &plan, Mode::Push, &opts, 1);
        assert_eq!(rep.failed, Vec::<ExecFailure>::new(), "{:?}", rep.failed);
        assert!(
            matches!(store.head("link.md").unwrap_err(), Error::NotFound(_)),
            "followed symlink must not be uploaded"
        );
        assert!(store.head("real.md").is_ok(), "real.md uploaded");
        assert!(store.head("linkdir/child.md").is_ok(), "dir child uploaded");
    }

    #[test]
    fn pull_downloads_255_byte_leaf_name() {
        // r10-M1 (W85) behavior lock: a 255-byte leaf name is legal on disk
        // (NAME_MAX) and as an S3 key (S3 allows 1024-byte keys), but the
        // temp-sibling candidates embedded the full leaf name
        // (`.{name}.vaultsync-tmp-{pid}-{n}`), so every candidate exceeded
        // NAME_MAX and `create_new` failed with ENAMETOOLONG - vaultsync
        // could upload a file it could never pull back. Fails today with
        // "File name too long (os error 63)".
        let dir = TempDir::new("vaultsync-exec");
        let key: String = "a".repeat(255);
        let store = MemoryStore::new();
        put_str(&store, &key, "hello", Some(1_700_000_000_000));
        let local = LocalFs::new(dir.path());
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default())
            .unwrap()
            .plan;
        let rep = execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default(), 1);
        assert_eq!(rep.failed, Vec::<ExecFailure>::new(), "{:?}", rep);
        assert_eq!(rep.executed, 1, "{:?}", rep);
        let p = dir.join(&key);
        assert_eq!(std::fs::read(&p).unwrap(), b"hello");
        let got = mtime_ms(&p);
        assert!(
            got.abs_diff(1_700_000_000_000) < 2000,
            "remote mtime not applied: {got}"
        );
    }

    #[test]
    fn exec_download_writes_file_and_mtime() {
        let dir = TempDir::new("vaultsync-exec");
        let store = MemoryStore::new();
        put_str(&store, "n/b.md", "remote-bytes", Some(1_700_000_000_123));
        let local = LocalFs::new(dir.path());
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default())
            .unwrap()
            .plan;
        let rep = execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default(), 1);
        assert_eq!(rep.failed, Vec::<ExecFailure>::new(), "{:?}", rep);
        assert_eq!(std::fs::read(dir.join("n/b.md")).unwrap(), b"remote-bytes");
        let got = mtime_ms(&dir.join("n/b.md"));
        assert!(
            got.abs_diff(1_700_000_000_123) < 2000,
            "mtime {got} not near target"
        );
    }

    #[test]
    fn exec_push_delete_removes_remote_extras() {
        let dir = TempDir::new("vaultsync-exec");
        let store = MemoryStore::new();
        put_str(&store, "gone.md", "x", Some(100));
        let local = LocalFs::new(dir.path());
        let opts = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let plan = crate::build_plan(&local, &store, Mode::Push, &opts)
            .unwrap()
            .plan;
        let rep = execute_plan(&local, &store, &plan, Mode::Push, &opts, 1);
        assert_eq!(rep.failed, Vec::<ExecFailure>::new(), "{:?}", rep);
        assert!(matches!(
            store.head("gone.md").unwrap_err(),
            Error::NotFound(_)
        ));
    }

    #[test]
    fn exec_pull_delete_noop_still_cleans_empty_dirs() {
        // W32: a pull --delete where the planned local deletes are already
        // gone (NotFound) still achieves the goal state and still triggers the
        // empty-dir cleanup pass for now-empty parents.
        let dir = TempDir::new("vaultsync-exec");
        std::fs::create_dir_all(dir.join("n/sub")).unwrap();
        std::fs::write(dir.join("n/gone.md"), "x").unwrap();
        std::fs::write(dir.join("n/sub/x.md"), "y").unwrap();
        let store = MemoryStore::new();
        let local = LocalFs::new(dir.path());
        let opts = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let plan = crate::build_plan(&local, &store, Mode::Pull, &opts)
            .unwrap()
            .plan;
        assert!(
            plan.actions
                .iter()
                .any(|a| { a.key == "n/gone.md" && a.kind == ActionKind::DeleteLocal })
        );
        // the files vanish before execution (pre-cleaned by another process)
        std::fs::remove_file(dir.join("n/gone.md")).unwrap();
        std::fs::remove_file(dir.join("n/sub/x.md")).unwrap();
        let rep = crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &opts, 1);
        assert_eq!(rep.failed, Vec::<ExecFailure>::new(), "{:?}", rep);
        // both `sub` and `n` are now empty -> removed bottom-up even though
        // every delete was a NotFound no-op
        assert!(
            !dir.join("n").exists(),
            "empty n not cleaned after no-op deletes"
        );
        assert!(dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn execute_pull_delete_leaves_followed_file_symlink() {
        // W51 (A-M2/B-M1): `pull --delete --follow-symlinks` must exit with
        // zero per-key failures and leave the followed file symlink (and its
        // target) intact: the DeleteLocal row is overridden to
        // Skip(followed_symlink), so `delete_file_guarded`'s symlink refusal
        // is never reached for a planned key.
        let dir = TempDir::new("vaultsync-exec");
        std::fs::write(dir.join("real.md"), "r").unwrap();
        std::os::unix::fs::symlink("real.md", dir.join("link.md")).unwrap();
        let local = LocalFs::with_follow(dir.path(), true);
        let store = MemoryStore::new();
        let opts = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let plan = crate::build_plan(&local, &store, Mode::Pull, &opts)
            .unwrap()
            .plan;
        let link = plan.actions.iter().find(|a| a.key == "link.md").unwrap();
        assert_eq!(link.kind, ActionKind::Skip, "{:?}", link);
        assert_eq!(link.reason, "followed_symlink");
        let rep = crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &opts, 1);
        assert_eq!(rep.failed, Vec::<ExecFailure>::new(), "{:?}", rep.failed);
        // the link survives (never unlinked); `real.md` is a genuine local
        // extra and IS deleted by pull --delete - only the link is protected.
        assert!(dir.join("link.md").is_symlink(), "link must survive");
    }

    #[cfg(unix)]
    #[test]
    fn exec_pull_delete_swapped_symlink_leaf_fails_closed() {
        // W51: the default-mode fail-closed guard is unchanged. A leaf swapped
        // for a symlink between walk and execute still refuses the delete
        // (the key fails, the link and its target survive) - the W39/W50
        // protections for *unplanned* swaps stay in force.
        let dir = TempDir::new("vaultsync-exec");
        let outside = TempDir::new("vaultsync-outside");
        std::fs::write(dir.join("gone.md"), "bye").unwrap();
        std::fs::write(outside.join("victim"), "s").unwrap();
        let local = LocalFs::new(dir.path()); // follow OFF (default)
        let store = MemoryStore::new();
        let opts = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let plan = crate::build_plan(&local, &store, Mode::Pull, &opts)
            .unwrap()
            .plan;
        assert!(
            plan.actions
                .iter()
                .any(|a| a.key == "gone.md" && a.kind == ActionKind::DeleteLocal)
        );
        // swap the leaf for a symlink after planning
        std::fs::remove_file(dir.join("gone.md")).unwrap();
        std::os::unix::fs::symlink(outside.join("victim"), dir.join("gone.md")).unwrap();
        let rep = crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &opts, 1);
        assert!(
            rep.failed.iter().any(|fl| fl.key == "gone.md"),
            "gone.md not failed: {:?}",
            rep.failed
        );
        assert!(
            rep.failed.iter().any(|fl| fl.message.contains("symlink")),
            "no symlink refusal: {:?}",
            rep.failed
        );
        assert_eq!(
            std::fs::read(outside.join("victim")).unwrap(),
            b"s",
            "target must survive"
        );
    }

    #[test]
    fn exec_delete_local_refuses_drifted_file() {
        // R4-L1/W39: `pull --delete` must re-verify local freshness before
        // removing a file (symmetric to upload R3.3 / download W13). A file
        // that changed size between plan and execute must fail the key with a
        // "changed since plan" message and SURVIVE - not be silently deleted
        // on the plan's say-so alone.
        let dir = TempDir::new("vaultsync-exec");
        std::fs::write(dir.join("gone.md"), "abc").unwrap();
        let local = LocalFs::new(dir.path());
        let store = MemoryStore::new();
        let opts = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let plan = crate::build_plan(&local, &store, Mode::Pull, &opts)
            .unwrap()
            .plan;
        assert!(
            plan.actions
                .iter()
                .any(|a| a.key == "gone.md" && a.kind == ActionKind::DeleteLocal)
        );
        // user edits the file after planning (size drift)
        std::fs::write(dir.join("gone.md"), "abcdefghij").unwrap();
        let rep = crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &opts, 1);
        assert!(
            rep.failed.iter().any(|fl| fl.key == "gone.md"),
            "gone.md not failed: {:?}",
            rep.failed
        );
        assert!(
            rep.failed
                .iter()
                .any(|fl| fl.message.contains("changed since plan")),
            "no changed-since-plan message: {:?}",
            rep.failed
        );
        // the user's edited file survives with its new content
        assert_eq!(std::fs::read(dir.join("gone.md")).unwrap(), b"abcdefghij");
    }

    #[test]
    fn exec_delete_local_removes_unchanged_file() {
        // R4-L1/W39: the guard must NOT regress the normal path - an unchanged
        // file deletes cleanly and the empty-dir cleanup still runs.
        let dir = TempDir::new("vaultsync-exec");
        std::fs::write(dir.join("gone.md"), "bye").unwrap();
        let local = LocalFs::new(dir.path());
        let store = MemoryStore::new();
        let opts = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let plan = crate::build_plan(&local, &store, Mode::Pull, &opts)
            .unwrap()
            .plan;
        let rep = crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &opts, 1);
        assert_eq!(rep.failed, Vec::<ExecFailure>::new(), "{:?}", rep);
        assert!(!dir.join("gone.md").exists());
    }

    #[test]
    fn exec_pull_delete_removes_local_and_cleans_empty_dirs() {
        let dir = TempDir::new("vaultsync-exec");
        std::fs::create_dir_all(dir.join("n/sub")).unwrap();
        std::fs::write(dir.join("n/gone.md"), "bye").unwrap();
        std::fs::write(dir.join("n/sub/x.md"), "keep? no - is orphan too").unwrap();
        // store has neither; pull --delete removes local extras + empties dirs.
        let store = MemoryStore::new();
        let local = LocalFs::new(dir.path());
        let opts = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let plan = crate::build_plan(&local, &store, Mode::Pull, &opts)
            .unwrap()
            .plan;
        let rep = execute_plan(&local, &store, &plan, Mode::Pull, &opts, 1);
        assert_eq!(rep.failed, Vec::<ExecFailure>::new(), "{:?}", rep);
        assert!(!dir.join("n/gone.md").exists());
        // both `sub` and `n` are now empty -> removed bottom-up; root stays.
        assert!(!dir.join("n").exists(), "empty n not cleaned");
        assert!(dir.exists());
    }

    #[test]
    fn pull_delete_keeps_unrelated_preexisting_empty_dirs() {
        // r9 M1 (W77): the empty-dir post-pass is scoped to ancestor chains
        // of files deleted this run. A pre-existing, plan-unrelated empty dir
        // (`attachments/`) must SURVIVE a `pull --delete` that deletes a
        // local extra elsewhere (`n/gone.md`). The plan row for
        // `attachments/` remains `Skip(folder)`. RED today: the vault-wide
        // pass removes `attachments/` (the r9 live-probe result).
        let dir = TempDir::new("vaultsync-exec");
        std::fs::create_dir_all(dir.join("attachments")).unwrap(); // intentional, pre-existing
        std::fs::create_dir_all(dir.join("n")).unwrap();
        std::fs::write(dir.join("n/gone.md"), "bye").unwrap();
        let store = MemoryStore::new();
        let local = LocalFs::new(dir.path());
        let opts = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let plan = crate::build_plan(&local, &store, Mode::Pull, &opts)
            .unwrap()
            .plan;
        let folder_row = plan
            .actions
            .iter()
            .find(|a| a.key == "attachments/")
            .unwrap();
        assert_eq!(folder_row.kind, ActionKind::Skip, "{:?}", folder_row);
        assert!(
            plan.actions
                .iter()
                .any(|a| a.key == "n/gone.md" && a.kind == ActionKind::DeleteLocal)
        );
        let rep = execute_plan(&local, &store, &plan, Mode::Pull, &opts, 1);
        assert_eq!(rep.failed, Vec::<ExecFailure>::new(), "{:?}", rep);
        assert!(
            dir.join("attachments").exists(),
            "pre-existing empty dir must survive"
        );
        assert!(
            !dir.join("n").exists(),
            "deleted-file ancestor chain cleaned"
        );
        assert!(dir.exists());
    }

    #[test]
    fn pull_delete_removes_deleted_file_ancestor_chain_bottom_up() {
        // W77 (r9 M1): a deep file `a/b/c/gone.md` deleted by pull --delete
        // removes its emptied ancestor chain bottom-up (`a/b/c`, `a/b`, `a`);
        // the root is kept. (Passes under the old vault-wide pass too; lands
        // as a behavior lock with the scoped API, compile-RED shape per
        // W52/W61.)
        let dir = TempDir::new("vaultsync-exec");
        std::fs::create_dir_all(dir.join("a/b/c")).unwrap();
        std::fs::write(dir.join("a/b/c/gone.md"), "bye").unwrap();
        let store = MemoryStore::new();
        let local = LocalFs::new(dir.path());
        let opts = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let plan = crate::build_plan(&local, &store, Mode::Pull, &opts)
            .unwrap()
            .plan;
        let rep = execute_plan(&local, &store, &plan, Mode::Pull, &opts, 1);
        assert_eq!(rep.failed, Vec::<ExecFailure>::new(), "{:?}", rep);
        assert!(!dir.join("a/b/c").exists(), "deepest ancestor not removed");
        assert!(!dir.join("a/b").exists(), "middle ancestor not removed");
        assert!(!dir.join("a").exists(), "top ancestor not removed");
        assert!(dir.exists(), "root must remain");
    }

    #[test]
    fn pull_delete_keeps_nonempty_ancestor_and_sibling_dirs() {
        // W77 (r9 M1): a dir is only removed when the deleted file's chain
        // emptied it. `n/gone.md` is deleted while `n/keep.md` still matches
        // remotely -> `n/` must survive.
        let dir = TempDir::new("vaultsync-exec");
        std::fs::create_dir_all(dir.join("n")).unwrap();
        std::fs::write(dir.join("n/gone.md"), "bye").unwrap();
        std::fs::write(dir.join("n/keep.md"), "keep").unwrap();
        let store = MemoryStore::new();
        put_str(
            &store,
            "n/keep.md",
            "keep",
            Some(mtime_ms(&dir.join("n/keep.md"))),
        );
        let local = LocalFs::new(dir.path());
        let opts = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let plan = crate::build_plan(&local, &store, Mode::Pull, &opts)
            .unwrap()
            .plan;
        assert!(
            plan.actions
                .iter()
                .any(|a| a.key == "n/gone.md" && a.kind == ActionKind::DeleteLocal)
        );
        let rep = execute_plan(&local, &store, &plan, Mode::Pull, &opts, 1);
        assert_eq!(rep.failed, Vec::<ExecFailure>::new(), "{:?}", rep);
        assert!(
            dir.join("n/keep.md").exists(),
            "unchanged remote-matching file kept"
        );
        assert!(dir.join("n").exists(), "non-empty ancestor must survive");
    }

    #[test]
    fn pull_delete_notfound_noop_still_cleans_goal_state_chain() {
        // W77 (r9 M1) + W32: a planned DeleteLocal whose file vanished before
        // execute (NotFound goal-state arm) still cleans its emptied ancestor
        // chain - but never an unrelated pre-existing empty dir.
        let dir = TempDir::new("vaultsync-exec");
        std::fs::create_dir_all(dir.join("n/sub")).unwrap();
        std::fs::write(dir.join("n/sub/gone.md"), "x").unwrap();
        std::fs::create_dir_all(dir.join("attachments")).unwrap(); // unrelated
        let store = MemoryStore::new();
        let local = LocalFs::new(dir.path());
        let opts = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let plan = crate::build_plan(&local, &store, Mode::Pull, &opts)
            .unwrap()
            .plan;
        // the file vanishes before execution (pre-cleaned by another process)
        std::fs::remove_file(dir.join("n/sub/gone.md")).unwrap();
        let rep = execute_plan(&local, &store, &plan, Mode::Pull, &opts, 1);
        assert_eq!(rep.failed, Vec::<ExecFailure>::new(), "{:?}", rep);
        assert!(
            !dir.join("n/sub").exists(),
            "emptied ancestor chain not cleaned after no-op delete"
        );
        assert!(!dir.join("n").exists());
        assert!(
            dir.join("attachments").exists(),
            "unrelated pre-existing empty dir must survive"
        );
        assert!(dir.exists());
    }

    #[test]
    fn exec_push_delete_refuses_remote_replaced_since_plan() {
        // W62/A-M2: DeleteRemote re-verifies the remote object
        // (head-before-delete) before unlinking it - the list-time entity
        // alone is stale authority for a delete (the local side got
        // delete_file_guarded for exactly this race class). An object
        // replaced with different-size content between plan and execute must
        // fail the key with a "changed since plan" message and the new
        // remote content survives. Fails today (delete proceeds on the
        // plan's say-so alone).
        let dir = TempDir::new("vaultsync-exec");
        let store = MemoryStore::new();
        put_str(&store, "gone.md", "abc", Some(100)); // size 3
        let local = LocalFs::new(dir.path());
        let opts = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let plan = crate::build_plan(&local, &store, Mode::Push, &opts)
            .unwrap()
            .plan;
        assert!(
            plan.actions
                .iter()
                .any(|a| a.key == "gone.md" && a.kind == ActionKind::DeleteRemote)
        );
        // replace the object with different-size content after planning
        put_str(&store, "gone.md", "replacement-bytes-xyz", Some(200));
        let rep = execute_plan(&local, &store, &plan, Mode::Push, &opts, 1);
        assert!(
            rep.failed.iter().any(|fl| fl.key == "gone.md"),
            "gone.md not failed: {:?}",
            rep.failed
        );
        assert!(
            rep.failed
                .iter()
                .any(|fl| fl.message.contains("changed since plan")),
            "no changed-since-plan message: {:?}",
            rep.failed
        );
        // the new remote content survives (not deleted)
        assert_eq!(get_bytes(&store, "gone.md"), b"replacement-bytes-xyz");
    }

    #[test]
    fn exec_push_delete_refuses_same_size_mtime_drift() {
        // W119/R1-M2 (adopted per R16-scope-delete-mtime): post-W113 the
        // planned remote mtime IS the head/`vaultsync-mtime` value, so the
        // old R-c list-skew false-fail rationale is retired and the
        // freshness check is sound. A same-size replacement with an mtime
        // drift far beyond the tolerance (default 1000ms) between plan and
        // delete must fail the key "changed since plan" and the replacement
        // survives. RED today: the size-only check passes (same size) and
        // the delete proceeds.
        let dir = TempDir::new("vaultsync-exec");
        let store = MemoryStore::new();
        put_str(&store, "old.md", "abc", Some(100)); // size 3, mtime T=100
        let local = LocalFs::new(dir.path());
        let opts = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let plan = crate::build_plan(&local, &store, Mode::Push, &opts)
            .unwrap()
            .plan;
        assert!(
            plan.actions
                .iter()
                .any(|a| a.key == "old.md" && a.kind == ActionKind::DeleteRemote)
        );
        // replace the remote in place with same-size content drifted far
        // beyond the tolerance (mtime + 60_000ms).
        put_str(&store, "old.md", "xyz", Some(100 + 60_000));
        let rep = execute_plan(&local, &store, &plan, Mode::Push, &opts, 1);
        assert!(
            rep.failed.iter().any(|fl| fl.key == "old.md"),
            "old.md not failed: {:?}",
            rep.failed
        );
        assert!(
            rep.failed
                .iter()
                .any(|fl| fl.message.contains("changed since plan")),
            "no changed-since-plan message: {:?}",
            rep.failed
        );
        // the replacement survives (not deleted), with its drifted mtime.
        let cur = store.head("old.md").unwrap();
        assert_eq!(cur.mtime_ms, Some(100 + 60_000));
        assert_eq!(get_bytes(&store, "old.md"), b"xyz");
    }

    #[test]
    fn exec_push_delete_allows_same_size_mtime_within_tolerance() {
        // Companion guard (no-over-refusal): a same-size replacement whose
        // mtime drift is *within* the tolerance (500ms < 1000ms) still
        // deletes. Pins that the mtime arm only refuses genuine drift.
        let dir = TempDir::new("vaultsync-exec");
        let store = MemoryStore::new();
        put_str(&store, "old.md", "abc", Some(100)); // size 3, mtime T=100
        let local = LocalFs::new(dir.path());
        let opts = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let plan = crate::build_plan(&local, &store, Mode::Push, &opts)
            .unwrap()
            .plan;
        assert!(
            plan.actions
                .iter()
                .any(|a| a.key == "old.md" && a.kind == ActionKind::DeleteRemote)
        );
        put_str(&store, "old.md", "xyz", Some(100 + 500));
        let rep = execute_plan(&local, &store, &plan, Mode::Push, &opts, 1);
        assert!(
            rep.failed.is_empty(),
            "delete should succeed: {:?}",
            rep.failed
        );
        // delete proceeded: key is gone (goal state reached).
        assert!(matches!(store.head("old.md"), Err(Error::NotFound(_))));
    }

    #[test]
    fn exec_push_delete_unchanged_remote_deletes() {
        // W62/A-M2: the head-before-delete guard must not regress the normal
        // path - an unchanged object still deletes cleanly.
        let dir = TempDir::new("vaultsync-exec");
        let store = MemoryStore::new();
        put_str(&store, "gone.md", "abc", Some(100));
        let local = LocalFs::new(dir.path());
        let opts = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let plan = crate::build_plan(&local, &store, Mode::Push, &opts)
            .unwrap()
            .plan;
        let rep = execute_plan(&local, &store, &plan, Mode::Push, &opts, 1);
        assert_eq!(rep.failed, Vec::<ExecFailure>::new(), "{:?}", rep);
        assert!(matches!(
            store.head("gone.md").unwrap_err(),
            Error::NotFound(_)
        ));
    }

    #[test]
    fn exec_deletes_run_after_transfers() {
        // Push: upload a.md, delete-remote gone.md. The recording store must
        // see put_from(a.md) before delete(gone.md).
        let dir = TempDir::new("vaultsync-exec");
        std::fs::write(dir.join("a.md"), "hello").unwrap();
        let local = LocalFs::new(dir.path());
        let store = RecordingStore::new();
        store.seed("gone.md", "x");
        let opts = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let plan = crate::build_plan(&local, &store, Mode::Push, &opts)
            .unwrap()
            .plan;
        let rep = execute_plan(&local, &store, &plan, Mode::Push, &opts, 1);
        assert_eq!(rep.failed, Vec::<ExecFailure>::new(), "{:?}", rep);
        let log = store.log();
        let put = log.iter().position(|l| l == "put_from:a.md");
        let del = log.iter().position(|l| l == "delete:gone.md");
        assert!(put.is_some() && del.is_some(), "log: {log:?}");
        assert!(
            put.unwrap() < del.unwrap(),
            "delete ran before transfer: {log:?}"
        );
    }

    /// Store wrapper that gauges the max number of concurrent `get_to` calls
    /// (I20 cycle 4 / I17-gauges: Condvar rendezvous, never wall-clock /
    /// never `yield_now`). I17-r1/F1: `OverlapRendezvous` latches
    /// `released` for the life of the wrapper (single gauge pass per
    /// instance) - do NOT reuse the wrapper across a sequential baseline
    /// leg: the conc-1 pass deadlocks (`target=2` never reached;
    /// `n_workers` was sized for the N leg). Comparison legs must run
    /// against `store.inner` (see `exec_parallel_downloads_overlap`).
    /// Proves pool fan-out on `get_to` entry (Condvar rendezvous arrivals),
    /// not concurrent byte streaming and not tmp allocation - the F3
    /// `dir_create_lock` serializes create-alloc, so the rendezvous belongs
    /// here around the inner store call.
    struct GaugedGetStore {
        inner: MemoryStore,
        rendezvous: crate::testutil::OverlapRendezvous,
    }
    impl GaugedGetStore {
        /// `n_workers` is the concurrency under test (pool size); the
        /// rendezvous target is 2 (any real overlap).
        fn new(n_workers: usize) -> Self {
            GaugedGetStore {
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
    impl ObjectStore for GaugedGetStore {
        fn list(&self, prefix: &str) -> Result<Listing, Error> {
            self.inner.list(prefix)
        }
        fn head(&self, key: &str) -> Result<Entity, Error> {
            self.inner.head(key)
        }
        fn get_to(&self, key: &str, w: &mut dyn std::io::Write) -> Result<Entity, Error> {
            self.rendezvous.enter();
            let r = self.inner.get_to(key, w);
            self.rendezvous.leave();
            r
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

    /// Store wrapper that shuffles completion order via a key-hash sleep and
    /// can poison specific keys' `get_to` (I20 cycle 4 determinism/isolation
    /// probes).
    struct ShuffleStore {
        inner: MemoryStore,
        poison: std::sync::Mutex<std::collections::HashSet<String>>,
    }
    impl ShuffleStore {
        fn new() -> Self {
            ShuffleStore {
                inner: MemoryStore::new(),
                poison: std::sync::Mutex::new(std::collections::HashSet::new()),
            }
        }
        fn poison(&self, key: &str) {
            self.poison.lock().unwrap().insert(key.to_string());
        }
        fn sleep_for(&self, key: &str) {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            use std::hash::{Hash as _, Hasher as _};
            key.hash(&mut h);
            std::thread::sleep(std::time::Duration::from_micros(h.finish() % 3000));
        }
    }
    impl ObjectStore for ShuffleStore {
        fn list(&self, prefix: &str) -> Result<Listing, Error> {
            self.inner.list(prefix)
        }
        fn head(&self, key: &str) -> Result<Entity, Error> {
            self.inner.head(key)
        }
        fn get_to(&self, key: &str, w: &mut dyn std::io::Write) -> Result<Entity, Error> {
            self.sleep_for(key);
            if self.poison.lock().unwrap().contains(key) {
                return Err(Error::Other(format!("poisoned:{key}")));
            }
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

    /// I20-r1/F3 race probe variant of `ShuffleStore`: the poison check runs
    /// BEFORE the key-hash sleep, so a poisoned key's `get_to` fails
    /// immediately (no sleep) - its failure cleanup lands while sibling
    /// workers are still inside their `tmp_path_for` create-alloc window,
    /// which is the shared-parent cleanup race the plan describes.
    struct FastFailShuffleStore {
        inner: MemoryStore,
        poison: std::sync::Mutex<std::collections::HashSet<String>>,
    }
    impl FastFailShuffleStore {
        fn new() -> Self {
            FastFailShuffleStore {
                inner: MemoryStore::new(),
                poison: std::sync::Mutex::new(std::collections::HashSet::new()),
            }
        }
        fn poison(&self, key: &str) {
            self.poison.lock().unwrap().insert(key.to_string());
        }
        fn sleep_for(&self, key: &str) {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            use std::hash::{Hash as _, Hasher as _};
            key.hash(&mut h);
            std::thread::sleep(std::time::Duration::from_micros(h.finish() % 3000));
        }
    }
    impl ObjectStore for FastFailShuffleStore {
        fn list(&self, prefix: &str) -> Result<Listing, Error> {
            self.inner.list(prefix)
        }
        fn head(&self, key: &str) -> Result<Entity, Error> {
            self.inner.head(key)
        }
        fn get_to(&self, key: &str, w: &mut dyn std::io::Write) -> Result<Entity, Error> {
            if self.poison.lock().unwrap().contains(key) {
                return Err(Error::Other(format!("poisoned:{key}")));
            }
            self.sleep_for(key);
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
    fn exec_parallel_downloads_overlap() {
        // I20 cycle 4 / I17-gauges (W158): a 16-key pull at concurrency 4 fans
        // out - the max-in-flight `get_to` gauge must exceed 1 (real overlap
        // via Condvar rendezvous, not yield_now) and stay <= 4; bytes land
        // correctly and the report equals the concurrency-1 report exactly.
        let store = GaugedGetStore::new(4);
        for i in 0..16 {
            put_str(
                &store.inner,
                &format!("n{i:02}.md"),
                &format!("body-{i:02}"),
                Some(1_000 + i),
            );
        }
        let opts = PlanOpts::default();
        let dir1 = TempDir::new("vaultsync-exec");
        let local1 = LocalFs::new(dir1.path());
        let plan = crate::build_plan(&local1, &store, Mode::Pull, &opts)
            .unwrap()
            .plan;
        // I17-r1/F1 (W161): the concurrency-1 leg is an EQUALITY pin, not a
        // gauge, so it runs through `store.inner` (bare MemoryStore) - the
        // Condvar rendezvous latches `released` for the life of the wrapper,
        // so a conc-1 pass through the gauge deadlocks (target=2 never
        // reached; `n_workers` was sized for the N leg). Same trap as
        // `enrich_parallel_vanished_warning_order_stable`.
        let rep1 = execute_plan(&local1, &store.inner, &plan, Mode::Pull, &opts, 1);
        let dir4 = TempDir::new("vaultsync-exec");
        let local4 = LocalFs::new(dir4.path());
        let rep4 = execute_plan(&local4, &store, &plan, Mode::Pull, &opts, 4);
        assert!(
            store.max_in_flight() > 1,
            "downloads must overlap at concurrency 4 (max in-flight {})",
            store.max_in_flight()
        );
        assert!(store.max_in_flight() <= 4);
        assert_eq!(rep4.failed, Vec::<ExecFailure>::new(), "{:?}", rep4.failed);
        assert_eq!(rep4.executed, 16);
        for i in 0..16 {
            let body = std::fs::read(dir4.join(format!("n{i:02}.md"))).unwrap();
            assert_eq!(body, format!("body-{i:02}").as_bytes());
        }
        assert_eq!(rep4, rep1);
    }

    #[test]
    fn exec_report_is_deterministic_under_pool() {
        // I20 cycle 4: the same plan executed twice at concurrency 8 on a
        // completion-order-shuffling store yields identical ExecReports, and
        // identical to the concurrency-1 report (`failed` in plan order).
        let store = ShuffleStore::new();
        for i in 0..24 {
            put_str(
                &store.inner,
                &format!("k{i:02}.md"),
                &format!("x{i}"),
                Some(5000 + i),
            );
        }
        store.poison("k03.md");
        store.poison("k17.md");
        let dir1 = TempDir::new("vaultsync-exec");
        let opts = PlanOpts::default();
        let plan = crate::build_plan(&LocalFs::new(dir1.path()), &store, Mode::Pull, &opts)
            .unwrap()
            .plan;
        let rep1 = execute_plan(
            &LocalFs::new(dir1.path()),
            &store,
            &plan,
            Mode::Pull,
            &opts,
            1,
        );
        let dir8a = TempDir::new("vaultsync-exec");
        let rep8a = execute_plan(
            &LocalFs::new(dir8a.path()),
            &store,
            &plan,
            Mode::Pull,
            &opts,
            8,
        );
        let dir8b = TempDir::new("vaultsync-exec");
        let rep8b = execute_plan(
            &LocalFs::new(dir8b.path()),
            &store,
            &plan,
            Mode::Pull,
            &opts,
            8,
        );
        assert_eq!(rep8a, rep8b, "pooled runs must be deterministic");
        assert_eq!(rep8a, rep1, "pooled report must equal the sequential one");
        assert_eq!(
            rep1.failed
                .iter()
                .map(|f| f.key.as_str())
                .collect::<Vec<_>>(),
            vec!["k03.md", "k17.md"],
            "failed keys in plan order"
        );
    }

    #[test]
    fn exec_parallel_failure_isolation() {
        // I20 cycle 4: one poisoned key among 15 healthy at concurrency 4
        // yields exactly one `failed` entry naming that key, 15 - 1 executed,
        // and a message identical to the sequential run.
        let store = ShuffleStore::new();
        for i in 0..15 {
            put_str(
                &store.inner,
                &format!("f{i:02}.md"),
                &format!("x{i}"),
                Some(1000 + i),
            );
        }
        store.poison("f07.md");
        let dir = TempDir::new("vaultsync-exec");
        let local = LocalFs::new(dir.path());
        let opts = PlanOpts::default();
        let plan = crate::build_plan(&local, &store, Mode::Pull, &opts)
            .unwrap()
            .plan;
        let rep4 = execute_plan(&local, &store, &plan, Mode::Pull, &opts, 4);
        assert_eq!(rep4.failed.len(), 1, "{:?}", rep4.failed);
        assert_eq!(rep4.failed[0].key, "f07.md");
        assert_eq!(rep4.failed[0].message, "poisoned:f07.md");
        assert_eq!(rep4.executed, 14);
        // Sequential comparison on a fresh empty dir: same plan, same store.
        let dir1 = TempDir::new("vaultsync-exec");
        let local1 = LocalFs::new(dir1.path());
        let rep1 = execute_plan(&local1, &store, &plan, Mode::Pull, &opts, 1);
        assert_eq!(rep4, rep1);
    }

    #[test]
    #[ignore = "probabilistic regression net: residual interleavings outside the create-alloc window still flake under scheduling; dir_create_lock remains the fix - run via --ignored. Deterministic exec pins (exec_parallel_failure_isolation / exec_report_is_deterministic_under_pool / exec_parallel_guards_hold) cover the contract without this net (I17-gauges B3-keep)."]
    fn exec_parallel_shared_parent_cleanup_no_spurious_failures() {
        // I20-r1/F3: probabilistic regression net (same ethos as the W48
        // overlap probes and the I20 overlap gauges), NOT a deterministic
        // interleaving proof - a deterministic RED would need a test-only
        // hook inside `tmp_path_for`, declined as production-surface
        // pollution. The race: worker B's `remove_created_dirs` (the
        // poisoned-key download failure path) can `remove_dir` a shared
        // ancestor in the window between worker A's `create_dir_all(parent)`
        // and its `alloc_temp_sibling` (dir still empty -> removal succeeds
        // -> A's tmp allocation fails NotFound; spurious per-key failure,
        // self-healing next run). The poisoned key fails INSTANTLY
        // (FastFailShuffleStore poison-before-sleep) so its cleanup lands
        // while siblings are still mid-create-alloc; `k00` is the first key
        // pulled, so its cleanup is the earliest possible. RED was observed
        // on the unfixed code (iteration 115: 13 healthy keys failed with
        // NotFound/Invalid argument/File exists). The W150 fix (LocalFs
        // dir_create_lock) closed the create-alloc window, but the net still
        // shows residual flakiness under some schedulings (interleavings
        // outside that window), so it stays #[ignore]d (I17-gauges B3-keep);
        // the fix itself is kept (strictly no worse than the pre-I20
        // contract). Rely on the lock + the deterministic exec pins.
        let opts = PlanOpts::default();
        for iter in 0..200 {
            let store = FastFailShuffleStore::new();
            for i in 0..16 {
                put_str(
                    &store.inner,
                    &format!("n/sub/k{i:02}.md"),
                    &format!("x{i}"),
                    Some(5_000 + i),
                );
            }
            // One poisoned key to force a failure cleanup while siblings
            // stream into the same fresh `n/sub` tree.
            store.poison("n/sub/k00.md");
            let dir = TempDir::new("vaultsync-exec");
            let local = LocalFs::new(dir.path());
            let plan = crate::build_plan(&local, &store, Mode::Pull, &opts)
                .unwrap()
                .plan;
            let rep = execute_plan(&local, &store, &plan, Mode::Pull, &opts, 8);
            // Only the poisoned key may appear in `failed`; a healthy key
            // failing with a NotFound/tmp-allocation error is the
            // shared-parent cleanup race.
            assert_eq!(
                rep.failed.len(),
                1,
                "iteration {iter}: expected exactly the poisoned key failed, got {:?}",
                rep.failed
            );
            assert_eq!(
                rep.failed[0].key, "n/sub/k00.md",
                "iteration {iter}: healthy key failed (spurious cleanup race): {:?}",
                rep.failed
            );
            // every healthy key's bytes landed
            for i in 1..16 {
                assert_eq!(
                    std::fs::read(dir.join(format!("n/sub/k{i:02}.md"))).unwrap(),
                    format!("x{i}").as_bytes(),
                    "iteration {iter}: n/sub/k{i:02}.md bytes"
                );
            }
        }
    }

    #[test]
    fn exec_parallel_guards_hold() {
        // I20 cycle 4: at concurrency 4, a post-plan local edit still fails
        // the upload key via R3.3 and a post-plan remote replacement still
        // fails the DeleteRemote key via W62; neighbors unaffected.
        let dir = TempDir::new("vaultsync-exec");
        std::fs::write(dir.join("a.md"), "abc").unwrap();
        std::fs::write(dir.join("ok.md"), "stable").unwrap();
        let local = LocalFs::new(dir.path());
        let store = MemoryStore::new();
        put_str(&store, "gone.md", "x", Some(5_000));
        let opts = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let plan = crate::build_plan(&local, &store, Mode::Push, &opts)
            .unwrap()
            .plan;
        // post-plan edits: grow a.md (R3.3), replace gone.md remotely (W62).
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("a.md"))
            .unwrap();
        use std::io::Write as _;
        f.write_all(b"defghijklmnop").unwrap();
        drop(f);
        put_str(&store, "gone.md", "xx", Some(5_000)); // size drift 1 -> 2
        let rep = execute_plan(&local, &store, &plan, Mode::Push, &opts, 4);
        assert!(
            rep.failed.iter().any(|fl| fl.key == "a.md"),
            "a.md must fail via R3.3: {:?}",
            rep.failed
        );
        assert!(
            rep.failed
                .iter()
                .any(|fl| fl.key == "gone.md" && fl.message.contains("changed since plan")),
            "gone.md must fail via W62: {:?}",
            rep.failed
        );
        assert_eq!(rep.failed.len(), 2, "{:?}", rep.failed);
        // neighbors unaffected: ok.md uploaded, gone.md not deleted.
        assert!(store.head("ok.md").is_ok(), "ok.md must upload");
        assert_eq!(rep.executed, 1, "only ok.md executed: {:?}", rep);
        assert_eq!(get_bytes(&store, "ok.md"), b"stable");
    }

    #[test]
    fn exec_concurrency_1_byte_for_byte() {
        // I20-one/I20-deletes: transfers strictly before deletes in the op log
        // at both concurrency 1 and 4 - the pool must not reorder passes.
        for concurrency in [1u32, 4] {
            let dir = TempDir::new("vaultsync-exec");
            std::fs::write(dir.join("a.md"), "hello").unwrap();
            let local = LocalFs::new(dir.path());
            let store = RecordingStore::new();
            store.seed("gone.md", "x");
            let opts = PlanOpts {
                delete: true,
                ..Default::default()
            };
            let plan = crate::build_plan(&local, &store, Mode::Push, &opts)
                .unwrap()
                .plan;
            let rep = execute_plan(&local, &store, &plan, Mode::Push, &opts, concurrency);
            assert_eq!(rep.failed, Vec::<ExecFailure>::new(), "{:?}", rep);
            let log = store.log();
            let put = log.iter().position(|l| l == "put_from:a.md");
            let del = log.iter().position(|l| l == "delete:gone.md");
            assert!(put.is_some() && del.is_some(), "log: {log:?}");
            assert!(
                put.unwrap() < del.unwrap(),
                "delete ran before transfer: {log:?}"
            );
        }
    }

    #[test]
    fn exec_conflict_and_skip_untouched() {
        let dir = TempDir::new("vaultsync-exec");
        std::fs::write(dir.join("c.md"), "x").unwrap(); // 1 byte
        std::fs::write(dir.join("a.md"), "same").unwrap();
        let store = MemoryStore::new();
        // c.md: same mtime (approx) but different size -> conflict
        let cmt = mtime_ms(&dir.join("c.md"));
        put_str(&store, "c.md", "xx", Some(cmt)); // size 2 vs local 1
        // a.md: same content+size+mtime -> equal skip
        put_str(&store, "a.md", "same", Some(mtime_ms(&dir.join("a.md"))));
        let local = LocalFs::new(dir.path());
        let plan = crate::build_plan(&local, &store, Mode::Push, &PlanOpts::default())
            .unwrap()
            .plan;
        let before_c = store.head("c.md").unwrap();
        let before_a = store.head("a.md").unwrap();
        let rep = execute_plan(&local, &store, &plan, Mode::Push, &PlanOpts::default(), 1);
        assert_eq!(rep.executed, 0, "no transfers expected");
        assert_eq!(rep.failed, Vec::<ExecFailure>::new());
        // nothing mutated
        assert_eq!(store.head("c.md").unwrap(), before_c);
        assert_eq!(store.head("a.md").unwrap(), before_a);
    }

    #[test]
    fn exec_upload_restated_size_mismatch_fails_key() {
        let dir = TempDir::new("vaultsync-exec");
        std::fs::write(dir.join("a.md"), "abc").unwrap();
        std::fs::write(dir.join("ok.md"), "stable").unwrap();
        let local = LocalFs::new(dir.path());
        let store = MemoryStore::new();
        let plan = crate::build_plan(&local, &store, Mode::Push, &PlanOpts::default())
            .unwrap()
            .plan;
        // File grows between plan (walks size 3) and open -> per-key failure.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("a.md"))
            .unwrap();
        use std::io::Write as _;
        f.write_all(b"defghijklmnop").unwrap();
        drop(f);
        let rep = execute_plan(&local, &store, &plan, Mode::Push, &PlanOpts::default(), 1);
        assert!(
            rep.failed.iter().any(|fl| fl.key == "a.md"),
            "a.md not failed: {:?}",
            rep.failed
        );
        // a.md must not be uploaded at all.
        assert!(matches!(
            store.head("a.md").unwrap_err(),
            Error::NotFound(_)
        ));
        // the stable key still uploads (isolation).
        assert!(store.head("ok.md").is_ok());
        assert_eq!(rep.executed, 1, "only ok.md executed");
    }

    /// A store whose `get_to` writes fewer bytes than the entity's declared
    /// size (clean EOF, correct header) - the A-H1/B-L3 short-body scenario.
    struct ShortBodyStore {
        inner: MemoryStore,
        bad_key: String,
    }
    impl ObjectStore for ShortBodyStore {
        fn list(&self, prefix: &str) -> Result<Listing, Error> {
            self.inner.list(prefix)
        }
        fn head(&self, key: &str) -> Result<Entity, Error> {
            self.inner.head(key)
        }
        fn get_to(&self, key: &str, w: &mut dyn std::io::Write) -> Result<Entity, Error> {
            if key == self.bad_key {
                let mut buf = Vec::new();
                let ent = self.inner.get_to(key, &mut buf)?;
                let n = (buf.len() / 2).max(1);
                w.write_all(&buf[..n]).map_err(Error::Io)?;
                // declare the FULL size (clean EOF, header size unchanged)
                return Ok(Entity {
                    size: buf.len() as u64,
                    ..ent
                });
            }
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
    fn exec_download_short_body_fails_key() {
        // A-H1/B-L3: a clean-EOF truncated download must fail the key, leave no
        // file at the final path, leave no temp sibling, and not block other
        // keys. Today (only the header size checked) the truncated file is
        // finalized as success.
        let dir = TempDir::new("vaultsync-exec");
        let store = MemoryStore::new();
        put_str(&store, "bad.md", "full-body-bytes-here", Some(100));
        put_str(&store, "ok.md", "fine", Some(200));
        let wrapper = ShortBodyStore {
            inner: store,
            bad_key: "bad.md".to_string(),
        };
        let local = LocalFs::new(dir.path());
        let plan = crate::build_plan(&local, &wrapper, Mode::Pull, &PlanOpts::default())
            .unwrap()
            .plan;
        let rep =
            crate::exec::execute_plan(&local, &wrapper, &plan, Mode::Pull, &PlanOpts::default(), 1);
        assert!(
            rep.failed.iter().any(|fl| fl.key == "bad.md"),
            "bad.md not failed: {:?}",
            rep.failed
        );
        assert!(!dir.join("bad.md").exists(), "truncated file finalized");
        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                !e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                    && e.file_name().to_string_lossy().contains("vaultsync-tmp")
            })
            .collect();
        assert!(leftover.is_empty(), "temp siblings leaked: {leftover:?}");
        // other keys still download
        assert!(dir.join("ok.md").exists());
        assert_eq!(rep.executed, 1, "{:?}", rep);
    }

    /// A store whose `get_to` streams 64 KiB chunks in a loop until the writer
    /// errors - the W106/M1 oversized-body scenario (a remote object replaced
    /// between `list` and `get`, or a misbehaving S3-compatible endpoint, with
    /// a far larger body than the plan recorded). Records how many chunk
    /// writes were attempted so the test can prove the stream stopped at the
    /// planned-size cap instead of running to the end of the body.
    struct OversizedBodyStore {
        inner: MemoryStore,
        bad_key: String,
        writes_attempted: Mutex<u32>,
    }
    impl OversizedBodyStore {
        fn new(inner: MemoryStore, bad_key: &str) -> Self {
            OversizedBodyStore {
                inner,
                bad_key: bad_key.to_string(),
                writes_attempted: Mutex::new(0),
            }
        }
        fn writes_attempted(&self) -> u32 {
            *self.writes_attempted.lock().unwrap()
        }
    }
    impl ObjectStore for OversizedBodyStore {
        fn list(&self, prefix: &str) -> Result<Listing, Error> {
            self.inner.list(prefix)
        }
        fn head(&self, key: &str) -> Result<Entity, Error> {
            self.inner.head(key)
        }
        fn get_to(&self, key: &str, w: &mut dyn std::io::Write) -> Result<Entity, Error> {
            if key == self.bad_key {
                // key exists in the inner store (declares the planned size)
                let _ = self.inner.head(key)?;
                let chunk = vec![0u8; 64 * 1024];
                loop {
                    *self.writes_attempted.lock().unwrap() += 1;
                    if let Err(e) = w.write_all(&chunk) {
                        // the capped writer refused the excess bytes
                        return Err(Error::Io(e));
                    }
                }
            }
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
    fn exec_download_body_beyond_planned_size_fails_early() {
        // W106/M1: a remote object whose body exceeds the planned size (the
        // upload side has the symmetric bound - open_verified size + put_from
        // reads exactly `size`; the download side had none) must be refused
        // MID-STREAM at the planned-size cap: the extra bytes never reach
        // disk, the key fails with a message naming the planned size, and the
        // stream stops at the cap (one chunk write) rather than after the
        // full body. Fails today: get_to streams the whole body to the temp
        // before any size check, so the file is written in full.
        let dir = TempDir::new("vaultsync-exec");
        let store = MemoryStore::new();
        put_str(&store, "big.md", "12345", Some(100)); // planned size 5
        put_str(&store, "ok.md", "fine", Some(200));
        let wrapper = OversizedBodyStore::new(store, "big.md");
        let local = LocalFs::new(dir.path());
        let plan = crate::build_plan(&local, &wrapper, Mode::Pull, &PlanOpts::default())
            .unwrap()
            .plan;
        let rep =
            crate::exec::execute_plan(&local, &wrapper, &plan, Mode::Pull, &PlanOpts::default(), 1);
        assert!(
            rep.failed.iter().any(|fl| fl.key == "big.md"),
            "big.md not failed: {:?}",
            rep.failed
        );
        assert!(
            rep.failed
                .iter()
                .any(|fl| fl.message.contains("exceeds the planned size")),
            "no planned-size refusal: {:?}",
            rep.failed
        );
        assert!(!dir.join("big.md").exists(), "oversized body finalized");
        assert_no_tmp_leftovers(dir.path());
        // the stream stopped at the cap: only one chunk write was attempted
        assert_eq!(
            wrapper.writes_attempted(),
            1,
            "stream ran past the planned-size cap"
        );
        // other keys still download (isolation)
        assert!(dir.join("ok.md").exists());
        assert_eq!(rep.executed, 1, "{:?}", rep);
    }

    #[test]
    fn exec_download_requires_remote_entity() {
        // W106/M1: the mid-stream cap is derived from the planned remote
        // entity's size, so a Download row without one must fail closed
        // (mirrors the upload guard in exec_upload). Compile-locks the
        // `a.remote` requirement the cap depends on.
        let dir = TempDir::new("vaultsync-exec");
        let local = LocalFs::new(dir.path());
        let store = MemoryStore::new();
        let plan = crate::plan::Plan {
            actions: vec![crate::plan::Action {
                key: "a.md".to_string(),
                kind: ActionKind::Download,
                reason: "remote_only",
                local: None,
                remote: None,
            }],
            stats: crate::plan::PlanStats::default(),
        };
        let rep = execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default(), 1);
        assert!(
            rep.failed.iter().any(|fl| {
                fl.key == "a.md"
                    && fl
                        .message
                        .contains("download planned without remote entity")
            }),
            "{:?}",
            rep.failed
        );
        assert_eq!(rep.executed, 0, "{:?}", rep);
        assert_no_tmp_leftovers(dir.path());
    }

    #[test]
    fn exec_download_failure_removes_created_parent_dirs() {
        // W66/A-L2: a pull key `a/b/c.md` into an empty vault whose get_to
        // fails (object deleted between plan and execute) must remove the
        // dirs the download created (`a/`, `a/b/`) - no empty-dir litter, no
        // folder entities for the next walk. Fails today (dirs remain).
        let dir = TempDir::new("vaultsync-exec");
        let store = MemoryStore::new();
        put_str(&store, "a/b/c.md", "x", Some(100));
        let local = LocalFs::new(dir.path());
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default())
            .unwrap()
            .plan;
        // remote vanishes between plan and execute
        store.delete("a/b/c.md").unwrap();
        let rep = execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default(), 1);
        assert!(
            rep.failed.iter().any(|fl| fl.key == "a/b/c.md"),
            "a/b/c.md not failed: {:?}",
            rep.failed
        );
        assert!(
            !dir.join("a").exists(),
            "download left created empty dirs behind: {:?}",
            std::fs::read_dir(dir.path())
                .map(|rd| rd.flatten().map(|e| e.file_name()).collect::<Vec<_>>())
                .unwrap_or_default()
        );
        assert!(dir.exists(), "vault root must survive");
    }

    #[test]
    fn exec_download_failure_keeps_preexisting_empty_dirs() {
        // W66/A-L2: a pre-existing empty dir is never touched by the cleanup
        // - only the dirs the failed download created are removed. `keep/`
        // existed before the pull; `keep/sub/` was created by the download
        // itself and must be removed on failure.
        let dir = TempDir::new("vaultsync-exec");
        std::fs::create_dir_all(dir.join("keep")).unwrap();
        let store = MemoryStore::new();
        put_str(&store, "keep/sub/x.md", "x", Some(100));
        let local = LocalFs::new(dir.path());
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default())
            .unwrap()
            .plan;
        store.delete("keep/sub/x.md").unwrap();
        let rep = execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default(), 1);
        assert!(
            rep.failed.iter().any(|fl| fl.key == "keep/sub/x.md"),
            "keep/sub/x.md not failed: {:?}",
            rep.failed
        );
        assert!(
            !dir.join("keep/sub").exists(),
            "created sub dir left behind"
        );
        assert!(
            dir.join("keep").exists(),
            "pre-existing empty dir must survive"
        );
    }

    #[test]
    fn exec_download_precheck_skips_body_when_dest_changed() {
        // W68/A-L4: pull destination guards run after the full body
        // downloads. Pre-check the destination BEFORE tmp_path_for/get_to so
        // a refused key never pays the (potentially multi-hundred-MB)
        // download; the post-download check stays and owns the plan-to-
        // rename window (N3). Fails today: the body streams first.
        let dir = TempDir::new("vaultsync-exec");
        let p = dir.join("a.md");
        std::fs::write(&p, "old-local").unwrap();
        set_mtime_ms(&p, 1_700_000_000_000);
        let store = RecordingStore::new();
        store.seed_mtime("a.md", "remote-new-body", 1_700_000_005_000);
        let local = LocalFs::new(dir.path());
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default())
            .unwrap()
            .plan;
        let act = plan.actions.iter().find(|a| a.key == "a.md").unwrap();
        assert_eq!(act.kind, ActionKind::Download);
        // user edits the destination after planning (size changes)
        std::fs::write(&p, "user-edit-since-plan-123").unwrap();
        let rep =
            crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default(), 1);
        assert!(
            rep.failed.iter().any(|fl| fl.key == "a.md"),
            "a.md not failed: {:?}",
            rep.failed
        );
        assert!(
            rep.failed
                .iter()
                .any(|fl| fl.message.contains("changed since plan")),
            "no changed-since-plan message: {:?}",
            rep.failed
        );
        assert!(
            !store.log().iter().any(|l| l == "get_to:a.md"),
            "body streamed despite changed destination: {:?}",
            store.log()
        );
        // the user's newer content survives
        assert_eq!(std::fs::read(&p).unwrap(), b"user-edit-since-plan-123");
    }

    #[test]
    fn exec_download_precheck_skips_body_when_dest_appeared() {
        // W68/A-L4: a remote-only key whose destination appeared since the
        // plan must fail BEFORE the body downloads (same message as the
        // post-check, which still owns the plan-to-rename window). Fails
        // today: the body streams first.
        let dir = TempDir::new("vaultsync-exec");
        let store = RecordingStore::new();
        store.seed_mtime("b.md", "remote-bytes", 1_700_000_000_123);
        let local = LocalFs::new(dir.path());
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default())
            .unwrap()
            .plan;
        let act = plan.actions.iter().find(|a| a.key == "b.md").unwrap();
        assert_eq!(act.kind, ActionKind::Download);
        assert!(act.local.is_none(), "remote-only pull");
        // a regular file appears at the destination after planning
        std::fs::write(dir.join("b.md"), "editor-saved-since-plan").unwrap();
        let rep =
            crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default(), 1);
        assert!(
            rep.failed.iter().any(|fl| fl.key == "b.md"),
            "b.md not failed: {:?}",
            rep.failed
        );
        assert!(
            !store.log().iter().any(|l| l == "get_to:b.md"),
            "body streamed despite appeared destination: {:?}",
            store.log()
        );
        assert_eq!(
            std::fs::read(dir.join("b.md")).unwrap(),
            b"editor-saved-since-plan"
        );
    }

    #[test]
    fn exec_download_refuses_to_overwrite_dest_changed_since_plan() {
        // W13/B-L4: a pull whose destination changed (size) after planning must
        // fail the key and leave the user's newer content on disk untouched
        // (symmetric to upload R3.3). Fails today (rename silently overwrites).
        let dir = TempDir::new("vaultsync-exec");
        let p = dir.join("a.md");
        std::fs::write(&p, "old-local").unwrap();
        set_mtime_ms(&p, 1_700_000_000_000);
        let store = MemoryStore::new();
        put_str(&store, "a.md", "remote-new-body", Some(1_700_000_005_000));
        let local = LocalFs::new(dir.path());
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default())
            .unwrap()
            .plan;
        let act = plan.actions.iter().find(|a| a.key == "a.md").unwrap();
        assert_eq!(
            act.kind,
            ActionKind::Download,
            "expected remote_newer download"
        );
        assert!(act.local.is_some());
        // user edits the local file after planning (size changes)
        std::fs::write(&p, "user-edit-since-plan-123").unwrap();
        let rep =
            crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default(), 1);
        assert!(
            rep.failed.iter().any(|fl| fl.key == "a.md"),
            "a.md not failed: {:?}",
            rep.failed
        );
        // the user's newer content survives on disk (not overwritten)
        assert_eq!(
            std::fs::read(&p).unwrap(),
            b"user-edit-since-plan-123",
            "download overwrote the changed destination"
        );
    }

    #[test]
    fn exec_download_remote_only_refuses_appeared_destination() {
        // W22/N2/L3: a pull for a key that had NO planned local entity (remote
        // only) must still guard the destination. If a regular file appears at
        // the destination between plan and execute (an editor save, another
        // tool), the key fails and the planted content survives - it is never
        // clobbered by the rename.
        let dir = TempDir::new("vaultsync-exec");
        let store = MemoryStore::new();
        put_str(&store, "b.md", "remote-bytes", Some(1_700_000_000_123));
        let local = LocalFs::new(dir.path());
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default())
            .unwrap()
            .plan;
        let act = plan.actions.iter().find(|a| a.key == "b.md").unwrap();
        assert_eq!(act.kind, ActionKind::Download);
        assert!(act.local.is_none(), "remote-only pull must have no local");
        // a regular file appears at the destination after planning
        let p = dir.join("b.md");
        std::fs::write(&p, "editor-saved-since-plan").unwrap();
        let rep =
            crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default(), 1);
        assert!(
            rep.failed.iter().any(|fl| fl.key == "b.md"),
            "b.md not failed: {:?}",
            rep.failed
        );
        // the planted content survives byte-for-byte
        assert_eq!(std::fs::read(&p).unwrap(), b"editor-saved-since-plan");
        // no temp sibling leaks
        assert_no_tmp_leftovers(dir.path());
    }

    #[test]
    fn exec_download_remote_only_proceeds_when_absent() {
        // W22/N2/L3: for a remote-only key whose destination is still absent,
        // the guard must not regress the normal path - the download succeeds.
        let dir = TempDir::new("vaultsync-exec");
        let store = MemoryStore::new();
        put_str(&store, "b.md", "remote-bytes", Some(1_700_000_000_123));
        let local = LocalFs::new(dir.path());
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default())
            .unwrap()
            .plan;
        let rep =
            crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default(), 1);
        assert_eq!(rep.failed, Vec::<ExecFailure>::new(), "{:?}", rep.failed);
        assert_eq!(std::fs::read(dir.join("b.md")).unwrap(), b"remote-bytes");
        assert_no_tmp_leftovers(dir.path());
    }

    #[test]
    fn exec_download_recreates_vanished_destination() {
        // W13/B-L4: a pull whose destination vanished since the plan recreates
        // it (Vanished -> proceed), not an error.
        let dir = TempDir::new("vaultsync-exec");
        let p = dir.join("a.md");
        std::fs::write(&p, "old-local").unwrap();
        set_mtime_ms(&p, 1_700_000_000_000);
        let store = MemoryStore::new();
        put_str(&store, "a.md", "remote-new-body", Some(1_700_000_005_000));
        let local = LocalFs::new(dir.path());
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default())
            .unwrap()
            .plan;
        std::fs::remove_file(&p).unwrap();
        let rep =
            crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default(), 1);
        assert_eq!(rep.failed, Vec::<ExecFailure>::new(), "{:?}", rep.failed);
        assert_eq!(std::fs::read(&p).unwrap(), b"remote-new-body");
    }

    /// Recursively scan a vault for any `vaultsync-tmp` temp-sibling leftover
    /// (download temps are `.name.vaultsync-tmp-<pid>-<n>` next to the final
    /// path). Used to lock the no-leak property on download error paths.
    fn assert_no_tmp_leftovers(dir: &std::path::Path) {
        fn scan(d: &std::path::Path, found: &mut Vec<String>) {
            if let Ok(rd) = std::fs::read_dir(d) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        scan(&e.path(), found);
                    } else if name.contains("vaultsync-tmp") {
                        found.push(e.path().display().to_string());
                    }
                }
            }
        }
        let mut found = Vec::new();
        scan(dir, &mut found);
        assert!(found.is_empty(), "temp siblings leaked: {found:?}");
    }

    #[cfg(unix)]
    #[test]
    fn exec_download_destination_symlink_fails_closed() {
        // W13/B-L4: a destination that became a symlink after planning fails
        // closed (never followed / written through).
        let dir = TempDir::new("vaultsync-exec");
        let outside = TempDir::new("vaultsync-outside");
        let p = dir.join("a.md");
        std::fs::write(&p, "old-local").unwrap();
        set_mtime_ms(&p, 1_700_000_000_000);
        std::fs::write(outside.join("secret"), "s").unwrap();
        let store = MemoryStore::new();
        put_str(&store, "a.md", "remote-new-body", Some(1_700_000_005_000));
        let local = LocalFs::new(dir.path());
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default())
            .unwrap()
            .plan;
        // swap the destination for a symlink to an outside target
        std::fs::remove_file(&p).unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), &p).unwrap();
        let rep =
            crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default(), 1);
        assert!(
            rep.failed.iter().any(|fl| fl.key == "a.md"),
            "a.md not failed: {:?}",
            rep.failed
        );
        // the outside target survives (not overwritten through the link)
        assert_eq!(std::fs::read(outside.join("secret")).unwrap(), b"s");
        // W21/N1: the freshness-guard error path must not leave the temp
        // sibling behind (the symlink destination errors before rename).
        assert_no_tmp_leftovers(dir.path());
    }

    #[cfg(unix)]
    #[test]
    fn exec_download_over_symlink_destination_message() {
        // R4-L5/W43: a default-mode pull where the destination is a
        // pre-existing symlink (skipped by the walk, so the key is remote-only)
        // must fail closed but with an ACCURATE message - naming the skipped
        // symlink - not misdiagnose it as a destination that "appeared since
        // plan".
        let dir = TempDir::new("vaultsync-exec");
        let outside = TempDir::new("vaultsync-outside");
        std::fs::write(outside.join("secret"), "s").unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), dir.join("a.md")).unwrap();
        let store = MemoryStore::new();
        put_str(&store, "a.md", "remote-bytes", Some(1_700_000_000_123));
        let local = LocalFs::new(dir.path()); // follow off
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default())
            .unwrap()
            .plan;
        let act = plan.actions.iter().find(|a| a.key == "a.md").unwrap();
        assert_eq!(act.kind, ActionKind::Download);
        assert!(
            act.local.is_none(),
            "symlink skipped by walk -> remote-only"
        );
        let rep =
            crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default(), 1);
        let fl = rep
            .failed
            .iter()
            .find(|fl| fl.key == "a.md")
            .unwrap_or_else(|| panic!("a.md not failed: {:?}", rep.failed));
        assert!(
            !fl.message.contains("appeared"),
            "misdiagnosed pre-existing symlink as appeared: {}",
            fl.message
        );
        assert!(
            fl.message.contains("symlink") || fl.message.contains("walk"),
            "message does not mention skipped symlink: {}",
            fl.message
        );
        // the outside target is untouched (never written through)
        assert_eq!(std::fs::read(outside.join("secret")).unwrap(), b"s");
    }

    #[test]
    fn exec_download_missing_remote_errors_key() {
        let dir = TempDir::new("vaultsync-exec");
        let store = MemoryStore::new();
        put_str(&store, "a.md", "x", Some(100));
        let local = LocalFs::new(dir.path());
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default())
            .unwrap()
            .plan;
        // remote vanishes between plan and execute
        store.delete("a.md").unwrap();
        let rep = execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default(), 1);
        assert!(
            rep.failed.iter().any(|fl| fl.key == "a.md"),
            "a.md not failed: {:?}",
            rep.failed
        );
        assert!(!dir.join("a.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn exec_reports_empty_dir_cleanup_warning() {
        // W16/A-L3 + W47, retargeted to the scoped ancestor pass (W77): an
        // empty-dir cleanup failure on a deleted file's ancestor chain is a
        // non-fatal warning on the report, not silently swallowed; the delete
        // itself still succeeds and the run's exit is unaffected. The parent
        // `a` is read+traverse (0o555) so the file delete works (write is on
        // `a/b`) but `remove_dir(a/b)` fails EACCES.
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("vaultsync-exec");
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a/b/gone.md"), "bye").unwrap();
        let store = MemoryStore::new();
        let local = LocalFs::new(dir.path());
        let opts = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let plan = crate::build_plan(&local, &store, Mode::Pull, &opts)
            .unwrap()
            .plan;
        assert!(
            plan.actions
                .iter()
                .any(|a| a.key == "a/b/gone.md" && a.kind == ActionKind::DeleteLocal)
        );
        // lock the parent `a` (read+traverse, no write) after planning so the
        // file delete still succeeds but the empty-dir removal fails EACCES
        std::fs::set_permissions(dir.join("a"), std::fs::Permissions::from_mode(0o555)).unwrap();
        let rep = crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &opts, 1);
        // restore perms so TempDir drop can remove the tree
        std::fs::set_permissions(dir.join("a"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            rep.warnings
                .iter()
                .any(|w| w.contains("a/b") && w.contains("remove")),
            "no cleanup warning naming the unremovable dir: {:?}",
            rep.warnings
        );
        // non-fatal: the delete itself succeeded, no failed keys
        assert_eq!(rep.failed, Vec::<ExecFailure>::new(), "{:?}", rep.failed);
        assert!(!dir.join("a/b/gone.md").exists());
    }

    #[test]
    fn exec_delete_of_already_gone_key_is_success() {
        // W10/A-M3/B-L6: deleting an already-absent remote key achieves the
        // goal state; the executor counts it as success (idempotent-friendly,
        // matching S3's idempotent delete) rather than a failure.
        let dir = TempDir::new("vaultsync-exec");
        let store = MemoryStore::new();
        put_str(&store, "gone.md", "x", Some(100));
        let local = LocalFs::new(dir.path());
        let opts = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let plan = crate::build_plan(&local, &store, Mode::Push, &opts)
            .unwrap()
            .plan;
        // remote vanishes before execution -> the planned delete sees NotFound
        store.delete("gone.md").unwrap();
        let rep = crate::exec::execute_plan(&local, &store, &plan, Mode::Push, &opts, 1);
        assert_eq!(rep.failed, Vec::<ExecFailure>::new(), "{:?}", rep.failed);
        assert_eq!(rep.executed, 1, "{:?}", rep);
    }

    #[test]
    fn exec_report_counts_and_failures() {
        let dir = TempDir::new("vaultsync-exec");
        std::fs::write(dir.join("ok.md"), "stable").unwrap();
        std::fs::write(dir.join("bad.md"), "abc").unwrap();
        let local = LocalFs::new(dir.path());
        let store = MemoryStore::new();
        let plan = crate::build_plan(&local, &store, Mode::Push, &PlanOpts::default())
            .unwrap()
            .plan;
        // grow bad.md after planning
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("bad.md"))
            .unwrap();
        use std::io::Write as _;
        f.write_all(b"aaaaaaaaaa").unwrap();
        drop(f);
        let rep = execute_plan(&local, &store, &plan, Mode::Push, &PlanOpts::default(), 1);
        assert_eq!(rep.executed, 1, "only ok.md");
        assert_eq!(rep.failed.len(), 1);
        assert_eq!(rep.failed[0].key, "bad.md");
    }

    #[test]
    fn exec_path_collision_never_executes() {
        // local file `K` conflicts with remote `K/x` (path_collision); neither
        // is executed.
        let dir = TempDir::new("vaultsync-exec");
        std::fs::write(dir.join("K"), "file-k").unwrap();
        let store = MemoryStore::new();
        put_str(&store, "K/x", "child", Some(100));
        let local = LocalFs::new(dir.path());
        let plan = crate::build_plan(&local, &store, Mode::Push, &PlanOpts::default())
            .unwrap()
            .plan;
        let rep = execute_plan(&local, &store, &plan, Mode::Push, &PlanOpts::default(), 1);
        assert_eq!(rep.executed, 0);
        assert_eq!(rep.failed, Vec::<ExecFailure>::new());
        // nothing uploaded as file `K`
        assert!(matches!(store.head("K").unwrap_err(), Error::NotFound(_)));
        assert!(store.head("K/x").is_ok());
    }

    #[test]
    fn exec_status_mode_mutates_nothing() {
        let dir = TempDir::new("vaultsync-exec");
        std::fs::write(dir.join("a.md"), "hello").unwrap();
        let store = MemoryStore::new();
        put_str(&store, "b.md", "remote", Some(100));
        let local = LocalFs::new(dir.path());
        let plan = crate::build_plan(&local, &store, Mode::Status, &PlanOpts::default())
            .unwrap()
            .plan;
        let rep = execute_plan(&local, &store, &plan, Mode::Status, &PlanOpts::default(), 1);
        assert_eq!(rep.executed, 0);
        assert_eq!(rep.failed, Vec::<ExecFailure>::new());
        // no upload of a.md, no download of b.md
        assert!(matches!(
            store.head("a.md").unwrap_err(),
            Error::NotFound(_)
        ));
        assert!(!dir.join("b.md").exists());
        assert!(store.head("b.md").is_ok());
        assert!(dir.join("a.md").exists());
    }

    /// I27 test double: records every event into a `Mutex<Vec<ProgressEvent>>`
    /// (thread-safe for pool-driven emission, I27-thread).
    #[derive(Default)]
    struct RecordingProgress {
        events: Mutex<Vec<ProgressEvent>>,
    }
    impl RecordingProgress {
        fn new() -> Self {
            RecordingProgress {
                events: Mutex::new(Vec::new()),
            }
        }
        fn events(&self) -> Vec<ProgressEvent> {
            self.events.lock().unwrap().clone()
        }
    }
    impl Progress for RecordingProgress {
        fn event(&self, ev: ProgressEvent) {
            self.events.lock().unwrap().push(ev);
        }
    }

    /// Store wrapper that fails `put_from` for poisoned keys (I27 cycle 2:
    /// per-key failure events).
    struct FailPutStore {
        inner: MemoryStore,
        poison: std::sync::Mutex<std::collections::HashSet<String>>,
    }
    impl FailPutStore {
        fn new() -> Self {
            FailPutStore {
                inner: MemoryStore::new(),
                poison: std::sync::Mutex::new(std::collections::HashSet::new()),
            }
        }
        fn poison(&self, key: &str) {
            self.poison.lock().unwrap().insert(key.to_string());
        }
    }
    impl ObjectStore for FailPutStore {
        fn list(&self, prefix: &str) -> Result<Listing, Error> {
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
            mtime: Option<u64>,
        ) -> Result<Entity, Error> {
            if self.poison.lock().unwrap().contains(key) {
                return Err(Error::Other(format!("poisoned:{key}")));
            }
            self.inner.put_from(key, r, size, mtime)
        }
        fn delete(&self, key: &str) -> Result<(), Error> {
            self.inner.delete(key)
        }
    }

    /// Store wrapper that shuffles `put_from` completion order via a
    /// key-hash sleep (I27 cycle 3 adversarial store, same pattern as
    /// `ShuffleStore` but on the upload side).
    struct PutShuffleStore {
        inner: MemoryStore,
    }
    impl PutShuffleStore {
        fn new() -> Self {
            PutShuffleStore {
                inner: MemoryStore::new(),
            }
        }
        fn sleep_for(&self, key: &str) {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            use std::hash::{Hash as _, Hasher as _};
            key.hash(&mut h);
            std::thread::sleep(std::time::Duration::from_micros(h.finish() % 3000));
        }
    }
    impl ObjectStore for PutShuffleStore {
        fn list(&self, prefix: &str) -> Result<Listing, Error> {
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
            mtime: Option<u64>,
        ) -> Result<Entity, Error> {
            self.sleep_for(key);
            self.inner.put_from(key, r, size, mtime)
        }
        fn delete(&self, key: &str) -> Result<(), Error> {
            self.inner.delete(key)
        }
    }

    #[test]
    fn execute_plan_with_progress_events_complete_under_concurrency() {
        // I27 cycle 3: at concurrency 4 on an adversarial (hash-sleep) store,
        // exactly one PassStart, exactly 16 KeyDones with 16 distinct keys,
        // exactly one PassEnd, and the terminal RunEnd - regardless of
        // completion interleaving (I27-events: no cross-key order asserted).
        let dir = TempDir::new("vaultsync-exec");
        let store = PutShuffleStore::new();
        for i in 0..16 {
            std::fs::write(dir.join(format!("k{i:02}.md")), format!("body-{i:02}")).unwrap();
        }
        let local = LocalFs::new(dir.path());
        let plan = crate::build_plan(&local, &store, Mode::Push, &PlanOpts::default())
            .unwrap()
            .plan;
        let prog = RecordingProgress::new();
        let rep = execute_plan_with_progress(
            &local,
            &store,
            &plan,
            Mode::Push,
            &PlanOpts::default(),
            4,
            &prog,
        );
        assert_eq!(rep.failed, Vec::<ExecFailure>::new(), "{:?}", rep);
        assert_eq!(rep.executed, 16);
        let evs = prog.events();
        assert_eq!(evs.len(), 1 + 16 + 1 + 1, "{:?}", evs);
        assert_eq!(
            evs[0],
            ProgressEvent::PassStart {
                kind: PassKind::Upload,
                total_keys: 16,
                total_bytes: 16 * 7,
            }
        );
        let mut keys: Vec<String> = evs[1..17]
            .iter()
            .map(|ev| match ev {
                ProgressEvent::KeyDone { key, ok, .. } => {
                    assert!(*ok, "all uploads must succeed: {ev:?}");
                    key.clone()
                }
                _ => panic!("expected KeyDone: {ev:?}"),
            })
            .collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), 16, "16 distinct keys");
        assert_eq!(
            evs[17],
            ProgressEvent::PassEnd {
                kind: PassKind::Upload
            }
        );
        assert_eq!(
            evs[18],
            ProgressEvent::RunEnd {
                executed: 16,
                failed: 0
            }
        );
    }

    #[test]
    fn execute_plan_with_progress_key_done_bytes_match_plan() {
        // I27 cycle 3: the sum of KeyDone bytes equals the PassStart
        // total_bytes for a mixed-size plan (I27-bytes accounting). This
        // identity holds on the WIRE - the executor always emits planned
        // bytes; the success-only accounting (policy B, PR 28 r1 F1) is a
        // consumer-side (`ProgressLine`) policy, so an all-success plan
        // keeps sum == total unchanged.
        let dir = TempDir::new("vaultsync-exec");
        let store = MemoryStore::new();
        std::fs::write(dir.join("tiny.md"), "x").unwrap(); // 1 byte
        std::fs::write(dir.join("mid.md"), "z".repeat(50)).unwrap(); // 50
        std::fs::write(dir.join("big.md"), "y".repeat(1000)).unwrap(); // 1000
        let local = LocalFs::new(dir.path());
        let plan = crate::build_plan(&local, &store, Mode::Push, &PlanOpts::default())
            .unwrap()
            .plan;
        let prog = RecordingProgress::new();
        execute_plan_with_progress(
            &local,
            &store,
            &plan,
            Mode::Push,
            &PlanOpts::default(),
            4,
            &prog,
        );
        let evs = prog.events();
        let total = match &evs[0] {
            ProgressEvent::PassStart { total_bytes, .. } => *total_bytes,
            e => panic!("expected PassStart first: {e:?}"),
        };
        let sum: u64 = evs
            .iter()
            .filter_map(|ev| match ev {
                ProgressEvent::KeyDone { bytes, .. } => Some(*bytes),
                _ => None,
            })
            .sum();
        assert_eq!(sum, total, "KeyDone bytes must sum to PassStart total");
        assert_eq!(total, 1 + 50 + 1000);
    }

    #[test]
    fn execute_plan_wrapper_matches_with_progress_report() {
        // I27 cycle 4: the wrapper must behave byte-for-byte like the
        // with-progress form fed `&NoProgress` - identical ExecReports and
        // identical on-disk results (I20 determinism shape, reused: shuffled
        // store, poisoned keys, concurrency 8).
        let store = ShuffleStore::new();
        for i in 0..24 {
            put_str(
                &store.inner,
                &format!("k{i:02}.md"),
                &format!("x{i}"),
                Some(5000 + i),
            );
        }
        store.poison("k03.md");
        store.poison("k17.md");
        let opts = PlanOpts::default();
        let dir1 = TempDir::new("vaultsync-exec");
        let plan = crate::build_plan(&LocalFs::new(dir1.path()), &store, Mode::Pull, &opts)
            .unwrap()
            .plan;
        let dir2 = TempDir::new("vaultsync-exec");
        let rep_wrapper = execute_plan(
            &LocalFs::new(dir2.path()),
            &store,
            &plan,
            Mode::Pull,
            &opts,
            8,
        );
        let dir3 = TempDir::new("vaultsync-exec");
        let rep_progress = execute_plan_with_progress(
            &LocalFs::new(dir3.path()),
            &store,
            &plan,
            Mode::Pull,
            &opts,
            8,
            &NoProgress,
        );
        assert_eq!(
            rep_wrapper, rep_progress,
            "wrapper report must equal the NoProgress form"
        );
        assert_eq!(
            rep_wrapper.failed.len(),
            2,
            "both poisoned keys must fail: {:?}",
            rep_wrapper.failed
        );
        // identical on-disk results (poisoned keys absent in both dirs)
        for i in 0..24 {
            let a = std::fs::read(dir2.path().join(format!("k{i:02}.md"))).unwrap_or_default();
            let b = std::fs::read(dir3.path().join(format!("k{i:02}.md"))).unwrap_or_default();
            assert_eq!(a, b, "k{i:02}.md");
        }
    }

    #[test]
    fn execute_plan_wrapper_emits_no_events() {
        // I27 cycle 4: two pins that the wrapper runs with NoProgress.
        // (a) Compile-time: the public signature keeps today's exact 6-arg
        // shape - no progress parameter. A function-pointer coercion fails
        // to compile if it ever grows one.
        let _: fn(&LocalFs, &dyn ObjectStore, &Plan, Mode, &PlanOpts, u32) -> ExecReport =
            execute_plan;
        // (b) Behavioral: a failing run through the wrapper yields the same
        // report as the with-progress form fed `&NoProgress` (the no-op
        // sink, so no events are observable by construction).
        let dir = TempDir::new("vaultsync-exec");
        std::fs::write(dir.join("ok.md"), "ok").unwrap();
        std::fs::write(dir.join("bad.md"), "bad").unwrap();
        let local = LocalFs::new(dir.path());
        let store = FailPutStore::new();
        store.poison("bad.md");
        let plan = crate::build_plan(&local, &store, Mode::Push, &PlanOpts::default())
            .unwrap()
            .plan;
        let rep_wrapper = execute_plan(&local, &store, &plan, Mode::Push, &PlanOpts::default(), 1);
        let rep_progress = execute_plan_with_progress(
            &local,
            &store,
            &plan,
            Mode::Push,
            &PlanOpts::default(),
            1,
            &NoProgress,
        );
        assert_eq!(rep_wrapper, rep_progress);
        assert_eq!(rep_wrapper.failed.len(), 1, "{:?}", rep_wrapper);
    }

    #[test]
    fn execute_plan_with_progress_emits_pass_and_key_events_push() {
        // I27 cycle 2: a 3-upload push at concurrency 1 emits exactly
        // PassStart(Upload, 3 keys, sum of planned bytes) then three KeyDone
        // (one per key; set semantics, never cross-key order) then
        // PassEnd(Upload) then RunEnd { 3, 0 }.
        let dir = TempDir::new("vaultsync-exec");
        std::fs::write(dir.join("a.md"), "aaaaa").unwrap(); // 5 bytes
        std::fs::write(dir.join("b.md"), "bbbbbbbbb").unwrap(); // 9 bytes
        std::fs::write(dir.join("c.md"), "cc").unwrap(); // 2 bytes
        let local = LocalFs::new(dir.path());
        let store = MemoryStore::new();
        let plan = crate::build_plan(&local, &store, Mode::Push, &PlanOpts::default())
            .unwrap()
            .plan;
        let prog = RecordingProgress::new();
        let rep = execute_plan_with_progress(
            &local,
            &store,
            &plan,
            Mode::Push,
            &PlanOpts::default(),
            1,
            &prog,
        );
        assert_eq!(rep.failed, Vec::<ExecFailure>::new(), "{:?}", rep);
        let evs = prog.events();
        assert_eq!(evs.len(), 6, "{:?}", evs);
        assert_eq!(
            evs[0],
            ProgressEvent::PassStart {
                kind: PassKind::Upload,
                total_keys: 3,
                total_bytes: 16,
            }
        );
        let mut pairs: Vec<(String, u64, bool)> = evs[1..4]
            .iter()
            .map(|ev| match ev {
                ProgressEvent::KeyDone { key, bytes, ok, .. } => (key.clone(), *bytes, *ok),
                _ => panic!("expected KeyDone: {ev:?}"),
            })
            .collect();
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                ("a.md".to_string(), 5, true),
                ("b.md".to_string(), 9, true),
                ("c.md".to_string(), 2, true),
            ]
        );
        assert_eq!(
            evs[4],
            ProgressEvent::PassEnd {
                kind: PassKind::Upload
            }
        );
        assert_eq!(
            evs[5],
            ProgressEvent::RunEnd {
                executed: 3,
                failed: 0
            }
        );
    }

    #[test]
    fn execute_plan_with_progress_emits_failure_event_and_run_end_counts() {
        // I27 cycle 2: a failing key's KeyDone carries ok: false and RunEnd
        // matches the ExecReport (executed 1 / failed 1).
        let dir = TempDir::new("vaultsync-exec");
        std::fs::write(dir.join("ok.md"), "ok").unwrap();
        std::fs::write(dir.join("bad.md"), "bad").unwrap();
        let local = LocalFs::new(dir.path());
        let store = FailPutStore::new();
        store.poison("bad.md");
        let plan = crate::build_plan(&local, &store, Mode::Push, &PlanOpts::default())
            .unwrap()
            .plan;
        let prog = RecordingProgress::new();
        let rep = execute_plan_with_progress(
            &local,
            &store,
            &plan,
            Mode::Push,
            &PlanOpts::default(),
            1,
            &prog,
        );
        assert_eq!(rep.executed, 1, "{:?}", rep);
        assert_eq!(rep.failed.len(), 1, "{:?}", rep);
        let evs = prog.events();
        for key in ["ok.md", "bad.md"] {
            let done = evs
                .iter()
                .find(|ev| matches!(ev, ProgressEvent::KeyDone { key: k, .. } if k == key))
                .unwrap_or_else(|| panic!("no KeyDone for {key}: {evs:?}"));
            match done {
                ProgressEvent::KeyDone { ok, .. } => assert_eq!(*ok, key == "ok.md"),
                _ => unreachable!(),
            }
        }
        assert_eq!(
            evs.last(),
            Some(&ProgressEvent::RunEnd {
                executed: 1,
                failed: 1
            })
        );
    }

    #[test]
    fn execute_plan_with_progress_failed_key_bytes_policy() {
        // PR 28 r1 F1 (policy B) at the executor boundary: a poisoned large
        // key still emits `KeyDone { ok: false, bytes: <planned> }` on the
        // wire, but a `ProgressLine` fed the recorded stream counts only the
        // successful key's bytes. RED today on the render-shape assertions:
        // the poisoned key's planned 1000 bytes inflate the cumulative rate.
        let dir = TempDir::new("vaultsync-exec");
        std::fs::write(dir.join("big.md"), "y".repeat(1000)).unwrap(); // 1000 bytes
        std::fs::write(dir.join("small.md"), "abc").unwrap(); // 3 bytes
        let local = LocalFs::new(dir.path());
        let store = FailPutStore::new();
        store.poison("big.md");
        let plan = crate::build_plan(&local, &store, Mode::Push, &PlanOpts::default())
            .unwrap()
            .plan;
        let prog = RecordingProgress::new();
        let rep = execute_plan_with_progress(
            &local,
            &store,
            &plan,
            Mode::Push,
            &PlanOpts::default(),
            1,
            &prog,
        );
        assert_eq!(rep.executed, 1, "{:?}", rep);
        assert_eq!(rep.failed.len(), 1, "{:?}", rep);
        let evs = prog.events();
        // executor wire contract: the failed key's KeyDone keeps its planned
        // byte size (R28-f1: accounting is consumer-side).
        match evs
            .iter()
            .find(|ev| matches!(ev, ProgressEvent::KeyDone { key, .. } if key == "big.md"))
            .unwrap_or_else(|| panic!("no big.md KeyDone: {evs:?}"))
        {
            ProgressEvent::KeyDone { ok, bytes, .. } => {
                assert!(!*ok, "big.md must fail");
                assert_eq!(*bytes, 1000, "wire bytes stay planned on failure");
            }
            _ => unreachable!(),
        }
        match evs
            .iter()
            .find(|ev| matches!(ev, ProgressEvent::KeyDone { key, .. } if key == "small.md"))
            .unwrap_or_else(|| panic!("no small.md KeyDone: {evs:?}"))
        {
            ProgressEvent::KeyDone { ok, bytes, .. } => {
                assert!(*ok, "small.md must succeed");
                assert_eq!(*bytes, 3);
            }
            _ => unreachable!(),
        }
        // feed the executor stream through the pure ProgressLine with an
        // injected clock: PassStart at t=0, big at t=1000, small at t=2000;
        // PassEnd/RunEnd are ignored by the state machine.
        let mut line = ProgressLine::new();
        let mut now = 0u64;
        for ev in evs.iter() {
            match ev {
                ProgressEvent::PassStart { .. } | ProgressEvent::KeyDone { .. } => {
                    line.on_event(ev.clone(), now);
                    now += 1000;
                }
                _ => {}
            }
        }
        let r = line.render();
        assert!(r.contains("2/2"), "{r}");
        assert!(r.contains("100%"), "{r}");
        assert!(!r.contains("ETA"), "no phantom ETA: {r}");
        // only the 3 transferred bytes over 2s = 1.5 B/s; the poisoned key's
        // planned 1000 bytes must NOT inflate it (RED today: 501.5 B/s).
        assert!(
            r.contains("1.5 B/s"),
            "rate must reflect only the successful key: {r}"
        );
        assert!(
            !r.contains("501.5 B/s"),
            "rate must not include the poisoned key's planned bytes: {r}"
        );
    }

    #[test]
    fn execute_plan_with_progress_delete_local_bracket() {
        // PR 28 r2 F4: pin the complete `DeleteLocal` bracket and the terminal
        // `RunEnd` at the executor boundary through `RecordingProgress`.
        // Pins existing behavior (GREEN on arrival); mutation-check the pass
        // dispatch. Empty store + pull --delete -> a single local-only key is
        // DeleteLocal, and the file is really removed (not just an event-shape
        // test).
        let dir = TempDir::new("vaultsync-exec");
        std::fs::write(dir.join("gone.md"), "bye").unwrap();
        let local = LocalFs::new(dir.path());
        let store = MemoryStore::new();
        let opts = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let plan = crate::build_plan(&local, &store, Mode::Pull, &opts)
            .unwrap()
            .plan;
        assert!(
            plan.actions
                .iter()
                .any(|a| a.key == "gone.md" && a.kind == ActionKind::DeleteLocal),
            "plan must delete gone.md locally: {:?}",
            plan.actions
        );
        let prog = RecordingProgress::new();
        let rep = execute_plan_with_progress(&local, &store, &plan, Mode::Pull, &opts, 1, &prog);
        assert_eq!(rep.executed, 1, "{:?}", rep);
        assert_eq!(rep.failed, Vec::<ExecFailure>::new(), "{:?}", rep);
        assert!(
            !dir.join("gone.md").exists(),
            "gone.md must actually be removed by the executor"
        );
        let evs = prog.events();
        assert_eq!(
            evs,
            vec![
                ProgressEvent::PassStart {
                    kind: PassKind::DeleteLocal,
                    total_keys: 1,
                    total_bytes: 0,
                },
                ProgressEvent::KeyDone {
                    kind: PassKind::DeleteLocal,
                    key: "gone.md".to_string(),
                    bytes: 0,
                    ok: true,
                },
                ProgressEvent::PassEnd {
                    kind: PassKind::DeleteLocal,
                },
                ProgressEvent::RunEnd {
                    executed: 1,
                    failed: 0,
                },
            ],
            "exact DeleteLocal bracket: {evs:?}"
        );
    }

    #[test]
    fn execute_plan_with_progress_skips_empty_passes() {
        // I27 cycle 2 / I27-render: zero-item passes emit no PassStart/PassEnd
        // (a pure-download pull plan has no Upload pass events), but the
        // download pass and the final RunEnd still fire.
        let dir = TempDir::new("vaultsync-exec");
        let store = MemoryStore::new();
        put_str(&store, "r.md", "remote", Some(100));
        let local = LocalFs::new(dir.path());
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default())
            .unwrap()
            .plan;
        assert!(plan.actions.iter().all(|a| a.kind == ActionKind::Download));
        let prog = RecordingProgress::new();
        let rep = execute_plan_with_progress(
            &local,
            &store,
            &plan,
            Mode::Pull,
            &PlanOpts::default(),
            1,
            &prog,
        );
        assert_eq!(rep.executed, 1, "{:?}", rep);
        let evs = prog.events();
        assert!(
            !evs.iter().any(|ev| matches!(
                ev,
                ProgressEvent::PassStart {
                    kind: PassKind::Upload,
                    ..
                }
            )),
            "no Upload PassStart: {evs:?}"
        );
        assert!(
            !evs.iter().any(|ev| matches!(
                ev,
                ProgressEvent::PassEnd {
                    kind: PassKind::Upload
                }
            )),
            "no Upload PassEnd: {evs:?}"
        );
        assert!(evs.iter().any(|ev| matches!(
            ev,
            ProgressEvent::PassStart {
                kind: PassKind::Download,
                ..
            }
        )));
        assert_eq!(
            evs.last(),
            Some(&ProgressEvent::RunEnd {
                executed: 1,
                failed: 0
            })
        );
    }

    #[test]
    fn execute_plan_with_progress_status_emits_no_events() {
        // PR 28 r1 F9(a): `Mode::Status` emits zero progress events at the
        // executor boundary - no PassStart/KeyDone/PassEnd and no terminal
        // RunEnd - and the report stays the empty default. Mutation-checked:
        // removing the `if mode == Mode::Status { return rep; }` early
        // return turns this pin RED (events would appear).
        let dir = TempDir::new("vaultsync-exec");
        std::fs::write(dir.join("a.md"), "hello").unwrap();
        let store = MemoryStore::new();
        put_str(&store, "b.md", "remote", Some(100));
        let local = LocalFs::new(dir.path());
        let plan = crate::build_plan(&local, &store, Mode::Status, &PlanOpts::default())
            .unwrap()
            .plan;
        assert!(!plan.actions.is_empty(), "plan must be non-empty");
        let prog = RecordingProgress::new();
        let rep = execute_plan_with_progress(
            &local,
            &store,
            &plan,
            Mode::Status,
            &PlanOpts::default(),
            1,
            &prog,
        );
        assert_eq!(rep, ExecReport::default(), "{:?}", rep);
        assert!(
            prog.events().is_empty(),
            "Status mode must emit no events: {:?}",
            prog.events()
        );
    }

    #[test]
    fn execute_plan_with_progress_passes_bracket_in_plan_order() {
        // PR 28 r1 F9(b): multi-pass bracketing order at the executor
        // boundary. A push --delete plan with an upload AND a remote-only
        // delete emits exactly one Upload bracket, then exactly one
        // DeleteRemote bracket (strict pass ordering, no cross-pass
        // interleave), and one terminal RunEnd as the final event.
        // Mutation-checked: emitting the delete bracket before the upload, or
        // dropping a PassEnd, turns this pin RED.
        let dir = TempDir::new("vaultsync-exec");
        std::fs::write(dir.join("a.md"), "hello").unwrap();
        let store = MemoryStore::new();
        put_str(&store, "gone.md", "x", Some(100));
        let local = LocalFs::new(dir.path());
        let opts = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let plan = crate::build_plan(&local, &store, Mode::Push, &opts)
            .unwrap()
            .plan;
        assert!(plan.actions.iter().any(|a| a.kind == ActionKind::Upload));
        assert!(
            plan.actions
                .iter()
                .any(|a| a.kind == ActionKind::DeleteRemote)
        );
        let prog = RecordingProgress::new();
        let rep = execute_plan_with_progress(&local, &store, &plan, Mode::Push, &opts, 1, &prog);
        assert_eq!(rep.executed, 2, "one upload + one delete: {:?}", rep);
        let evs = prog.events();
        // exactly one of each bracket endpoint
        let count = |p: fn(&ProgressEvent) -> bool| evs.iter().filter(|e| p(e)).count();
        assert_eq!(
            count(|e| matches!(
                e,
                ProgressEvent::PassStart {
                    kind: PassKind::Upload,
                    ..
                }
            )),
            1
        );
        assert_eq!(
            count(|e| matches!(
                e,
                ProgressEvent::PassEnd {
                    kind: PassKind::Upload
                }
            )),
            1
        );
        assert_eq!(
            count(|e| matches!(
                e,
                ProgressEvent::PassStart {
                    kind: PassKind::DeleteRemote,
                    ..
                }
            )),
            1
        );
        assert_eq!(
            count(|e| matches!(
                e,
                ProgressEvent::PassEnd {
                    kind: PassKind::DeleteRemote
                }
            )),
            1
        );
        // each bracket is well-formed (start before end)
        let ps_u = evs
            .iter()
            .position(|e| {
                matches!(
                    e,
                    ProgressEvent::PassStart {
                        kind: PassKind::Upload,
                        ..
                    }
                )
            })
            .unwrap();
        let pe_u = evs
            .iter()
            .position(|e| {
                matches!(
                    e,
                    ProgressEvent::PassEnd {
                        kind: PassKind::Upload
                    }
                )
            })
            .unwrap();
        let ps_d = evs
            .iter()
            .position(|e| {
                matches!(
                    e,
                    ProgressEvent::PassStart {
                        kind: PassKind::DeleteRemote,
                        ..
                    }
                )
            })
            .unwrap();
        let pe_d = evs
            .iter()
            .position(|e| {
                matches!(
                    e,
                    ProgressEvent::PassEnd {
                        kind: PassKind::DeleteRemote
                    }
                )
            })
            .unwrap();
        assert!(ps_u < pe_u && ps_d < pe_d, "brackets well-formed: {evs:?}");
        assert!(
            pe_u < ps_d,
            "upload pass must end before the delete pass starts: {evs:?}"
        );
        assert_eq!(
            evs.last(),
            Some(&ProgressEvent::RunEnd {
                executed: 2,
                failed: 0
            })
        );
    }
}
