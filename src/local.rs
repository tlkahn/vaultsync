//! Local filesystem walker: turns a vault directory tree into [`Entity`] keys.
//!
//! Walker omissions (Phase 1, all silent and by design):
//! - symlinks - files **and** directories - are skipped entirely
//!   (`--follow-symlinks` is a Phase 2 policy decision, P1r4-symlink);
//! - a symlinked vault **root** is followed (`fs::metadata` on the root
//!   resolves it); the symlink skip applies to entries below the root
//!   (P1r6-root-symlink, locked by `local_list_follows_symlinked_root`);
//! - device / FIFO / socket nodes are skipped unconditionally (only
//!   `is_dir` / `is_file` entries are emitted; their names are never
//!   validated, since they are never emitted, P1r7-special-node-key);
//! - entries that vanish mid-walk (`NotFound`) are skipped; other IO errors
//!   (e.g. permission) abort the walk loudly;
//! - a local file whose name fails key validation (now including control
//!   chars and whitespace-only segments, P1r4-key-ctl) fails the walk loud
//!   instead of emitting a corrupt key.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::entity::Entity;
use crate::error::Error;

/// Walks a vault root directory.
pub struct LocalFs {
    root: PathBuf,
    follow_symlinks: bool,
    /// Walk report, mutated at the end of a walk and read by `report()`.
    /// `Mutex` (not `RefCell`) so `LocalFs` is `Send`/`Sync` ahead of Phase 3
    /// concurrency (W82/r8a-2); single-threaded callers see no difference.
    report: std::sync::Mutex<WalkReport>,
    /// Canonicalized vault root, computed once per instance on first use and
    /// cached (W81/r8a-1 + r9-N2). See [`LocalFs::root_canonical`].
    root_canon: std::sync::OnceLock<PathBuf>,
}

/// Pull destination state relative to the plan (W13/B-L4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Destination still matches the planned size/mtime.
    Fresh,
    /// Destination changed since the plan - do not overwrite.
    Changed,
    /// Destination vanished since the plan - recreate it.
    Vanished,
}

/// Report surfaced from a walk (symlink policy, Slice 9).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct WalkReport {
    /// Symlinks skipped (default mode, or out-of-vault followed targets).
    pub skipped_symlinks: u32,
    /// Reserved vaultsync temp/probe files skipped (W23/M1 + R4-L4/W42): a
    /// crash leftover `.*.vaultsync-tmp-*` or `.vaultsync-check-*` is never
    /// surfaced as a real key. W41 reports this count on stderr.
    pub skipped_temp_files: u32,
    /// Keys of followed *file* symlinks (R4-M1/W38). The planner uses this to
    /// mark such rows `Skip(followed_symlink)` in mutating modes: the walker
    /// follows them (inventory), but transfers refuse to open a symlink, so
    /// Push/Pull must not plan them. `--follow-symlinks` is inventory-only in
    /// v1. Dir-symlink children are NOT listed here - they transfer fine.
    pub followed_files: std::collections::BTreeSet<String>,
    /// Human warnings (e.g. a followed symlink escaping the vault root).
    pub warnings: Vec<String>,
}

/// Walker mode flags.
struct WalkOpts {
    follow_symlinks: bool,
}

