//! Pure planner: classifies a local/remote entity diff into actions.
//!
//! No IO and no network: inputs are hand-built [`Entity`] lists.

use std::collections::HashMap;

use crate::entity::Entity;

/// The direction the planned actions will be interpreted for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Status,
    Push,
    Pull,
}

/// Planner options.
#[derive(Debug, Clone, Copy)]
pub struct PlanOpts {
    /// mtime difference (ms) treated as "equal". Default 1000.
    pub mtime_tolerance_ms: u64,
    /// Map delete direction based on mode (`--delete`).
    pub delete: bool,
    /// On conflict, local wins. Mode-aware: Push/Status plan Upload; Pull
    /// keeps local (Skip). Never flips non-Conflict deltas.
    pub force_local: bool,
    /// On conflict, remote wins. Mode-aware: Pull/Status plan Download; Push
    /// keeps remote (Skip). Never flips non-Conflict deltas.
    pub force_remote: bool,
}

impl Default for PlanOpts {
    fn default() -> Self {
        PlanOpts {
            mtime_tolerance_ms: 1000,
            delete: false,
            force_local: false,
            force_remote: false,
        }
    }
}

/// What to do with a single key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Upload,
    Download,
    DeleteLocal,
    DeleteRemote,
    Skip,
    Conflict,
}

/// One planned action for a key.
#[derive(Debug, Clone, PartialEq)]
pub struct Action {
    pub key: String,
    pub kind: ActionKind,
    pub reason: &'static str,
    pub local: Option<Entity>,
    pub remote: Option<Entity>,
}

/// Counts per action kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlanStats {
    pub upload: u32,
    pub download: u32,
    pub delete_local: u32,
    pub delete_remote: u32,
    pub skip: u32,
    pub conflict: u32,
}

/// A full classification of local vs remote.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    pub actions: Vec<Action>,
    pub stats: PlanStats,
}

/// Per-key delta classification between two sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delta {
    Equal,
    LocalOnly,
    RemoteOnly,
    LocalNewer,
    RemoteNewer,
    Conflict,
    /// Both sides present, either mtime unknown, sizes equal -> visible skip.
    EqualUnknownMtime,
    /// Both sides present, either mtime unknown, sizes differ -> conflict.
    ConflictUnknownMtime,
}

/// Reason strings stable enough for tests and human output.
mod reason {
    pub const EQUAL: &str = "equal";
    pub const LOCAL_ONLY: &str = "local_only";
    pub const REMOTE_ONLY: &str = "remote_only";
    pub const LOCAL_NEWER: &str = "local_newer";
    pub const REMOTE_NEWER: &str = "remote_newer";
    pub const CONFLICT_MTIME_SIZE: &str = "conflict_mtime_size";
    pub const CONFLICT_UNKNOWN_MTIME: &str = "conflict_mtime_unknown";
    pub const EQUAL_UNKNOWN_MTIME: &str = "equal_unknown_mtime";
    pub const PATH_COLLISION: &str = "path_collision";
    pub const FOLDER: &str = "folder";
    pub const FORCE_LOCAL: &str = "force_local";
    pub const FORCE_REMOTE: &str = "force_remote";
}

/// Classify one local/remote pair into a delta.
fn classify_pair(local: Option<&Entity>, remote: Option<&Entity>, tol: u64) -> Delta {
    match (local, remote) {
        (None, None) => Delta::Equal,
        (Some(_), None) => Delta::LocalOnly,
        (None, Some(_)) => Delta::RemoteOnly,
        (Some(l), Some(r)) => {
            debug_assert!(
                !l.is_folder() && !r.is_folder(),
                "folders must be filtered in resolve"
            );
            // Phase 2 (4b) unknown-mtime policy: whether either side's mtime is
            // unknown (`None` - the FS could not provide it), an equal-size
            // pair is a visible Skip (zero overwrite risk), a diff-size pair is
            // a Conflict - never a one-sided winner. The old `None -> 0`
            // classifier rule and its silent-pull hole are retired.
            if l.mtime_ms.is_none() || r.mtime_ms.is_none() {
                return if l.size == r.size {
                    Delta::EqualUnknownMtime
                } else {
                    Delta::ConflictUnknownMtime
                };
            }
            // Both mtimes known. Pre-epoch times saturate to `Some(0)` and
            // remain comparable (P1r4-mtime-pre-epoch).
            let lm = l.mtime_ms.unwrap();
            let rm = r.mtime_ms.unwrap();
            if lm.abs_diff(rm) <= tol {
                if l.size == r.size {
                    Delta::Equal
                } else {
                    Delta::Conflict
                }
            } else if lm > rm {
                Delta::LocalNewer
            } else {
                Delta::RemoteNewer
            }
        }
    }
}

