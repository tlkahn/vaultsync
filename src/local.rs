//! Local filesystem walker: turns a vault directory tree into [`Entity`] keys.

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
        let mut out = Vec::new();
        walk(&self.root, &self.root, &mut out)?;
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }
}

fn walk(dir: &Path, root: &Path, out: &mut Vec<Entity>) -> Result<(), Error> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
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
            out.push(Entity {
                key,
                size: 0,
                mtime_ms: mtime_of(&path)?,
                etag: None,
            });
            walk(&path, root, out)?;
        } else if ft.is_file() {
            let md = std::fs::metadata(&path)?;
            out.push(Entity {
                key,
                size: md.len(),
                mtime_ms: mtime_of(&path)?,
                etag: None,
            });
        }
    }
    Ok(())
}

/// Build a vault-relative key from an already-stripped path, normalizing to
/// `/` separators and validating it as a key. Fails closed on invalid keys
/// (e.g. `..` or empty segments) rather than emitting them downstream.
fn path_to_key(rel: &Path) -> Result<String, Error> {
    let key = rel.to_string_lossy().replace('\\', "/");
    crate::entity::ensure_valid_key(&key)?;
    Ok(key)
}

fn mtime_of(path: &Path) -> Result<Option<u64>, Error> {
    let md = std::fs::metadata(path)?;
    match md.modified() {
        Ok(t) => Ok(system_time_to_ms(t)),
        Err(_) => Ok(None),
    }
}

fn system_time_to_ms(t: SystemTime) -> Option<u64> {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .ok()
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

    #[test]
    fn local_missing_root_errors() {
        let missing = PathBuf::from("/nonexistent/vaultsync-xyz");
        let fs = LocalFs::new(&missing);
        assert!(fs.list().is_err());
    }
}