impl LocalFs {
    /// Default: symlinks below the root are skipped and counted.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        LocalFs {
            root: root.into(),
            follow_symlinks: false,
            report: std::sync::Mutex::new(WalkReport::default()),
            root_canon: std::sync::OnceLock::new(),
        }
    }

    /// With `--follow-symlinks`: follow symlinks (loops guarded; out-of-vault
    /// targets are still skipped with a warning, never synced silently).
    pub fn with_follow(root: impl Into<PathBuf>, follow_symlinks: bool) -> Self {
        LocalFs {
            root: root.into(),
            follow_symlinks,
            report: std::sync::Mutex::new(WalkReport::default()),
            root_canon: std::sync::OnceLock::new(),
        }
    }

    /// Canonicalized vault root, computed once per [`LocalFs`] and cached
    /// (W81/r8a-1 + r9-N2). Previously every `ensure_locality` call
    /// re-canonicalized the root (9 call sites, tens of thousands of
    /// redundant syscalls at 10k-file scale) and `handle_followed_symlink`
    /// did so per symlink. Caching also makes a mid-run root-symlink swap
    /// yield one consistent boundary decision per instance instead of
    /// potentially disagreeing canonicalizations (the reviewer's correctness
    /// half). Errors propagate exactly as a per-call canonicalize did; the
    /// success is cached, a failure is retried next call
    /// (`get_or_try_init` is unstable, hence this shape).
    fn root_canonical(&self) -> Result<&Path, Error> {
        if let Some(c) = self.root_canon.get() {
            return Ok(c);
        }
        let canon = std::fs::canonicalize(&self.root)?;
        Ok(self.root_canon.get_or_init(|| canon).as_path())
    }

    /// The report from the most recent walk (symlink skips/warnings).
    pub fn report(&self) -> WalkReport {
        self.report.lock().unwrap().clone()
    }

    /// Walk files and folders under the root into vault-relative keys.
    pub fn list(&self) -> Result<Vec<Entity>, Error> {
        self.list_report().map(|(entities, _)| entities)
    }

    /// Walk, returning entities plus the walk report.
    pub fn list_report(&self) -> Result<(Vec<Entity>, WalkReport), Error> {
        // The root itself must exist: a missing vault is a loud error, not an
        // empty plan. `walk` tolerates `NotFound` only for directories
        // discovered mid-walk (vanished between enumeration and recursion).
        let md = std::fs::metadata(&self.root)?;
        if !md.is_dir() {
            return Err(Error::Other(format!(
                "vault root is not a directory: {}",
                self.root.display()
            )));
        }
        let mut report = WalkReport::default();
        let mut visited: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        let mut out = Vec::new();
        let opts = WalkOpts {
            follow_symlinks: self.follow_symlinks,
        };
        // W81/r8a-1: the canonicalized root is computed once per LocalFs and
        // threaded through the walk, instead of being re-canonicalized per
        // followed symlink - a 10k-file tree no longer pays tens of thousands
        // of redundant syscalls, and a mid-run root symlink swap yields one
        // consistent boundary decision.
        let root_canon = self.root_canonical()?;
        walk(
            &self.root,
            &self.root,
            root_canon,
            &mut out,
            &opts,
            &mut report,
            &mut visited,
        )?;
        out.sort_by(|a, b| a.key.cmp(&b.key));
        *self.report.lock().unwrap() = report.clone();
        Ok((out, report))
    }

    /// Open a local file for reading (upload side), verifying it is still a
    /// regular, non-symlink file of the expected size and (optionally) mtime
    /// after walking (R3.3 / P1r7-symlink-swap TOCTOU guard).
    ///
    /// - type recheck: the path must not currently be a symlink (no-follow
    ///   look via `symlink_metadata`) and the opened descriptor must be a
    ///   regular file;
    /// - size recheck: the opened descriptor's own size must equal
    ///   `expected_size` (a file that grew/shrunk between walk and open is a
    ///   per-key error, never a silently-truncated object);
    /// - mtime recheck: if `expected_mtime_ms` is `Some`, the opened
    ///   descriptor's mtime must be within ``tolerance_ms`` of it (W2/PR2
    ///   A-H2/B-M1 threads the resolved config tolerance here; the hardcoded
    ///   default is now only the resolution default).
    pub fn open_verified(
        &self,
        key: &str,
        expected_size: u64,
        expected_mtime_ms: Option<u64>,
        tolerance_ms: u64,
    ) -> Result<std::fs::File, Error> {
        let path = key_to_local_path(&self.root, key)?;
        // No-follow type check: reject a symlink swapped in after the walk.
        let smd = match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NotFound(key.to_string()));
            }
            Err(e) => return Err(e.into()),
            Ok(md) => md,
        };
        if smd.file_type().is_symlink() {
            return Err(Error::Other(format!(
                "refusing to open symlink (was listed as a file): {}",
                path.display()
            )));
        }
        // W26/M4: re-verify parent locality before opening (mirrors the
        // download/write sides). A parent dir swapped for an out-of-vault
        // symlink since the walk canonicalizes outside the root and must be
        // refused - the outside file must never be uploaded under a vault key.
        let parent = path.parent().ok_or_else(|| {
            Error::Other(format!(
                "open_verified has no parent dir: {}",
                path.display()
            ))
        })?;
        self.ensure_locality(parent)?;
        let f = std::fs::File::open(&path)?;
        // Stats the OPENED descriptor (not a second path stat): a re-stat of
        // the path could describe a different file that replaced it.
        let fmd = f.metadata()?;
        // W26/L5-leaf: on unix, close the leaf TOCTOU window with std only - a
        // leaf swapped between the symlink_metadata above and open would have a
        // different (dev, ino); refuse rather than read a planted file.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if fmd.dev() != smd.dev() || fmd.ino() != smd.ino() {
                return Err(Error::Other(format!(
                    "open_verified: leaf identity changed for {} (refusing to open a swapped file)",
                    key
                )));
            }
        }
        if !fmd.is_file() {
            return Err(Error::Other(format!(
                "open_verified: not a regular file: {}",
                path.display()
            )));
        }
        if fmd.len() != expected_size {
            return Err(Error::Other(format!(
                "open_verified: size changed for {} (expected {expected_size}, found {})",
                key,
                fmd.len()
            )));
        }
        if let Some(expect_ms) = expected_mtime_ms {
            if let Ok(mt) = fmd.modified() {
                if let Some(actual_ms) = system_time_to_ms(mt) {
                    if actual_ms.abs_diff(expect_ms) > tolerance_ms {
                        return Err(Error::Other(format!(
                            "open_verified: mtime changed for {} (expected {expect_ms}, found {actual_ms})",
                            key
                        )));
                    }
                }
            }
        }
        Ok(f)
    }

    /// Validate a download target and create its parents, allocating a unique
    /// temp file whose later rename is the atomic commit. The path stays under
    /// the canonical vault root (P1r7 download half). The file is created with
    /// `create_new` + bounded retry (R4-L2/W40) so a pre-planted symlink or
    /// stale leftover at a predictable name is skipped, never followed.
    ///
    /// The returned third element is the chain of directories this call is
    /// about to create (`create_dir_all(parent)`), deepest-first (W66/A-L2):
    /// a later download failure removes exactly what we created, never a
    /// pre-existing empty dir.
    pub fn tmp_path_for(&self, key: &str) -> Result<(PathBuf, std::fs::File, Vec<PathBuf>), Error> {
        let path = key_to_local_path(&self.root, key)?;
        let parent = path.parent().ok_or_else(|| {
            Error::Other(format!(
                "tmp_path_for has no parent dir: {}",
                path.display()
            ))
        })?;
        self.ensure_locality(parent)?;
        // W66/A-L2: computed BEFORE `create_dir_all` so the failure cleanup
        // can remove exactly the dirs we created.
        let created_dirs = created_dir_chain(parent);
        // W78/r9 L2: the remaining fallible tail (create_dir_all, the second
        // ensure_locality, alloc_temp_sibling) runs under the cleanup helper,
        // so ANY post-creation failure removes exactly the dirs we created -
        // the leak is structurally impossible, including the three exits the
        // review enumerated (second ensure_locality, alloc exhaustion, and a
        // partially-creating create_dir_all). The entry-time ensure_locality
        // and `created_dir_chain` stay outside the closure: they create
        // nothing.
        with_created_dirs_cleanup(&created_dirs, || {
            std::fs::create_dir_all(parent)?;
            self.ensure_locality(parent)?;
            alloc_temp_sibling(&path)
        })
        .map(|(tmp, f)| (tmp, f, created_dirs))
    }

    /// Commit a temp file written by a download to its final path, applying
    /// the remote mtime (atomic rename; temp removed on failure).
    ///
    /// Durability level (r10-L3/W89): file bytes synced before the rename
    /// (W48) + parent dirent synced post-commit on unix (via
    /// [`LocalFs::commit_temp`]); non-unix has no dir fsync in std and the
    /// gap is a documented decision, not an accident.
    ///
    /// r2-L2 decline rationale (recorded): the temp file is re-opened by
    /// path here after the executor's writer handle is dropped, deliberately.
    /// Passing an open handle into `finalize_write` would couple the
    /// executor's writer lifetime to the locality re-check/rename tail for
    /// one saved open+close per file, and `finalize_write` also needs the
    /// path (not just the fd) for the temp-cleanup and rename tails. The
    /// re-open window is provably harmless (unique temp name, `create_new`
    /// allocation, owner-only file).
    pub fn finalize_write(
        &self,
        key: &str,
        tmp: &Path,
        mtime_ms: Option<u64>,
    ) -> Result<(), Error> {
        let path = key_to_local_path(&self.root, key)?;
        let result = (|| -> Result<(), Error> {
            let f = std::fs::File::open(tmp)?;
            if let Some(ms) = mtime_ms {
                set_file_mtime_ms(&f, ms)?;
            }
            f.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(tmp);
            return result;
        }
        // W6/B-M3: re-verify locality immediately before the atomic rename
        // (mirrors write_atomic's post-create_dir_all re-check). A parent
        // swapped for an out-of-vault symlink since `tmp_path_for` must not
        // be renamed through; on refusal the temp sibling is removed. W15/
        // A-L2: a rename error likewise cleans the temp sibling.
        let parent = path.parent().ok_or_else(|| {
            Error::Other(format!(
                "finalize_write has no parent dir: {}",
                path.display()
            ))
        })?;
        self.commit_temp(parent, tmp, &path)
    }

    /// Pull destination freshness (W13/B-L4, symmetric to upload R3.3): before
    /// a download overwrites, re-stat the destination and report whether it
    /// still matches the plan (size + mtime within `tolerance_ms`). A vanished
    /// destination returns [`Freshness::Vanished`] (recreate it); a symlinked
    /// or non-regular destination is an error (fail closed). A destination
    /// that changed since planning returns [`Freshness::Changed`] (do NOT
    /// overwrite the user's newer edits).
    pub fn destination_freshness(
        &self,
        key: &str,
        expected_size: u64,
        expected_mtime_ms: Option<u64>,
        tolerance_ms: u64,
    ) -> Result<Freshness, Error> {
        let path = key_to_local_path(&self.root, key)?;
        let smd = match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Freshness::Vanished),
            Err(e) => return Err(e.into()),
            Ok(md) => md,
        };
        if smd.file_type().is_symlink() {
            return Err(Error::Other(format!(
                "download destination is now a symlink: {}",
                path.display()
            )));
        }
        if !smd.is_file() {
            return Err(Error::Other(format!(
                "download destination is no longer a regular file: {}",
                path.display()
            )));
        }
        if smd.len() != expected_size {
            return Ok(Freshness::Changed);
        }
        if let Some(expect_ms) = expected_mtime_ms {
            if let Ok(mt) = smd.modified() {
                if let Some(actual_ms) = system_time_to_ms(mt) {
                    if actual_ms.abs_diff(expect_ms) > tolerance_ms {
                        return Ok(Freshness::Changed);
                    }
                }
            }
        }
        Ok(Freshness::Fresh)
    }

    /// Whether a pull destination is a pre-existing symlink. Used by the
    /// remote-only download guard to give an accurate message (R4-L5/W43): a
    /// symlink was skipped by the walk (so the key is remote-only), which is
    /// not the same as a destination that *appeared since plan*. `NotFound`
    /// -> false.
    pub fn is_symlink_destination(&self, key: &str) -> Result<bool, Error> {
        let path = key_to_local_path(&self.root, key)?;
        match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
            Ok(md) => Ok(md.file_type().is_symlink()),
        }
    }

    /// Whether a pull destination is currently absent (W22/N2/L3). Used to
    /// guard a `remote_only` download that had no planned local entity: a
    /// destination that appeared since the plan (a regular file, directory, or
    /// symlink) must never be clobbered by the rename. `NotFound` is the only
    /// "absent" case; anything that exists counts as present (fail closed).
    pub fn destination_absent(&self, key: &str) -> Result<bool, Error> {
        let path = key_to_local_path(&self.root, key)?;
        match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(e) => Err(e.into()),
            Ok(_) => Ok(false),
        }
    }

    /// Atomically write `expected_size` bytes from `r` to `key`'s file and set
    /// its mtime. Created via a temp sibling + rename so a partial write is
    /// never visible at the final path. Rejects a path escaping the canonical
    /// vault root (P1r7 download half).
    pub fn write_atomic(
        &self,
        key: &str,
        r: &mut dyn std::io::Read,
        expected_size: u64,
        mtime_ms: Option<u64>,
    ) -> Result<(), Error> {
        let path = key_to_local_path(&self.root, key)?;
        let parent = path.parent().ok_or_else(|| {
            Error::Other(format!(
                "write_atomic has no parent dir: {}",
                path.display()
            ))
        })?;
        // Refuse to write outside the canonicalized vault root even when a
        // parent component has been swapped for an out-of-vault symlink.
        self.ensure_locality(parent)?;

        std::fs::create_dir_all(parent)?;
        // Re-check after dir creation, in case create_dir_all resolved a symlink.
        self.ensure_locality(parent)?;

        let (tmp, mut f) = alloc_temp_sibling(&path)?;
        let write_result = (|| -> Result<(), Error> {
            let copied = std::io::copy(&mut r.take(expected_size), &mut f)?;
            if copied != expected_size {
                return Err(Error::Other(format!(
                    "write_atomic: short write for {key} (expected {expected_size}, got {copied})"
                )));
            }
            if let Some(ms) = mtime_ms {
                set_file_mtime_ms(&f, ms)?;
            }
            f.sync_all()?;
            Ok(())
        })();

        if write_result.is_err() {
            let _ = std::fs::remove_file(&tmp);
            // Surface the underlying short-write as such, not a rename failure.
            return write_result;
        }
        // W27/M5: route the commit through the shared locality-rechecking tail
        // (pre-rename locality + temp cleanup on any failure), matching
        // finalize_write's W6/W15 behavior.
        self.commit_temp(parent, &tmp, &path)
    }

    /// Delete a single file (never a directory). Missing keys are
    /// [`Error::NotFound`] (not idempotent, matching the store trait).
    /// W50/A-M1: re-verifies parent locality before `remove_file` (mirrors
    /// `open_verified`): a parent swapped for an out-of-vault symlink since
    /// the walk must be refused, never unlinked through.
    pub fn delete_file(&self, key: &str) -> Result<(), Error> {
        let path = key_to_local_path(&self.root, key)?;
        let parent = path.parent().ok_or_else(|| {
            Error::Other(format!("delete_file has no parent dir: {}", path.display()))
        })?;
        self.ensure_locality(parent)?;
        self.unlink_local_file(parent, &path, key)
    }

    /// Delete a single file only if it still matches the plan (R4-L1/W39,
    /// symmetric to upload R3.3 / download W13): a `pull --delete` must not
    /// remove a file on the plan's say-so alone. Re-stats the path (no-follow)
    /// and refuses when it vanished (`NotFound`), became a symlink or a
    /// non-regular file, or drifted in size / mtime beyond `tolerance_ms`.
    ///
    /// W50/A-M1: re-verifies parent locality *before* the freshness stat -
    /// a parent swapped for an out-of-vault symlink must be refused even
    /// when the planted outside inode matches the planned size/mtime
    /// (freshness authenticates the inode, not the path resolution).
    ///
    /// N3-residual (W60): the guarded delete is a check-then-act stat
    /// followed by a by-path `remove_file` (std has no fd-based delete), so
    /// a leaf swapped in the window between the stat and the unlink is still
    /// removed - the same residual class as the download note; fd-based
    /// delete is a post-v1 item. The *parent* swap half is closed by the
    /// pre-unlink locality re-check in [`LocalFs::unlink_local_file`] (W60,
    /// B-M1), which mirrors `commit_temp`.
    pub fn delete_file_guarded(
        &self,
        key: &str,
        expected_size: u64,
        expected_mtime_ms: Option<u64>,
        tolerance_ms: u64,
    ) -> Result<(), Error> {
        let path = key_to_local_path(&self.root, key)?;
        let parent = path.parent().ok_or_else(|| {
            Error::Other(format!(
                "delete_file_guarded has no parent dir: {}",
                path.display()
            ))
        })?;
        // W50/A-M1: re-verify parent locality before the freshness stat. A
        // `notes/` -> out-of-vault symlink swap between walk and execute must
        // be refused (fail closed) even when the outside file matches the
        // planned size/mtime - the outside inode is never unlinked.
        self.ensure_locality(parent)?;
        let smd = match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NotFound(key.to_string()));
            }
            Err(e) => return Err(e.into()),
            Ok(md) => md,
        };
        // Never unlink a symlink the walk listed as a file (or a node the walk
        // could not have produced): no-follow, fail closed.
        if smd.file_type().is_symlink() {
            return Err(Error::Other(format!(
                "refusing to delete a symlink (was planned as a file): {}",
                path.display()
            )));
        }
        if !smd.is_file() {
            return Err(Error::Other(format!(
                "refusing to delete a non-regular file: {}",
                path.display()
            )));
        }
        if smd.len() != expected_size {
            return Err(Error::Other(format!(
                "local file changed since plan for {key}; not deleting"
            )));
        }
        if let Some(expect_ms) = expected_mtime_ms {
            if let Ok(mt) = smd.modified() {
                if let Some(actual_ms) = system_time_to_ms(mt) {
                    if actual_ms.abs_diff(expect_ms) > tolerance_ms {
                        return Err(Error::Other(format!(
                            "local file changed since plan for {key}; not deleting"
                        )));
                    }
                }
            }
        }
        // W76 (r8b M1 / r8a-5): route the unlink through the shared seam
        // (`unlink_local_file`) like `delete_file` does. This closes the
        // stat-to-unlink parent-swap window on the only executor-used delete
        // path (mirroring `commit_temp`) and maps a mid-window vanish to
        // `Error::NotFound` (the W32 goal-state arm) instead of a raw IO
        // error. The entry-time `ensure_locality` above stays - it guards the
        // freshness stat itself (W50).
        self.unlink_local_file(parent, &path, key)
    }

    /// Shared unlink tail for both delete APIs (`delete_file` and
    /// `delete_file_guarded`): re-verify parent locality immediately before
    /// the unlink (W60/B-M1, mirroring `commit_temp`). A parent swapped for
    /// an out-of-vault symlink since the entry-time check - the race window
    /// between the freshness stat and the unlink - must be refused, never
    /// unlinked through. `NotFound` maps to [`Error::NotFound`] (the
    /// unguarded API's contract). Leaf-swap residuals are unchanged (the
    /// entry checks already close the walk-to-stat window; the stat-to-
    /// unlink leaf window remains, fd-based delete is post-v1).
    ///
    /// Durability level (r10-L3/W89): after a successful `remove_file`, the
    /// parent directory is fsynced best-effort on unix so the dirent removal
    /// survives a crash/power loss; non-unix platforms have no dir fsync in
    /// std and the gap is a documented decision, not an accident.
    fn unlink_local_file(&self, parent: &Path, path: &Path, key: &str) -> Result<(), Error> {
        // W76/r8b M1: fire the test-only pre-unlink hook (if any) so tests
        // can inject the stat-to-unlink race through production APIs.
        #[cfg(test)]
        run_pre_unlink_hook();
        self.ensure_locality(parent)?;
        match std::fs::remove_file(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(Error::NotFound(key.to_string()))
            }
            Err(e) => Err(e.into()),
            Ok(()) => {
                // r10-L3/W89: the unlink is committed; fsync the parent
                // best-effort so the dirent update is durable (never surfaced
                // as an op failure - the goal state is already achieved).
                let _ = fsync_parent_dir(parent);
                Ok(())
            }
        }
    }

    /// Remove now-empty directories bottom-up (R2.1 option a): children-first,
    /// stop at any non-empty dir, never remove the vault root. Returns how
    /// many directories were removed plus per-dir warning strings for dirs
    /// that could not be removed (R4/R5 nit, W47) or whose emptiness could
    /// not be probed (an unreadable dir, W57) - unspecified failures are
    /// surfaced instead of silently swallowed. Only touches dirs that are
    /// currently empty; file deletes are planned separately by the executor.
    /// Remove now-empty ancestor directories of `keys` bottom-up (W77/r9 M1,
    /// R2.1 option a, scoped): children-first along each deleted file's
    /// ancestor chain, stop at the first non-empty or non-removable dir in a
    /// chain, never remove the vault root, dedup dirs across chains. Replaces
    /// the old vault-wide `remove_empty_dirs_bottom_up`, which removed
    /// pre-existing, plan-unrelated empty dirs and reported nothing. Returns
    /// how many directories were removed plus per-dir warning strings for
    /// dirs that could not be removed (R4/R5 nit, W47) or whose emptiness
    /// could not be probed (an unreadable dir, W57) - unspecified failures
    /// are surfaced instead of silently swallowed. Only touches dirs that are
    /// currently empty; file deletes are planned separately by the executor.
    pub fn remove_empty_ancestor_dirs(&self, keys: &[String]) -> Result<(u32, Vec<String>), Error> {
        let root_canon = self.root_canonical()?;
        let mut removed = 0u32;
        let mut warnings: Vec<String> = Vec::new();
        // Ancestor dirs of each key, deepest-first (the file's parent first,
        // walking up to but excluding the root), dedup'd across chains.
        let mut dirs: Vec<PathBuf> = Vec::new();
        for key in keys {
            // Keys come from the plan (already validated), but re-route
            // through the single join site defensively; an invalid key simply
            // contributes no chain.
            let Ok(path) = key_to_local_path(&self.root, key) else {
                continue;
            };
            let mut probe = path.parent();
            while let Some(p) = probe {
                if p == self.root {
                    break; // never remove the vault root
                }
                if !dirs.iter().any(|d| d == p) {
                    dirs.push(p.to_path_buf());
                }
                probe = p.parent();
            }
        }
        // Deepest-first: a parent must only be probed after its children
        // were (possibly) removed. Each single key's chain is accumulated
        // deepest-first, but interleaving across keys can put a shallower
        // shared ancestor before a deeper dir of another chain, so sort by
        // path depth descending.
        dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
        for d in &dirs {
            // Canonicality guard (P1r7): a dir must still resolve under the
            // canonicalized vault root - an ancestor swapped for an
            // out-of-vault symlink is never removed through.
            let canon = match std::fs::canonicalize(d) {
                Ok(c) => c,
                Err(_) => continue, // vanished mid-pass
            };
            if canon == root_canon {
                continue; // never remove the root (belt and braces)
            }
            if !canon.starts_with(root_canon) {
                continue;
            }
            // W57: the emptiness probe is honest - a directory whose read_dir
            // fails (e.g. chmod 0o000) is surfaced as a cleanup warning
            // naming the dir, never silently treated as non-empty.
            let empty = match is_empty_dir(d) {
                Ok(b) => b,
                Err(e) => {
                    warnings.push(format!("could not inspect empty dir {}: {e}", d.display()));
                    continue;
                }
            };
            if !empty {
                continue;
            }
            match std::fs::remove_dir(d) {
                Ok(()) => removed += 1,
                // Vanished mid-pass: the goal state (dir absent) is
                // achieved; do not report.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => warnings.push(format!("could not remove empty dir {}: {e}", d.display())),
            }
        }
        Ok((removed, warnings))
    }

    /// Guard: the deepest existing ancestor of `dir` must canonicalize to stay
    /// under the canonicalized vault root. A parent swapped for an out-of-vault
    /// symlink canonicalizes outside and is refused before any mkdir/write.
    fn ensure_locality(&self, dir: &Path) -> Result<(), Error> {
        let root_canon = self.root_canonical()?;
        let mut probe: &Path = dir;
        loop {
            if probe.exists() {
                let canon = std::fs::canonicalize(probe)?;
                if !canon.starts_with(root_canon) {
                    return Err(Error::Other(format!(
                        "refusing to operate outside the vault root: {}",
                        probe.display()
                    )));
                }
                return Ok(());
            }
            match probe.parent() {
                Some(p) if !p.as_os_str().is_empty() => probe = p,
                _ => return Ok(()),
            }
        }
    }

    /// Shared commit tail for the two atomic write paths (`write_atomic` and
    /// `finalize_write`): re-verify parent locality immediately before the
    /// atomic rename (W6/M5) and remove the temp sibling on any failure
    /// (W15/A-L2). Keeping both write paths on one helper prevents them from
    /// drifting (W27/M5).
    ///
    /// Durability level (r10-L3/W89): the temp file's bytes are synced
    /// before the rename (W48), and after a successful rename the parent
    /// directory is fsynced best-effort on unix so the dirent update
    /// survives a crash/power loss; non-unix platforms have no dir fsync in
    /// std and the gap is a documented decision, not an accident.
    fn commit_temp(&self, parent: &Path, tmp: &Path, final_path: &Path) -> Result<(), Error> {
        if let Err(e) = self.ensure_locality(parent) {
            let _ = std::fs::remove_file(tmp);
            return Err(e);
        }
        if let Err(e) = std::fs::rename(tmp, final_path) {
            let _ = std::fs::remove_file(tmp);
            return Err(e.into());
        }
        // r10-L3/W89: the rename is committed; fsync the parent best-effort
        // so the dirent update is durable (never surfaced as an op failure -
        // the file is already at its final path, and a reported failure would
        // invite a misleading retry).
        let _ = fsync_parent_dir(parent);
        Ok(())
    }
}

