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
    report: std::cell::RefCell<WalkReport>,
}

/// Report surfaced from a walk (symlink policy, Slice 9).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct WalkReport {
    /// Symlinks skipped (default mode, or out-of-vault followed targets).
    pub skipped_symlinks: u32,
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
            report: std::cell::RefCell::new(WalkReport::default()),
        }
    }

    /// With `--follow-symlinks`: follow symlinks (loops guarded; out-of-vault
    /// targets are still skipped with a warning, never synced silently).
    pub fn with_follow(root: impl Into<PathBuf>, follow_symlinks: bool) -> Self {
        LocalFs {
            root: root.into(),
            follow_symlinks,
            report: std::cell::RefCell::new(WalkReport::default()),
        }
    }

    /// The report from the most recent walk (symlink skips/warnings).
    pub fn report(&self) -> WalkReport {
        self.report.borrow().clone()
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
        walk(
            &self.root,
            &self.root,
            &mut out,
            &opts,
            &mut report,
            &mut visited,
        )?;
        out.sort_by(|a, b| a.key.cmp(&b.key));
        *self.report.borrow_mut() = report.clone();
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
        let f = std::fs::File::open(&path)?;
        // Stats the OPENED descriptor (not a second path stat): a re-stat of
        // the path could describe a different file that replaced it.
        let fmd = f.metadata()?;
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

    /// Validate a download target and create its parents, returning a unique
    /// temp path whose later rename is the atomic commit. The path stays under
    /// the canonical vault root (P1r7 download half).
    pub fn tmp_path_for(&self, key: &str) -> Result<PathBuf, Error> {
        let path = key_to_local_path(&self.root, key)?;
        let parent = path.parent().ok_or_else(|| {
            Error::Other(format!("tmp_path_for has no parent dir: {}", path.display()))
        })?;
        self.ensure_locality(parent)?;
        std::fs::create_dir_all(parent)?;
        self.ensure_locality(parent)?;
        Ok(temp_sibling(&path))
    }

    /// Commit a temp file written by a download to its final path, applying
    /// the remote mtime (atomic rename; temp removed on failure).
    pub fn finalize_write(&self, key: &str, tmp: &Path, mtime_ms: Option<u64>) -> Result<(), Error> {
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
        // be renamed through; on refusal the temp sibling is removed.
        let parent = path.parent().ok_or_else(|| {
            Error::Other(format!(
                "finalize_write has no parent dir: {}",
                path.display()
            ))
        })?;
        if let Err(e) = self.ensure_locality(parent) {
            let _ = std::fs::remove_file(tmp);
            return Err(e);
        }
        std::fs::rename(tmp, &path)?;
        Ok(())
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
            Error::Other(format!("write_atomic has no parent dir: {}", path.display()))
        })?;
        // Refuse to write outside the canonicalized vault root even when a
        // parent component has been swapped for an out-of-vault symlink.
        self.ensure_locality(parent)?;

        std::fs::create_dir_all(parent)?;
        // Re-check after dir creation, in case create_dir_all resolved a symlink.
        self.ensure_locality(parent)?;

        let tmp = temp_sibling(&path);
        let write_result = (|| -> Result<(), Error> {
            let mut f = std::fs::File::create(&tmp)?;
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
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Delete a single file (never a directory). Missing keys are
    /// [`Error::NotFound`] (not idempotent, matching the store trait).
    pub fn delete_file(&self, key: &str) -> Result<(), Error> {
        let path = key_to_local_path(&self.root, key)?;
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(Error::NotFound(key.to_string()))
            }
            Err(e) => Err(e.into()),
            Ok(()) => Ok(()),
        }
    }

    /// Remove now-empty directories bottom-up (R2.1 option a): children-first,
    /// stop at any non-empty dir, never remove the vault root. Returns how
    /// many directories were removed. Only touches dirs that are currently
    /// empty; file deletes are planned separately by the executor.
    pub fn remove_empty_dirs_bottom_up(&self) -> Result<u32, Error> {
        let root_canon = std::fs::canonicalize(&self.root)?;
        let mut removed = 0u32;
        let mut dirs: Vec<PathBuf> = Vec::new();
        collect_dirs(&self.root, &mut dirs)?;
        // Post-order: deepest first (reverse of pre-order accumulation, which
        // pushes parents before children).
        for d in dirs.iter().rev() {
            let canon = match std::fs::canonicalize(d) {
                Ok(c) => c,
                Err(_) => continue, // vanished mid-pass
            };
            if canon == root_canon {
                continue; // never remove the root
            }
            if canon.starts_with(&root_canon) && is_empty_dir(d) {
                match std::fs::remove_dir(d) {
                    Ok(()) => removed += 1,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => {}
                }
            }
        }
        Ok(removed)
    }

    /// Guard: the deepest existing ancestor of `dir` must canonicalize to stay
    /// under the canonicalized vault root. A parent swapped for an out-of-vault
    /// symlink canonicalizes outside and is refused before any mkdir/write.
    fn ensure_locality(&self, dir: &Path) -> Result<(), Error> {
        let root_canon = std::fs::canonicalize(&self.root)?;
        let mut probe: &Path = dir;
        loop {
            if probe.exists() {
                let canon = std::fs::canonicalize(probe)?;
                if !canon.starts_with(&root_canon) {
                    return Err(Error::Other(format!(
                        "refusing to write outside the vault root: {}",
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

/// A unique temp sibling path (`.name.vaultsync-tmp-<pid>-<n>`) used so an
/// atomic rename replaces the final path only on success.
fn temp_sibling(final_path: &Path) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = final_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "obj".to_string());
    let dir = final_path.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!(".{name}.vaultsync-tmp-{}-{n}", std::process::id()))
}

/// Set a file's mtime in ms since epoch (std `File::set_times`, stable).
fn set_file_mtime_ms(f: &std::fs::File, ms: u64) -> Result<(), Error> {
    let t = UNIX_EPOCH + std::time::Duration::from_millis(ms);
    let times = std::fs::FileTimes::new().set_modified(t);
    f.set_times(times)?;
    Ok(())
}

/// Whether a directory currently has no entries.
fn is_empty_dir(d: &Path) -> bool {
    std::fs::read_dir(d).map(|mut rd| rd.next().is_none()).unwrap_or(false)
}

/// Pre-order accumulate of all directories under `root` (root included).
fn collect_dirs(root: &Path, out: &mut Vec<PathBuf>) -> Result<(), Error> {
    let rd = match std::fs::read_dir(root) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
        Ok(rd) => rd,
    };
    for entry in rd {
        let entry = entry?;
        let ft = entry.file_type()?;
        let p = entry.path();
        if ft.is_dir() {
            out.push(p.clone());
            collect_dirs(&p, out)?;
        }
    }
    out.push(root.to_path_buf());
    Ok(())
}

fn walk(
    dir: &Path,
    root: &Path,
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
            handle_followed_symlink(&entry.path(), root, out, report, visited)?;
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
            walk(&path, root, out, opts, report, visited)?;
        } else if ft.is_file() {
            let key = path_to_key(rel)?;
            if let Some(e) = file_entity(&path, &key)? {
                out.push(e);
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
    out: &mut Vec<Entity>,
    report: &mut WalkReport,
    visited: &mut std::collections::HashSet<PathBuf>,
) -> Result<(), Error> {
    let rel = path
        .strip_prefix(root)
        .map_err(|_| Error::Other(format!("walk symlink escaped root: {}", path.display())))?;
    let target = match std::fs::canonicalize(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
        Ok(t) => t,
    };
    let root_canon = match std::fs::canonicalize(root) {
        Ok(r) => r,
        Err(e) => return Err(e.into()),
    };
    if !target.starts_with(&root_canon) {
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
        if !visited.insert(target) {
            return Ok(()); // cycle guard
        }
        let key = format!("{}/", path_to_key(rel)?);
        if let Some(e) = folder_entity(path, &key)? {
            out.push(e);
        }
        // Recursing into a symlink-to-dir path: `read_dir` follows the link,
        // so children get vault-relative keys under the symlink's own path.
        walk(
            path,
            root,
            out,
            &WalkOpts {
                follow_symlinks: true,
            },
            report,
            visited,
        )?;
    } else if tmd.is_file() {
        let key = path_to_key(rel)?;
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
        walk(&missing_sub, &dir, &mut out, &WalkOpts { follow_symlinks: false }, &mut report, &mut visited).unwrap();
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

    #[test]
    fn local_open_verified_rejects_size_mismatch() {
        // The opened descriptor's size must match the expected size (R3.3).
        let dir = TempDir::new("vaultsync-test");
        std::fs::write(dir.join("a.md"), "12345").unwrap();
        let fs = LocalFs::new(dir.path());
        assert!(fs.open_verified("a.md", 5, None, 1000).is_ok());
        assert!(fs.open_verified("a.md", 6, None, 1000).is_err(), "size mismatch not caught");
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
            fh.set_times(std::fs::FileTimes::new().set_modified(t)).unwrap();
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
        assert!(format!("{err}").to_lowercase().contains("short"), "err: {err}");
        assert!(!dir.join("a.md").exists(), "partial file visible at final path");
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
    fn finalize_write_rechecks_locality_before_rename() {
        // W6/B-M3: a parent swapped for an out-of-vault symlink between tmp
        // allocation and finalize must be refused (locality) before the rename,
        // and the temp sibling removed; nothing is written outside. Fails
        // today (rename commits through the swapped parent).
        let dir = TempDir::new("vaultsync-test");
        let outside = TempDir::new("vaultsync-outside");
        let fs = LocalFs::new(dir.path());
        let tmp = fs.tmp_path_for("sub/a.md").unwrap(); // creates root/sub
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

    #[test]
    fn local_remove_empty_dirs_bottom_up() {
        // Empty nested dirs are removed children-first; the root is never
        // removed; a non-empty sibling stops the pass for that branch.
        let dir = TempDir::new("vaultsync-test");
        std::fs::create_dir_all(dir.join("a/b/c")).unwrap();
        std::fs::create_dir_all(dir.join("keep/sub")).unwrap();
        std::fs::write(dir.join("keep/sub/file.md"), "x").unwrap();
        let fs = LocalFs::new(dir.path());
        let removed = fs.remove_empty_dirs_bottom_up().unwrap();
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
    fn walk_follow_symlinks_includes_in_vault_target() {
        // follow=true: a symlink whose target is in-vault is followed.
        let dir = TempDir::new("vaultsync-test");
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("target/f.md"), "x").unwrap();
        std::os::unix::fs::symlink("target", dir.join("lnk")).unwrap();
        let fs = LocalFs::with_follow(dir.path(), true);
        let (ents, _) = fs.list_report().unwrap();
        let keys: Vec<String> = ents.iter().map(|e| e.key.clone()).collect();
        assert!(keys.iter().any(|k| k == "lnk/"), "followed dir listed: {keys:?}");
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
        assert!(!keys.iter().any(|k| k == "escape"), "escaping target emitted: {keys:?}");
        assert!(!keys.iter().any(|k| k.starts_with("escape/")));
        assert!(
            report.warnings.iter().any(|w| w.contains("escape") && w.contains("vault root")),
            "warning missing: {:?}",
            report.warnings
        );
        assert_eq!(report.skipped_symlinks, 1);
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
        assert!(!keys.iter().any(|k| k == "a/back/back/"), "cycle not cut: {keys:?}");
        assert!(keys.len() < 8, "unexpected expansion: {keys:?}");
    }
}

