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


use crate::local::LocalFs;
use crate::plan::{ActionKind, Mode, Plan};
use crate::store::ObjectStore;
use crate::error::Error;

/// Outcome of an execution run.
#[derive(Debug, Default, PartialEq)]
pub struct ExecReport {
    /// Number of transfers/deletes that succeeded.
    pub executed: u32,
    /// Failed keys with a human message (one bad key never aborts the run).
    pub failed: Vec<ExecFailure>,
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
    for a in plan.actions.iter().filter(|a| a.kind == ActionKind::Download) {
        match exec_download(local, store, a) {
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

    // Pass 3: destination-side deletes, after successful transfers.
    for a in plan.actions.iter().filter(|a| a.kind == ActionKind::DeleteRemote) {
        match store.delete(&a.key) {
            Ok(()) => rep.executed += 1,
            Err(e) => fail(&mut rep, &a.key, e),
        }
    }
    let mut deleted_local = false;
    for a in plan.actions.iter().filter(|a| a.kind == ActionKind::DeleteLocal) {
        match local.delete_file(&a.key) {
            Ok(()) => {
                rep.executed += 1;
                deleted_local = true;
            }
            Err(e) => fail(&mut rep, &a.key, e),
        }
    }
    // R2.1 option (a): after local deletes, clean now-empty dirs bottom-up.
    if deleted_local {
        let _ = local.remove_empty_dirs_bottom_up();
    }

    rep
}

fn fail(rep: &mut ExecReport, key: &str, e: Error) {
    rep.failed.push(ExecFailure {
        key: key.to_string(),
        message: format!("{e}"),
    });
}

/// Download one key into an atomic temp + rename, applying the remote mtime
/// from the `get_to` metadata (which carries the client `vaultsync-mtime`, not
/// the list's LastModified).
fn exec_download(local: &LocalFs, store: &dyn ObjectStore, a: &crate::plan::Action) -> Result<(), Error> {
    let tmp = local.tmp_path_for(&a.key)?;
    let result = (|| -> Result<Option<u64>, Error> {
        let remote_mtime = {
            let mut f = std::fs::File::create(&tmp)?;
            let remote = store.get_to(&a.key, &mut f)?;
            f.sync_all()?;
            // Truth-check the downloaded size against the planned remote entity
            // (a vanished/changed object mid-run should not silently truncate).
            if let Some(planned) = &a.remote {
                if planned.size != remote.size {
                    return Err(Error::Other(format!(
                        "download size mismatch for {} (expected {}, got {})",
                        a.key, planned.size, remote.size
                    )));
                }
            }
            remote.mtime_ms
        };
        Ok(remote_mtime)
    })();
    let (remote_mtime, err) = match result {
        Ok(m) => (m, None),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            (None, Some(e))
        }
    };
    if let Some(e) = err {
        return Err(e);
    }
    local.finalize_write(&a.key, &tmp, remote_mtime)
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
    use crate::plan::{PlanOpts};
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
        assert!(put.unwrap() < del.unwrap(), "delete ran before transfer: {log:?}");
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
        let mut f = std::fs::OpenOptions::new().append(true).open(dir.join("a.md")).unwrap();
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
        assert!(matches!(store.head("a.md").unwrap_err(), Error::NotFound(_)));
        // the stable key still uploads (isolation).
        assert!(store.head("ok.md").is_ok());
        assert_eq!(rep.executed, 1, "only ok.md executed");
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

    #[test]
    fn exec_report_counts_and_failures() {
        let dir = TempDir::new("vaultsync-exec");
        std::fs::write(dir.join("ok.md"), "stable").unwrap();
        std::fs::write(dir.join("bad.md"), "abc").unwrap();
        let local = LocalFs::new(dir.path());
        let store = MemoryStore::new();
        let plan = crate::build_plan(&local, &store, Mode::Push, &PlanOpts::default()).unwrap();
        // grow bad.md after planning
        let mut f = std::fs::OpenOptions::new().append(true).open(dir.join("bad.md")).unwrap();
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
        assert!(matches!(store.head("a.md").unwrap_err(), Error::NotFound(_)));
        assert!(!dir.join("b.md").exists());
        assert!(store.head("b.md").is_ok());
        assert!(dir.join("a.md").exists());
    }
}