// Test-only pre-unlink hook machinery (W76/r8b M1): the stat-to-unlink race
// window of a delete lives *inside* one call, so it cannot be injected from
// outside the process. The hook fires inside [`LocalFs::unlink_local_file`]
// immediately before its pre-unlink locality re-check, letting a test swap
// the parent (or vanish the file) deterministically through ANY production
// API that routes through the seam. Because the hook lives in the seam, a
// future drift (a delete API ending in a bare `remove_file`) fails those
// tests loudly instead of silently skipping them - the reviewer's "thin
// test-only hook that cannot drift from the production tail" shape.
#[cfg(test)]
thread_local! {
    static PRE_UNLINK_HOOK: std::cell::RefCell<Option<Box<dyn FnMut() + 'static>>> =
        std::cell::RefCell::new(None);
}

/// Run the currently-installed pre-unlink hook, if any. Called inside
/// `unlink_local_file` under `#[cfg(test)]`; a no-op when no hook is set.
#[cfg(test)]
fn run_pre_unlink_hook() {
    PRE_UNLINK_HOOK.with(|h| {
        if let Some(f) = h.borrow_mut().as_mut() {
            f();
        }
    });
}

// r10-L3/W89: durability counter bumped by [`fsync_parent_dir`] (cfg(test),
// unix) so tests can assert the seam actually fires on the production
// commit/delete tails - the W76 pre-unlink-hook pattern, so a future drift
// (a commit/delete tail that skips the fsync) fails loudly instead of
// silently skipping.
#[cfg(all(test, unix))]
thread_local! {
    static FSYNC_COUNT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// fsync a directory so a rename/unlink's dirent update is durable
/// (r10-L3/W89). Callers invoke it best-effort AFTER the rename/unlink
/// already succeeded: the operation is committed, so an fsync failure is
/// never surfaced as an op failure (that would invite a misleading retry of
/// an already-done rename/unlink). Std-only on unix (`File::open(parent)` +
/// `sync_all`); a documented no-op on non-unix (see the `commit_temp` /
/// `unlink_local_file` doc comments for the durability level statement).
fn fsync_parent_dir(parent: &Path) -> std::io::Result<()> {
    #[cfg(all(test, unix))]
    FSYNC_COUNT.with(|c| c.set(c.get() + 1));
    #[cfg(unix)]
    {
        std::fs::File::open(parent)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
        Ok(())
    }
}

/// Install a pre-unlink hook for the current test thread; the returned guard
/// restores the previous hook on drop (cargo test runs tests on parallel
/// threads, so the thread-local scoping keeps hooks from leaking between
/// tests). The hook fires exactly once, inside the seam, after the guarded
/// delete's freshness stat and before its pre-unlink locality re-check.
#[cfg(test)]
fn with_pre_unlink_hook(f: impl FnMut() + 'static) -> PreUnlinkHookGuard {
    PRE_UNLINK_HOOK.with(|h| {
        let prev = h.borrow_mut().take();
        *h.borrow_mut() = Some(Box::new(f));
        PreUnlinkHookGuard { prev }
    })
}

#[cfg(test)]
struct PreUnlinkHookGuard {
    prev: Option<Box<dyn FnMut() + 'static>>,
}

#[cfg(test)]
impl Drop for PreUnlinkHookGuard {
    fn drop(&mut self) {
        PRE_UNLINK_HOOK.with(|h| *h.borrow_mut() = self.prev.take());
    }
}

/// The single vault-relative -> local-path join site (R2.2 lock). All file
/// operations (reader, writer, deletes) route through here so a key is
/// validated exactly once before any filesystem action. Rejects invalid keys
/// (`..`, absolute, control chars via [`crate::entity::ensure_valid_key`]) and
/// folder keys (trailing `/`) which are not valid file-operation targets.
pub fn key_to_local_path(root: &Path, key: &str) -> Result<PathBuf, Error> {
    crate::entity::ensure_valid_key(key)?;
    if key.ends_with('/') {
        return Err(Error::InvalidKey(format!(
            "folder keys are not valid for file operations: {key:?}"
        )));
    }
    Ok(root.join(key))
}

/// 100 unique temp-sibling candidate paths (`.name.vaultsync-tmp-<pid>-<n>`),
/// so the allocator can skip a stale leftover or a pre-planted symlink and
/// retry at the next candidate instead of following/truncating through it
/// (R4-L2/W40, mirroring the W29 upload-side `alloc_first` pattern).
///
/// NAME_MAX invariant (r10-M1/W85): every candidate's final component is
/// at most 255 bytes. A leaf longer than `TEMP_NAME_LEAF_BUDGET` bytes has
/// its embedded name replaced by `{first <budget> bytes on a char
/// boundary}-{fnv1a64(name):016x}` - readability from the head, uniqueness
/// from the hash of the full name (two leaves sharing the head but differing
/// later stay disjoint even with the same pid+counter). The budget reserves
/// the fixed `.`+`-`+16-hex+`.`+`.vaultsync-tmp-`+`-` suffix (35 bytes) plus
/// the theoretical maxima of the `pid` (u32, 10 digits) and `n` (u64, 20
/// digits) so the invariant holds unconditionally, not just for typical
/// pids/counters.
const TEMP_NAME_LEAF_BUDGET: usize = 190;

/// FNV-1a 64-bit (std-only, no external dep): the uniqueness hash for long
/// leaf names in [`temp_sibling_candidates`]. Stable across runs and
/// platforms - the hash must never feed back into allocator semantics beyond
/// candidate-name uniqueness.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Truncate `s` to at most `budget` bytes on a UTF-8 char boundary (never
/// split a multi-byte char). Returns `s` unchanged when it already fits.
fn truncate_char_boundary(s: &str, budget: usize) -> &str {
    if s.len() <= budget {
        return s;
    }
    let mut end = budget;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn temp_sibling_candidates(final_path: &Path) -> Vec<PathBuf> {
    let name = final_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "obj".to_string());
    // r10-M1/W85: cap the embedded leaf name so a legal 255-byte (or
    // multibyte) leaf cannot overflow NAME_MAX in the candidate names.
    let name_part = if name.len() > TEMP_NAME_LEAF_BUDGET {
        format!(
            "{}-{:016x}",
            truncate_char_boundary(&name, TEMP_NAME_LEAF_BUDGET),
            fnv1a64(name.as_bytes())
        )
    } else {
        name
    };
    let dir = final_path.parent().unwrap_or_else(|| Path::new("."));
    let pid = std::process::id();
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let start = COUNTER.fetch_add(100, std::sync::atomic::Ordering::Relaxed);
    (0..100)
        .map(|i| dir.join(format!(".{name_part}.vaultsync-tmp-{pid}-{}", start + i as u64)))
        .collect()
}

/// Allocate the first free temp-sibling candidate exclusively with `create_new`
/// (R4-L2/W40): a pre-planted symlink or stale leftover at an earlier
/// candidate is skipped untouched - never followed/truncated - and the fresh
/// file is created at the next free one. Bounded (100 candidates); only if
/// every candidate is taken is it a loud error (never an infinite loop).
fn alloc_temp_sibling(final_path: &Path) -> Result<(PathBuf, std::fs::File), Error> {
    alloc_temp_sibling_from(&temp_sibling_candidates(final_path))
}

/// Slice variant of [`alloc_temp_sibling`] that consumes an explicit candidate
/// list, so tests can plant a leftover/symlink in a known candidate and verify
/// the allocator skips it and moves to the next.
fn alloc_temp_sibling_from(candidates: &[PathBuf]) -> Result<(PathBuf, std::fs::File), Error> {
    for p in candidates {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(p)
        {
            Ok(f) => return Ok((p.clone(), f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(Error::Io(e)),
        }
    }
    Err(Error::Io(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique temp file",
    )))
}

/// Whether a single path segment / file name is the reserved connectivity-
/// probe name `.vaultsync-check-*` (W19/W24 + R4-L4/W42, W54/A-L2). Shared
/// by the local walker (final segment of a local path) and the remote ingest
/// filter (final segment of a remote key) so the two policies cannot drift.
pub(crate) fn is_check_probe_name(name: &str) -> bool {
    name.starts_with(".vaultsync-check-")
}

/// Whether a file name (a key's final segment or a path's file name) is a
/// reserved vaultsync temp/probe name. The string form of
/// [`is_reserved_vaultsync_name`], shared by the local walker and the remote
/// ingest filter so the two policies cannot drift (W63/A-L3). Covers:
///
/// - the temp sibling pattern `.*.vaultsync-tmp-*` (`.name.vaultsync-tmp-
///   <pid>-<n>`), written by the download/upload temp paths and never
///   syncable (W23/M1);
/// - the connectivity-probe prefix `.vaultsync-check-*`, a crashed `check`
///   leftover (W19/W24 + R4-L4/W42) - a materialized stray probe must never
///   re-upload.
pub(crate) fn is_reserved_vaultsync_key_name(name: &str) -> bool {
    (name.starts_with('.') && name.contains(".vaultsync-tmp-")) || is_check_probe_name(name)
}

/// The directories `create_dir_all(parent)` will create (W66/A-L2): the chain
/// from `parent` up to but excluding the deepest pre-existing ancestor, in
/// deepest-first order (children before parents) so a later bottom-up
/// best-effort removal can iterate as-is and only touch what we created.
/// Computed BEFORE `create_dir_all`, so a failure cleanup never removes a
/// pre-existing empty dir.
fn created_dir_chain(parent: &Path) -> Vec<PathBuf> {
    let mut created = Vec::new();
    let mut probe = parent;
    loop {
        if probe.exists() {
            break;
        }
        created.push(probe.to_path_buf());
        match probe.parent() {
            Some(p) if !p.as_os_str().is_empty() => probe = p,
            _ => break,
        }
    }
    created
}

/// Run a fallible post-creation tail; on `Err`, remove the created dirs
/// bottom-up (best-effort, only while empty) before returning the error
/// (W78/r9 L2). Used by `tmp_path_for` so no post-creation failure can leak
/// the dirs it just created.
fn with_created_dirs_cleanup<T>(
    created_dirs: &[PathBuf],
    tail: impl FnOnce() -> Result<T, Error>,
) -> Result<T, Error> {
    match tail() {
        Ok(v) => Ok(v),
        Err(e) => {
            remove_created_dirs(created_dirs);
            Err(e)
        }
    }
}

/// Best-effort bottom-up removal of the dirs a failing operation created
/// (W66/A-L2, shared by exec's download cleanup and `tmp_path_for`'s own
/// failure cleanup W78/r9 L2): `remove_dir` refuses a non-empty dir (and a
/// `NotFound` is the goal state), so only still-empty created dirs are
/// removed, deepest-first as `created_dir_chain` produced them. Errors are
/// swallowed: the primary failure is already being reported, and any leftover
/// is either pre-existing or will be cleaned by the next empty-dir pass.
pub(crate) fn remove_created_dirs(created: &[PathBuf]) {
    for d in created {
        let _ = std::fs::remove_dir(d);
    }
}

/// Whether a file name is a reserved vaultsync temp/probe name. The walker
/// treats these as never-syncable and skips/counts them.
fn is_reserved_vaultsync_name(name: Option<&std::ffi::OsStr>) -> bool {
    let Some(name) = name else { return false };
    is_reserved_vaultsync_key_name(&name.to_string_lossy())
}

/// Set a file's mtime in ms since epoch (std `File::set_times`, stable).
fn set_file_mtime_ms(f: &std::fs::File, ms: u64) -> Result<(), Error> {
    let t = UNIX_EPOCH + std::time::Duration::from_millis(ms);
    let times = std::fs::FileTimes::new().set_modified(t);
    f.set_times(times)?;
    Ok(())
}

/// Whether a directory currently has no entries. `Err` when the emptiness
/// cannot be determined (e.g. an unreadable directory) - the caller surfaces
/// it as a warning instead of silently assuming non-empty (W57).
fn is_empty_dir(d: &Path) -> std::io::Result<bool> {
    let mut rd = std::fs::read_dir(d)?;
    Ok(rd.next().is_none())
}

fn walk(
    dir: &Path,
    root: &Path,
    root_canon: &Path,
    out: &mut Vec<Entity>,
    opts: &WalkOpts,
    report: &mut WalkReport,
    visited: &mut std::collections::HashSet<PathBuf>,
) -> Result<(), Error> {
    // `NotFound` here means the directory vanished after its parent
    // enumerated it (or between `read_dir` and first use): skip silently.
    // All other IO errors (permission, etc.) stay fatal.
    let read_dir = match std::fs::read_dir(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
        Ok(rd) => rd,
    };
    for entry in read_dir {
        let entry = match entry {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
            Ok(e) => e,
        };
        let ft = match entry.file_type() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
            Ok(ft) => ft,
        };
        if ft.is_symlink() {
            if !opts.follow_symlinks {
                // Slice 9 default: skipped + counted (not plan rows).
                report.skipped_symlinks += 1;
                continue;
            }
            handle_followed_symlink(&entry.path(), root, root_canon, out, report, visited)?;
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .map_err(|_| Error::Other(format!("walk path escaped root: {}", path.display())))?;

        // Key construction/validation happens only for nodes that will be
        // emitted. Special files (FIFO/socket/device) fall through both arms
        // below with their names never inspected: they are always skipped, so
        // an invalid name must not abort the walk (P1r7-special-node-key).
        if ft.is_dir() {
            let key = format!("{}/", path_to_key(rel)?);
            if let Some(e) = folder_entity(&path, &key)? {
                out.push(e);
            }
            walk(&path, root, root_canon, out, opts, report, visited)?;
        } else if ft.is_file() {
            // W23/M1: a reserved vaultsync temp sibling (crash leftover) is
            // never emitted as a key.
            if is_reserved_vaultsync_name(path.file_name()) {
                report.skipped_temp_files += 1;
            } else {
                let key = path_to_key(rel)?;
                if let Some(e) = file_entity(&path, &key)? {
                    out.push(e);
                }
            }
        }
    }
    Ok(())
}

/// Follow a symlink (only called with `follow_symlinks` on). A target that
/// escapes the canonicalized vault root is skipped with a warning; a dir
/// cycle is guarded by a canonical-path visited set.
fn handle_followed_symlink(
    path: &Path,
    root: &Path,
    root_canon: &Path,
    out: &mut Vec<Entity>,
    report: &mut WalkReport,
    visited: &mut std::collections::HashSet<PathBuf>,
) -> Result<(), Error> {
    let rel = path
        .strip_prefix(root)
        .map_err(|_| Error::Other(format!("walk symlink escaped root: {}", path.display())))?;
    let target = match std::fs::canonicalize(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // r10-L1/W87: a dangling symlink in follow mode is skipped WITH
            // a count and a warning, mirroring the out-of-vault and
            // duplicate-target arms below - a dangling link most often
            // signals a moved/deleted target and must not be silent (it was
            // previously invisible: count 0, no warning).
            report.warnings.push(format!(
                "skipping {} (dangling symlink: target does not exist)",
                path_to_key(rel)?
            ));
            report.skipped_symlinks += 1;
            return Ok(());
        }
        Err(e) => return Err(e.into()),
        Ok(t) => t,
    };
    if !target.starts_with(root_canon) {
        report.warnings.push(format!(
            "skipping {} (symlink escapes vault root)",
            path_to_key(rel)?
        ));
        report.skipped_symlinks += 1;
        return Ok(());
    }
    let tmd = match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
        Ok(m) => m,
    };
    if tmd.is_dir() {
        if !visited.insert(target.clone()) {
            // R5-L8/W46: a second link to an already-followed target (or a
            // true cycle) is skipped, but now warns and counts instead of
            // being silently omitted.
            report.warnings.push(format!(
                "skipping {} (symlink target already reached via another link)",
                path_to_key(rel)?
            ));
            report.skipped_symlinks += 1;
            return Ok(()); // cycle guard / duplicate-target guard
        }
        // W67/A-L5: an in-vault dir-symlink target is always independently
        // walked (out-of-vault targets are skipped earlier), so the alias
        // always double-lists the target's content under both keys - push a
        // deterministic warning. Dedup is DECLINED (R-b): which copy would
        // survive depends on `read_dir` enumeration order, so both copies
        // stay listed and synced; a sync tool must not have a
        // nondeterministic inventory.
        let target_rel = target
            .strip_prefix(root_canon)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| target.display().to_string());
        report.warnings.push(format!(
            "following {} duplicates {}/ (dir symlink target is inside the vault); both copies are listed and synced",
            path_to_key(rel)?,
            target_rel
        ));
        let key = format!("{}/", path_to_key(rel)?);
        if let Some(e) = folder_entity(path, &key)? {
            out.push(e);
        }
        // Recursing into a symlink-to-dir path: `read_dir` follows the link,
        // so children get vault-relative keys under the symlink's own path.
        walk(
            path,
            root,
            root_canon,
            out,
            &WalkOpts {
                follow_symlinks: true,
            },
            report,
            visited,
        )?;
    } else if tmd.is_file() {
        // W23/M1: skip a reserved temp sibling reached via a followed symlink,
        // for symmetry with the default walk.
        if is_reserved_vaultsync_name(path.file_name()) {
            report.skipped_temp_files += 1;
            return Ok(());
        }
        let key = path_to_key(rel)?;
        // R4-M1/W38: record that this file key came from a followed *file*
        // symlink. Transfers refuse to open a symlink, so the planner marks
        // these rows Skip(followed_symlink) in mutating modes; `--follow-
        // symlinks` is inventory-only in v1. Dir-symlink children are NOT
        // recorded here (they transfer fine).
        report.followed_files.insert(key.clone());
        if let Some(e) = file_entity(path, &key)? {
            out.push(e);
        }
    }
    Ok(())
}