/// Build the full plan.
pub fn plan(local: &[Entity], remote: &[Entity], mode: Mode, opts: &PlanOpts) -> Plan {
    let local_map: HashMap<&str, &Entity> = local.iter().map(|e| (e.key.as_str(), e)).collect();
    let remote_map: HashMap<&str, &Entity> = remote.iter().map(|e| (e.key.as_str(), e)).collect();
    // Duplicate input keys currently last-win silently in the map; assert the
    // contract that plan inputs are deduplicated so the behavior cannot creep.
    debug_assert_eq!(
        local_map.len(),
        local.len(),
        "duplicate local keys in plan input"
    );
    debug_assert_eq!(
        remote_map.len(),
        remote.len(),
        "duplicate remote keys in plan input"
    );

    let mut keys: Vec<&str> = local_map.keys().chain(remote_map.keys()).copied().collect();
    keys.sort_unstable();
    keys.dedup();

    // 4a: detect file/folder path collisions (e.g. file `K` coexisting with
    // folder `K/` or a child `K/x`). Such rows are Conflicts in every mode and
    // are never force-resolvable.
    let path_collided = path_collision_keys(&keys);

    let mut actions: Vec<Action> = Vec::new();
    for key in keys {
        let loc = local_map.get(key).copied();
        let rem = remote_map.get(key).copied();
        let (kind, reason) = resolve(loc, rem, mode, opts, path_collided.contains(key));
        actions.push(Action {
            key: key.to_string(),
            kind,
            reason,
            local: loc.cloned(),
            remote: rem.cloned(),
        });
    }

    let mut stats = PlanStats::default();
    for a in &actions {
        match a.kind {
            ActionKind::Upload => stats.upload += 1,
            ActionKind::Download => stats.download += 1,
            ActionKind::DeleteLocal => stats.delete_local += 1,
            ActionKind::DeleteRemote => stats.delete_remote += 1,
            ActionKind::Skip => stats.skip += 1,
            ActionKind::Conflict => stats.conflict += 1,
        }
    }

    Plan { actions, stats }
}

/// Keys involved in a file-vs-folder path collision (P1r-type-collision):
/// any file key `K` that coexists with a `K/` folder key or a `K/...`
/// descendant, plus the colliding folder keys themselves.
fn path_collision_keys(keys: &[&str]) -> std::collections::BTreeSet<String> {
    let mut collided = std::collections::BTreeSet::new();
    for k in keys {
        if k.ends_with('/') {
            let base = &k[..k.len() - 1];
            if keys.contains(&base) {
                collided.insert(k.to_string());
                collided.insert(base.to_string());
            }
        } else {
            let prefix = format!("{k}/");
            for k2 in keys {
                if k2.starts_with(&prefix) {
                    collided.insert(k.to_string());
                    collided.insert(k2.to_string());
                }
            }
        }
    }
    collided
}

/// Map a key's delta to an action kind + reason given the mode and options.
fn resolve(
    local: Option<&Entity>,
    remote: Option<&Entity>,
    mode: Mode,
    opts: &PlanOpts,
    path_collided: bool,
) -> (ActionKind, &'static str) {
    // 4a: a path collision is a Conflict in every mode and is never
    // force-resolvable (the executor never touches these rows).
    if path_collided {
        return (ActionKind::Conflict, reason::PATH_COLLISION);
    }

    // Folders never carry transfer meaning.
    if local.is_some_and(|e| e.is_folder()) || remote.is_some_and(|e| e.is_folder()) {
        return (ActionKind::Skip, reason::FOLDER);
    }

    let delta = classify_pair(local, remote, opts.mtime_tolerance_ms);
    use ActionKind::*;
    match delta {
        Delta::Equal => (Skip, reason::EQUAL),
        // 4b: equal-size with unknown mtime - visible skip, zero overwrite risk.
        Delta::EqualUnknownMtime => (Skip, reason::EQUAL_UNKNOWN_MTIME),
        Delta::LocalOnly => match mode {
            Mode::Status => (Upload, reason::LOCAL_ONLY),
            Mode::Push => (Upload, reason::LOCAL_ONLY),
            Mode::Pull => {
                if opts.delete {
                    (DeleteLocal, reason::LOCAL_ONLY)
                } else {
                    (Skip, reason::LOCAL_ONLY)
                }
            }
        },
        Delta::RemoteOnly => match mode {
            Mode::Status => (Download, reason::REMOTE_ONLY),
            Mode::Push => {
                if opts.delete {
                    (DeleteRemote, reason::REMOTE_ONLY)
                } else {
                    (Skip, reason::REMOTE_ONLY)
                }
            }
            Mode::Pull => (Download, reason::REMOTE_ONLY),
        },
        Delta::LocalNewer => match mode {
            Mode::Status => (Upload, reason::LOCAL_NEWER),
            Mode::Push => (Upload, reason::LOCAL_NEWER),
            Mode::Pull => (Skip, reason::LOCAL_NEWER),
        },
        Delta::RemoteNewer => match mode {
            Mode::Status => (Download, reason::REMOTE_NEWER),
            Mode::Push => (Skip, reason::REMOTE_NEWER),
            Mode::Pull => (Download, reason::REMOTE_NEWER),
        },
        Delta::Conflict => force_conflict(opts, mode, reason::CONFLICT_MTIME_SIZE),
        Delta::ConflictUnknownMtime => {
            force_conflict(opts, mode, reason::CONFLICT_UNKNOWN_MTIME)
        }
    }
}

