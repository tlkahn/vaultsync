//! vaultsync library core.
//!
//! Phase 1 modules: `entity`, `plan`, `local`, `store`.

pub mod cli;
pub mod entity;
pub mod error;
pub mod local;
pub mod plan;
pub mod store;

use std::path::Path;

use crate::error::Error;
use crate::local::LocalFs;
use crate::plan::{ActionKind, Mode, Plan, PlanOpts};
use crate::store::ObjectStore;

/// Build a [`Plan`] from a local walk + a store listing.
pub fn build_plan(
    local: &LocalFs,
    store: &dyn ObjectStore,
    mode: Mode,
    opts: &PlanOpts,
) -> Result<Plan, Error> {
    let local_entities = local.list()?;
    let remote_entities = store.list("")?;
    Ok(plan::plan(&local_entities, &remote_entities, mode, opts))
}

/// Build a [`Plan`] against a store for a real vault directory (Status mode).
pub fn status_with_store(
    vault: &Path,
    store: &dyn ObjectStore,
    opts: &PlanOpts,
) -> Result<Plan, Error> {
    let local = LocalFs::new(vault);
    build_plan(&local, store, Mode::Status, opts)
}

/// Format a plan as human-readable text (Phase 1 subset of [cli.md]).
pub fn format_plan_human(plan: &Plan) -> String {
    let s = &plan.stats;
    let mut out = String::new();
    out.push_str(&format!(
        "plan: {} upload, {} download, {} delete_local, {} delete_remote, {} skip, {} conflict\n",
        s.upload, s.download, s.delete_local, s.delete_remote, s.skip, s.conflict
    ));
    for a in &plan.actions {
        let prefix = match a.kind {
            ActionKind::Upload => "U  ",
            ActionKind::Download => "D  ",
            ActionKind::DeleteLocal => "DL ",
            ActionKind::DeleteRemote => "DR ",
            ActionKind::Skip => "S  ",
            ActionKind::Conflict => "*  ",
        };
        match a.kind {
            ActionKind::Conflict => {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{ActionKind, PlanOpts};
    use crate::store::ObjectStore;
    use crate::store::mock::MemoryStore;
    use crate::testutil::TempDir;

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
}
