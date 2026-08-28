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
        let p = build_plan(&local, &store, Mode::Status, &PlanOpts::default()).unwrap();
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
        let p = build_plan(&local, &store, Mode::Status, &PlanOpts::default()).unwrap();
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
        let p = build_plan(&local, &store, Mode::Status, &PlanOpts::default()).unwrap();
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

    #[test]
    fn build_plan_ignores_tmp_leftover() {
        // W23/M1: a planted vaultsync temp sibling produces no Upload row and
        // no stray key; only the real file is planned.
        let dir = TempDir::new("vaultsync-lib-test");
        std::fs::write(dir.join("note.md"), "real").unwrap();
        std::fs::write(dir.join(".note.md.vaultsync-tmp-123-4"), "leftover").unwrap();
        let local = LocalFs::new(dir.path());
        let store = MemoryStore::new();
        let p = build_plan(&local, &store, Mode::Push, &PlanOpts::default()).unwrap();
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
        assert!(store.list("").unwrap().is_empty());
    }

    /// A store whose `get_to` always errors after a successful put, to inject
    /// a probe read failure (W24/M2).
    struct GetFailStore {
        inner: MemoryStore,
    }
    impl ObjectStore for GetFailStore {
        fn list(&self, prefix: &str) -> Result<Vec<Entity>, Error> {
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
        fn list(&self, prefix: &str) -> Result<Vec<Entity>, Error> {
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
        let keys: Vec<String> = store.list("").unwrap().into_iter().map(|e| e.key).collect();
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
}
