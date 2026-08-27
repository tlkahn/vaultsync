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
}

/// Reason strings stable enough for tests and human output.
mod reason {
    pub const EQUAL: &str = "equal";
    pub const LOCAL_ONLY: &str = "local_only";
    pub const REMOTE_ONLY: &str = "remote_only";
    pub const LOCAL_NEWER: &str = "local_newer";
    pub const REMOTE_NEWER: &str = "remote_newer";
    pub const CONFLICT_MTIME_SIZE: &str = "conflict_mtime_size";
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
            // Folders never reach here: `resolve` short-circuits folder keys
            // before classify. A same-key file/folder pair is impossible
            // (folder keys end with `/`).
            debug_assert!(
                !l.is_folder() && !r.is_folder(),
                "folders must be filtered in resolve"
            );
            let lm = l.mtime_ms.unwrap_or(0);
            let rm = r.mtime_ms.unwrap_or(0);
            if lm.abs_diff(rm) <= tol {
                // Within tolerance of one another: equal only when size matches,
                // otherwise the two sides disagree and it is a conflict.
                if l.size == r.size {
                    Delta::Equal
                } else {
                    Delta::Conflict
                }
            } else if lm > rm {
                Delta::LocalNewer
            } else {
                // rm > lm (abs_diff beyond tol means they cannot be equal)
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

    let mut actions: Vec<Action> = Vec::new();
    for key in keys {
        let loc = local_map.get(key).copied();
        let rem = remote_map.get(key).copied();
        let (kind, reason) = resolve(loc, rem, mode, opts);
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

/// Map a key's delta to an action kind + reason given the mode and options.
fn resolve(
    local: Option<&Entity>,
    remote: Option<&Entity>,
    mode: Mode,
    opts: &PlanOpts,
) -> (ActionKind, &'static str) {
    // Folders never carry transfer meaning in Phase 1.
    if local.is_some_and(|e| e.is_folder()) || remote.is_some_and(|e| e.is_folder()) {
        return (ActionKind::Skip, reason::FOLDER);
    }

    let delta = classify_pair(local, remote, opts.mtime_tolerance_ms);
    use ActionKind::*;
    match delta {
        Delta::Equal => (Skip, reason::EQUAL),
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
        Delta::Conflict => {
            let forcing = opts.force_local as u8 + opts.force_remote as u8;
            if forcing > 1 {
                // Both forces set: they cancel. Treat as no force rather than
                // silently letting local (or remote) win by arbitrary precedence.
                (Conflict, reason::CONFLICT_MTIME_SIZE)
            } else if opts.force_local {
                match mode {
                    // Push and Status may plan Upload; Pull must keep local
                    // (mode invariant: Pull never plans Upload).
                    Mode::Push | Mode::Status => (Upload, reason::FORCE_LOCAL),
                    Mode::Pull => (Skip, reason::FORCE_LOCAL),
                }
            } else if opts.force_remote {
                match mode {
                    // Pull and Status may plan Download; Push must keep remote
                    // (mode invariant: Push never plans Download).
                    Mode::Pull | Mode::Status => (Download, reason::FORCE_REMOTE),
                    Mode::Push => (Skip, reason::FORCE_REMOTE),
                }
            } else {
                (Conflict, reason::CONFLICT_MTIME_SIZE)
            }
        }
    }
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
    fn plan_pull_remote_none_mtime_diff_size_skips_as_local_newer() {
        // Characterization lock (P1r5-mtime-pull): under the Phase 1
        // `None -> 0` rule, a remote missing mtime against a present local
        // classifies `local_newer`; Pull plans Skip (local kept). Phase 2
        // must revisit this pull-direction staleness deliberately.
        let p = plan(
            &[file("a.md", 100, Some(5000))],
            &[file("a.md", 200, None)],
            Mode::Pull,
            &opts(),
        );
        assert_eq!(kinds(&p), vec![ActionKind::Skip]);
        assert_eq!(p.actions[0].reason, "local_newer");
    }

    #[test]
    fn plan_pull_remote_none_mtime_same_size_skips_as_local_newer() {
        // Same-size variant: Equal is *not* chosen because `5000` vs the
        // None-derived `0` exceeds the tolerance; the pair still classifies
        // `local_newer` and Pull keeps local.
        let p = plan(
            &[file("a.md", 100, Some(5000))],
            &[file("a.md", 100, None)],
            Mode::Pull,
            &opts(),
        );
        assert_eq!(kinds(&p), vec![ActionKind::Skip]);
        assert_eq!(p.actions[0].reason, "local_newer");
    }

    #[test]
    fn plan_status_remote_none_mtime_diff_size_uploads_as_local_newer() {
        // Counters the "no signal" framing: Status *does* surface a row for
        // a None-mtime remote (Upload / local_newer); the real hole is the
        // misleading Pull Skip, not a total absence of output.
        let p = plan(
            &[file("a.md", 100, Some(5000))],
            &[file("a.md", 200, None)],
            Mode::Status,
            &opts(),
        );
        assert_eq!(kinds(&p), vec![ActionKind::Upload]);
        assert_eq!(p.actions[0].reason, "local_newer");
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
}
