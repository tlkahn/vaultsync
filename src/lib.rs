//! vaultsync library core.
//!
//! Phase 1 modules: `entity`, `plan`, `local`, `store`.

pub mod cli;
pub mod config;
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
) -> Result<Plan, Error> {
    let local_entities = local.list()?;
    let remote_entities = store.list("")?;
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
            if collided.contains(&a.key) {
                a.kind = plan::ActionKind::Conflict;
                a.reason = "case_collision";
            }
        }
        p.stats = compute_stats(&p.actions);
    }
    Ok(p)
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
    use crate::entity::Entity;
    use crate::plan::{ActionKind, PlanOpts};
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
        fn list(&self, _prefix: &str) -> Result<Vec<Entity>, Error> {
            Ok(self.listed.clone())
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
        let p = build_plan(&local, &store, Mode::Status, &PlanOpts::default()).unwrap();
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
        let remote_ents = store.list("").unwrap();
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
        let p = build_plan(&local, &store, Mode::Status, &PlanOpts::default()).unwrap();
        let note = p.actions.iter().find(|a| a.key == "Note.md").expect("Note.md");
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


}