/// Mode-aware force handling shared by the Conflict variants. Both forces set
/// => they cancel (P1r-both-forces), treated as no force.
fn force_conflict(
    opts: &PlanOpts,
    mode: Mode,
    default_reason: &'static str,
) -> (ActionKind, &'static str) {
    use ActionKind::*;
    let forcing = opts.force_local as u8 + opts.force_remote as u8;
    if forcing > 1 {
        (Conflict, default_reason)
    } else if opts.force_local {
        match mode {
            Mode::Push | Mode::Status => (Upload, reason::FORCE_LOCAL),
            Mode::Pull => (Skip, reason::FORCE_LOCAL),
        }
    } else if opts.force_remote {
        match mode {
            Mode::Pull | Mode::Status => (Download, reason::FORCE_REMOTE),
            Mode::Push => (Skip, reason::FORCE_REMOTE),
        }
    } else {
        (Conflict, default_reason)
    }
}

/// Keys involved in a case-only collision (A2/B4 preflight, 4c lock).
///
/// v1 key identity is case-sensitive, codepoint-exact, with **no NFC
/// normalization** (documented). This detects case-only collisions - within
/// either side's list or across the local/remote pairing - via a Unicode
/// lower-case fold heuristic, so a `Note.md` vs `note.md` clash is a Conflict
/// (`case_collision`) that is never auto-paired as Equal.
///
/// Pure: no IO.
pub fn case_collision_keys(
    local: &[Entity],
    remote: &[Entity],
) -> std::collections::BTreeSet<String> {
    use std::collections::{BTreeSet, HashMap};
    // fold -> (local actual keys, remote actual keys)
    let mut fold_map: HashMap<String, (BTreeSet<String>, BTreeSet<String>)> = HashMap::new();
    for e in local {
        fold_map
            .entry(e.key.to_lowercase())
            .or_default()
            .0
            .insert(e.key.clone());
    }
    for e in remote {
        fold_map
            .entry(e.key.to_lowercase())
            .or_default()
            .1
            .insert(e.key.clone());
    }
    let mut collided = BTreeSet::new();
    for (_, (lset, rset)) in fold_map {
        let same_side_dup = lset.len() > 1 || rset.len() > 1;
        let cross_side = !lset.is_empty() && !rset.is_empty() && lset != rset;
        if same_side_dup || cross_side {
            for k in &lset {
                collided.insert(k.clone());
            }
            for k in &rset {
                collided.insert(k.clone());
            }
        }
    }
    collided
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{file, folder};

    fn opts() -> PlanOpts {
        PlanOpts::default()
    }

    fn kinds(p: &Plan) -> Vec<ActionKind> {
        p.actions.iter().map(|a| a.kind).collect()
    }

    fn run_status(local: &[Entity], remote: &[Entity]) -> Plan {
        plan(local, remote, Mode::Status, &opts())
    }

    fn run_status_tol(local: &[Entity], remote: &[Entity], tol: u64) -> Plan {
        let o = PlanOpts {
            mtime_tolerance_ms: tol,
            ..Default::default()
        };
        plan(local, remote, Mode::Status, &o)
    }

    #[test]
    fn plan_pre_epoch_zero_vs_none_mtime_same_size_skips_unknown_mtime() {
        // Phase 2 4b ENTERS (decision row 4b): the retired Phase 1 rule
        // `None -> 0` is deleted. `Some(0)` vs `None` with equal size now
        // classifies `equal_unknown_mtime` (visible Skip, zero overwrite risk)
        // because either side's mtime is unknown. Amends P1r6-mtime-zero.
        let p = plan(
            &[file("a.md", 5, Some(0))],
            &[file("a.md", 5, None)],
            Mode::Status,
            &opts(),
        );
        assert_eq!(kinds(&p), vec![ActionKind::Skip]);
        assert_eq!(p.actions[0].reason, "equal_unknown_mtime");
    }

    #[test]
    fn plan_pre_epoch_zero_vs_none_mtime_diff_size_conflicts_unknown() {
        // Phase 2 4b ENTERS: `Some(0)` vs `None` with different sizes now
        // surfaces a `conflict_mtime_unknown` row (never a silent winner).
        let p = plan(
            &[file("a.md", 5, Some(0))],
            &[file("a.md", 6, None)],
            Mode::Status,
            &opts(),
        );
        assert_eq!(kinds(&p), vec![ActionKind::Conflict]);
        assert_eq!(p.actions[0].reason, "conflict_mtime_unknown");
    }

    #[test]
    fn plan_pull_remote_none_mtime_diff_size_conflicts() {
        // Phase 2 4b ENTERS (fixes the P1r5-mtime-pull hole): a remote missing
        // mtime against a present local with different sizes is a Conflict
        // `conflict_mtime_unknown`, not a local-wins Skip.
        let p = plan(
            &[file("a.md", 100, Some(5000))],
            &[file("a.md", 200, None)],
            Mode::Pull,
            &opts(),
        );
        assert_eq!(kinds(&p), vec![ActionKind::Conflict]);
        assert_eq!(p.actions[0].reason, "conflict_mtime_unknown");
    }

    #[test]
    fn plan_pull_remote_none_mtime_same_size_skips_unknown() {
        // Phase 2 4b ENTERS: same-size with a None-mtime remote is a visible
        // Skip `equal_unknown_mtime` (no overwrite risk), not a local-wins
        // Skip.
        let p = plan(
            &[file("a.md", 100, Some(5000))],
            &[file("a.md", 100, None)],
            Mode::Pull,
            &opts(),
        );
        assert_eq!(kinds(&p), vec![ActionKind::Skip]);
        assert_eq!(p.actions[0].reason, "equal_unknown_mtime");
    }

    #[test]
    fn plan_status_remote_none_mtime_diff_size_conflicts() {
        // Phase 2 4b ENTERS: Status now surfaces a None-mtime remote with a
        // size mismatch as a Conflict (was Upload / local_newer under the
        // retired `None -> 0` rule).
        let p = plan(
            &[file("a.md", 100, Some(5000))],
            &[file("a.md", 200, None)],
            Mode::Status,
            &opts(),
        );
        assert_eq!(kinds(&p), vec![ActionKind::Conflict]);
        assert_eq!(p.actions[0].reason, "conflict_mtime_unknown");
    }

    #[test]
    fn plan_both_empty() {
        let p = run_status(&[], &[]);
        assert!(p.actions.is_empty());
        assert_eq!(p.stats, PlanStats::default());
    }

    #[test]
    fn plan_local_only_file_status() {
        let p = run_status(&[file("a.md", 1, Some(100))], &[]);
        assert_eq!(kinds(&p), vec![ActionKind::Upload]);
        assert_eq!(p.actions[0].reason, "local_only");
        assert_eq!(p.stats.upload, 1);
    }

    #[test]
    fn plan_remote_only_file_status() {
        let p = run_status(&[], &[file("a.md", 1, Some(100))]);
        assert_eq!(kinds(&p), vec![ActionKind::Download]);
        assert_eq!(p.actions[0].reason, "remote_only");
        assert_eq!(p.stats.download, 1);
    }

    #[test]
    fn plan_equal_files_skip() {
        let p = run_status(
            &[file("a.md", 5, Some(1000))],
            &[file("a.md", 5, Some(1000))],
        );
        assert_eq!(kinds(&p), vec![ActionKind::Skip]);
        assert_eq!(p.actions[0].reason, "equal");
    }

    #[test]
    fn plan_mtime_tolerance_skip() {
        let p = run_status_tol(
            &[file("a.md", 5, Some(1000))],
            &[file("a.md", 5, Some(1500))],
            1000,
        );
        assert_eq!(kinds(&p), vec![ActionKind::Skip]);
    }

    #[test]
    fn plan_mtime_out_of_tolerance_uploads() {
        // diff 4000 > tol 1000 -> local newer
        let p = run_status_tol(
            &[file("a.md", 5, Some(5000))],
            &[file("a.md", 5, Some(1000))],
            1000,
        );
        assert_eq!(kinds(&p), vec![ActionKind::Upload]);
        assert_eq!(p.actions[0].reason, "local_newer");
    }

    #[test]
    fn plan_local_newer_upload() {
        let p = run_status(
            &[file("a.md", 5, Some(5000))],
            &[file("a.md", 5, Some(1000))],
        );
        assert_eq!(kinds(&p), vec![ActionKind::Upload]);
        assert_eq!(p.actions[0].reason, "local_newer");
    }

    #[test]
    fn plan_remote_newer_download() {
        let p = run_status(
            &[file("a.md", 5, Some(1000))],
            &[file("a.md", 5, Some(5000))],
        );
        assert_eq!(kinds(&p), vec![ActionKind::Download]);
        assert_eq!(p.actions[0].reason, "remote_newer");
    }

    #[test]
    fn plan_conflict_mtime_within_tol_diff_size() {
        // mtimes differ by 500 <= tol 1000, but sizes differ -> Conflict
        let p = run_status(
            &[file("a.md", 1, Some(1500))],
            &[file("a.md", 2, Some(1000))],
        );
        assert_eq!(kinds(&p), vec![ActionKind::Conflict]);
        assert_eq!(p.actions[0].reason, "conflict_mtime_size");
        assert_eq!(p.stats.conflict, 1);
    }

    #[test]
    fn plan_conflict_mtime_within_tol_diff_size_remote_higher() {
        // remote mtime slightly higher, within tol, sizes differ -> Conflict
        let p = run_status(
            &[file("a.md", 1, Some(1000))],
            &[file("a.md", 2, Some(1500))],
        );
        assert_eq!(kinds(&p), vec![ActionKind::Conflict]);
        assert_eq!(p.actions[0].reason, "conflict_mtime_size");
        assert_eq!(p.stats.conflict, 1);
    }

    #[test]
    fn plan_diff_size_local_newer_beyond_tol_uploads() {
        // mtimes differ 4000 > tol 1000 and sizes differ: number side still wins
        let p = run_status_tol(
            &[file("a.md", 1, Some(5000))],
            &[file("a.md", 9, Some(1000))],
            1000,
        );
        assert_eq!(kinds(&p), vec![ActionKind::Upload]);
        assert_eq!(p.actions[0].reason, "local_newer");
    }

    #[test]
    fn plan_diff_size_remote_newer_beyond_tol_downloads() {
        let p = run_status_tol(
            &[file("a.md", 1, Some(1000))],
            &[file("a.md", 9, Some(5000))],
            1000,
        );
        assert_eq!(kinds(&p), vec![ActionKind::Download]);
        assert_eq!(p.actions[0].reason, "remote_newer");
    }

    #[test]
    fn plan_conflict_same_mtime_diff_size() {
        let p = run_status(
            &[file("a.md", 1, Some(1000))],
            &[file("a.md", 2, Some(1000))],
        );
        assert_eq!(kinds(&p), vec![ActionKind::Conflict]);
        assert_eq!(p.actions[0].reason, "conflict_mtime_size");
    }

    #[test]
    fn plan_folders_both_sides_skip() {
        let p = run_status(&[folder("n")], &[folder("n")]);
        assert_eq!(kinds(&p), vec![ActionKind::Skip]);
        assert_eq!(p.actions[0].reason, "folder");
    }

    #[test]
    fn plan_push_mode_filters_download() {
        let p = plan(&[], &[file("a.md", 1, Some(100))], Mode::Push, &opts());
        assert!(!kinds(&p).contains(&ActionKind::Download));
        assert_eq!(kinds(&p), vec![ActionKind::Skip]);
    }

    #[test]
    fn plan_pull_mode_filters_upload() {
        let p = plan(&[file("a.md", 1, Some(100))], &[], Mode::Pull, &opts());
        assert!(!kinds(&p).contains(&ActionKind::Upload));
        assert_eq!(kinds(&p), vec![ActionKind::Skip]);
    }

    #[test]
    fn plan_push_delete_remote_only() {
        let o = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let p = plan(&[], &[file("gone.md", 1, Some(100))], Mode::Push, &o);
        assert_eq!(kinds(&p), vec![ActionKind::DeleteRemote]);
    }

    #[test]
    fn plan_pull_delete_local_only() {
        let o = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let p = plan(&[file("gone.md", 1, Some(100))], &[], Mode::Pull, &o);
        assert_eq!(kinds(&p), vec![ActionKind::DeleteLocal]);
    }

    #[test]
    fn plan_push_without_delete_keeps_remote_only_as_skip() {
        let p = plan(&[], &[file("r.md", 1, Some(100))], Mode::Push, &opts());
        assert_eq!(kinds(&p), vec![ActionKind::Skip]);
        assert_eq!(p.actions[0].reason, "remote_only");
    }

    #[test]
    fn plan_push_local_newer_uploads() {
        let p = plan(
            &[file("a.md", 5, Some(5000))],
            &[file("a.md", 5, Some(1000))],
            Mode::Push,
            &opts(),
        );
        assert_eq!(kinds(&p), vec![ActionKind::Upload]);
    }

    #[test]
    fn plan_pull_remote_newer_downloads() {
        let p = plan(
            &[file("a.md", 5, Some(1000))],
            &[file("a.md", 5, Some(5000))],
            Mode::Pull,
            &opts(),
        );
        assert_eq!(kinds(&p), vec![ActionKind::Download]);
    }

    #[test]
    fn plan_push_remote_newer_skips() {
        let p = plan(
            &[file("a.md", 5, Some(1000))],
            &[file("a.md", 5, Some(5000))],
            Mode::Push,
            &opts(),
        );
        assert_eq!(kinds(&p), vec![ActionKind::Skip]);
        assert_eq!(p.actions[0].reason, "remote_newer");
    }

    #[test]
    fn plan_pull_local_newer_skips() {
        let p = plan(
            &[file("a.md", 5, Some(5000))],
            &[file("a.md", 5, Some(1000))],
            Mode::Pull,
            &opts(),
        );
        assert_eq!(kinds(&p), vec![ActionKind::Skip]);
        assert_eq!(p.actions[0].reason, "local_newer");
    }

    #[test]
    fn plan_conflict_both_forces_cancel_in_pull() {
        let o = PlanOpts {
            force_local: true,
            force_remote: true,
            ..Default::default()
        };
        let p = plan(
            &[file("a.md", 1, Some(1000))],
            &[file("a.md", 2, Some(1000))],
            Mode::Pull,
            &o,
        );
        assert_eq!(kinds(&p), vec![ActionKind::Conflict]);
        assert_eq!(p.actions[0].reason, "conflict_mtime_size");
    }

    #[test]
    fn plan_conflict_both_forces_cancel() {
        let o = PlanOpts {
            force_local: true,
            force_remote: true,
            ..Default::default()
        };
        let p = plan(
            &[file("a.md", 1, Some(1000))],
            &[file("a.md", 2, Some(1000))],
            Mode::Status,
            &o,
        );
        assert_eq!(kinds(&p), vec![ActionKind::Conflict]);
        assert_eq!(p.actions[0].reason, "conflict_mtime_size");
    }

    #[test]
    fn plan_pull_force_local_conflict_skips() {
        let o = PlanOpts {
            force_local: true,
            ..Default::default()
        };
        let p = plan(
            &[file("a.md", 1, Some(1000))],
            &[file("a.md", 2, Some(1000))],
            Mode::Pull,
            &o,
        );
        assert_eq!(kinds(&p), vec![ActionKind::Skip]);
        assert_eq!(p.actions[0].reason, "force_local");
        assert!(!kinds(&p).contains(&ActionKind::Upload));
    }

    #[test]
    fn plan_push_force_remote_conflict_skips() {
        let o = PlanOpts {
            force_remote: true,
            ..Default::default()
        };
        let p = plan(
            &[file("a.md", 1, Some(1000))],
            &[file("a.md", 2, Some(1000))],
            Mode::Push,
            &o,
        );
        assert_eq!(kinds(&p), vec![ActionKind::Skip]);
        assert_eq!(p.actions[0].reason, "force_remote");
        assert!(!kinds(&p).contains(&ActionKind::Download));
    }

    #[test]
    fn plan_push_force_local_conflict_uploads() {
        let o = PlanOpts {
            force_local: true,
            ..Default::default()
        };
        let p = plan(
            &[file("a.md", 1, Some(1000))],
            &[file("a.md", 2, Some(1000))],
            Mode::Push,
            &o,
        );
        assert_eq!(kinds(&p), vec![ActionKind::Upload]);
        assert_eq!(p.actions[0].reason, "force_local");
    }

    #[test]
    fn plan_pull_force_remote_conflict_downloads() {
        let o = PlanOpts {
            force_remote: true,
            ..Default::default()
        };
        let p = plan(
            &[file("a.md", 1, Some(1000))],
            &[file("a.md", 2, Some(1000))],
            Mode::Pull,
            &o,
        );
        assert_eq!(kinds(&p), vec![ActionKind::Download]);
        assert_eq!(p.actions[0].reason, "force_remote");
    }

    #[test]
    fn plan_conflict_force_local_uploads() {
        let o = PlanOpts {
            force_local: true,
            ..Default::default()
        };
        let p = plan(
            &[file("a.md", 1, Some(1000))],
            &[file("a.md", 2, Some(1000))],
            Mode::Status,
            &o,
        );
        assert_eq!(kinds(&p), vec![ActionKind::Upload]);
    }

    #[test]
    fn plan_conflict_force_remote_downloads() {
        let o = PlanOpts {
            force_remote: true,
            ..Default::default()
        };
        let p = plan(
            &[file("a.md", 1, Some(1000))],
            &[file("a.md", 2, Some(1000))],
            Mode::Status,
            &o,
        );
        assert_eq!(kinds(&p), vec![ActionKind::Download]);
    }

    #[test]
    fn plan_force_local_does_not_flip_remote_newer() {
        // Forces apply to Conflict rows only: RemoteNewer stays a Download
        // even with force_local (locks the "no flip" invariant, R2.7e).
        let o = PlanOpts {
            force_local: true,
            ..Default::default()
        };
        let p = plan(
            &[file("a.md", 5, Some(1000))],
            &[file("a.md", 5, Some(5000))],
            Mode::Status,
            &o,
        );
        assert_eq!(kinds(&p), vec![ActionKind::Download]);
        assert_eq!(p.actions[0].reason, "remote_newer");
    }

    #[test]
    fn plan_force_remote_does_not_flip_local_newer() {
        let o = PlanOpts {
            force_remote: true,
            ..Default::default()
        };
        let p = plan(
            &[file("a.md", 5, Some(5000))],
            &[file("a.md", 5, Some(1000))],
            Mode::Status,
            &o,
        );
        assert_eq!(kinds(&p), vec![ActionKind::Upload]);
        assert_eq!(p.actions[0].reason, "local_newer");
    }

    #[test]
    fn plan_stats_counts() {
        let local = vec![
            file("a.md", 1, Some(100)),  // local only -> upload
            file("c.md", 5, Some(1000)), // equal -> skip
            file("d.md", 1, Some(1000)), // conflict (size differs)
        ];
        let remote = vec![
            file("b.md", 1, Some(100)), // remote only -> download
            file("c.md", 5, Some(1000)),
            file("d.md", 2, Some(1000)),
        ];
        let p = run_status(&local, &remote);
        assert_eq!(p.actions.len(), 4);
        assert_eq!(p.stats.upload, 1);
        assert_eq!(p.stats.download, 1);
        assert_eq!(p.stats.conflict, 1);
        assert_eq!(p.stats.skip, 1);
    }

    #[test]
    fn plan_actions_sorted_by_key() {
        let local = vec![file("z.md", 1, Some(100)), file("a.md", 1, Some(100))];
        let p = run_status(&local, &[]);
        let keys: Vec<_> = p.actions.iter().map(|a| a.key.clone()).collect();
        assert_eq!(keys, vec!["a.md".to_string(), "z.md".to_string()]);
    }

    #[test]
    fn plan_pull_delete_local_only_folder_still_skips() {
        // Locks Phase 1: folders short-circuit in `resolve` before delete
        // mapping, so `--delete` never removes folder rows. Phase 2 owns a
        // folder-delete policy decision (checklist P1r3-folder-delete).
        let o = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let p = plan(&[folder("n")], &[], Mode::Pull, &o);
        assert_eq!(kinds(&p), vec![ActionKind::Skip]);
        assert_eq!(p.actions[0].reason, "folder");
        assert!(!kinds(&p).contains(&ActionKind::DeleteLocal));
    }

    #[test]
    fn plan_push_delete_remote_only_folder_still_skips() {
        let o = PlanOpts {
            delete: true,
            ..Default::default()
        };
        let p = plan(&[], &[folder("n")], Mode::Push, &o);
        assert_eq!(kinds(&p), vec![ActionKind::Skip]);
        assert_eq!(p.actions[0].reason, "folder");
        assert!(!kinds(&p).contains(&ActionKind::DeleteRemote));
    }

    #[test]
    fn plan_local_only_folder_skips_transfer() {
        for mode in [Mode::Status, Mode::Push, Mode::Pull] {
            let p = plan(&[folder("n")], &[], mode, &opts());
            assert_eq!(kinds(&p), vec![ActionKind::Skip]);
            assert_eq!(p.actions[0].reason, "folder");
        }
    }

    #[test]
    fn plan_remote_only_folder_skips_transfer() {
        for mode in [Mode::Status, Mode::Push, Mode::Pull] {
            let p = plan(&[], &[folder("n")], mode, &opts());
            assert_eq!(kinds(&p), vec![ActionKind::Skip]);
            assert_eq!(p.actions[0].reason, "folder");
        }
    }
    // --- Phase 2 Slice 4a: file/folder path collision ---

    #[test]
    fn plan_file_vs_folder_key_conflicts() {
        // Local file `K` + remote folder `K/` (and its child) -> Conflict
        // path_collision, not Upload+Download.
        let p = plan(
            &[file("K", 5, Some(1000))],
            &[folder("K"), file("K/x", 1, Some(1))],
            Mode::Status,
            &opts(),
        );
        let acts: Vec<_> = p.actions.iter().map(|a| (a.key.as_str(), a.kind)).collect();
        assert!(acts.iter().any(|(k, kind)| *k == "K" && *kind == ActionKind::Conflict));
    }

    #[test]
    fn plan_folder_vs_file_key_conflicts() {
        // Mirror: local folder `K/` vs remote file `K` -> Conflict.
        let p = plan(
            &[folder("K"), file("K/child.md", 1, Some(1))],
            &[file("K", 5, Some(1000))],
            Mode::Status,
            &opts(),
        );
        let act = p.actions.iter().find(|a| a.key == "K").unwrap();
        assert_eq!(act.kind, ActionKind::Conflict);
        assert_eq!(act.reason, "path_collision");
    }

    #[test]
    fn path_collision_survives_all_modes() {
        // Status/Push/Pull all Conflict; forces do not resolve type collisions.
        for mode in [Mode::Status, Mode::Push, Mode::Pull] {
            for force in [
                PlanOpts { force_local: true, ..Default::default() },
                PlanOpts { force_remote: true, ..Default::default() },
                PlanOpts {
                    force_local: true,
                    force_remote: true,
                    ..Default::default()
                },
            ] {
                let p = plan(
                    &[file("K", 5, Some(1000))],
                    &[file("K/x", 1, Some(1))],
                    mode,
                    &force,
                );
                let act = p.actions.iter().find(|a| a.key == "K").unwrap();
                assert_eq!(
                    act.kind,
                    ActionKind::Conflict,
                    "mode {mode:?} force {force:?}"
                );
                assert_eq!(act.reason, "path_collision");
            }
        }
    }

    // --- Phase 2 Slice 4b: unknown-mtime policy ---

    #[test]
    fn plan_none_mtime_diff_size_conflicts() {
        // Local mtime set, remote None, sizes differ -> Conflict
        // conflict_mtime_unknown (fixes the silent-pull hole).
        for mode in [Mode::Status, Mode::Push, Mode::Pull] {
            let p = plan(
                &[file("a.md", 100, Some(5000))],
                &[file("a.md", 200, None)],
                mode,
                &opts(),
            );
            let act = p.actions.iter().find(|a| a.key == "a.md").unwrap();
            assert_eq!(act.kind, ActionKind::Conflict, "mode {mode:?}");
            assert_eq!(act.reason, "conflict_mtime_unknown");
        }
    }

    #[test]
    fn plan_none_mtime_same_size_skips_visible() {
        let p = plan(
            &[file("a.md", 100, Some(5000))],
            &[file("a.md", 100, None)],
            Mode::Status,
            &opts(),
        );
        assert_eq!(kinds(&p), vec![ActionKind::Skip]);
        assert_eq!(p.actions[0].reason, "equal_unknown_mtime");
    }

    #[test]
    fn plan_both_none_mtime_diff_size_conflicts() {
        let p = plan(
            &[file("a.md", 5, None)],
            &[file("a.md", 6, None)],
            Mode::Status,
            &opts(),
        );
        assert_eq!(kinds(&p), vec![ActionKind::Conflict]);
        assert_eq!(p.actions[0].reason, "conflict_mtime_unknown");
    }

    #[test]
    fn plan_local_none_remote_set_diff_size_conflicts() {
        // Symmetric direction to plan_none_mtime_diff_size_conflicts.
        let p = plan(
            &[file("a.md", 200, None)],
            &[file("a.md", 100, Some(5000))],
            Mode::Pull,
            &opts(),
        );
        assert_eq!(kinds(&p), vec![ActionKind::Conflict]);
        assert_eq!(p.actions[0].reason, "conflict_mtime_unknown");
    }

    #[test]
    fn plan_pre_epoch_real_zero_still_compares() {
        // `Some(0)` is a real (pre-epoch saturating) mtime, not "unknown":
        // Some(0) vs Some(5000) is remote_newer, as before.
        let p = plan(
            &[file("a.md", 5, Some(0))],
            &[file("a.md", 5, Some(5000))],
            Mode::Status,
            &opts(),
        );
        assert_eq!(kinds(&p), vec![ActionKind::Download]);
        assert_eq!(p.actions[0].reason, "remote_newer");
    }

    #[test]
    fn plan_unknown_mtime_conflict_force_resolvable() {
        // conflict_mtime_unknown is force-resolvable per the mode-aware table
        // (unlike path_collision).
        let o = PlanOpts {
            force_local: true,
            ..Default::default()
        };
        let p = plan(
            &[file("a.md", 100, Some(5000))],
            &[file("a.md", 200, None)],
            Mode::Push,
            &o,
        );
        assert_eq!(kinds(&p), vec![ActionKind::Upload]);
        assert_eq!(p.actions[0].reason, "force_local");
    }

    // --- Phase 2 Slice 4d: etag policy (decision only, no planner code) ---

    #[test]
    fn plan_etag_ignored() {
        // Phase 2 does not compare etags and never hashes local files (4d
        // lock). plan() ignores etag fields entirely: same size + mtime with
        // DIFFERENT etags still plans Equal (skip).
        let mut l = file("a.md", 5, Some(1000));
        l.etag = Some("etag-A".to_string());
        let mut r = file("a.md", 5, Some(1000));
        r.etag = Some("etag-B".to_string());
        let p = plan(&[l], &[r], Mode::Status, &opts());
        assert_eq!(kinds(&p), vec![ActionKind::Skip]);
        assert_eq!(p.actions[0].reason, "equal");
    }
    // --- Phase 2 Slice 4c: case-collision preflight (pure detection) ---

    #[test]
    fn case_collision_same_side_detected() {
        // Two keys differing only by case on one side -> both flagged. (Tested
        // at the pure layer because a real dir on a case-insensitive FS cannot
        // hold both `Note.md` and `note.md`.)
        let local = vec![file("Note.md", 1, Some(1)), file("note.md", 1, Some(1))];
        let c = case_collision_keys(&local, &[]);
        assert!(c.contains("Note.md"));
        assert!(c.contains("note.md"));
    }

    #[test]
    fn case_collision_cross_side_detected() {
        // local `Note.md` vs remote `note.md` (different content) -> both
        // flagged, never auto-paired as Equal.
        let local = vec![file("Note.md", 5, Some(1000))];
        let remote = vec![file("note.md", 9, Some(2000))];
        let c = case_collision_keys(&local, &remote);
        assert!(c.contains("Note.md"), "c: {c:?}");
        assert!(c.contains("note.md"), "c: {c:?}");
    }

    #[test]
    fn case_collision_none_for_distinct_keys() {
        // Keys that differ beyond case are not collisions.
        let local = vec![file("Note.md", 1, Some(1)), file("other.md", 1, Some(1))];
        let remote = vec![file("note2.md", 1, Some(1))];
        let c = case_collision_keys(&local, &remote);
        assert!(c.is_empty(), "c: {c:?}");
    }
}

