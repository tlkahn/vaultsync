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
pub fn execute_plan(
    local: &LocalFs,
    store: &dyn ObjectStore,
    plan: &Plan,
    mode: Mode,
    opts: &crate::plan::PlanOpts,
) -> ExecReport {
    let mut rep = ExecReport::default();
    if mode == Mode::Status {
        return rep;
    }

    // Pass 1: downloads (pull).
    for a in plan
        .actions
        .iter()
        .filter(|a| a.kind == ActionKind::Download)
    {
        match exec_download(local, store, a, opts.mtime_tolerance_ms) {
            Ok(()) => rep.executed += 1,
            Err(e) => fail(&mut rep, &a.key, e),
        }
    }

    // Pass 2: uploads (push).
    for a in plan.actions.iter().filter(|a| a.kind == ActionKind::Upload) {
        match exec_upload(local, store, a, opts.mtime_tolerance_ms) {
            Ok(()) => rep.executed += 1,
            Err(e) => fail(&mut rep, &a.key, e),
        }
    }

    // Pass 3: destination-side deletes, after successful transfers. W10
    // (A-M3/B-L6): delete is idempotent-friendly across backends - deleting an
    // already-gone key achieves the goal state, so NotFound is normalized to a
    // success here (S3 delete is idempotent; LocalFs.delete_file still reports
    // NotFound for a missing key, and the executor absorbs it).
    //
    // W62/A-M2: head-before-delete. The list-time entity alone is stale
    // authority for a delete (the local side got `delete_file_guarded` for
    // exactly this race class). Re-verify the remote object immediately
    // before unlinking it: a NotFound means the goal state (absent) is
    // already achieved (counts as success, matching W10); a size drift means
    // the object changed since the plan - the key fails and the new remote
    // content survives. mtime is deliberately NOT compared (R-c): the
    // planned entity's `mtime_ms` comes from the listing's `LastModified`
    // while `head` prefers the `vaultsync-mtime` client metadata, so an
    // mtime comparison would systematically false-fail every key whose
    // client mtime differs from its upload time beyond the tolerance (the
    // documented list-skew limitation). Same-size replacement between list
    // and delete remains a documented residual.
    for a in plan
        .actions
        .iter()
        .filter(|a| a.kind == ActionKind::DeleteRemote)
    {
        let Some(planned_remote) = &a.remote else {
            fail(
                &mut rep,
                &a.key,
                Error::Other(format!(
                    "delete-remote planned without remote entity: {}",
                    a.key
                )),
            );
            continue;
        };
        match store.head(&a.key) {
            Ok(cur) => {
                if cur.size != planned_remote.size {
                    fail(
                        &mut rep,
                        &a.key,
                        Error::Other(format!(
                            "remote changed since plan for {}; not deleting",
                            a.key
                        )),
                    );
                    continue;
                }
            }
            Err(Error::NotFound(_)) => {
                // goal state already achieved (W10 idempotent-delete arm)
                rep.executed += 1;
                continue;
            }
            Err(e) => {
                fail(&mut rep, &a.key, e);
                continue;
            }
        }
        match store.delete(&a.key) {
            Ok(()) => rep.executed += 1,
            // W10: head is best-effort; a delete-time NotFound still means
            // the goal state was reached.
            Err(Error::NotFound(_)) => rep.executed += 1,
            Err(e) => fail(&mut rep, &a.key, e),
        }
    }
    let mut deleted_keys: Vec<String> = Vec::new();
    for a in plan
        .actions
        .iter()
        .filter(|a| a.kind == ActionKind::DeleteLocal)
    {
        // R4-L1/W39: a `pull --delete` re-verifies local freshness before
        // removing the file (symmetric to upload R3.3 / download W13). The
        // planned local entity is the truth the walk recorded; `a.local` is
        // always `Some` for DeleteLocal - a missing one is a per-key error,
        // never an unguarded delete.
        let Some(planned_local) = &a.local else {
            fail(
                &mut rep,
                &a.key,
                Error::Other(format!(
                    "delete-local planned without local entity: {}",
                    a.key
                )),
            );
            continue;
        };
        match local.delete_file_guarded(
            &a.key,
            planned_local.size,
            planned_local.mtime_ms,
            opts.mtime_tolerance_ms,
        ) {
            Ok(()) => {
                rep.executed += 1;
                deleted_keys.push(a.key.clone());
            }
            Err(Error::NotFound(_)) => {
                // W32: the goal state (file absent) is achieved, so count a
                // no-op delete as reaching it and keep the empty-dir cleanup
                // pass active for this key's ancestor chain. (The guarded
                // delete reports NotFound before any freshness check,
                // matching the old delete_file contract.)
                rep.executed += 1;
                deleted_keys.push(a.key.clone());
            }
            Err(e) => fail(&mut rep, &a.key, e),
        }
    }
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

    rep
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

/// Download one key into an atomic temp + rename, applying the remote mtime
/// from the `get_to` metadata (which carries the client `vaultsync-mtime`, not
/// the list's LastModified).
fn exec_download(
    local: &LocalFs,
    store: &dyn ObjectStore,
    a: &crate::plan::Action,
    tolerance_ms: u64,
) -> Result<(), Error> {
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
            let remote = store.get_to(&a.key, &mut f)?;
            // A-H1/B-L3: truth-check the bytes actually on disk (not just the
            // backend's declared size or the planned remote entity). A backend
            // that truncates the body while returning a clean EOF is caught
            // here and the key fails closed; the tmp is removed on the error
            // path below (belt-and-braces over the store-side count in get_to).
            let on_disk = std::fs::metadata(&tmp)?.len();
            let expected = a.remote.as_ref().map(|r| r.size).unwrap_or(remote.size);
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
            crate::local::remove_created_dirs(&created_dirs);
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
        fn list(&self, prefix: &str) -> Result<Vec<Entity>, Error> {
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
        let plan = crate::build_plan(&local, &store, Mode::Push, &PlanOpts::default()).unwrap();
        let rep = execute_plan(&local, &store, &plan, Mode::Push, &PlanOpts::default());
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
        let plan = crate::build_plan(&local, &store, Mode::Push, &opts).unwrap();
        // drift the file 3000 ms after the plan captured its mtime
        set_mtime_ms(&p, base + 3000);
        let rep = crate::exec::execute_plan(&local, &store, &plan, Mode::Push, &opts);
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
        let plan = crate::build_plan(&local, &store, Mode::Push, &opts).unwrap();
        let link = plan.actions.iter().find(|a| a.key == "link.md").unwrap();
        assert_eq!(link.kind, ActionKind::Skip, "link.md must be planned skip");
        let rep = crate::exec::execute_plan(&local, &store, &plan, Mode::Push, &opts);
        assert_eq!(rep.failed, Vec::<ExecFailure>::new(), "{:?}", rep.failed);
        assert!(
            matches!(store.head("link.md").unwrap_err(), Error::NotFound(_)),
            "followed symlink must not be uploaded"
        );
        assert!(store.head("real.md").is_ok(), "real.md uploaded");
        assert!(store.head("linkdir/child.md").is_ok(), "dir child uploaded");
    }

    #[test]
    fn exec_download_writes_file_and_mtime() {
        let dir = TempDir::new("vaultsync-exec");
        let store = MemoryStore::new();
        put_str(&store, "n/b.md", "remote-bytes", Some(1_700_000_000_123));
        let local = LocalFs::new(dir.path());
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default()).unwrap();
        let rep = execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default());
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
        let plan = crate::build_plan(&local, &store, Mode::Push, &opts).unwrap();
        let rep = execute_plan(&local, &store, &plan, Mode::Push, &opts);
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
        let plan = crate::build_plan(&local, &store, Mode::Pull, &opts).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| { a.key == "n/gone.md" && a.kind == ActionKind::DeleteLocal })
        );
        // the files vanish before execution (pre-cleaned by another process)
        std::fs::remove_file(dir.join("n/gone.md")).unwrap();
        std::fs::remove_file(dir.join("n/sub/x.md")).unwrap();
        let rep = crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &opts);
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
        let plan = crate::build_plan(&local, &store, Mode::Pull, &opts).unwrap();
        let link = plan.actions.iter().find(|a| a.key == "link.md").unwrap();
        assert_eq!(link.kind, ActionKind::Skip, "{:?}", link);
        assert_eq!(link.reason, "followed_symlink");
        let rep = crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &opts);
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
        let plan = crate::build_plan(&local, &store, Mode::Pull, &opts).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.key == "gone.md" && a.kind == ActionKind::DeleteLocal)
        );
        // swap the leaf for a symlink after planning
        std::fs::remove_file(dir.join("gone.md")).unwrap();
        std::os::unix::fs::symlink(outside.join("victim"), dir.join("gone.md")).unwrap();
        let rep = crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &opts);
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
        let plan = crate::build_plan(&local, &store, Mode::Pull, &opts).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.key == "gone.md" && a.kind == ActionKind::DeleteLocal)
        );
        // user edits the file after planning (size drift)
        std::fs::write(dir.join("gone.md"), "abcdefghij").unwrap();
        let rep = crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &opts);
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
        let plan = crate::build_plan(&local, &store, Mode::Pull, &opts).unwrap();
        let rep = crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &opts);
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
        let plan = crate::build_plan(&local, &store, Mode::Pull, &opts).unwrap();
        let rep = execute_plan(&local, &store, &plan, Mode::Pull, &opts);
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
        let plan = crate::build_plan(&local, &store, Mode::Pull, &opts).unwrap();
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
        let rep = execute_plan(&local, &store, &plan, Mode::Pull, &opts);
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
        let plan = crate::build_plan(&local, &store, Mode::Pull, &opts).unwrap();
        let rep = execute_plan(&local, &store, &plan, Mode::Pull, &opts);
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
        let plan = crate::build_plan(&local, &store, Mode::Pull, &opts).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.key == "n/gone.md" && a.kind == ActionKind::DeleteLocal)
        );
        let rep = execute_plan(&local, &store, &plan, Mode::Pull, &opts);
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
        let plan = crate::build_plan(&local, &store, Mode::Pull, &opts).unwrap();
        // the file vanishes before execution (pre-cleaned by another process)
        std::fs::remove_file(dir.join("n/sub/gone.md")).unwrap();
        let rep = execute_plan(&local, &store, &plan, Mode::Pull, &opts);
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
        let plan = crate::build_plan(&local, &store, Mode::Push, &opts).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.key == "gone.md" && a.kind == ActionKind::DeleteRemote)
        );
        // replace the object with different-size content after planning
        put_str(&store, "gone.md", "replacement-bytes-xyz", Some(200));
        let rep = execute_plan(&local, &store, &plan, Mode::Push, &opts);
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
        let plan = crate::build_plan(&local, &store, Mode::Push, &opts).unwrap();
        let rep = execute_plan(&local, &store, &plan, Mode::Push, &opts);
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
        let plan = crate::build_plan(&local, &store, Mode::Push, &opts).unwrap();
        let rep = execute_plan(&local, &store, &plan, Mode::Push, &opts);
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
        let plan = crate::build_plan(&local, &store, Mode::Push, &PlanOpts::default()).unwrap();
        let before_c = store.head("c.md").unwrap();
        let before_a = store.head("a.md").unwrap();
        let rep = execute_plan(&local, &store, &plan, Mode::Push, &PlanOpts::default());
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
        let plan = crate::build_plan(&local, &store, Mode::Push, &PlanOpts::default()).unwrap();
        // File grows between plan (walks size 3) and open -> per-key failure.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("a.md"))
            .unwrap();
        use std::io::Write as _;
        f.write_all(b"defghijklmnop").unwrap();
        drop(f);
        let rep = execute_plan(&local, &store, &plan, Mode::Push, &PlanOpts::default());
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
        fn list(&self, prefix: &str) -> Result<Vec<Entity>, Error> {
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
        let plan = crate::build_plan(&local, &wrapper, Mode::Pull, &PlanOpts::default()).unwrap();
        let rep =
            crate::exec::execute_plan(&local, &wrapper, &plan, Mode::Pull, &PlanOpts::default());
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
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default()).unwrap();
        // remote vanishes between plan and execute
        store.delete("a/b/c.md").unwrap();
        let rep = execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default());
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
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default()).unwrap();
        store.delete("keep/sub/x.md").unwrap();
        let rep = execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default());
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
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default()).unwrap();
        let act = plan.actions.iter().find(|a| a.key == "a.md").unwrap();
        assert_eq!(act.kind, ActionKind::Download);
        // user edits the destination after planning (size changes)
        std::fs::write(&p, "user-edit-since-plan-123").unwrap();
        let rep =
            crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default());
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
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default()).unwrap();
        let act = plan.actions.iter().find(|a| a.key == "b.md").unwrap();
        assert_eq!(act.kind, ActionKind::Download);
        assert!(act.local.is_none(), "remote-only pull");
        // a regular file appears at the destination after planning
        std::fs::write(dir.join("b.md"), "editor-saved-since-plan").unwrap();
        let rep =
            crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default());
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
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default()).unwrap();
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
            crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default());
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
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default()).unwrap();
        let act = plan.actions.iter().find(|a| a.key == "b.md").unwrap();
        assert_eq!(act.kind, ActionKind::Download);
        assert!(act.local.is_none(), "remote-only pull must have no local");
        // a regular file appears at the destination after planning
        let p = dir.join("b.md");
        std::fs::write(&p, "editor-saved-since-plan").unwrap();
        let rep =
            crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default());
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
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default()).unwrap();
        let rep =
            crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default());
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
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default()).unwrap();
        std::fs::remove_file(&p).unwrap();
        let rep =
            crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default());
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
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default()).unwrap();
        // swap the destination for a symlink to an outside target
        std::fs::remove_file(&p).unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), &p).unwrap();
        let rep =
            crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default());
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
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default()).unwrap();
        let act = plan.actions.iter().find(|a| a.key == "a.md").unwrap();
        assert_eq!(act.kind, ActionKind::Download);
        assert!(
            act.local.is_none(),
            "symlink skipped by walk -> remote-only"
        );
        let rep =
            crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default());
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
        let plan = crate::build_plan(&local, &store, Mode::Pull, &PlanOpts::default()).unwrap();
        // remote vanishes between plan and execute
        store.delete("a.md").unwrap();
        let rep = execute_plan(&local, &store, &plan, Mode::Pull, &PlanOpts::default());
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
        let plan = crate::build_plan(&local, &store, Mode::Pull, &opts).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.key == "a/b/gone.md" && a.kind == ActionKind::DeleteLocal)
        );
        // lock the parent `a` (read+traverse, no write) after planning so the
        // file delete still succeeds but the empty-dir removal fails EACCES
        std::fs::set_permissions(dir.join("a"), std::fs::Permissions::from_mode(0o555)).unwrap();
        let rep = crate::exec::execute_plan(&local, &store, &plan, Mode::Pull, &opts);
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
        let plan = crate::build_plan(&local, &store, Mode::Push, &opts).unwrap();
        // remote vanishes before execution -> the planned delete sees NotFound
        store.delete("gone.md").unwrap();
        let rep = crate::exec::execute_plan(&local, &store, &plan, Mode::Push, &opts);
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
        let plan = crate::build_plan(&local, &store, Mode::Push, &PlanOpts::default()).unwrap();
        // grow bad.md after planning
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("bad.md"))
            .unwrap();
        use std::io::Write as _;
        f.write_all(b"aaaaaaaaaa").unwrap();
        drop(f);
        let rep = execute_plan(&local, &store, &plan, Mode::Push, &PlanOpts::default());
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
        let plan = crate::build_plan(&local, &store, Mode::Push, &PlanOpts::default()).unwrap();
        let rep = execute_plan(&local, &store, &plan, Mode::Push, &PlanOpts::default());
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
        let plan = crate::build_plan(&local, &store, Mode::Status, &PlanOpts::default()).unwrap();
        let rep = execute_plan(&local, &store, &plan, Mode::Status, &PlanOpts::default());
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
}