/// Stat a single directory into an entity, tolerating `NotFound` (the
/// directory vanished between enumeration and stat). All other IO errors stay
/// fatal.
fn folder_entity(path: &Path, key: &str) -> Result<Option<Entity>, Error> {
    let md = match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
        Ok(md) => md,
    };
    Ok(Some(Entity {
        key: key.to_string(),
        size: 0,
        mtime_ms: mtime_of(&md),
        etag: None,
    }))
}

/// Stat a single file into an entity, tolerating `NotFound` (the entry
/// vanished between `read_dir` and the stat). All other IO errors stay fatal.
fn file_entity(path: &Path, key: &str) -> Result<Option<Entity>, Error> {
    let md = match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
        Ok(md) => md,
    };
    Ok(Some(Entity {
        key: key.to_string(),
        size: md.len(),
        mtime_ms: mtime_of(&md),
        etag: None,
    }))
}

/// Build a vault-relative key from an already-stripped path, normalizing to
/// `/` separators and validating it as a key. Fails closed on invalid keys
/// (e.g. `..` or empty segments) rather than emitting them downstream.
///
/// The key is built from `Path::components()` joined with `/`, never by a
/// blind `\` -> `/` rewrite: on Unix a filename that itself contains `\`
/// stays a single `Normal` component and is rejected by `ensure_valid_key`
/// (fail loud, consistent with P1r4-key-ctl); on Windows the components API
/// already yields separator-free parts. Non-UTF8 components also fail loud
/// (no U+FFFD lossy collapse).
fn path_to_key(rel: &Path) -> Result<String, Error> {
    let mut segments: Vec<&str> = Vec::new();
    for comp in rel.components() {
        match comp {
            std::path::Component::Normal(os) => {
                // Fail closed on non-UTF8 names instead of collapsing to
                // U+FFFD and colliding with a real replacement-char name.
                let seg = os.to_str().ok_or_else(|| {
                    Error::InvalidKey(format!("key component is not valid UTF-8: {os:?}"))
                })?;
                segments.push(seg);
            }
            // Defense in depth: walker-produced `rel` (from `strip_prefix`)
            // never starts with `.` and never contains an interior `.`, so a
            // `CurDir` here would be a leading `./...`; `ParentDir`/`Prefix`/
            // `RootDir` should not appear at all. Reject rather than guess.
            std::path::Component::ParentDir
            | std::path::Component::CurDir
            | std::path::Component::Prefix(_)
            | std::path::Component::RootDir => {
                return Err(Error::InvalidKey(format!(
                    "key contains a non-normal path component: {rel:?}"
                )));
            }
        }
    }
    let key = segments.join("/");
    crate::entity::ensure_valid_key(&key)?;
    Ok(key)
}

/// Client-visible mtime in ms since epoch from a `Metadata` already in hand
/// (size and mtime come from the same stat; no second syscall). `None` is
/// produced only when `md.modified()` fails - the conversion itself never
/// returns `None` (pre-epoch saturates via `system_time_to_ms`).
fn mtime_of(md: &std::fs::Metadata) -> Option<u64> {
    match md.modified() {
        Ok(t) => system_time_to_ms(t),
        Err(_) => None,
    }
}

