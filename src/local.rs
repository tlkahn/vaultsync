//! Local filesystem walker: turns a vault directory tree into [`Entity`] keys.
//!
//! Walker omissions (Phase 1, all silent and by design):
//! - symlinks - files **and** directories - are skipped entirely
//!   (`--follow-symlinks` is a Phase 2 policy decision, P1r4-symlink);
//! - device / FIFO / socket nodes are skipped (only `is_dir` / `is_file`
//!   entries are emitted);
//! - entries that vanish mid-walk (`NotFound`) are skipped; other IO errors
//!   (e.g. permission) abort the walk loudly;
//! - a local file whose name fails key validation (now including control
//!   chars and whitespace-only segments, P1r4-key-ctl) fails the walk loud
//!   instead of emitting a corrupt key.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::entity::Entity;
use crate::error::Error;

/// Walks a vault root directory. Phase 1 only requires `list`.
pub struct LocalFs {
    root: PathBuf,
}

impl LocalFs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        LocalFs { root: root.into() }
    }

    /// Walk files and folders under the root into vault-relative keys.
    ///
    /// - keys are relative, `/`-separated, no leading `/`
    /// - folders end with `/`
    /// - the root itself is not emitted
    /// - symlinks are skipped
    /// - mtime/size come from `fs::metadata`
    pub fn list(&self) -> Result<Vec<Entity>, Error> {
        // The root itself must exist: a missing vault is a loud error, not an
        // empty plan. `walk` tolerates `NotFound` only for directories
        // discovered mid-walk (vanished between enumeration and recursion).
        let md = std::fs::metadata(&self.root)?;
        if !md.is_dir() {
            // A file-as-root must fail with a clear, vaultsync-owned message
            // (not a raw `Not a directory` OS string).
            return Err(Error::Other(format!(
                "vault root is not a directory: {}",
                self.root.display()
            )));
        }
        let mut out = Vec::new();
        walk(&self.root, &self.root, &mut out)?;
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }
}

fn walk(dir: &Path, root: &Path, out: &mut Vec<Entity>) -> Result<(), Error> {
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
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .map_err(|_| Error::Other(format!("walk path escaped root: {}", path.display())))?;
        let key = path_to_key(rel)?;

        if ft.is_dir() {
            let key = format!("{key}/");
            if let Some(e) = folder_entity(&path, &key)? {
                out.push(e);
            }
            walk(&path, root, out)?;
        } else if ft.is_file()
            && let Some(e) = file_entity(&path, &key)?
        {
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
            // Defense in depth: `rel` is already root-stripped and relative,
            // so none of these should appear; reject them rather than guess.
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
/// (size and mtime come from the same stat; no second syscall).
fn mtime_of(md: &std::fs::Metadata) -> Option<u64> {
    match md.modified() {
        Ok(t) => system_time_to_ms(t),
        Err(_) => None,
    }
}

/// Convert a `SystemTime` to ms since epoch. Pre-epoch times saturate to
/// `Some(0)` (known, very old) rather than collapsing to `None`; `None` again
/// means only "the FS could not provide an mtime".
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
        walk(&missing_sub, &dir, &mut out).unwrap();
        assert!(out.is_empty());
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
}