/// Convert a `SystemTime` to ms since epoch. Always returns `Some`: pre-epoch
/// times saturate to `Some(0)` (known, very old) rather than collapsing to
/// `None`. The `Option` in the caller (`mtime_of`) is carried by
/// `md.modified()` failing, not by this conversion.
fn system_time_to_ms(t: SystemTime) -> Option<u64> {
    Some(
        t.duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn keys(fs: &LocalFs) -> Vec<String> {
        fs.list().unwrap().iter().map(|e| e.key.clone()).collect()
    }

    #[test]
    fn local_path_to_key_normalizes_and_validates() {
        // plain relative path is fine and slash-normalized
        assert_eq!(path_to_key(Path::new("a/b.md")).unwrap(), "a/b.md");
        // a `..` component in the relative path is rejected (fail-closed)
        assert!(path_to_key(Path::new("foo/../bar.md")).is_err());
    }

    #[test]
    fn local_path_to_key_rejects_unix_backslash_name() {
        // A Unix file whose *name* contains a backslash is a single path
        // component; it must fail key validation, not be silently rewritten
        // into the nested key `a/b.md` (H1).
        let err = path_to_key(Path::new("a\\b.md")).unwrap_err();
        assert!(matches!(err, Error::InvalidKey(_)));
        let msg = format!("{err}");
        assert!(msg.contains("backslash"), "msg: {msg}");
    }

    #[test]
    fn local_list_empty_dir() {
        let dir = TempDir::new("vaultsync-test");
        let fs = LocalFs::new(dir.path());
        assert_eq!(fs.list().unwrap(), Vec::<Entity>::new());
    }

    #[test]
    fn local_list_files_and_nested() {
        let dir = TempDir::new("vaultsync-test");
        std::fs::write(dir.join("a.md"), "hi").unwrap();
        std::fs::create_dir_all(dir.join("n")).unwrap();
        std::fs::write(dir.join("n/b.md"), "yo").unwrap();
        let fs = LocalFs::new(dir.path());
        let ks = keys(&fs);
        assert!(ks.contains(&"a.md".to_string()));
        assert!(ks.contains(&"n/".to_string()));
        assert!(ks.contains(&"n/b.md".to_string()));
    }

    #[test]
    fn local_keys_use_slash_not_backslash() {
        let dir = TempDir::new("vaultsync-test");
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::write(dir.join("a/b.txt"), "x").unwrap();
        let fs = LocalFs::new(dir.path());
        for k in keys(&fs) {
            assert!(!k.contains('\\'), "key {k:?} has backslash");
        }
        assert!(keys(&fs).contains(&"a/b.txt".to_string()));
    }

    #[test]
    fn local_mtime_and_size_populated() {
        let dir = TempDir::new("vaultsync-test");
        let payload = b"12345";
        std::fs::write(dir.join("a.md"), payload).unwrap();
        let fs = LocalFs::new(dir.path());
        let ents = fs.list().unwrap();
        let a = ents.iter().find(|e| e.key == "a.md").unwrap();
        assert_eq!(a.size, 5);
        assert!(a.mtime_ms.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn local_list_rejects_backslash_filename() {
        // End-to-end walk: a file literally named `a\b.md` must fail the walk
        // loud (`InvalidKey`), never emit a nested key `a/b.md` (H1).
        use std::os::unix::ffi::OsStrExt;
        let dir = TempDir::new("vaultsync-test");
        let name = std::ffi::OsStr::from_bytes(b"a\\b.md");
        std::fs::write(dir.join(name), "x").unwrap();
        let fs = LocalFs::new(dir.path());
        let res = fs.list();
        assert!(res.is_err(), "expected error, got {res:?}");
        assert!(matches!(res.unwrap_err(), Error::InvalidKey(_)));
    }

    #[cfg(unix)]
    #[test]
    fn local_list_backslash_name_does_not_collide_with_nested_file() {
        // With both `a\b.md` and a real `a/b.md` present, the walk must err
        // (never return two `a/b.md` rows: read_dir order is unspecified, but
        // either order ends in the same loud failure).
        use std::os::unix::ffi::OsStrExt;
        let dir = TempDir::new("vaultsync-test");
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::write(dir.join("a/b.md"), "nested").unwrap();
        let name = std::ffi::OsStr::from_bytes(b"a\\b.md");
        std::fs::write(dir.join(name), "flat").unwrap();
        let fs = LocalFs::new(dir.path());
        assert!(fs.list().is_err(), "expected error, got {:?}", fs.list());
    }

    #[cfg(unix)]
    #[test]
    fn local_path_to_key_rejects_non_utf8_component() {
        // A non-UTF8 filename must fail loud (`InvalidKey`), not collapse to
        // U+FFFD via `to_string_lossy` (L2). Single-component variant keeps
        // the assert independent of separator handling.
        use std::os::unix::ffi::OsStrExt;
        let p = PathBuf::from(std::ffi::OsStr::from_bytes(b"a/\x80.md"));
        let err = path_to_key(&p).unwrap_err();
        assert!(matches!(err, Error::InvalidKey(_)));
        let msg = format!("{err}");
        assert!(msg.contains("UTF-8"), "msg: {msg}");
    }

    #[cfg(unix)]
    #[test]
    fn local_list_follows_symlinked_root() {
        // Characterization lock (P1r6-root-symlink / L4): a symlinked vault
        // *root* is followed by design - the user explicitly named it, and
        // `fs::metadata(&self.root)` resolves the link. The symlink skip
        // applies only to entries *below* the root.
        let dir = TempDir::new("vaultsync-test");
        let real = dir.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("a.md"), "hi").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let fs = LocalFs::new(&link);
        let ks = keys(&fs);
        assert!(ks.contains(&"a.md".to_string()), "ks: {ks:?}");
    }

    #[cfg(unix)]
    #[test]
    fn local_skips_symlinks() {
        let dir = TempDir::new("vaultsync-test");
        let outside = TempDir::new("vaultsync-outside");
        std::fs::write(outside.join("secret.txt"), "s").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), dir.join("link.txt")).unwrap();
        std::fs::write(dir.join("real.txt"), "r").unwrap();
        let fs = LocalFs::new(dir.path());
        let ks = keys(&fs);
        assert!(ks.contains(&"real.txt".to_string()));
        assert!(!ks.iter().any(|k| k == "link.txt"), "symlink not skipped");
    }

    #[cfg(unix)]
    #[test]
    fn local_skips_symlinked_directories() {
        // A symlink to an outside directory is skipped entirely: neither the
        // link itself nor its children appear (B5; `--follow-symlinks` is a
        // Phase 2 policy). Sibling real files still list.
        let dir = TempDir::new("vaultsync-test");
        let outside = TempDir::new("vaultsync-outside");
        std::fs::create_dir_all(outside.join("sub")).unwrap();
        std::fs::write(outside.join("sub/deep.md"), "d").unwrap();
        std::os::unix::fs::symlink(outside.join("sub"), dir.join("linkdir")).unwrap();
        std::fs::write(dir.join("real.md"), "r").unwrap();
        let fs = LocalFs::new(dir.path());
        let ks = keys(&fs);
        assert!(ks.contains(&"real.md".to_string()));
        assert!(
            !ks.iter()
                .any(|k| k == "linkdir" || k.starts_with("linkdir/")),
            "symlinked dir not skipped: {ks:?}"
        );
    }

    #[test]
    fn walk_file_entity_missing_returns_none() {
        // A file that vanishes between `read_dir` and the stat (TOCTOU) must
        // be skipped, not abort the whole walk (A3).
        let dir = TempDir::new("vaultsync-test");
        let missing = dir.join("gone.md");
        assert!(!missing.exists());
        let got = file_entity(&missing, "gone.md").unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn walk_skips_vanished_subdir() {
        // A directory that vanishes before its `read_dir` runs (TOCTOU) must
        // be skipped, not abort the walk (A3). Driven at the walk level: a
        // missing subdir path yields Ok with nothing pushed.
        let dir = TempDir::new("vaultsync-test");
        let missing_sub = dir.join("sub");
        assert!(!missing_sub.exists());
        let mut out = Vec::new();
        let mut report = WalkReport::default();
        let mut visited = std::collections::HashSet::new();
        walk(
            &missing_sub,
            &dir,
            &dir,
            &mut out,
            &WalkOpts {
                follow_symlinks: false,
            },
            &mut report,
            &mut visited,
        )
        .unwrap();
        assert!(out.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn local_list_skips_special_file_with_backslash_name() {
        // A FIFO whose *name* contains a backslash must be skipped - its name
        // is never validated because it is never emitted - while a real file
        // in the same directory still lists (M1/P1r7-special-node-key).
        use std::os::unix::ffi::OsStrExt;
        let dir = TempDir::new("vaultsync-test");
        let name = std::ffi::OsStr::from_bytes(b"a\\b.fifo");
        let ok = std::process::Command::new("mkfifo")
            .arg(dir.join(name))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "mkfifo unavailable or failed to create FIFO");
        std::fs::write(dir.join("real.md"), "x").unwrap();
        let fs = LocalFs::new(dir.path());
        let ks = keys(&fs);
        assert_eq!(ks, vec!["real.md".to_string()], "ks: {ks:?}");
    }

    #[cfg(unix)]
    #[test]
    fn walk_non_notfound_errors_still_fail() {
        // Permission errors are not swallowed: a readable-then-locked subdir
        // aborts the walk (only `NotFound` is tolerated as "vanished").
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("vaultsync-test");
        let sub = dir.join("locked");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("x.md"), "x").unwrap();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o000)).unwrap();
        let fs = LocalFs::new(dir.path());
        let res = fs.list();
        // restore before TempDir drop so cleanup can remove the tree
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(res.is_err(), "expected permission error, got {res:?}");
    }

    #[test]
    fn system_time_to_ms_saturates_pre_epoch() {
        // Pre-1970 times saturate to Some(0) (known, very old) instead of
        // collapsing to None ("unknown mtime"). Post-epoch values unchanged.
        let pre = std::time::UNIX_EPOCH - std::time::Duration::from_secs(1);
        assert_eq!(system_time_to_ms(pre), Some(0));
        let post = std::time::UNIX_EPOCH + std::time::Duration::from_millis(1234);
        assert_eq!(system_time_to_ms(post), Some(1234));
    }

    #[test]
    fn local_pre_epoch_file_mtime_saturates_zero() {
        // End-to-end: a file with a 1955 mtime walks as `mtime_ms: Some(0)`
        // (A4). std-only `FileTimes` (stable since 1.75).
        let dir = TempDir::new("vaultsync-test");
        let f = dir.join("old.md");
        std::fs::write(&f, "old").unwrap();
        let pre_epoch =
            std::time::UNIX_EPOCH - std::time::Duration::from_secs(60 * 60 * 24 * 365 * 15);
        let times = std::fs::FileTimes::new().set_modified(pre_epoch);
        std::fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_times(times)
            .unwrap();
        let fs = LocalFs::new(dir.path());
        let ents = fs.list().unwrap();
        let old = ents.iter().find(|e| e.key == "old.md").unwrap();
        assert_eq!(old.mtime_ms, Some(0));
    }

    #[test]
    fn local_root_file_errors_clearly() {
        // A vault root that is a plain file must fail with a vaultsync-owned
        // message naming the path, not a raw `Not a directory (os error 20)`
        // string from the OS (L1).
        let dir = TempDir::new("vaultsync-test");
        let f = dir.join("root.md");
        std::fs::write(&f, "x").unwrap();
        let fs = LocalFs::new(&f);
        let err = fs.list().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("vault root is not a directory"), "msg: {msg}");
        assert!(msg.contains("root.md"), "msg: {msg}");
    }

    #[test]
    fn local_missing_root_errors() {
        let missing = PathBuf::from("/nonexistent/vaultsync-xyz");
        let fs = LocalFs::new(&missing);
        assert!(fs.list().is_err());
    }
    // --- Phase 2 Slice 3: key_to_local_path, reader, writer, delete ---

    #[test]
    fn key_to_local_path_joins_under_root() {
        let dir = TempDir::new("vaultsync-test");
        let p = key_to_local_path(dir.path(), "notes/a.md").unwrap();
        assert_eq!(p, dir.join("notes/a.md"));
    }

    #[test]
    fn key_to_local_path_rejects_traversal() {
        // `..`, absolute, control-char keys are rejected *before* any join -
        // the single validation site for all file operations.
        let dir = TempDir::new("vaultsync-test");
        for bad in ["../evil.md", "/abs.md", "a/\nb.md", "a/../b.md", ".."] {
            let err = key_to_local_path(dir.path(), bad).unwrap_err();
            assert!(matches!(err, Error::InvalidKey(_)), "key {bad:?}: {err}");
        }
    }

    #[test]
    fn key_to_local_path_rejects_folder_key_for_file_ops() {
        // A trailing `/` (folder key) is not a valid file-operation target.
        let dir = TempDir::new("vaultsync-test");
        let err = key_to_local_path(dir.path(), "notes/").unwrap_err();
        assert!(matches!(err, Error::InvalidKey(_)));
    }

    #[test]
    fn local_read_streams_bytes() {
        let dir = TempDir::new("vaultsync-test");
        std::fs::write(dir.join("a.md"), "hello world").unwrap();
        let fs = LocalFs::new(dir.path());
        let mut f = fs.open_verified("a.md", 11, None, 1000).unwrap();
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut f, &mut buf).unwrap();
        assert_eq!(buf, "hello world");
    }

    #[cfg(unix)]
    #[test]
    fn local_open_rechecks_type_not_symlink() {
        // A listed file replaced by a symlink between list and open must fail
        // open loud (no-follow). Driven by creating the symlink directly at
        // open time; the TOCTOU window itself is not timing-tested.
        let dir = TempDir::new("vaultsync-test");
        let outside = TempDir::new("vaultsync-outside");
        std::fs::write(outside.join("secret"), "s").unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), dir.join("a.md")).unwrap();
        let fs = LocalFs::new(dir.path());
        let err = fs.open_verified("a.md", 1, None, 1000).unwrap_err();
        assert!(
            format!("{err}").contains("symlink"),
            "expected symlink refusal, got: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn open_verified_refuses_out_of_vault_parent_swap() {
        // W26/M4: if a parent dir is swapped for an out-of-vault symlink
        // between walk and open, `open_verified` must refuse (locality) and
        // never open the outside file under a vault key. `notes/` -> symlink to
        // an outside dir that also contains a.md.
        let dir = TempDir::new("vaultsync-test");
        let outside = TempDir::new("vaultsync-outside");
        std::fs::create_dir_all(dir.join("notes")).unwrap();
        std::fs::write(dir.join("notes/a.md"), "in-vault").unwrap();
        std::fs::create_dir_all(outside.join("notes")).unwrap();
        std::fs::write(outside.join("notes/a.md"), "OUTSIDE-SECRET").unwrap();
        let fs = LocalFs::new(dir.path());
        // capture size/mtime as the plan would
        let md = std::fs::metadata(dir.join("notes/a.md")).unwrap();
        // swap the parent for a symlink to the outside dir
        std::fs::remove_dir_all(dir.join("notes")).unwrap();
        std::os::unix::fs::symlink(outside.join("notes"), dir.join("notes")).unwrap();
        let err = fs
            .open_verified("notes/a.md", md.len(), None, 1000)
            .unwrap_err();
        assert!(
            format!("{err}").contains("outside"),
            "expected locality refusal: {err}"
        );
        // the outside file must be untouched (never opened/read as a vault key)
        assert_eq!(
            std::fs::read(outside.join("notes/a.md")).unwrap(),
            b"OUTSIDE-SECRET"
        );
    }

    #[test]
    fn local_open_verified_rejects_size_mismatch() {
        // The opened descriptor's size must match the expected size (R3.3).
        let dir = TempDir::new("vaultsync-test");
        std::fs::write(dir.join("a.md"), "12345").unwrap();
        let fs = LocalFs::new(dir.path());
        assert!(fs.open_verified("a.md", 5, None, 1000).is_ok());
        assert!(
            fs.open_verified("a.md", 6, None, 1000).is_err(),
            "size mismatch not caught"
        );
    }

    #[test]
    fn local_open_verified_respects_custom_tolerance() {
        // W2 (PR2 A-H2/B-M1): open_verified honors the ticket-in tolerance, not
        // the hardcoded default. A file 3000 ms off the expected mtime is
        // accepted with tolerance 5000 and rejected with tolerance 1000.
        let dir = TempDir::new("vaultsync-test");
        let f = dir.join("a.md");
        std::fs::write(&f, "12345").unwrap();
        let base = 1_700_000_000_000u64;
        {
            let fh = std::fs::File::open(&f).unwrap();
            let t = std::time::UNIX_EPOCH + std::time::Duration::from_millis(base);
            fh.set_times(std::fs::FileTimes::new().set_modified(t))
                .unwrap();
        }
        let fs = LocalFs::new(dir.path());
        assert!(fs.open_verified("a.md", 5, Some(base + 3000), 5000).is_ok());
        let err = fs
            .open_verified("a.md", 5, Some(base + 3000), 1000)
            .unwrap_err();
        assert!(format!("{err}").contains("mtime changed"), "err: {err}");
    }

    #[test]
    fn local_open_verified_missing_not_found() {
        let dir = TempDir::new("vaultsync-test");
        let fs = LocalFs::new(dir.path());
        assert!(matches!(
            fs.open_verified("gone.md", 1, None, 1000).unwrap_err(),
            Error::NotFound(_)
        ));
    }

    #[test]
    fn local_write_creates_parents_and_bytes() {
        let dir = TempDir::new("vaultsync-test");
        let fs = LocalFs::new(dir.path());
        let mut r = std::io::Cursor::new(b"deebee".to_vec());
        fs.write_atomic("n/deep/b.md", &mut r, 6, None).unwrap();
        assert_eq!(std::fs::read(dir.join("n/deep/b.md")).unwrap(), b"deebee");
    }

    #[test]
    fn local_write_sets_mtime() {
        let dir = TempDir::new("vaultsync-test");
        let fs = LocalFs::new(dir.path());
        let target_ms = 1_700_000_000_123u64;
        let mut r = std::io::Cursor::new(b"x".to_vec());
        fs.write_atomic("a.md", &mut r, 1, Some(target_ms)).unwrap();
        let got = std::fs::metadata(dir.join("a.md"))
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(
            got.abs_diff(target_ms) < 2000,
            "mtime {got} not near {target_ms}"
        );
    }

    #[test]
    fn local_write_atomic_tmp_rename() {
        // A short write (reader ends early) must not leave a partial file at
        // the final path, and must surface the short-write error.
        let dir = TempDir::new("vaultsync-test");
        let fs = LocalFs::new(dir.path());
        let mut r = std::io::Cursor::new(b"tiny".to_vec());
        let err = fs.write_atomic("a.md", &mut r, 100, None).unwrap_err();
        assert!(
            format!("{err}").to_lowercase().contains("short"),
            "err: {err}"
        );
        assert!(
            !dir.join("a.md").exists(),
            "partial file visible at final path"
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_write_stays_under_root() {
        // A key_to_local_path-validated path whose parent is an out-of-vault
        // symlink is detected (canonicalize parent under canonicalized root)
        // and refused before writing (P1r7 download half).
        let dir = TempDir::new("vaultsync-test");
        let outside = TempDir::new("vaultsync-outside");
        std::os::unix::fs::symlink(outside.path(), dir.join("sub")).unwrap();
        let fs = LocalFs::new(dir.path());
        let mut r = std::io::Cursor::new(b"x".to_vec());
        let err = fs.write_atomic("sub/a.md", &mut r, 1, None).unwrap_err();
        assert!(
            format!("{err}").contains("outside"),
            "expected locality refusal, got: {err}"
        );
        assert!(!outside.join("a.md").exists() || !dir.join("sub/a.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_removes_tmp_on_rename_failure() {
        // W27/M5: write_atomic's commit tail must clean its temp on a rename
        // failure (parity with finalize_write/W15) and re-verify parent
        // locality before the rename (parity with finalize_write/W6). Fails
        // today: the bare `rename` propagates the error without removing the
        // temp sibling.
        let dir = TempDir::new("vaultsync-test");
        let fs = LocalFs::new(dir.path());
        // make the final path an existing directory so the rename fails
        // (file -> dir -> EISDIR on Unix); the parent stays a real, writable
        // dir so the cleanup itself can succeed.
        std::fs::create_dir_all(dir.join("sub/a.md")).unwrap();
        let err = fs
            .write_atomic("sub/a.md", &mut std::io::Cursor::new(b"x"), 1, None)
            .unwrap_err();
        assert!(matches!(err, Error::Io(_)), "expected io error, got: {err}");
        let leftover: Vec<String> = std::fs::read_dir(dir.join("sub"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("vaultsync-tmp"))
            .collect();
        assert!(leftover.is_empty(), "write_atomic tmp leaked: {leftover:?}");
    }

    #[test]
    fn finalize_write_rechecks_locality_before_rename() {
        // W6/B-M3: a parent swapped for an out-of-vault symlink between tmp
        // allocation and finalize must be refused (locality) before the rename,
        // and the temp sibling removed; nothing is written outside. Fails
        // today (rename commits through the swapped parent).
        let dir = TempDir::new("vaultsync-test");
        let outside = TempDir::new("vaultsync-outside");
        let fs = LocalFs::new(dir.path());
        let (tmp, _f, _created) = fs.tmp_path_for("sub/a.md").unwrap(); // creates root/sub
        // swap `sub` for an out-of-vault symlink; the tmp now resolves to
        // outside/.a.md.tmp, which we repopulate so finalize's open()/sync
        // succeed and only the locality re-check can refuse.
        std::fs::remove_dir_all(dir.join("sub")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.join("sub")).unwrap();
        std::fs::write(&tmp, "payload").unwrap();
        let err = fs.finalize_write("sub/a.md", &tmp, None).unwrap_err();
        assert!(
            format!("{err}").contains("outside"),
            "expected locality refusal: {err}"
        );
        assert!(!tmp.exists(), "tmp not removed on locality refusal");
        assert!(
            !outside.join("a.md").exists(),
            "renamed through the swapped parent into outside"
        );
    }

    #[cfg(unix)]
    #[test]
    fn fsync_parent_dir_ok_on_real_dir() {
        // r10-L3 (W89): the durability seam itself must succeed on a real
        // directory (File::open + sync_all on the dir). RED: the helper does
        // not exist (compile failure).
        let dir = TempDir::new("vaultsync-test");
        fsync_parent_dir(dir.path()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn commit_and_delete_fsync_parent_dir() {
        // r10-L3 (W89) durability lock: after `finalize_write` (rename) and
        // after `delete_file_guarded` (unlink), the containing directory must
        // have been fsynced - a crash/power loss after a "successful" pull
        // could otherwise lose the dirent update (file bytes are synced
        // pre-rename, W48, but the dirent is not). The counter lives on the
        // fsync seam itself (the W76 pre-unlink-hook pattern), so a future
        // drift - a commit/delete tail that skips the fsync - fails this test
        // loudly instead of silently skipping it. RED: FSYNC_COUNT /
        // fsync_parent_dir unknown (compile failure).
        let dir = TempDir::new("vaultsync-test");
        let fs = LocalFs::new(dir.path());
        // finalize_write path: temp sibling + rename commit
        let (tmp, mut f) = alloc_temp_sibling(&dir.join("a.md")).unwrap();
        std::io::Write::write_all(&mut f, b"x").unwrap();
        fs.finalize_write("a.md", &tmp, None).unwrap();
        // delete_file_guarded path: guarded unlink
        fs.delete_file_guarded("a.md", 1, None, 1000).unwrap();
        let count = FSYNC_COUNT.with(|c| c.get());
        assert!(
            count >= 2,
            "expected >= 2 parent-dir fsyncs (commit + unlink), got {count}"
        );
    }

    #[test]
    fn finalize_write_removes_tmp_on_rename_failure() {
        // W15/A-L2: if the atomic rename fails the temp sibling must be
        // removed. Drive the rename to fail by making the final path an
        // existing directory (rename file->dir -> EISDIR on Unix); the parent
        // stays a real, writable dir so the cleanup itself can succeed (the
        // read-only-parent variant would also block remove_file).
        let dir = TempDir::new("vaultsync-test");
        let fs = LocalFs::new(dir.path());
        let (tmp, _f, _created) = fs.tmp_path_for("sub/a.md").unwrap();
        std::fs::write(&tmp, "payload").unwrap();
        std::fs::create_dir_all(dir.join("sub/a.md")).unwrap();
        let err = fs.finalize_write("sub/a.md", &tmp, None).unwrap_err();
        assert!(matches!(err, Error::Io(_)), "expected io error, got: {err}");
        assert!(
            !tmp.exists(),
            "temp sibling leaked on rename failure: {:?}",
            tmp
        );
        // the directory (final path) is untouched
        assert!(dir.join("sub/a.md").is_dir());
    }

    #[test]
    fn created_dirs_removed_on_tail_failure() {
        // W78 (r9 L2): the cleanup-on-error helper removes the created-dirs
        // chain on a failing tail and keeps it on a succeeding tail. Real
        // temp dirs, helper-level (compile-RED on the helper, the accepted
        // W52/W61 pattern - a deterministic end-to-end RED for the leak is
        // not constructible through the public API).
        let dir = TempDir::new("vaultsync-test");
        // the chain is computed BEFORE the dirs exist, exactly as tmp_path_for
        // does (W66/A-L2), so it names precisely the dirs about to be created
        let parent = dir.join("x/y/z");
        let chain = created_dir_chain(&parent);
        assert_eq!(chain.len(), 3, "expected x/y/z, x/y, x: {chain:?}");
        std::fs::create_dir_all(&parent).unwrap();
        // a failing tail cleans the chain bottom-up
        let err = with_created_dirs_cleanup(&chain, || {
            Err::<(), Error>(Error::Other("tail failed".to_string()))
        })
        .unwrap_err();
        assert!(format!("{err}").contains("tail failed"));
        assert!(!dir.join("x/y/z").exists(), "chain not cleaned on Err");
        assert!(!dir.join("x/y").exists(), "middle not cleaned on Err");
        assert!(!dir.join("x").exists(), "top not cleaned on Err");
        // a succeeding tail keeps the dirs
        let keep = TempDir::new("vaultsync-test");
        let keep_parent = keep.join("x/y/z");
        let keep_chain = created_dir_chain(&keep_parent);
        std::fs::create_dir_all(&keep_parent).unwrap();
        with_created_dirs_cleanup(&keep_chain, || Ok::<(), Error>(())).unwrap();
        assert!(keep.join("x/y/z").exists(), "dirs removed on Ok");
    }

    #[test]
    fn alloc_temp_sibling_all_candidates_taken_errors() {
        // W78 (r9 L2) companion: the allocator's exhaustion error (all 100
        // candidates taken) stays a loud, deterministic error. Locked at the
        // slice level: end-to-end injection through `tmp_path_for` is not
        // deterministic because the candidate counter is process-global and
        // shared across test threads (each `temp_sibling_candidates` call
        // advances it by 100), so the plan's literal pre-plant of "the" 100
        // candidates can never target the range the allocator will actually
        // use.
        let dir = TempDir::new("vaultsync-test");
        let path = dir.join("a.md");
        let candidates = temp_sibling_candidates(&path);
        assert_eq!(candidates.len(), 100);
        for c in &candidates {
            std::fs::write(c, "planted").unwrap();
        }
        let err = alloc_temp_sibling_from(&candidates).unwrap_err();
        assert!(
            format!("{err}").contains("could not allocate a unique temp file"),
            "unexpected err: {err}"
        );
        assert!(!dir.join("a.md").exists(), "planted sibling untouched");
    }

    #[cfg(unix)]
    #[test]
    fn tmp_path_for_creation_failure_leaks_no_dirs() {
        // W78 (r9 L2) smoke through the production API: a post-creation
        // failure inside `tmp_path_for` (create_dir_all on a read-only vault
        // root -> PermissionDenied) propagates cleanly and leaks no created
        // dirs; the root stays intact. (A partial-creation failure is not
        // deterministically injectable either - see the helper-level RED
        // test for the structural guarantee.)
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("vaultsync-test");
        let fs = LocalFs::new(dir.path());
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let err = fs.tmp_path_for("a/b/c.md").unwrap_err();
        // restore perms so TempDir drop can remove the tree
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            format!("{err}").contains("io error"),
            "expected an io error, got: {err}"
        );
        assert!(!dir.join("a").exists(), "created dirs leaked on failure");
        assert!(dir.exists(), "root must be intact");
    }

    #[cfg(unix)]
    #[test]
    fn temp_sibling_create_new_skips_preplanted_symlink() {
        // R4-L2/W40: a pre-planted symlink at an earlier temp candidate
        // (pointing at a victim outside the vault) must be skipped by the
        // allocator (`create_new` -> AlreadyExists), the fresh file created at
        // the next candidate, and the victim left untouched. The old
        // `File::create` semantics would truncate through the link.
        let dir = TempDir::new("vaultsync-test");
        let outside = TempDir::new("vaultsync-outside");
        std::fs::write(outside.join("victim"), "secret").unwrap();
        let final_path = dir.join("f.md");
        let candidates = temp_sibling_candidates(&final_path);
        std::os::unix::fs::symlink(outside.join("victim"), &candidates[0]).unwrap();
        let (tmp, _f) = alloc_temp_sibling_from(&candidates).unwrap();
        assert_ne!(tmp, candidates[0], "must skip the planted symlink");
        assert_eq!(tmp, candidates[1], "fresh file at next candidate");
        // the victim file is untouched (never truncated through the link)
        assert_eq!(std::fs::read(outside.join("victim")).unwrap(), b"secret");
        for c in &candidates {
            let _ = std::fs::remove_file(c);
        }
    }

    #[test]
    fn temp_sibling_create_new_skips_stale_leftover() {
        // R4-L2/W40: a stale regular file at an earlier candidate (a crashed
        // run's leftover) is left untouched and skipped; the fresh file is
        // allocated at the next candidate.
        let dir = TempDir::new("vaultsync-test");
        let final_path = dir.join("f.md");
        let candidates = temp_sibling_candidates(&final_path);
        std::fs::write(&candidates[0], "stale").unwrap();
        let (tmp, _f) = alloc_temp_sibling_from(&candidates).unwrap();
        assert_ne!(tmp, candidates[0], "must skip the stale candidate");
        assert_eq!(tmp, candidates[1], "fresh file at next candidate");
        assert_eq!(std::fs::read(&candidates[0]).unwrap(), b"stale");
        for c in &candidates {
            let _ = std::fs::remove_file(c);
        }
    }

    #[test]
    fn temp_sibling_candidates_caps_long_leaf_names() {
        // r10-M1 (W85): a leaf name longer than ~231 bytes makes every temp
        // sibling candidate exceed NAME_MAX (255 bytes), so `create_new` fails
        // with ENAMETOOLONG and `alloc_temp_sibling_from` returns the raw io
        // error - a 255-byte leaf is legal on disk and as an S3 key, so
        // vaultsync could upload a file it can never pull back. The candidates
        // must cap the embedded leaf name and keep uniqueness via a hash of
        // the full name. Fails today: candidates for a 255-byte leaf are
        // ~280 bytes (NAME_MAX overflow).
        let dir = TempDir::new("vaultsync-test");
        // 255-byte ASCII leaf (legal on disk, NAME_MAX = 255)
        let long: String = "a".repeat(255);
        let candidates = temp_sibling_candidates(&dir.join(&long));
        assert_eq!(candidates.len(), 100);
        for c in &candidates {
            let name_bytes = c.file_name().unwrap().to_string_lossy().len();
            assert!(
                name_bytes <= 255,
                "candidate {} is {name_bytes} bytes (NAME_MAX 255)",
                c.display()
            );
        }
        // 300-byte multibyte leaf ("é" is 2 bytes in UTF-8): the truncation
        // must stay on a char boundary - a broken UTF-8 candidate name would
        // itself be a defect.
        let wide: String = "é".repeat(150);
        assert_eq!(wide.len(), 300);
        let candidates = temp_sibling_candidates(&dir.join(&wide));
        for c in &candidates {
            let name = c.file_name().unwrap().to_str().unwrap();
            assert!(
                name.len() <= 255,
                "candidate {name:?} too long: {} bytes",
                name.len()
            );
        }
        // uniqueness from the hash, not pid+counter: two leaves sharing the
        // first 190 bytes but differing later truncate to the same prefix and
        // must still produce disjoint candidate sets.
        let shared: String = "a".repeat(190);
        let leaf1 = format!("{shared}AAA");
        let leaf2 = format!("{shared}BBB");
        let set1: std::collections::HashSet<PathBuf> =
            temp_sibling_candidates(&dir.join(&leaf1)).into_iter().collect();
        let set2: std::collections::HashSet<PathBuf> =
            temp_sibling_candidates(&dir.join(&leaf2)).into_iter().collect();
        assert!(
            set1.is_disjoint(&set2),
            "candidate sets must be disjoint (hash uniqueness)"
        );
    }

    #[test]
    fn local_destination_absent() {
        // W22/N2/L3: `NotFound` is the only "absent" case; anything that
        // exists - file, dir, or symlink - counts as present (fail closed).
        let dir = TempDir::new("vaultsync-test");
        let fs = LocalFs::new(dir.path());
        assert!(fs.destination_absent("a.md").unwrap());
        std::fs::write(dir.join("a.md"), "x").unwrap();
        assert!(!fs.destination_absent("a.md").unwrap(), "file present");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        assert!(!fs.destination_absent("sub").unwrap(), "dir present");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("a.md"), dir.join("link")).unwrap();
            assert!(
                !fs.destination_absent("link").unwrap(),
                "symlink present (fail closed)"
            );
        }
    }

    #[test]
    fn local_list_skips_vaultsync_tmp_siblings() {
        // W23/M1: the walker must never emit vaultsync temp siblings (a crash
        // leftover `.name.vaultsync-tmp-<pid>-<n>` next to the final path would
        // otherwise list as a normal file and get pushed as a real key).
        let dir = TempDir::new("vaultsync-test");
        std::fs::write(dir.join("note.md"), "real").unwrap();
        std::fs::write(dir.join(".note.md.vaultsync-tmp-123-4"), "crash-leftover").unwrap();
        let fs = LocalFs::new(dir.path());
        let (ents, rep) = fs.list_report().unwrap();
        let keys: Vec<&str> = ents.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["note.md"], "tmp sibling listed: {keys:?}");
        assert_eq!(rep.skipped_temp_files, 1);
    }

    #[test]
    fn walk_skips_check_probe_leftovers() {
        // R4-L4/W42: a `.vaultsync-check-*` probe leftover on the local side
        // (materialized by an earlier stray download) must be skipped and
        // counted, never re-uploaded as a real key.
        let dir = TempDir::new("vaultsync-test");
        std::fs::write(dir.join("note.md"), "real").unwrap();
        std::fs::write(dir.join(".vaultsync-check-1-2-3"), "stray").unwrap();
        let fs = LocalFs::new(dir.path());
        let (ents, rep) = fs.list_report().unwrap();
        let keys: Vec<&str> = ents.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["note.md"], "check leftover listed: {keys:?}");
        assert_eq!(rep.skipped_temp_files, 1);
    }

    #[test]
    fn local_delete_file() {
        let dir = TempDir::new("vaultsync-test");
        std::fs::write(dir.join("a.md"), "x").unwrap();
        let fs = LocalFs::new(dir.path());
        fs.delete_file("a.md").unwrap();
        assert!(!dir.join("a.md").exists());
        // missing -> NotFound (not idempotent)
        assert!(matches!(
            fs.delete_file("a.md").unwrap_err(),
            Error::NotFound(_)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn local_ops_reuse_cached_root_canonicalization() {
        // W81 (r8a-1/r9-N2) characterization: the canonicalized vault root is
        // computed once per LocalFs and cached. A root-level symlink
        // retargeted mid-run does NOT silently move the security boundary:
        // the instance keeps deciding against its first canonicalization
        // (refuses the new target), while a fresh instance adopts the new
        // root - one consistent decision per instance. RED before W81: a
        // per-call canonicalize followed the retarget and accepted the second
        // decision.
        let dir = TempDir::new("vaultsync-test");
        let real1 = dir.join("real1");
        let real2 = dir.join("real2");
        std::fs::create_dir_all(real1.join("notes")).unwrap();
        std::fs::create_dir_all(real2.join("notes")).unwrap();
        std::os::unix::fs::symlink(&real1, dir.join("vault")).unwrap();
        let fs = LocalFs::new(dir.join("vault"));
        // first use: canonicalize the root (cached) and accept an in-root path
        fs.ensure_locality(&dir.join("vault/notes")).unwrap();
        // retarget the root symlink to a DIFFERENT dir mid-run
        std::fs::remove_file(dir.join("vault")).unwrap();
        std::os::unix::fs::symlink(&real2, dir.join("vault")).unwrap();
        // the same instance keeps its first boundary: the new target is
        // outside it and must be refused
        let err = fs.ensure_locality(&dir.join("vault/notes")).unwrap_err();
        assert!(
            format!("{err}").contains("outside"),
            "cached boundary moved: {err}"
        );
        // a fresh instance adopts the new root (per-instance consistency)
        let fresh = LocalFs::new(dir.join("vault"));
        fresh.ensure_locality(&dir.join("vault/notes")).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn delete_file_guarded_refuses_out_of_vault_parent_swap() {
        // W50/A-M1: if a parent dir is swapped for an out-of-vault symlink
        // between walk and delete, `delete_file_guarded` must refuse
        // (locality) before its freshness stat - the outside file must never
        // be unlinked under a vault key. Mirrors open_verified (upload) and
        // the download sides, which already re-check locality. Fails today:
        // the outside file is unlinked (the freshness stat authenticates the
        // inode through the swapped parent).
        let dir = TempDir::new("vaultsync-test");
        let outside = TempDir::new("vaultsync-outside");
        std::fs::create_dir_all(dir.join("notes")).unwrap();
        std::fs::write(dir.join("notes/a.md"), "in-vault").unwrap();
        std::fs::create_dir_all(outside.join("notes")).unwrap();
        // plant a matching outside file: same size + mtime, so only the
        // locality re-check can refuse (freshness would authenticate it).
        let md = std::fs::metadata(dir.join("notes/a.md")).unwrap();
        std::fs::write(outside.join("notes/a.md"), "in-vault").unwrap();
        {
            let f = std::fs::File::open(outside.join("notes/a.md")).unwrap();
            f.set_times(std::fs::FileTimes::new().set_modified(md.modified().unwrap()))
                .unwrap();
        }
        let fs = LocalFs::new(dir.path());
        // swap the parent for a symlink to the outside dir
        std::fs::remove_dir_all(dir.join("notes")).unwrap();
        std::os::unix::fs::symlink(outside.join("notes"), dir.join("notes")).unwrap();
        let err = fs
            .delete_file_guarded(
                "notes/a.md",
                md.len(),
                system_time_to_ms(md.modified().unwrap()),
                1000,
            )
            .unwrap_err();
        assert!(
            format!("{err}").contains("outside"),
            "expected locality refusal: {err}"
        );
        // the outside file must survive (never unlinked through the swapped parent)
        assert_eq!(
            std::fs::read(outside.join("notes/a.md")).unwrap(),
            b"in-vault"
        );
    }

    #[cfg(unix)]
    #[test]
    fn delete_file_guarded_rechecks_locality_before_unlink() {
        // W60 (B-M1) + W76 (r8b M1): the entry-time locality check guards the
        // freshness stat, but the unlink itself must re-check immediately
        // before `remove_file` (mirroring `commit_temp`). This test exercises
        // the PRODUCTION `delete_file_guarded` API: the `#[cfg(test)]` hook
        // inside the shared seam (`unlink_local_file`) swaps the parent for
        // an out-of-vault symlink in the stat-to-unlink window. The outside
        // file must survive - a parent swapped after the entry check must
        // never be unlinked through. RED before W76: the guarded API ended in
        // a bare `remove_file` and never reached the seam, so the hook never
        // fired, the in-vault file was unlinked, `Ok` was returned, and the
        // `unwrap_err` below failed.
        let dir = TempDir::new("vaultsync-test");
        let outside = TempDir::new("vaultsync-outside");
        std::fs::create_dir_all(dir.join("notes")).unwrap();
        std::fs::write(dir.join("notes/a.md"), "in-vault").unwrap();
        std::fs::create_dir_all(outside.join("notes")).unwrap();
        // plant a matching outside file: same size + mtime, so only the
        // locality re-check can refuse (freshness would authenticate it).
        let md = std::fs::metadata(dir.join("notes/a.md")).unwrap();
        std::fs::write(outside.join("notes/a.md"), "in-vault").unwrap();
        {
            let f = std::fs::File::open(outside.join("notes/a.md")).unwrap();
            f.set_times(std::fs::FileTimes::new().set_modified(md.modified().unwrap()))
                .unwrap();
        }
        let fs = LocalFs::new(dir.path());
        // the pre-unlink hook fires inside the seam, in the stat-to-unlink
        // window (after the entry-time check + freshness stat pass against
        // the real in-vault dir): swap the parent for a symlink to the
        // outside dir.
        let hook_parent = dir.join("notes");
        let hook_outside = outside.join("notes");
        let _guard = with_pre_unlink_hook(move || {
            std::fs::remove_dir_all(&hook_parent).unwrap();
            std::os::unix::fs::symlink(&hook_outside, &hook_parent).unwrap();
        });
        let err = fs
            .delete_file_guarded(
                "notes/a.md",
                md.len(),
                system_time_to_ms(md.modified().unwrap()),
                1000,
            )
            .unwrap_err();
        assert!(
            format!("{err}").contains("outside"),
            "expected locality refusal: {err}"
        );
        // the outside file must survive (never unlinked through the swapped
        // parent)
        assert_eq!(
            std::fs::read(outside.join("notes/a.md")).unwrap(),
            b"in-vault"
        );
    }

    #[test]
    fn delete_file_guarded_mid_unlink_vanish_is_not_found() {
        // W76 (r8b M1 secondary fallout): a file that vanishes in the
        // stat-to-unlink window (deleted by another process between the
        // guarded freshness stat and the unlink) must map to
        // `Error::NotFound` (the W32 goal-state arm in the executor), never a
        // raw IO error. Exercised through the production `delete_file_guarded`
        // API via the pre-unlink seam hook. RED before W76: the hook never
        // fired (bare `remove_file` tail), the file was unlinked, and `Ok`
        // was returned.
        let dir = TempDir::new("vaultsync-test");
        std::fs::write(dir.join("a.md"), "bye").unwrap();
        let md = std::fs::metadata(dir.join("a.md")).unwrap();
        let fs = LocalFs::new(dir.path());
        let hook_file = dir.join("a.md");
        let _guard = with_pre_unlink_hook(move || {
            std::fs::remove_file(&hook_file).unwrap();
        });
        let err = fs
            .delete_file_guarded(
                "a.md",
                md.len(),
                system_time_to_ms(md.modified().unwrap()),
                1000,
            )
            .unwrap_err();
        let is_not_found = matches!(&err, Error::NotFound(k) if k == "a.md");
        assert!(is_not_found, "expected NotFound, got: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn delete_file_rechecks_locality_before_unlink() {
        // W60 (B-M1) + W76: same pre-unlink locality re-check for the
        // unguarded `delete_file` API, now exercised through the production
        // API via the pre-unlink seam hook (the entry-time check passes, the
        // hook swaps the parent in the window, the seam must refuse). Kept as
        // a lower-level lock: `delete_file` already routed through the seam,
        // so this test is green both before and after W76.
        let dir = TempDir::new("vaultsync-test");
        let outside = TempDir::new("vaultsync-outside");
        std::fs::create_dir_all(dir.join("notes")).unwrap();
        std::fs::write(dir.join("notes/a.md"), "in-vault").unwrap();
        std::fs::create_dir_all(outside.join("notes")).unwrap();
        std::fs::write(outside.join("notes/a.md"), "outside-plant").unwrap();
        let fs = LocalFs::new(dir.path());
        let hook_parent = dir.join("notes");
        let hook_outside = outside.join("notes");
        let _guard = with_pre_unlink_hook(move || {
            std::fs::remove_dir_all(&hook_parent).unwrap();
            std::os::unix::fs::symlink(&hook_outside, &hook_parent).unwrap();
        });
        let err = fs.delete_file("notes/a.md").unwrap_err();
        assert!(
            format!("{err}").contains("outside"),
            "expected locality refusal: {err}"
        );
        assert_eq!(
            std::fs::read(outside.join("notes/a.md")).unwrap(),
            b"outside-plant"
        );
    }

    #[cfg(unix)]
    #[test]
    fn delete_file_refuses_out_of_vault_parent_swap() {
        // W50/A-M1: the unguarded `delete_file` API has the same locality gap
        // (test-only callers today, but public). A parent swapped for an
        // out-of-vault symlink must be refused and the outside file survives.
        let dir = TempDir::new("vaultsync-test");
        let outside = TempDir::new("vaultsync-outside");
        std::fs::create_dir_all(dir.join("notes")).unwrap();
        std::fs::write(dir.join("notes/a.md"), "in-vault").unwrap();
        std::fs::create_dir_all(outside.join("notes")).unwrap();
        std::fs::write(outside.join("notes/a.md"), "OUTSIDE-SECRET").unwrap();
        let fs = LocalFs::new(dir.path());
        std::fs::remove_dir_all(dir.join("notes")).unwrap();
        std::os::unix::fs::symlink(outside.join("notes"), dir.join("notes")).unwrap();
        let err = fs.delete_file("notes/a.md").unwrap_err();
        assert!(
            format!("{err}").contains("outside"),
            "expected locality refusal: {err}"
        );
        assert_eq!(
            std::fs::read(outside.join("notes/a.md")).unwrap(),
            b"OUTSIDE-SECRET"
        );
    }

    #[cfg(unix)]
    #[test]
    fn remove_empty_ancestor_dirs_reports_failures() {
        // R4/R5 nit (W47), retargeted to the scoped ancestor API (W77): a
        // per-dir removal failure (EACCES) on a deleted file's ancestor chain
        // must be surfaced as a per-dir warning naming the dir, not silently
        // swallowed. The removed count is 0 (nothing succeeded).
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("vaultsync-test");
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a/b/gone.md"), "x").unwrap();
        std::fs::remove_file(dir.join("a/b/gone.md")).unwrap();
        let fs = LocalFs::new(dir.path());
        // lock the parent `a` (read+traverse, no write) so remove_dir(a/b)
        // fails EACCES while the emptiness probe can still read it
        std::fs::set_permissions(dir.join("a"), std::fs::Permissions::from_mode(0o555)).unwrap();
        let (removed, warnings) = fs
            .remove_empty_ancestor_dirs(&["a/b/gone.md".to_string()])
            .unwrap();
        // restore perms so TempDir drop can remove the tree
        std::fs::set_permissions(dir.join("a"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(removed, 0, "no dir removed under locked parent");
        assert!(
            warnings.iter().any(|w| w.contains("b")),
            "no per-dir warning naming b: {warnings:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn remove_empty_ancestor_dirs_warns_on_unreadable_dir() {
        // W57 (B nit), retargeted to the scoped ancestor API (W77): a
        // directory on a deleted file's ancestor chain whose emptiness cannot
        // be probed (read_dir fails, e.g. chmod 0o000) is surfaced as a
        // cleanup warning naming the dir - never silently treated as
        // non-empty and skipped. The removed count is 0 (nothing succeeded).
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("vaultsync-test");
        std::fs::create_dir_all(dir.join("n/zero")).unwrap();
        let fs = LocalFs::new(dir.path());
        std::fs::set_permissions(dir.join("n/zero"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let (removed, warnings) = fs
            .remove_empty_ancestor_dirs(&["n/zero/gone.md".to_string()])
            .unwrap();
        // restore perms so TempDir drop can remove the tree
        std::fs::set_permissions(dir.join("n/zero"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert_eq!(removed, 0, "no dir removed under an unreadable dir");
        assert!(
            warnings.iter().any(|w| w.contains("zero")),
            "no warning naming the unreadable dir: {warnings:?}"
        );
    }

    #[test]
    fn local_remove_empty_ancestor_dirs_removes_chain_bottom_up() {
        // W77 (r9 M1): only the ancestor chains of the given keys are
        // considered, deepest-first; the root is never removed; a non-empty
        // sibling stops the pass for that branch; unrelated dirs are never
        // touched.
        let dir = TempDir::new("vaultsync-test");
        std::fs::create_dir_all(dir.join("a/b/c")).unwrap();
        std::fs::write(dir.join("a/b/c/gone.md"), "x").unwrap();
        std::fs::remove_file(dir.join("a/b/c/gone.md")).unwrap();
        std::fs::create_dir_all(dir.join("keep/sub")).unwrap();
        std::fs::write(dir.join("keep/sub/file.md"), "x").unwrap();
        let fs = LocalFs::new(dir.path());
        let (removed, _warnings) = fs
            .remove_empty_ancestor_dirs(&["a/b/c/gone.md".to_string()])
            .unwrap();
        assert!(!dir.join("a/b/c").exists());
        assert!(!dir.join("a/b").exists());
        assert!(!dir.join("a").exists());
        assert!(dir.exists(), "root must remain");
        assert!(dir.join("keep/sub/file.md").exists(), "non-empty kept");
        assert!(removed >= 3, "removed {removed}");
    }
    // --- Phase 2 Slice 9: symlink policy ---

    #[cfg(unix)]
    #[test]
    fn walk_follow_records_followed_file_keys() {
        // R4-M1/W38: the walker reports exactly which file keys came from a
        // followed *file* symlink. `--follow-symlinks` is inventory-only in v1:
        // the planner needs this set to Skip those rows in mutating modes.
        // Dir-symlink children and regular files are NOT in the set.
        let dir = TempDir::new("vaultsync-test");
        std::fs::write(dir.join("real.md"), "r").unwrap();
        std::fs::create_dir_all(dir.join("realdir")).unwrap();
        std::fs::write(dir.join("realdir/child.md"), "c").unwrap();
        std::os::unix::fs::symlink("real.md", dir.join("link.md")).unwrap();
        std::os::unix::fs::symlink("realdir", dir.join("linkdir")).unwrap();
        let fs = LocalFs::with_follow(dir.path(), true);
        let (ents, report) = fs.list_report().unwrap();
        let keys: Vec<String> = ents.iter().map(|e| e.key.clone()).collect();
        assert!(keys.iter().any(|k| k == "link.md"), "followed file listed");
        assert!(
            keys.iter().any(|k| k == "linkdir/child.md"),
            "dir child listed"
        );
        assert_eq!(
            report.followed_files,
            std::collections::BTreeSet::from(["link.md".to_string()]),
            "followed-file set wrong: {:?}",
            report.followed_files
        );
    }

    #[cfg(unix)]
    #[test]
    fn walk_symlink_skipped_counted_by_default() {
        // Default (follow=false): a symlink file is skipped and counted in the
        // walk report; entities are unchanged from Phase 1.
        let dir = TempDir::new("vaultsync-test");
        let outside = TempDir::new("vaultsync-outside");
        std::fs::write(outside.join("secret"), "s").unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), dir.join("link.txt")).unwrap();
        std::fs::write(dir.join("real.txt"), "r").unwrap();
        let fs = LocalFs::new(dir.path());
        let (ents, report) = fs.list_report().unwrap();
        let keys: Vec<String> = ents.iter().map(|e| e.key.clone()).collect();
        assert!(keys.iter().any(|k| k == "real.txt"));
        assert!(!keys.iter().any(|k| k == "link.txt"), "symlink not skipped");
        assert_eq!(report.skipped_symlinks, 1, "report count");
        assert!(report.warnings.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn walk_follow_symlinks_counts_dangling() {
        // r10-L1 (W87): in follow mode a dangling symlink (target does not
        // exist) is silently dropped - no count, no warning - while the
        // out-of-vault and duplicate-target cases both warn and count. The
        // dangling case most often signals a moved/deleted target, so the
        // follow-mode walk must report it the same way. Fails today: count
        // 0, no warning.
        let dir = TempDir::new("vaultsync-test");
        std::os::unix::fs::symlink("missing-target", dir.join("dangling.md")).unwrap();
        let fs = LocalFs::with_follow(dir.path(), true);
        let (ents, report) = fs.list_report().unwrap();
        let keys: Vec<String> = ents.iter().map(|e| e.key.clone()).collect();
        assert!(
            !keys.iter().any(|k| k == "dangling.md"),
            "dangling symlink must not be listed: {keys:?}"
        );
        assert_eq!(report.skipped_symlinks, 1, "dangling must be counted");
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("dangling.md") && w.contains("dangling")),
            "no dangling warning: {:?}",
            report.warnings
        );
    }

    #[cfg(unix)]
    #[test]
    fn walk_follow_symlinks_includes_in_vault_target() {
        // follow=true: a symlink whose target is in-vault is followed.
        let dir = TempDir::new("vaultsync-test");
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("target/f.md"), "x").unwrap();
        std::os::unix::fs::symlink("target", dir.join("lnk")).unwrap();
        let fs = LocalFs::with_follow(dir.path(), true);
        let (ents, _) = fs.list_report().unwrap();
        let keys: Vec<String> = ents.iter().map(|e| e.key.clone()).collect();
        assert!(
            keys.iter().any(|k| k == "lnk/"),
            "followed dir listed: {keys:?}"
        );
        assert!(
            keys.iter().any(|k| k == "lnk/f.md"),
            "followed child listed: {keys:?}"
        );
        // the real target still lists too
        assert!(keys.iter().any(|k| k == "target/f.md"));
    }

    #[cfg(unix)]
    #[test]
    fn walk_follow_symlinks_skips_escaping_target_with_warning() {
        // follow=true: a symlink escaping the vault root is skipped with a
        // warning (never synced silently).
        let dir = TempDir::new("vaultsync-test");
        let outside = TempDir::new("vaultsync-outside");
        std::fs::write(outside.join("secret"), "s").unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), dir.join("escape")).unwrap();
        let fs = LocalFs::with_follow(dir.path(), true);
        let (ents, report) = fs.list_report().unwrap();
        let keys: Vec<String> = ents.iter().map(|e| e.key.clone()).collect();
        assert!(
            !keys.iter().any(|k| k == "escape"),
            "escaping target emitted: {keys:?}"
        );
        assert!(!keys.iter().any(|k| k.starts_with("escape/")));
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("escape") && w.contains("vault root")),
            "warning missing: {:?}",
            report.warnings
        );
        assert_eq!(report.skipped_symlinks, 1);
    }

    #[cfg(unix)]
    #[test]
    fn walk_follow_warns_on_duplicate_dir_target() {
        // R5-L8/W46: two dir symlinks to the SAME in-vault target. The first
        // is followed; the second must be skipped with an "already reached"
        // warning (and counted), not silently omitted.
        let dir = TempDir::new("vaultsync-test");
        std::fs::create_dir_all(dir.join("realdir")).unwrap();
        std::fs::write(dir.join("realdir/x.md"), "x").unwrap();
        std::os::unix::fs::symlink("realdir", dir.join("d1")).unwrap();
        std::os::unix::fs::symlink("realdir", dir.join("d2")).unwrap();
        let fs = LocalFs::with_follow(dir.path(), true);
        let (ents, report) = fs.list_report().unwrap();
        let keys: Vec<String> = ents.iter().map(|e| e.key.clone()).collect();
        let d1 = keys.iter().any(|k| k == "d1/");
        let d2 = keys.iter().any(|k| k == "d2/");
        // exactly one of d1/, d2/ appears (readdir order is unspecified)
        assert!(d1 ^ d2, "exactly one link dir expected: {keys:?}");
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("already reached") && (w.contains("d1") || w.contains("d2"))),
            "no duplicate-target warning: {:?}",
            report.warnings
        );
        assert_eq!(report.skipped_symlinks, 1);
    }

    #[cfg(unix)]
    #[test]
    fn walk_follow_symlinks_warns_on_in_vault_dir_alias() {
        // W67/A-L5: a dir symlink whose canonical target is inside the vault
        // (which is every followed dir symlink - out-of-vault targets are
        // skipped earlier, and an in-vault target is always independently
        // walked) double-lists the target's content under both keys. The walk
        // must warn deterministically; both copies are still listed and synced
        // (dedup is declined: the surviving key set would depend on read_dir
        // order). Fails today (no warning).
        let dir = TempDir::new("vaultsync-test");
        std::fs::create_dir_all(dir.join("realdir")).unwrap();
        std::fs::write(dir.join("realdir/child.md"), "c").unwrap();
        std::os::unix::fs::symlink("realdir", dir.join("linkdir")).unwrap();
        let fs = LocalFs::with_follow(dir.path(), true);
        let (ents, report) = fs.list_report().unwrap();
        let keys: Vec<String> = ents.iter().map(|e| e.key.clone()).collect();
        // no behavior change: both copies are still listed
        assert!(keys.iter().any(|k| k == "realdir/child.md"));
        assert!(keys.iter().any(|k| k == "linkdir/child.md"));
        assert!(
            report.warnings.iter().any(|w| w.contains("linkdir")
                && w.contains("duplicates")
                && w.contains("realdir/")),
            "no in-vault alias warning: {:?}",
            report.warnings
        );
    }

    #[cfg(unix)]
    #[test]
    fn walk_follow_symlinks_loop_safe() {
        // A dir cycle (a/back -> a) must terminate under follow.
        let dir = TempDir::new("vaultsync-test");
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::write(dir.join("a/leaf.md"), "x").unwrap();
        std::os::unix::fs::symlink(dir.join("a"), dir.join("a/back")).unwrap();
        let fs = LocalFs::with_follow(dir.path(), true);
        let (ents, _) = fs.list_report().unwrap(); // must not hang
        // bounded - the cycle is cut after one level
        let keys: Vec<String> = ents.iter().map(|e| e.key.clone()).collect();
        assert!(keys.iter().any(|k| k == "a/leaf.md"));
        assert!(keys.iter().any(|k| k == "a/back/"));
        // a/back/back must NOT reappear (cycle cut)
        assert!(
            !keys.iter().any(|k| k == "a/back/back/"),
            "cycle not cut: {keys:?}"
        );
        assert!(keys.len() < 8, "unexpected expansion: {keys:?}");
    }
}
