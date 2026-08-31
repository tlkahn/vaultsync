//! CLI argument parsing and command dispatch (clap-based, Phase 2).
//!
//! Migration note (P2-cli, Slice 1): the Phase 1 hand-rolled parser is
//! replaced by `clap`. Locks carried over:
//! - `parse_args` -> [`Command`] and `run_with_io` seams are kept so dispatch
//!   tests stay process-free.
//! - Global flags (`--config`, `--vault`, `--json`, `-v/--verbose`) are
//!   accepted **before or after** the subcommand (clap `global = true`).
//! - `--json` parses in Phase 2 but dispatch rejects it as "not implemented"
//!   (schema stability is Phase 3).
//! - `--yes` / `--max-delete` are rejected as unknown until Phase 3
//!   (delete-safety rails are Phase 3). `--concurrency` stays rejected as
//!   unknown: the knob is config-only (`[transfer].concurrency`, live since
//!   issue 20 - it bounds transfer passes and list-enrichment heads).
//! - Every parse error includes usage (clap does this natively).

use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use clap::builder::ArgAction;
use clap::{Args, CommandFactory, Parser, Subcommand};

use crate::plan::{Mode, PlanOpts};
use crate::progress::{Progress, ProgressMode, ResolvedMode};
use crate::store::ObjectStore;
use crate::store::mock::MemoryStore;

/// Parsed top-level command (kept as the authoritative dispatch value, so
/// tests can build it directly without the clap layer).
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Status {
        vault: PathBuf,
        json: bool,
        config: Option<PathBuf>,
        verbose: u8,
        follow_symlinks: bool,
    },
    Push {
        vault: PathBuf,
        delete: bool,
        dry_run: bool,
        force_local: bool,
        force_remote: bool,
        json: bool,
        config: Option<PathBuf>,
        verbose: u8,
        follow_symlinks: bool,
    },
    Pull {
        vault: PathBuf,
        delete: bool,
        dry_run: bool,
        force_local: bool,
        force_remote: bool,
        json: bool,
        config: Option<PathBuf>,
        verbose: u8,
        follow_symlinks: bool,
    },
    Check {
        config: Option<PathBuf>,
        verbose: u8,
        json: bool,
    },
    Repair {
        vault: PathBuf,
        json: bool,
        config: Option<PathBuf>,
        verbose: u8,
        dry_run: bool,
        force: bool,
    },
    Version,
    Help,
}

impl Command {
    /// Status with a vault; remaining flags at defaults (test ergonomics).
    pub fn status(vault: PathBuf) -> Command {
        Command::Status {
            vault,
            json: false,
            config: None,
            verbose: 0,
            follow_symlinks: false,
        }
    }
    /// Push with a vault + delete; remaining flags at defaults.
    pub fn push(vault: PathBuf, delete: bool) -> Command {
        Command::Push {
            vault,
            delete,
            dry_run: false,
            force_local: false,
            force_remote: false,
            json: false,
            config: None,
            verbose: 0,
            follow_symlinks: false,
        }
    }
    /// Pull with a vault + delete; remaining flags at defaults.
    pub fn pull(vault: PathBuf, delete: bool) -> Command {
        Command::Pull {
            vault,
            delete,
            dry_run: false,
            force_local: false,
            force_remote: false,
            json: false,
            config: None,
            verbose: 0,
            follow_symlinks: false,
        }
    }
    /// Check with defaults.
    pub fn check() -> Command {
        Command::Check {
            config: None,
            verbose: 0,
            json: false,
        }
    }
    /// Repair with defaults (dry-run off, force off). The vault defaults to
    /// the same sentinel as clap (`VAULT_UNSET`), matching how a no-flag
    /// invocation resolves through `resolve_vault_from_config`.
    pub fn repair() -> Command {
        Command::Repair {
            vault: PathBuf::from(VAULT_UNSET),
            json: false,
            config: None,
            verbose: 0,
            dry_run: false,
            force: false,
        }
    }
}

/// clap top level. Global args are `global = true` so they parse before or
/// after the subcommand.
#[derive(Parser)]
#[command(
    name = "vaultsync",
    version,
    about = "Minimal, Unix-style sync of a plain directory (Obsidian vault) to object storage",
    subcommand_negates_reqs = true,
    disable_help_subcommand = true,
    after_help = "WARNING: --delete removes files permanently with no confirmation prompt until Phase 3"
)]
struct Cli {
    /// Config file (default: ./.vaultsync.toml then ~/.config/vaultsync/config.toml)
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Vault root override
    #[arg(long, global = true, value_name = "PATH", default_value = ".")]
    vault: PathBuf,

    /// Machine-readable stdout (not implemented until Phase 3)
    #[arg(long, global = true)]
    json: bool,

    /// Verbose noise on stderr (repeatable)
    #[arg(short = 'v', long = "verbose", global = true, action = ArgAction::Count)]
    verbose: u8,

    /// Follow symlinks below the vault root (off by default; out-of-vault
    /// targets are still skipped with a warning).
    #[arg(long, global = true)]
    follow_symlinks: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

/// Push/pull shared transfer flags.
#[derive(Args, Debug, Clone, PartialEq)]
struct TransferArgs {
    /// With push: remove remote extras; with pull: remove local extras.
    /// (No confirmation prompt yet - destructive and permanent until Phase 3.)
    #[arg(long)]
    delete: bool,

    /// Plan only; no mutations.
    #[arg(long)]
    dry_run: bool,

    /// On conflict, local wins (push/status: upload; pull: keep local).
    #[arg(long)]
    force_local: bool,

    /// On conflict, remote wins (pull/status: download; push: keep remote).
    #[arg(long)]
    force_remote: bool,
}

#[derive(Args, Debug, Clone, PartialEq)]
struct RepairArgs {
    /// Rebuild the manifest body and report the entry count, but write
    /// nothing.
    #[arg(long)]
    dry_run: bool,
    /// Overwrite the manifest unconditionally (no If-Match precondition).
    #[arg(long)]
    force: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Show diff between local vault and remote prefix
    Status,
    /// Upload local-newer and remote-missing paths
    Push(TransferArgs),
    /// Download remote-newer and local-missing paths
    Pull(TransferArgs),
    /// Connectivity probe against the configured store
    Check,
    /// Rebuild the remote inventory manifest from a live list+head
    Repair(RepairArgs),
    /// Print version
    Version,
    /// Show help
    Help,
}

impl Cli {
    fn into_command(self) -> Command {
        let config = self.config;
        let verbose = self.verbose;
        match self.command {
            None => Command::Help,
            Some(Commands::Status) => Command::Status {
                vault: self.vault,
                json: self.json,
                config,
                verbose,
                follow_symlinks: self.follow_symlinks,
            },
            Some(Commands::Push(a)) => Command::Push {
                vault: self.vault,
                delete: a.delete,
                dry_run: a.dry_run,
                force_local: a.force_local,
                force_remote: a.force_remote,
                json: self.json,
                config,
                verbose,
                follow_symlinks: self.follow_symlinks,
            },
            Some(Commands::Pull(a)) => Command::Pull {
                vault: self.vault,
                delete: a.delete,
                dry_run: a.dry_run,
                force_local: a.force_local,
                force_remote: a.force_remote,
                json: self.json,
                config,
                verbose,
                follow_symlinks: self.follow_symlinks,
            },
            Some(Commands::Check) => Command::Check {
                config,
                verbose,
                json: self.json,
            },
            Some(Commands::Repair(a)) => Command::Repair {
                vault: self.vault,
                json: self.json,
                config,
                verbose,
                dry_run: a.dry_run,
                force: a.force,
            },
            Some(Commands::Version) => Command::Version,
            Some(Commands::Help) => Command::Help,
        }
    }
}

/// Convert `args_os()` output into `String`s, failing loud on non-UTF8
/// arguments (M1). This seam is the only place OsString meets the parser.
fn os_args_to_strings(args: Vec<OsString>) -> Result<Vec<String>, String> {
    args.into_iter()
        .map(|a| {
            a.into_string()
                .map_err(|os| format!("argument is not valid UTF-8: {os:?}"))
        })
        .collect()
}

/// Parse argv (including the program name at `args[0]`).
pub fn parse_args(args: &[String]) -> Result<Command, String> {
    match Cli::try_parse_from(args) {
        Ok(cli) => Ok(cli.into_command()),
        Err(e) => match e.kind() {
            clap::error::ErrorKind::DisplayHelp => Ok(Command::Help),
            clap::error::ErrorKind::DisplayVersion => Ok(Command::Version),
            _ => Err(ensure_usage(e)),
        },
    }
}

/// Enforce the P1r7-parse-usage invariant: every parse error carries a usage
/// line. clap natively includes usage for most error kinds but omits it for a
/// few (e.g. a missing `--vault` value), so append the command usage when
/// absent.
fn ensure_usage(e: clap::Error) -> String {
    let text = e.to_string();
    let has_usage = text
        .lines()
        .any(|l| l.trim_start().to_lowercase().starts_with("usage:"));
    if has_usage {
        text
    } else {
        format!("{text}\n\n{}", Cli::command().render_usage())
    }
}

fn is_clean(p: &crate::plan::Plan) -> bool {
    p.stats.upload == 0
        && p.stats.download == 0
        && p.stats.delete_local == 0
        && p.stats.delete_remote == 0
        && p.stats.conflict == 0
}

/// Reject `--json` at dispatch time (schema stability is Phase 3).
fn reject_json(err: &mut dyn Write) -> i32 {
    let _ = writeln!(err, "error: --json is not implemented (Phase 3)");
    1
}

/// Resolved dispatch knobs bundled so `run_with_io` stays under clippy's
/// 7-argument limit (W18/B-L9, W203) while keeping the public test seam
/// callable without an arg explosion. `tolerance_ms` is the resolved
/// `transfer.mtime_tolerance_ms` threaded into every `PlanOpts` (W2, PR2
/// A-H2/B-M1); `concurrency` bounds the transfer passes (I20);
/// `progress_mode` selects the push/pull progress renderer (I27-test);
/// `ignore` is the compiled `[ignore]` set threaded into both the local walk
/// (`LocalFs::with_ignore`, issue #32) and `build_plan` (issue #33) so the
/// two halves always see the same matcher (D-both-sides, issue #34).
#[derive(Debug, Clone)]
pub struct DispatchCtx {
    pub tolerance_ms: u64,
    pub concurrency: u32,
    pub progress_mode: ProgressMode,
    pub ignore: crate::IgnoreSet,
    /// Resolved `[inventory].mode` (issue 45): threaded into every
    /// `build_plan` call (warm manifest vs live list+head).
    pub inventory_mode: crate::config::InventoryMode,
}

/// Dispatch a command against a store, writing to `out`/`err`. Returns exit code.
///
/// I27-test: `ctx.progress_mode` selects the push/pull progress renderer -
/// tests pass `Off` so their captured-stderr contracts stay untouched; the
/// real binary path passes `Auto` (resolved against `stderr().is_terminal()`
/// in dispatch); `Always` forces the bar for CLI progress tests.
///
/// `err` is `&mut (dyn Write + Send)` (not bare `dyn Write`) so the I27
/// renderer built over it can satisfy `Progress: Send + Sync`. In-tree `Send`
/// writers include `Vec<u8>`, `String`, and `Stderr` (the process handle);
/// `StderrLock` is `!Send`, so pass `std::io::stderr()` here (as
/// [`run_from_env`] does) rather than a `stderr().lock()`.
///
/// I27 (F7): `ProgressMode::Auto` probes the *process* stderr
/// (`std::io::stderr().is_terminal()`), so `Auto` is only meaningful when
/// `err` is the process stderr; captured-writer callers should pass
/// `Off`/`Always` explicitly.
pub fn run_with_io(
    cmd: Command,
    store: &dyn ObjectStore,
    ctx: &DispatchCtx,
    out: &mut dyn Write,
    err: &mut (dyn Write + Send),
) -> i32 {
    match cmd {
        Command::Version => {
            let _ = writeln!(out, "vaultsync {}", crate::version());
            0
        }
        Command::Help => {
            let _ = writeln!(out, "{}", Cli::command().render_help());
            0
        }
        Command::Check {
            config: _c,
            verbose: _v,
            json,
        } => {
            // W53/B-L1: `check --json` is rejected at dispatch like
            // status/push/pull - it must not silently drop the flag and run
            // human check (schema stability is Phase 3).
            if json {
                return reject_json(err);
            }
            match crate::check_store(store) {
                Ok(()) => {
                    let _ = writeln!(out, "check: ok");
                    0
                }
                Err(crate::error::Error::Unauthorized(_)) => {
                    let _ = writeln!(err, "check failed: credentials or permissions rejected");
                    let _ = writeln!(
                        err,
                        "hint: check your credentials/bucket/region (expired keys, wrong region, bad bucket)"
                    );
                    1
                }
                Err(e) => {
                    let _ = writeln!(err, "check failed: {e}");
                    1
                }
            }
        }
        Command::Repair {
            vault,
            json,
            config: _c,
            verbose: _v,
            dry_run,
            force,
        } => {
            // W242 (issue 45, D-repair): rebuild the manifest from a live
            // list+head. No plan table; short summary lines on stdout. Exit
            // 0 on success (including dry-run), 1 on store errors. `--json`
            // is not a Repair flag (schema stability is Phase 3; W252 makes
            // it loud like status/push/pull/check).
            if json {
                return reject_json(err);
            }
            let opts = crate::inventory::RepairOpts { force, dry_run };
            // M1/F1 (W251): repair refreshes the local mirror like push - the
            // rebuilt manifest's meta records the fresh remote etag.
            let cache = crate::inventory::CachePaths::new(&vault);
            match crate::inventory::repair_manifest(store, &opts, Some(&cache)) {
                Ok(report) => {
                    for w in &report.warnings {
                        let _ = writeln!(err, "warning: {w}");
                    }
                    let _ = writeln!(
                        out,
                        "repair: listed {} objects via list+head",
                        report.listed
                    );
                    if report.dry_run {
                        let _ = writeln!(
                            out,
                            "repair: dry-run ({} entries would be written)",
                            report.listed
                        );
                    } else {
                        let etag_suffix = match &report.etag {
                            Some(e) => format!(" etag={e}"),
                            None => String::new(),
                        };
                        let _ = writeln!(
                            out,
                            "repair: wrote {} ({} entries{})",
                            crate::local::MANIFEST_KEY,
                            report.listed,
                            etag_suffix
                        );
                    }
                    0
                }
                Err(e) => {
                    let _ = writeln!(err, "error: {e}");
                    1
                }
            }
        }
        Command::Status {
            vault,
            json,
            config: _c,
            verbose,
            follow_symlinks,
        } => {
            if json {
                return reject_json(err);
            }
            let opts = PlanOpts {
                mtime_tolerance_ms: ctx.tolerance_ms,
                ..Default::default()
            };
            let local = crate::local::LocalFs::with_follow(&vault, follow_symlinks)
                .with_ignore(ctx.ignore.clone());
            match crate::build_plan(
                &local,
                store,
                Mode::Status,
                &opts,
                &ctx.ignore,
                &crate::inventory::InventoryOpts {
                    mode: ctx.inventory_mode,
                    vault_root: Some(vault.clone()),
                },
            ) {
                Ok(report) => {
                    // H1 (W99): build_plan + store-listing warnings surface
                    // here, at the CLI layer - library code never writes to
                    // stderr.
                    for w in &report.warnings {
                        let _ = writeln!(err, "warning: {w}");
                    }
                    // W236 (issue 45): one always-on inventory source line.
                    print_inventory_line(&report, err);
                    print_walk_warnings(&local, follow_symlinks, err);
                    let plan = &report.plan;
                    let _ = write!(out, "{}", crate::format_plan_human_verbose(plan, verbose));
                    if is_clean(plan) { 0 } else { 2 }
                }
                Err(e) => {
                    let _ = writeln!(err, "error: {e}");
                    1
                }
            }
        }
        Command::Push {
            vault,
            delete,
            dry_run,
            force_local,
            force_remote,
            json,
            config: _c,
            verbose,
            follow_symlinks,
        } => {
            if json {
                return reject_json(err);
            }
            let opts = PlanOpts {
                mtime_tolerance_ms: ctx.tolerance_ms,
                delete,
                force_local,
                force_remote,
            };
            let flags = PlanFlags {
                dry_run,
                follow_symlinks,
                verbose,
                concurrency: ctx.concurrency,
                progress: ctx.progress_mode,
                ignore: ctx.ignore.clone(),
                inventory_mode: ctx.inventory_mode,
            };
            dispatch_plan(&vault, store, Mode::Push, &opts, &flags, out, err)
        }
        Command::Pull {
            vault,
            delete,
            dry_run,
            force_local,
            force_remote,
            json,
            config: _c,
            verbose,
            follow_symlinks,
        } => {
            if json {
                return reject_json(err);
            }
            let opts = PlanOpts {
                mtime_tolerance_ms: ctx.tolerance_ms,
                delete,
                force_local,
                force_remote,
            };
            let flags = PlanFlags {
                dry_run,
                follow_symlinks,
                verbose,
                concurrency: ctx.concurrency,
                progress: ctx.progress_mode,
                ignore: ctx.ignore.clone(),
                inventory_mode: ctx.inventory_mode,
            };
            dispatch_plan(&vault, store, Mode::Pull, &opts, &flags, out, err)
        }
    }
}

/// Print the W236 inventory source milestone line to stderr (always-on, like
/// the ignore warnings, so it is testable and script-visible): locked strings
/// `inventory: manifest (N entries)` (warm) vs `inventory: list+head (cold)`.
/// One line per plan build; #42 may later wrap the cold path with progress.
fn print_inventory_line(report: &crate::PlanReport, err: &mut dyn Write) {
    match &report.inventory_base.source {
        crate::inventory::InventorySource::Manifest { .. } => {
            let _ = writeln!(
                err,
                "inventory: manifest ({} entries)",
                report.inventory_base.file_entities.len()
            );
        }
        crate::inventory::InventorySource::LiveListHead => {
            let _ = writeln!(err, "inventory: list+head (cold)");
        }
    }
}

/// Print walk-report warnings to stderr: out-of-vault followed-symlink
/// skips, the default-mode skipped-symlink count hint, the reserved
/// temp/probe skip count (always-on crash-leftover signal), and the local
/// ignore count when `WalkReport.skipped_ignored > 0` (issue #34 D-report
/// local half; locked string `warning: ignored N local path(s) by ignore
/// patterns`, count-only, always-on).
fn print_walk_warnings(local: &crate::local::LocalFs, follow: bool, err: &mut dyn Write) {
    let rep = local.report();
    for w in &rep.warnings {
        let _ = writeln!(err, "warning: {w}");
    }
    if rep.skipped_symlinks > 0 && !follow {
        let _ = writeln!(
            err,
            "warning: skipped {} symlink(s); use --follow-symlinks to list them (status only; transfers skip followed symlinks in v1)",
            rep.skipped_symlinks
        );
    }
    if rep.skipped_temp_files > 0 {
        // R4-L3/W41: a reserved temp/probe leftover is a crash signal, so it
        // is always surfaced (not just under -v).
        let _ = writeln!(
            err,
            "warning: skipped {} vaultsync temp/probe file(s) (crash leftovers); remove them manually",
            rep.skipped_temp_files
        );
    }
    if rep.skipped_ignored > 0 {
        // Issue #34 D-report local half (locked string): always-on (not
        // -v-only), count-only (no key dump - a pathological pattern set
        // must not flood stderr). The remote half is a PlanReport warning
        // printed by the dispatch loops above.
        let _ = writeln!(
            err,
            "warning: ignored {} local path(s) by ignore patterns",
            rep.skipped_ignored
        );
    }
}

/// Push/pull value flags bundled so `dispatch_plan` stays under clippy's
/// 7-argument limit (W18/B-L9) while keeping the dry-run/verbosity/symlink
/// plumbing in one place. `concurrency` rides along (I20: resolved
/// `[transfer].concurrency`, bounds the transfer passes; 1 = sequential).
/// `progress` is the I27 renderer selection (I27-test). `ignore` is the
/// compiled `[ignore]` set (W203, issue #34) threaded into both
/// [`LocalFs::with_ignore`] and `build_plan` so the local walk and the
/// remote filter always see the same matcher (D-both-sides).
struct PlanFlags {
    dry_run: bool,
    follow_symlinks: bool,
    verbose: u8,
    concurrency: u32,
    progress: ProgressMode,
    ignore: crate::IgnoreSet,
    /// Resolved `[inventory].mode` (issue 45, W235) threaded into
    /// `dispatch_plan`'s `build_plan` call.
    inventory_mode: crate::config::InventoryMode,
}

/// Build the I27 progress renderer for a resolved mode (I27 cycle 8 refactor:
/// one small helper so a future `--progress=` flag lands in one place). The
/// renderer borrows `err` for the duration of the run; `Box<dyn Progress>`
/// keeps the two concrete renderers type-erased behind the executor's sink.
fn build_progress_renderer<'w>(
    resolved: ResolvedMode,
    err: &'w mut (dyn Write + Send),
) -> Box<dyn Progress + 'w> {
    match resolved {
        ResolvedMode::Render => Box::new(crate::progress::TermProgress::new(err)),
        ResolvedMode::Quiet => Box::new(crate::progress::QuietProgress::new(err)),
    }
}

/// Build a plan and dispatch push/pull execution.
///
/// Exit codes (P1r-stub-exit, retired in Slice 6): `0` all selected actions
/// succeeded and no conflict rows; `2` the plan contained any Conflict rows
/// (non-conflict actions still execute); `1` any transfer/fatal error.
///
/// With `--dry-run`: print the plan, mutate nothing, exit like status
/// (2 if dirty/conflicts, else 0).
///
/// I27: the resolved progress mode selects a `TermProgress`/`QuietProgress`
/// renderer over `err`; `--json` runs never reach here (rejected earlier in
/// `run_with_io`, I27-json).
fn dispatch_plan(
    vault: &PathBuf,
    store: &dyn ObjectStore,
    mode: Mode,
    opts: &PlanOpts,
    flags: &PlanFlags,
    out: &mut dyn Write,
    err: &mut (dyn Write + Send),
) -> i32 {
    let local = crate::local::LocalFs::with_follow(vault, flags.follow_symlinks)
        .with_ignore(flags.ignore.clone());
    // I27-tty: resolve the progress mode against the real stderr terminal
    // status once per dispatch (Auto follows it; tests inject Off/Always via
    // the seam).
    let resolved =
        crate::progress::resolve_progress_mode(flags.progress, std::io::stderr().is_terminal());
    match crate::build_plan(
        &local,
        store,
        mode,
        opts,
        &flags.ignore,
        &crate::inventory::InventoryOpts {
            mode: flags.inventory_mode,
            vault_root: Some(vault.clone()),
        },
    ) {
        Ok(report) => {
            // H1 (W99): build_plan + store-listing warnings surface here, at
            // the CLI layer - library code never writes to stderr.
            for w in &report.warnings {
                let _ = writeln!(err, "warning: {w}");
            }
            // W236 (issue 45): one always-on inventory source line per plan
            // build (push/pull too, not just status).
            print_inventory_line(&report, err);
            print_walk_warnings(&local, flags.follow_symlinks, err);
            let plan = &report.plan;
            let _ = write!(
                out,
                "{}",
                crate::format_plan_human_verbose(plan, flags.verbose)
            );
            if flags.dry_run {
                if is_clean(plan) { 0 } else { 2 }
            } else {
                // I20: `[transfer].concurrency` bounds the transfer passes
                // (1 = sequential). I27: the executor's progress events feed
                // the resolved renderer (bar on TTY/Auto or Always; no-op
                // otherwise); stdout never sees progress bytes (I27-shape).
                let inventory_base = report.inventory_base;
                let exec_report = {
                    let renderer = build_progress_renderer(resolved, err);
                    crate::exec::execute_plan_with_progress(
                        &local,
                        store,
                        plan,
                        mode,
                        opts,
                        flags.concurrency,
                        renderer.as_ref(),
                    )
                };
                for w in &exec_report.warnings {
                    let _ = writeln!(err, "warning: {w}");
                }
                for f in &exec_report.failed {
                    let _ = writeln!(err, "error: {}\n  {}", f.key, f.message);
                }
                // W240 (issue 45, D-commit-when): after a push with at least
                // one successful remote mutation, commit the manifest LAST
                // (bodies first, manifest last - D-commit-order). Pull and
                // status never commit; a dry run mutated nothing so it never
                // commits; zero mutations skip the write (SkippedNoMutations).
                if mode == Mode::Push && !exec_report.mutations.is_empty() {
                    // M1/F1 (W251, reviews 5472028291 + 5472033449): the
                    // production push commit refreshes the S7 local mirror
                    // (W246 contract live; `commit_manifest` fills the cache
                    // on Written).
                    let cache = crate::inventory::CachePaths::new(vault);
                    match crate::inventory::commit_manifest(
                        store,
                        &inventory_base,
                        &exec_report.mutations,
                        Some(&cache),
                    ) {
                        Ok(crate::inventory::CommitOutcome::Written { .. })
                        | Ok(crate::inventory::CommitOutcome::SkippedNoMutations) => {}
                        Ok(crate::inventory::CommitOutcome::PreconditionFailed) => {
                            // Q2: a lost conditional race is a warning, not an
                            // error - the transfers succeeded (bodies are
                            // live); another writer's manifest won. Suggest
                            // repair. Exit code stays 0 when transfers are ok.
                            let _ = writeln!(
                                err,
                                "warning: manifest not committed (lost race or changed under us); run vaultsync repair if status looks wrong"
                            );
                        }
                        Err(e) => {
                            // Transfers already done; bodies may be ahead of
                            // the manifest. Surface loudly, keep the data
                            // exit code (repair story).
                            let _ = writeln!(err, "warning: manifest commit failed: {e}");
                        }
                    }
                }
                if !exec_report.failed.is_empty() {
                    1
                } else if plan
                    .actions
                    .iter()
                    .any(|a| a.kind == crate::plan::ActionKind::Conflict)
                {
                    2
                } else {
                    0
                }
            }
        }
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            1
        }
    }
}

/// The `--vault` value clap reports when the user did not pass it (its
/// default). Used as the "unset" sentinel so a config `vault_root` can win
/// when no explicit `--vault` was given. Known limitation: an explicit
/// `--vault .` is indistinguishable from the default (documented; Phase 3
/// config polish may refine).
const VAULT_UNSET: &str = ".";

/// The explicit `--config` path carried by a command, if any.
fn cmd_config_path(cmd: &Command) -> Option<&Path> {
    match cmd {
        Command::Status { config, .. }
        | Command::Push { config, .. }
        | Command::Pull { config, .. }
        | Command::Check { config, .. }
        | Command::Repair { config, .. } => config.as_deref(),
        Command::Version | Command::Help => None,
    }
}

/// Replace `Command.vault` with the resolved config vault root when the clap
/// `--vault` was left at its unset sentinel (see [`VAULT_UNSET`]).
fn resolve_vault_from_config(cmd: Command, settings: &crate::config::Settings) -> Command {
    let want = settings.vault_root.clone();
    if want == Path::new(VAULT_UNSET) {
        return cmd;
    }
    match cmd {
        Command::Status {
            vault,
            json,
            config,
            verbose,
            follow_symlinks,
        } if vault == Path::new(VAULT_UNSET) => Command::Status {
            vault: want,
            json,
            config,
            verbose,
            follow_symlinks,
        },
        Command::Push {
            vault,
            delete,
            dry_run,
            force_local,
            force_remote,
            json,
            config,
            verbose,
            follow_symlinks,
        } if vault == Path::new(VAULT_UNSET) => Command::Push {
            vault: want,
            delete,
            dry_run,
            force_local,
            force_remote,
            json,
            config,
            verbose,
            follow_symlinks,
        },
        Command::Pull {
            vault,
            delete,
            dry_run,
            force_local,
            force_remote,
            json,
            config,
            verbose,
            follow_symlinks,
        } if vault == Path::new(VAULT_UNSET) => Command::Pull {
            vault: want,
            delete,
            dry_run,
            force_local,
            force_remote,
            json,
            config,
            verbose,
            follow_symlinks,
        },
        Command::Repair {
            vault,
            json,
            config,
            verbose,
            dry_run,
            force,
        } if vault == Path::new(VAULT_UNSET) => Command::Repair {
            vault: want,
            json,
            config,
            verbose,
            dry_run,
            force,
        },
        other => other,
    }
}

/// True when a command mutates or probes a real store and therefore must not
/// silently run (or delete) against the in-memory mock (A-M2/B-L5, W5).
fn requires_real_store(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::Push { .. }
            | Command::Pull { .. }
            | Command::Check { .. }
            | Command::Repair { .. }
    )
}

/// Stable command name used in the no-store refusal message.
fn command_name(cmd: &Command) -> &'static str {
    match cmd {
        Command::Push { .. } => "push",
        Command::Pull { .. } => "pull",
        Command::Check { .. } => "check",
        Command::Repair { .. } => "repair",
        _ => "this command",
    }
}

/// Dispatch a fully-resolved command + settings. W5 (A-M2/B-L5): push/pull/
/// check refuse loudly when no `[store]` is configured (the in-memory mock is
/// `status`-only); `status` keeps the mock for offline play.
///
/// W92 (r2-M3) seam: store construction (the only non-injectable part) stays
/// here; the dispatch body lives in [`run_with_settings_store`] so tests can
/// drive the full TOML -> resolve_settings -> dispatch wiring against an
/// injected store.
fn run_with_settings(
    cmd: Command,
    settings: &crate::config::Settings,
    progress_mode: ProgressMode,
    out: &mut dyn Write,
    err: &mut (dyn Write + Send),
) -> i32 {
    // Build the store: a real `[store]` bucket -> S3Store; otherwise the mock.
    let store: Box<dyn ObjectStore> = if settings.store.bucket.is_empty() {
        Box::new(MemoryStore::new())
    } else {
        match crate::store::s3::S3Store::new(&settings.store, &settings.retry, settings.concurrency)
        {
            Ok(s) => Box::new(s),
            Err(e) => {
                let _ = writeln!(err, "error: {e}");
                return 1;
            }
        }
    };
    run_with_settings_store(cmd, settings, store.as_ref(), progress_mode, out, err)
}

/// The dispatch body of [`run_with_settings`] with an externally-provided
/// store (W92/r2-M3 seam): every pre-flight check, the vault merge, and the
/// `run_with_io` handoff are exercised with a test-injectable store.
fn run_with_settings_store(
    cmd: Command,
    settings: &crate::config::Settings,
    store: &dyn ObjectStore,
    progress_mode: ProgressMode,
    out: &mut dyn Write,
    err: &mut (dyn Write + Send),
) -> i32 {
    if requires_real_store(&cmd) && settings.store.bucket.is_empty() {
        let _ = writeln!(
            err,
            "error: {} requires a configured [store] (set store.bucket and AWS credentials); the in-memory mock is for `status` only",
            command_name(&cmd)
        );
        return 1;
    }
    // Merge the config vault_root into the command when --vault was unset.
    let cmd = resolve_vault_from_config(cmd, settings);

    // Issue #34 (D-wire): compile the resolved ignore patterns once at the
    // settings boundary and thread the SAME set into both the local walk
    // (`LocalFs::with_ignore`, issue #32) and the remote filter
    // (`build_plan`, issue #33) so the two halves can never disagree
    // (D-both-sides). Patterns were already validated in `resolve_settings`;
    // a re-compile failure here is defensive - fail loudly (exit 1, same
    // shape as other settings errors), never `unwrap` in the production path.
    let ignore = match crate::IgnoreSet::from_patterns(&settings.resolved_ignore_patterns) {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return 1;
        }
    };

    run_with_io(
        cmd,
        store,
        &DispatchCtx {
            tolerance_ms: settings.mtime_tolerance_ms,
            concurrency: settings.concurrency,
            progress_mode,
            ignore,
            inventory_mode: settings.inventory_mode,
        },
        out,
        err,
    )
}

/// Entry point used by `main`: args from env, config load, store dispatch,
/// real stdout/stderr. The store is config-driven (Slice 6/7): a `[store]`
/// section builds S3Store; without one, `status` uses the in-memory mock and
/// push/pull/check are refused (W5).
pub fn run_from_env() -> i32 {
    let args: Vec<OsString> = std::env::args_os().collect();
    let args = match os_args_to_strings(args) {
        Ok(a) => a,
        Err(msg) => {
            let stderr = std::io::stderr();
            let mut err = stderr.lock();
            let _ = writeln!(err, "error: {msg}");
            return 1;
        }
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    // I27: use the `Stderr` handle (Send + Sync), not `StderrLock` (!Send),
    // so the progress renderer built over it satisfies `Progress: Send +
    // Sync`; `Stderr::write` locks internally per call, so worker-thread
    // emission serializes on it.
    let mut err = std::io::stderr();

    let cmd = match parse_args(&args) {
        Ok(c) => c,
        Err(msg) => {
            let _ = writeln!(err, "{msg}");
            return 1;
        }
    };

    // help/version need no config and must not fail on a bad config file.
    if matches!(cmd, Command::Help | Command::Version) {
        // help/version need no tolerance; the value is unused for these. The
        // ignore set stays empty (W203): help/version never walk or plan.
        let ctx = DispatchCtx {
            tolerance_ms: 0,
            concurrency: 1,
            progress_mode: ProgressMode::Auto,
            ignore: crate::IgnoreSet::empty(),
            inventory_mode: crate::config::InventoryMode::Auto,
        };
        return run_with_io(cmd, &MemoryStore::new(), &ctx, &mut out, &mut err);
    }

    // Load + resolve config.
    let explicit = cmd_config_path(&cmd).map(Path::to_path_buf);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from(VAULT_UNSET));
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let search = crate::config::default_search_paths(&cwd, home.as_deref());
    let settings = {
        let cfg = match crate::config::load_config(explicit.as_deref(), &search) {
            Ok(c) => c,
            Err(e) => {
                let _ = writeln!(err, "error: {e}");
                return 1;
            }
        };
        let envsnap = crate::config::EnvSnapshot {
            aws_region: std::env::var("AWS_REGION").ok(),
        };
        match crate::config::resolve_settings(&cfg, &envsnap) {
            Ok(s) => s,
            Err(e) => {
                let _ = writeln!(err, "error: {e}");
                return 1;
            }
        }
    };
    run_with_settings(cmd, &settings, ProgressMode::Auto, &mut out, &mut err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::mock::MemoryStore;
    use crate::testutil::TempDir;

    fn a() -> Vec<String> {
        vec!["vaultsync".to_string()]
    }

    #[test]
    fn vault_precedence_cli_over_config() {
        // W83/r9 N1: the --vault/config merge lives in ONE place -
        // `resolve_vault_from_config` (the old `Cli` merge arm inside
        // `resolve_settings` was test-only; production always passed
        // `Cli::default()`). An explicit `--vault` wins; an unset `--vault`
        // (the VAULT_UNSET sentinel) is replaced by the config vault root.
        let cfg_root = PathBuf::from("/cfg/vault");
        let settings = crate::config::Settings {
            vault_root: cfg_root.clone(),
            store: crate::config::StoreSettings {
                bucket: String::new(),
                region: None,
                endpoint: None,
                prefix: String::new(),
                path_style: false,
            },
            mtime_tolerance_ms: 1000,
            concurrency: 4,
            retry: crate::config::RetrySettings::default(),
            ignore_patterns: Vec::new(),
            resolved_ignore_patterns: Vec::new(),
            inventory_mode: crate::config::InventoryMode::Auto,
        };
        // explicit --vault wins over the config root
        let cli_explicit = Command::status(PathBuf::from("/cli/vault"));
        let resolved = resolve_vault_from_config(cli_explicit, &settings);
        match resolved {
            Command::Status { vault, .. } => {
                assert_eq!(vault, PathBuf::from("/cli/vault"))
            }
            other => panic!("expected Status, got {other:?}"),
        }
        // unset --vault (the sentinel) is replaced by the config vault root
        let cli_unset = Command::status(PathBuf::from(VAULT_UNSET));
        let resolved = resolve_vault_from_config(cli_unset, &settings);
        match resolved {
            Command::Status { vault, .. } => assert_eq!(vault, cfg_root),
            other => panic!("expected Status, got {other:?}"),
        }
    }

    // --- parse ---

    #[test]
    fn parse_version() {
        let mut args = a();
        args.push("version".into());
        assert_eq!(parse_args(&args).unwrap(), Command::Version);
    }

    #[test]
    fn parse_version_flag() {
        let mut args = a();
        args.push("--version".into());
        assert_eq!(parse_args(&args).unwrap(), Command::Version);
    }

    #[test]
    fn parse_version_rejects_trailing_token() {
        let mut args = a();
        args.push("version".into());
        args.push("--bogus".into());
        assert!(parse_args(&args).is_err());
    }

    #[test]
    fn parse_help() {
        let mut args = a();
        args.push("--help".into());
        assert_eq!(parse_args(&args).unwrap(), Command::Help);
    }

    #[test]
    fn parse_help_subcommand() {
        let mut args = a();
        args.push("help".into());
        assert_eq!(parse_args(&args).unwrap(), Command::Help);
    }

    #[test]
    fn parse_status_default_vault() {
        let mut args = a();
        args.push("status".into());
        assert_eq!(
            parse_args(&args).unwrap(),
            Command::status(PathBuf::from("."))
        );
    }

    #[test]
    fn parse_global_vault_before_subcommand() {
        // Global flags accepted before the subcommand (clap global=true).
        let mut args = a();
        args.push("--vault".into());
        args.push("/v".into());
        args.push("status".into());
        assert_eq!(
            parse_args(&args).unwrap(),
            Command::status(PathBuf::from("/v"))
        );
    }

    #[test]
    fn parse_vault_equals_form() {
        // `--vault=<path>` escape hatch (P1r5).
        let mut args = a();
        args.push("status".into());
        args.push("--vault=/v".into());
        assert_eq!(
            parse_args(&args).unwrap(),
            Command::status(PathBuf::from("/v"))
        );
    }

    #[test]
    fn parse_vault_dash_name_via_equals() {
        // A vault literally named `-foo` is reachable via the equals form.
        let mut args = a();
        args.push("status".into());
        args.push("--vault=-foo".into());
        assert_eq!(
            parse_args(&args).unwrap(),
            Command::status(PathBuf::from("-foo"))
        );
    }

    #[test]
    fn parse_double_dash_terminator() {
        // `--` ends flag parsing; a following token is a positional, and
        // status has none, so this is an error - not a swallowed flag.
        let mut args = a();
        args.push("status".into());
        args.push("--".into());
        args.push("--weird".into());
        assert!(parse_args(&args).is_err());
    }

    #[test]
    fn parse_config_flag() {
        let mut args = a();
        args.push("--config".into());
        args.push("/c.toml".into());
        args.push("status".into());
        match parse_args(&args).unwrap() {
            Command::Status { config, .. } => {
                assert_eq!(config, Some(PathBuf::from("/c.toml")));
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn parse_push_force_local() {
        let mut args = a();
        args.push("push".into());
        args.push("--force-local".into());
        match parse_args(&args).unwrap() {
            Command::Push { force_local, .. } => assert!(force_local),
            other => panic!("expected Push, got {other:?}"),
        }
    }

    #[test]
    fn parse_pull_force_remote() {
        let mut args = a();
        args.push("pull".into());
        args.push("--force-remote".into());
        match parse_args(&args).unwrap() {
            Command::Pull { force_remote, .. } => assert!(force_remote),
            other => panic!("expected Pull, got {other:?}"),
        }
    }

    #[test]
    fn parse_both_forces_accepted_planner_cancels() {
        // Both forces parse fine; the planner cancels them to Conflict
        // (P1r-both-forces) - unchanged by migration.
        let mut args = a();
        args.push("push".into());
        args.push("--force-local".into());
        args.push("--force-remote".into());
        match parse_args(&args).unwrap() {
            Command::Push {
                force_local,
                force_remote,
                ..
            } => {
                assert!(force_local);
                assert!(force_remote);
            }
            other => panic!("expected Push, got {other:?}"),
        }
    }

    #[test]
    fn parse_follow_symlinks() {
        let mut args = a();
        args.push("push".into());
        args.push("--follow-symlinks".into());
        match parse_args(&args).unwrap() {
            Command::Push {
                follow_symlinks, ..
            } => assert!(follow_symlinks),
            other => panic!("expected Push, got {other:?}"),
        }
        // also accepted before the subcommand (global)
        let mut args = a();
        args.push("--follow-symlinks".into());
        args.push("status".into());
        match parse_args(&args).unwrap() {
            Command::Status {
                follow_symlinks, ..
            } => assert!(follow_symlinks),
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn parse_dry_run_flag() {
        let mut args = a();
        args.push("push".into());
        args.push("--dry-run".into());
        match parse_args(&args).unwrap() {
            Command::Push { dry_run, .. } => assert!(dry_run),
            other => panic!("expected Push, got {other:?}"),
        }
    }

    #[test]
    fn parse_verbose_repeatable() {
        let mut args = a();
        args.push("-vv".into());
        args.push("status".into());
        match parse_args(&args).unwrap() {
            Command::Status { verbose, .. } => assert_eq!(verbose, 2),
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn parse_repeated_vault_still_errors() {
        // P1r4-vault-value: repeated `--vault` is a parse error.
        let mut args = a();
        args.push("status".into());
        args.push("--vault".into());
        args.push("a".into());
        args.push("--vault".into());
        args.push("b".into());
        assert!(parse_args(&args).is_err());
    }

    #[test]
    fn parse_repeated_delete_still_errors() {
        // P1r7-delete-repeat: repeated `--delete` is a parse error.
        let mut args = a();
        args.push("push".into());
        args.push("--delete".into());
        args.push("--delete".into());
        assert!(parse_args(&args).is_err());
    }

    #[test]
    fn parse_unknown_flag_errors_with_usage() {
        let mut args = a();
        args.push("status".into());
        args.push("--bogus".into());
        let msg = parse_args(&args).unwrap_err();
        assert!(
            msg.lines()
                .any(|l| l.trim_start().to_lowercase().starts_with("usage:")),
            "error lacks usage: {msg}"
        );
    }

    #[test]
    fn parse_help_per_subcommand() {
        let mut args = a();
        args.push("push".into());
        args.push("--help".into());
        assert_eq!(parse_args(&args).unwrap(), Command::Help);
    }

    #[test]
    fn parse_status_rejects_delete_flag() {
        // `--delete` is defined only on push/pull; status rejects it.
        let mut args = a();
        args.push("status".into());
        args.push("--delete".into());
        let msg = parse_args(&args).unwrap_err();
        assert!(msg.contains("--delete"), "msg: {msg}");
    }

    #[test]
    fn parse_unknown_command() {
        let mut args = a();
        args.push("foo".into());
        let msg = parse_args(&args).unwrap_err();
        assert!(
            msg.to_lowercase().contains("unrecognized")
                || msg.to_lowercase().contains("unknown")
                || msg.to_lowercase().contains("invalid"),
            "msg: {msg}"
        );
    }

    #[test]
    fn parse_unknown_flag() {
        let mut args = a();
        args.push("status".into());
        args.push("--bogus".into());
        assert!(parse_args(&args).is_err());
    }

    #[test]
    fn parse_yes_rejected_until_phase3() {
        // `--yes` / `--max-delete` stay Phase 3 rails (unknown until those
        // land). `--concurrency` is rejected too, but for a different reason:
        // the knob is config-only (`[transfer].concurrency`, live since
        // issue 20) - there is deliberately no CLI flag.
        for tok in ["--yes", "--max-delete=5", "--concurrency=4"] {
            let mut args = a();
            args.push("push".into());
            args.push(tok.into());
            assert!(parse_args(&args).is_err(), "flag {tok} should be rejected");
        }
    }

    #[test]
    fn parse_errors_always_include_usage() {
        // P1r7-parse-usage: every parse error carries a usage line (clap
        // native). Kept as a lowercased line-prefix match for clap's format.
        let cases: Vec<Vec<&str>> = vec![
            vec!["vaultsync", "foo"],                // unknown command
            vec!["vaultsync", "status", "--bogus"],  // unknown flag
            vec!["vaultsync", "status", "extra"],    // positional
            vec!["vaultsync", "status", "--delete"], // status delete
            vec!["vaultsync", "push", "--vault", "/a", "--vault", "/b"], // repeated vault
            vec!["vaultsync", "push", "--vault"],    // missing value
            vec!["vaultsync", "push", "--delete", "--delete"], // repeated delete
        ];
        for args in cases {
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let msg = parse_args(&args).unwrap_err();
            assert!(
                msg.lines()
                    .any(|l| l.trim_start().to_lowercase().starts_with("usage:")),
                "case {args:?} missing usage: {msg:?}"
            );
        }
    }

    // --- argv conversion seam ---

    #[cfg(unix)]
    #[test]
    fn cli_args_reject_non_utf8() {
        use std::os::unix::ffi::OsStrExt;
        let offending = std::ffi::OsStr::from_bytes(b"\xff\xfe").to_os_string();
        let expected_debug = format!("{offending:?}");
        let err =
            os_args_to_strings(vec![std::ffi::OsString::from("vaultsync"), offending]).unwrap_err();
        assert!(err.contains("UTF-8"), "msg: {err}");
        assert!(
            err.contains(&expected_debug),
            "offending bytes not shown: {err}"
        );
    }

    #[test]
    fn cli_args_valid_utf8_roundtrip() {
        let args = os_args_to_strings(vec!["vaultsync".into(), "status".into()]).unwrap();
        assert_eq!(
            parse_args(&args).unwrap(),
            Command::status(PathBuf::from("."))
        );
    }

    // --- dispatch ---

    // Ignore e2e must use run_with_settings / run_with_settings_store (real
    // resolve -> resolved_ignore_patterns). These helpers keep
    // IgnoreSet::empty() on purpose so pre-ignore contracts (progress,
    // tolerance, parse, store refusal, ...) stay isolated from the default
    // Obsidian profile.
    fn run(cmd: Command, store: &dyn ObjectStore) -> (i32, String, String) {
        run_tol(cmd, store, crate::config::DEFAULT_MTIME_TOLERANCE_MS)
    }

    fn run_tol(cmd: Command, store: &dyn ObjectStore, tolerance_ms: u64) -> (i32, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_io(
            cmd,
            store,
            &DispatchCtx {
                tolerance_ms,
                concurrency: 1,
                // I27-test: the existing suite captures stderr and must stay
                // progress-silent; only progress tests opt into Always.
                progress_mode: ProgressMode::Off,
                ignore: crate::IgnoreSet::empty(),
                // W235: list_head keeps this helper exactly today's behavior.
                inventory_mode: crate::config::InventoryMode::ListHead,
            },
            &mut out,
            &mut err,
        );
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn run_seam_defaults_progress_off_for_captured_io() {
        // I27-test no-change guard: a push over the captured-stderr seam with
        // ProgressMode::Off must not emit any bar/refresh bytes - the existing
        // suite's captured-stderr contracts stay untouched. The run still
        // executes and stdout still carries the plan text.
        let dir = TempDir::new("vaultsync-cli-test");
        for i in 0..4 {
            std::fs::write(dir.join(format!("n{i}.md")), format!("body-{i}")).unwrap();
        }
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_io(
            Command::push(dir.path().into(), false),
            &MemoryStore::new(),
            &DispatchCtx {
                tolerance_ms: crate::config::DEFAULT_MTIME_TOLERANCE_MS,
                concurrency: 1,
                progress_mode: ProgressMode::Off,
                ignore: crate::IgnoreSet::empty(),
                inventory_mode: crate::config::InventoryMode::ListHead,
            },
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0);
        let out = String::from_utf8(out).unwrap();
        let err = String::from_utf8(err).unwrap();
        assert!(!err.contains('\r'), "no carriage-return frames: {err:?}");
        assert!(!err.contains("\x1b[K"), "no clear-to-EOL: {err:?}");
        assert!(!err.contains("Uploading"), "no bar verb: {err:?}");
        assert!(out.contains("U  n0.md"), "plan text still on stdout: {out}");
    }

    /// I27 cycle 8 helper: run a command over the seam with an explicit
    /// progress mode and captured stdout/stderr buffers.
    fn run_mode(
        cmd: Command,
        store: &dyn ObjectStore,
        mode: ProgressMode,
    ) -> (i32, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_io(
            cmd,
            store,
            &DispatchCtx {
                tolerance_ms: crate::config::DEFAULT_MTIME_TOLERANCE_MS,
                concurrency: 1,
                progress_mode: mode,
                ignore: crate::IgnoreSet::empty(),
                inventory_mode: crate::config::InventoryMode::ListHead,
            },
            &mut out,
            &mut err,
        );
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn push_always_progress_writes_bar_frames_to_stderr() {
        // I27 cycle 8: under ProgressMode::Always a push writes \r refresh
        // frames with the pass verb to stderr, ending with an n/n frame;
        // stdout keeps only the plan text (I27-shape: progress never touches
        // stdout).
        let dir = TempDir::new("vaultsync-cli-test");
        for i in 0..4 {
            std::fs::write(dir.join(format!("n{i}.md")), format!("body-{i}")).unwrap();
        }
        let (code, out, err) = run_mode(
            Command::push(dir.path().into(), false),
            &MemoryStore::new(),
            ProgressMode::Always,
        );
        assert_eq!(code, 0, "stderr: {err}");
        assert!(err.contains('\r'), "expected \r frames: {err:?}");
        assert!(err.contains("\x1b[K"), "expected clear-to-EOL: {err:?}");
        assert!(err.contains("Uploading"), "expected pass verb: {err:?}");
        assert!(err.contains("4/4"), "expected final n/n frame: {err:?}");
        assert!(out.contains("U  n0.md"), "plan text on stdout: {out}");
        assert!(
            !out.contains('\r'),
            "stdout must stay progress-free: {out:?}"
        );
        assert!(
            !out.contains("\x1b[K"),
            "stdout must stay progress-free: {out:?}"
        );
    }

    #[test]
    fn push_off_progress_writes_nothing() {
        // I27 cycle 8: the same push under ProgressMode::Off writes no bar/
        // refresh bytes to stderr and leaves stdout byte-identical to the
        // Always run (the renderer is the only difference).
        let dir = TempDir::new("vaultsync-cli-test");
        for i in 0..4 {
            std::fs::write(dir.join(format!("n{i}.md")), format!("body-{i}")).unwrap();
        }
        // Two fresh stores: each leg must plan the same 4-upload push (a
        // shared store would let the Always leg mutate it first).
        let cmd = Command::push(dir.path().into(), false);
        let (code_on, out_on, err_on) =
            run_mode(cmd.clone(), &MemoryStore::new(), ProgressMode::Always);
        let (code_off, out_off, err_off) = run_mode(cmd, &MemoryStore::new(), ProgressMode::Off);
        assert_eq!(code_on, code_off);
        assert_eq!(out_on, out_off, "stdout must be byte-identical");
        assert!(!err_off.contains('\r'), "no frames under Off: {err_off:?}");
        assert!(
            !err_off.contains("Uploading"),
            "no verb under Off: {err_off:?}"
        );
        assert!(
            err_on.contains('\r') && !err_on.is_empty(),
            "sanity: the Always leg does render"
        );
    }

    #[test]
    fn pull_always_progress_writes_download_frames() {
        // I27 cycle 8: a pull renders Downloading frames.
        let dir = TempDir::new("vaultsync-cli-test");
        let store = MemoryStore::new();
        for i in 0..3 {
            let mut cursor = std::io::Cursor::new(format!("remote-{i}").into_bytes());
            store
                .put_from(
                    &format!("r{i}.md"),
                    &mut cursor,
                    format!("remote-{i}").len() as u64,
                    Some(1_700_000_000_000 + i),
                )
                .unwrap();
        }
        let (code, out, err) = run_mode(
            Command::pull(dir.path().into(), false),
            &store,
            ProgressMode::Always,
        );
        assert_eq!(code, 0, "stderr: {err}");
        assert!(err.contains('\r'), "expected \r frames: {err:?}");
        assert!(err.contains("Downloading"), "expected pass verb: {err:?}");
        assert!(err.contains("3/3"), "expected final n/n frame: {err:?}");
        assert!(out.contains("D  r0.md"), "plan text on stdout: {out}");
        assert!(
            !out.contains("\x1b[K"),
            "stdout must stay progress-free: {out:?}"
        );
    }

    #[test]
    fn dry_run_and_status_emit_no_progress() {
        // I27 cycle 8: --dry-run push and status never construct a renderer
        // (the executor does not run / is not dispatched), so even
        // ProgressMode::Always produces no frames.
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::write(dir.join("a.md"), "hello").unwrap();
        let dry_run_cmd = Command::Push {
            vault: dir.path().into(),
            delete: false,
            dry_run: true,
            force_local: false,
            force_remote: false,
            json: false,
            config: None,
            verbose: 0,
            follow_symlinks: false,
        };
        let (code_dry, _, err_dry) =
            run_mode(dry_run_cmd, &MemoryStore::new(), ProgressMode::Always);
        assert_eq!(code_dry, 2, "dry run of a dirty push exits 2 (like status)");
        assert!(
            !err_dry.contains('\r') && !err_dry.contains("Uploading"),
            "dry-run must not render: {err_dry:?}"
        );
        let (code_st, _, err_st) = run_mode(
            Command::status(dir.path().into()),
            &MemoryStore::new(),
            ProgressMode::Always,
        );
        assert!(
            !err_st.contains('\r')
                && !err_st.contains("Downloading")
                && !err_st.contains("Uploading"),
            "status must not render: {err_st:?}"
        );
        assert_eq!(code_st, 2, "dirty status exits 2");
    }

    #[test]
    fn progress_does_not_change_exit_codes() {
        // I27 cycle 8: the 0/1/2 exit-code table is unchanged under
        // ProgressMode::Always (conflict -> 2, transfer failure -> 1).
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::write(dir.join("c.md"), "x").unwrap();
        let mt = std::fs::metadata(dir.join("c.md"))
            .unwrap()
            .modified()
            .unwrap();
        let ms = mt
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let store = MemoryStore::new();
        let mut cursor = std::io::Cursor::new(b"xx".to_vec());
        store.put_from("c.md", &mut cursor, 2, Some(ms)).unwrap();
        let (code_conflict, _, _) = run_mode(
            Command::push(dir.path().into(), false),
            &store,
            ProgressMode::Always,
        );
        assert_eq!(code_conflict, 2, "conflict -> exit 2");

        let dir2 = TempDir::new("vaultsync-cli-test");
        std::fs::write(dir2.join("a.md"), "hello").unwrap();
        let (code_fail, _, err) = run_mode(
            Command::push(dir2.path().into(), false),
            &FailPutStore {
                inner: MemoryStore::new(),
            },
            ProgressMode::Always,
        );
        assert_eq!(code_fail, 1, "transfer failure -> exit 1, stderr: {err}");
    }

    #[test]
    fn status_reports_skipped_temp_files() {
        // R4-L3/W41: the walker counts skipped vaultsync temp/probe leftovers
        // but the CLI never surfaced the count. A vault containing one must
        // warn on stderr (a crash signal, not just debug noise).
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::write(dir.join("note.md"), "real").unwrap();
        std::fs::write(dir.join(".note.md.vaultsync-tmp-1-1"), "crash-leftover").unwrap();
        let (code, _, err) = run(Command::status(dir.path().into()), &MemoryStore::new());
        assert_ne!(code, 1, "unexpected error exit");
        assert!(
            err.contains("skipped 1 vaultsync temp/probe file(s)"),
            "skipped-temp warning missing: {err}"
        );
    }

    #[test]
    fn run_version_exit_0() {
        let (code, out, _) = run(Command::Version, &MemoryStore::new());
        assert_eq!(code, 0);
        assert!(out.starts_with("vaultsync 0.1.0"));
    }

    #[test]
    fn run_help_exit_0() {
        let (code, out, _) = run(Command::Help, &MemoryStore::new());
        assert_eq!(code, 0);
        assert!(out.contains("Usage:"));
    }

    #[test]
    fn run_status_clean_exit_0() {
        let dir = TempDir::new("vaultsync-cli-test");
        let (code, out, _) = run(Command::status(dir.path().into()), &MemoryStore::new());
        assert_eq!(code, 0);
        assert!(out.contains("plan:"));
    }

    #[test]
    fn run_status_dirty_exit_2() {
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::write(dir.join("a.md"), "hi").unwrap();
        let (code, out, _) = run(Command::status(dir.path().into()), &MemoryStore::new());
        assert_eq!(code, 2);
        assert!(out.lines().any(|l| l.starts_with("U  a.md")));
    }

    #[test]
    fn status_prints_inventory_source_line() {
        // W236 (issue 45): every plan build prints one always-on stderr
        // milestone line naming the inventory source (locked strings):
        // `inventory: manifest (N entries)` warm vs `inventory: list+head
        // (cold)`. Always-on (like ignore warnings) so it is testable and
        // script-visible; #42 may later add progress around the cold path.
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::write(dir.join("a.md"), "hi").unwrap();
        let local = crate::local::LocalFs::new(dir.path());
        let (local_entities, _) = local.list_report().unwrap();
        let files: Vec<_> = local_entities
            .iter()
            .filter(|e| !e.is_folder())
            .cloned()
            .collect();

        // Warm: a store holding only the manifest (mode Auto).
        let warm = MemoryStore::new();
        let m = crate::manifest::file_entities_to_manifest(&files, 0, None, None).unwrap();
        let body = crate::manifest::serialize_manifest(&m).unwrap();
        let body_len = body.len() as u64;
        let mut c = std::io::Cursor::new(body);
        warm.put_from(crate::local::MANIFEST_KEY, &mut c, body_len, None)
            .unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_io(
            Command::status(dir.path().into()),
            &warm,
            &DispatchCtx {
                tolerance_ms: crate::config::DEFAULT_MTIME_TOLERANCE_MS,
                concurrency: 1,
                progress_mode: ProgressMode::Off,
                ignore: crate::IgnoreSet::empty(),
                inventory_mode: crate::config::InventoryMode::Auto,
            },
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0);
        assert!(
            String::from_utf8(err.clone())
                .unwrap()
                .contains("inventory: manifest (1 entries)"),
            "err: {}",
            String::from_utf8_lossy(&err)
        );

        // Cold: mode list_head on the same local tree (no manifest needed).
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_io(
            Command::status(dir.path().into()),
            &MemoryStore::new(),
            &DispatchCtx {
                tolerance_ms: crate::config::DEFAULT_MTIME_TOLERANCE_MS,
                concurrency: 1,
                progress_mode: ProgressMode::Off,
                ignore: crate::IgnoreSet::empty(),
                inventory_mode: crate::config::InventoryMode::ListHead,
            },
            &mut out,
            &mut err,
        );
        assert_eq!(code, 2, "local-only rows make status dirty");
        assert!(
            String::from_utf8(err.clone())
                .unwrap()
                .contains("inventory: list+head (cold)"),
            "err: {}",
            String::from_utf8_lossy(&err)
        );
    }

    #[test]
    fn run_status_json_rejected_not_implemented() {
        // --json parses (Slice 1) but dispatch rejects it (Phase 3 schema).
        let dir = TempDir::new("vaultsync-cli-test");
        let cmd = Command::Status {
            vault: dir.path().into(),
            json: true,
            config: None,
            verbose: 0,
            follow_symlinks: false,
        };
        let (code, _, err) = run(cmd, &MemoryStore::new());
        assert_eq!(code, 1);
        assert!(err.contains("--json"), "err: {err}");
    }

    #[test]
    fn push_commits_manifest_with_uploaded_entries() {
        // W240 (issue 45, D-commit-when/order): a push that uploads writes
        // the manifest LAST - bodies first - with the uploaded files as
        // entries. A second push that is clean (zero mutations) skips the
        // commit (manifest unchanged).
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::write(dir.join("a.md"), "hi").unwrap();
        let store = MemoryStore::new();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_io(
            Command::push(dir.path().into(), false),
            &store,
            &DispatchCtx {
                tolerance_ms: crate::config::DEFAULT_MTIME_TOLERANCE_MS,
                concurrency: 1,
                progress_mode: ProgressMode::Off,
                ignore: crate::IgnoreSet::empty(),
                inventory_mode: crate::config::InventoryMode::Auto,
            },
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0, "err: {}", String::from_utf8_lossy(&err));
        // Body uploaded and manifest written with one entry.
        let mut buf = Vec::new();
        let ent = store
            .get_to(crate::local::MANIFEST_KEY, &mut buf)
            .expect("manifest written");
        assert!(ent.etag.is_some());
        let m = crate::manifest::parse_manifest_bytes(&buf).unwrap();
        assert_eq!(m.entry_count, 1);
        assert_eq!(m.entries[0].key, "a.md");
        assert_eq!(m.entries[0].size, 2);
        let etag1 = ent.etag;
        // Second clean push: zero mutations -> no commit (etag unchanged).
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_io(
            Command::push(dir.path().into(), false),
            &store,
            &DispatchCtx {
                tolerance_ms: crate::config::DEFAULT_MTIME_TOLERANCE_MS,
                concurrency: 1,
                progress_mode: ProgressMode::Off,
                ignore: crate::IgnoreSet::empty(),
                inventory_mode: crate::config::InventoryMode::Auto,
            },
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0);
        let h = store.head(crate::local::MANIFEST_KEY).unwrap();
        assert_eq!(h.etag, etag1, "clean push must skip the commit");
    }

    #[test]
    fn push_delete_removes_key_from_manifest() {
        // W240 (issue 45): `push --delete` folds successful DeleteRemote
        // keys out of the committed manifest.
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::write(dir.join("a.md"), "hi").unwrap();
        let store = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"x".to_vec());
        store.put_from("gone.md", &mut c, 1, Some(1)).unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_io(
            Command::push(dir.path().into(), true),
            &store,
            &DispatchCtx {
                tolerance_ms: crate::config::DEFAULT_MTIME_TOLERANCE_MS,
                concurrency: 1,
                progress_mode: ProgressMode::Off,
                ignore: crate::IgnoreSet::empty(),
                inventory_mode: crate::config::InventoryMode::Auto,
            },
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0, "err: {}", String::from_utf8_lossy(&err));
        assert!(
            matches!(
                store.head("gone.md").unwrap_err(),
                crate::error::Error::NotFound(_)
            ),
            "remote extra must be deleted"
        );
        let mut buf = Vec::new();
        store.get_to(crate::local::MANIFEST_KEY, &mut buf).unwrap();
        let m = crate::manifest::parse_manifest_bytes(&buf).unwrap();
        let keys: Vec<&str> = m.entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["a.md"],
            "deleted key must be absent from manifest"
        );
    }

    #[test]
    fn push_auto_replaces_corrupt_remote_manifest() {
        // W249/H1 (review 5472028291): under InventoryMode::Auto a corrupt
        // remote manifest makes planning fall back to list+head (cold base,
        // manifest_etag: None). The push must still commit - H1 heads at
        // commit time and If-Match overwrites the corrupt object instead of
        // create-only If-None-Match: * failing with PreconditionFailed and a
        // stuck "manifest not committed" warning.
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::write(dir.join("a.md"), "hi").unwrap();
        let store = MemoryStore::new();
        // Seed MANIFEST_KEY with garbage.
        let mut c = std::io::Cursor::new(b"not a manifest".to_vec());
        store
            .put_from(crate::local::MANIFEST_KEY, &mut c, 14, None)
            .unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_io(
            Command::push(dir.path().into(), false),
            &store,
            &DispatchCtx {
                tolerance_ms: crate::config::DEFAULT_MTIME_TOLERANCE_MS,
                concurrency: 1,
                progress_mode: ProgressMode::Off,
                ignore: crate::IgnoreSet::empty(),
                inventory_mode: crate::config::InventoryMode::Auto,
            },
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0, "err: {}", String::from_utf8_lossy(&err));
        let err_s = String::from_utf8_lossy(&err);
        assert!(
            !err_s.contains("not committed"),
            "no create-only stuck state: {err_s}"
        );
        // The overwritten remote manifest parses and lists a.md.
        let mut buf = Vec::new();
        store.get_to(crate::local::MANIFEST_KEY, &mut buf).unwrap();
        let m = crate::manifest::parse_manifest_bytes(&buf).unwrap();
        assert_eq!(m.entry_count, 1);
        assert_eq!(m.entries[0].key, "a.md");
    }

    #[test]
    fn push_refreshes_local_manifest_cache() {
        // W251 (M1/F1, reviews 5472028291 + 5472033449): production push
        // wires the S7 local cache so the mirror (W246 contract) is live -
        // the committed manifest's meta file records the remote etag and the
        // body file holds the committed body. RED today: the push arm passes
        // cache: None, so no cache files appear.
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::write(dir.join("a.md"), "hi").unwrap();
        let store = MemoryStore::new();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_io(
            Command::push(dir.path().into(), false),
            &store,
            &DispatchCtx {
                tolerance_ms: crate::config::DEFAULT_MTIME_TOLERANCE_MS,
                concurrency: 1,
                progress_mode: ProgressMode::Off,
                ignore: crate::IgnoreSet::empty(),
                inventory_mode: crate::config::InventoryMode::Auto,
            },
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0, "err: {}", String::from_utf8_lossy(&err));
        let cache = crate::inventory::CachePaths::new(dir.path());
        let meta: crate::inventory::CacheMeta = serde_json::from_slice(
            &std::fs::read(&cache.meta)
                .unwrap_or_else(|_| panic!("cache meta missing at {}", cache.meta.display())),
        )
        .unwrap();
        let remote = store.head(crate::local::MANIFEST_KEY).unwrap();
        assert_eq!(
            meta.remote_etag, remote.etag,
            "cache meta etag must equal the committed remote etag"
        );
        let body = std::fs::read(&cache.body)
            .unwrap_or_else(|_| panic!("cache body missing at {}", cache.body.display()));
        let m = crate::manifest::parse_manifest_bytes(&body).unwrap();
        assert_eq!(m.entry_count, 1);
        assert_eq!(m.entries[0].key, "a.md");
    }

    #[test]
    fn pull_does_not_write_manifest() {
        // W240 / Q6 (issue 45): pull never commits the manifest - the body
        // arrives but the manifest etag/body stay untouched.
        let dir = TempDir::new("vaultsync-cli-test");
        let store = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"hi".to_vec());
        store
            .put_from("a.md", &mut c, 2, Some(1_600_000_000_000))
            .unwrap();
        let body = br#"{"schema":"vaultsync.manifest.v1","created_ms":0,"entry_count":1,"entries":[{"key":"a.md","size":2,"mtime_ms":1600000000000}]}"#;
        let mut c = std::io::Cursor::new(body.to_vec());
        let put = store
            .put_from(crate::local::MANIFEST_KEY, &mut c, body.len() as u64, None)
            .unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_io(
            Command::pull(dir.path().into(), false),
            &store,
            &DispatchCtx {
                tolerance_ms: crate::config::DEFAULT_MTIME_TOLERANCE_MS,
                concurrency: 1,
                progress_mode: ProgressMode::Off,
                ignore: crate::IgnoreSet::empty(),
                inventory_mode: crate::config::InventoryMode::Auto,
            },
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0, "err: {}", String::from_utf8_lossy(&err));
        assert!(dir.join("a.md").exists(), "body pulled");
        let h = store.head(crate::local::MANIFEST_KEY).unwrap();
        assert_eq!(h.etag, put.etag, "pull must not rewrite the manifest");
    }

    #[test]
    fn push_race_warning_exit_0_with_warning() {
        // W240 / Q2 (issue 45): when the manifest commit loses its
        // conditional race, the CLI prints the locked warning (substring
        // `manifest not committed`), suggests repair, and exits 0 when the
        // transfers succeeded (bodies are live; data ok).
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::write(dir.join("a.md"), "hi").unwrap();
        let store = RaceCommitStore {
            inner: MemoryStore::new(),
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_io(
            Command::push(dir.path().into(), false),
            &store,
            &DispatchCtx {
                tolerance_ms: crate::config::DEFAULT_MTIME_TOLERANCE_MS,
                concurrency: 1,
                progress_mode: ProgressMode::Off,
                ignore: crate::IgnoreSet::empty(),
                inventory_mode: crate::config::InventoryMode::Auto,
            },
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0, "race warning must not fail a successful push");
        let err = String::from_utf8(err).unwrap();
        assert!(
            err.contains("manifest not committed"),
            "locked warning substring missing: {err}"
        );
        assert!(err.contains("repair"), "suggests repair: {err}");
        // Body is live; manifest was NOT written (no clobber).
        let body = store.inner.list("").unwrap();
        assert!(
            body.entities
                .iter()
                .any(|e| e.key == "a.md" && !e.is_folder()),
            "uploaded body must be live: {:?}",
            body.entities
        );
        assert!(
            !body
                .entities
                .iter()
                .any(|e| e.key == crate::local::MANIFEST_KEY),
            "manifest must not be written on a lost race"
        );
    }

    #[test]
    fn repair_parses_and_runs_with_summary() {
        // W242 (issue 45, D-repair): `vaultsync repair` parses with
        // `--dry-run` / `--force`, and the run prints the locked summary
        // substrings: `repair: listed N objects via list+head` and
        // `repair: wrote .vaultsync/manifest/v1.json (N entries`. Exit 0 on
        // success; dry-run writes nothing.
        match parse_args(&["vaultsync".into(), "repair".into()]).unwrap() {
            Command::Repair {
                dry_run: false,
                force: false,
                ..
            } => {}
            other => panic!("expected plain repair, got {other:?}"),
        }
        match parse_args(&["vaultsync".into(), "repair".into(), "--dry-run".into()]).unwrap() {
            Command::Repair { dry_run: true, .. } => {}
            other => panic!("expected --dry-run, got {other:?}"),
        }
        match parse_args(&["vaultsync".into(), "repair".into(), "--force".into()]).unwrap() {
            Command::Repair { force: true, .. } => {}
            other => panic!("expected --force, got {other:?}"),
        }

        let store = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"hi".to_vec());
        store
            .put_from("a.md", &mut c, 2, Some(1_600_000_000_000))
            .unwrap();
        // Explicit vault dir: with the W251 cache wiring, a real `repair`
        // refreshes `<vault>/.vaultsync/cache/` - a `.` vault would pollute
        // the test CWD.
        let dir = TempDir::new("vaultsync-cli-test");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_io(
            Command::Repair {
                vault: dir.path().into(),
                json: false,
                config: None,
                verbose: 0,
                dry_run: false,
                force: false,
            },
            &store,
            &DispatchCtx {
                tolerance_ms: crate::config::DEFAULT_MTIME_TOLERANCE_MS,
                concurrency: 1,
                progress_mode: ProgressMode::Off,
                ignore: crate::IgnoreSet::empty(),
                inventory_mode: crate::config::InventoryMode::ListHead,
            },
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0, "err: {}", String::from_utf8_lossy(&err));
        let out = String::from_utf8(out).unwrap();
        let err = String::from_utf8(err).unwrap();
        assert!(
            out.contains("repair: listed 1 objects via list+head"),
            "out: {out}"
        );
        assert!(
            out.contains("repair: wrote .vaultsync/manifest/v1.json (1 entries"),
            "out: {out}"
        );
        assert!(!err.contains("error"), "no error expected: {err}");
        // Manifest written and valid.
        let mut buf = Vec::new();
        store.get_to(crate::local::MANIFEST_KEY, &mut buf).unwrap();
        let m = crate::manifest::parse_manifest_bytes(&buf).unwrap();
        assert_eq!(m.entry_count, 1);
    }

    #[test]
    fn repair_dry_run_prints_count_and_writes_nothing() {
        // W242 (issue 45): `repair --dry-run` prints the entry count but
        // never writes the manifest object.
        let store = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"hi".to_vec());
        store
            .put_from("a.md", &mut c, 2, Some(1_600_000_000_000))
            .unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_io(
            Command::Repair {
                vault: PathBuf::from(VAULT_UNSET),
                json: false,
                config: None,
                verbose: 0,
                dry_run: true,
                force: false,
            },
            &store,
            &DispatchCtx {
                tolerance_ms: crate::config::DEFAULT_MTIME_TOLERANCE_MS,
                concurrency: 1,
                progress_mode: ProgressMode::Off,
                ignore: crate::IgnoreSet::empty(),
                inventory_mode: crate::config::InventoryMode::ListHead,
            },
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0);
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("repair: listed 1 objects via list+head"),
            "out: {out}"
        );
        assert!(out.contains("dry-run"), "dry-run must be named: {out}");
        assert!(matches!(
            store.head(crate::local::MANIFEST_KEY).unwrap_err(),
            crate::error::Error::NotFound(_)
        ));
    }

    #[test]
    fn repair_refreshes_local_manifest_cache() {
        // W251 (M1/F1, reviews 5472028291 + 5472033449): repair (like push)
        // wires the S7 local cache - the rebuilt manifest's meta records the
        // fresh remote etag and the body file holds the rebuilt body. RED
        // today: the repair arm passes cache: None.
        let dir = TempDir::new("vaultsync-cli-test");
        let store = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"hi".to_vec());
        store
            .put_from("a.md", &mut c, 2, Some(1_600_000_000_000))
            .unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_io(
            Command::Repair {
                vault: dir.path().into(),
                json: false,
                config: None,
                verbose: 0,
                dry_run: false,
                force: false,
            },
            &store,
            &DispatchCtx {
                tolerance_ms: crate::config::DEFAULT_MTIME_TOLERANCE_MS,
                concurrency: 1,
                progress_mode: ProgressMode::Off,
                ignore: crate::IgnoreSet::empty(),
                inventory_mode: crate::config::InventoryMode::ListHead,
            },
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0, "err: {}", String::from_utf8_lossy(&err));
        let cache = crate::inventory::CachePaths::new(dir.path());
        let meta: crate::inventory::CacheMeta = serde_json::from_slice(
            &std::fs::read(&cache.meta)
                .unwrap_or_else(|_| panic!("cache meta missing at {}", cache.meta.display())),
        )
        .unwrap();
        let remote = store.head(crate::local::MANIFEST_KEY).unwrap();
        assert_eq!(
            meta.remote_etag, remote.etag,
            "cache meta etag must equal the rebuilt remote etag"
        );
        let body = std::fs::read(&cache.body)
            .unwrap_or_else(|_| panic!("cache body missing at {}", cache.body.display()));
        let m = crate::manifest::parse_manifest_bytes(&body).unwrap();
        assert_eq!(m.entry_count, 1);
    }

    #[test]
    fn repair_fails_loudly_on_store_error() {
        // W242 (issue 45): a store error during repair exits 1 with the
        // error text (list failure propagates).
        let store = crate::testutil::FailListStore {
            inner: MemoryStore::new(),
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_io(
            Command::repair(),
            &store,
            &DispatchCtx {
                tolerance_ms: crate::config::DEFAULT_MTIME_TOLERANCE_MS,
                concurrency: 1,
                progress_mode: ProgressMode::Off,
                ignore: crate::IgnoreSet::empty(),
                inventory_mode: crate::config::InventoryMode::ListHead,
            },
            &mut out,
            &mut err,
        );
        assert_eq!(code, 1);
        assert!(
            String::from_utf8(err.clone()).unwrap().contains("error"),
            "err: {}",
            String::from_utf8_lossy(&err)
        );
    }

    #[test]
    fn repair_json_is_rejected() {
        // W252 (F2, review 5472033449): `repair --json` is loud, not silent -
        // the flag parses (Phase 3 schema stability) but dispatch rejects it
        // with the locked substring like status/push/pull/check.
        match parse_args(&["vaultsync".into(), "repair".into(), "--json".into()]).unwrap() {
            Command::Repair { json: true, .. } => {}
            other => panic!("expected --json carried on Repair, got {other:?}"),
        }
        let store = MemoryStore::new();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_io(
            Command::Repair {
                vault: PathBuf::from(VAULT_UNSET),
                json: true,
                config: None,
                verbose: 0,
                dry_run: false,
                force: false,
            },
            &store,
            &DispatchCtx {
                tolerance_ms: crate::config::DEFAULT_MTIME_TOLERANCE_MS,
                concurrency: 1,
                progress_mode: ProgressMode::Off,
                ignore: crate::IgnoreSet::empty(),
                inventory_mode: crate::config::InventoryMode::ListHead,
            },
            &mut out,
            &mut err,
        );
        assert_eq!(code, 1);
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("--json"), "err: {err}");
        assert!(err.contains("not implemented"), "err: {err}");
    }

    /// A store whose `put_from` always fails, to inject a transfer failure.
    struct FailPutStore {
        inner: MemoryStore,
    }
    impl ObjectStore for FailPutStore {
        fn list(&self, prefix: &str) -> Result<crate::store::Listing, crate::error::Error> {
            self.inner.list(prefix)
        }
        fn head(&self, key: &str) -> Result<crate::entity::Entity, crate::error::Error> {
            self.inner.head(key)
        }
        fn get_to(
            &self,
            key: &str,
            w: &mut dyn std::io::Write,
        ) -> Result<crate::entity::Entity, crate::error::Error> {
            self.inner.get_to(key, w)
        }
        fn put_from(
            &self,
            key: &str,
            _r: &mut dyn std::io::Read,
            _size: u64,
            _mtime: Option<u64>,
        ) -> Result<crate::entity::Entity, crate::error::Error> {
            Err(crate::error::Error::Other(format!(
                "injected put failure for {key}"
            )))
        }
        fn delete(&self, key: &str) -> Result<(), crate::error::Error> {
            self.inner.delete(key)
        }
    }

    /// Store whose CONDITIONAL put always loses the race (W240): plain puts
    /// (file bodies) succeed, but any `put_from_with` with a precondition
    /// answers PreconditionFailed - simulating another writer committing the
    /// manifest between our plan and our commit.
    struct RaceCommitStore {
        inner: MemoryStore,
    }
    impl ObjectStore for RaceCommitStore {
        fn list(&self, prefix: &str) -> Result<crate::store::Listing, crate::error::Error> {
            self.inner.list(prefix)
        }
        fn head(&self, key: &str) -> Result<crate::entity::Entity, crate::error::Error> {
            self.inner.head(key)
        }
        fn get_to(
            &self,
            key: &str,
            w: &mut dyn std::io::Write,
        ) -> Result<crate::entity::Entity, crate::error::Error> {
            self.inner.get_to(key, w)
        }
        fn put_from(
            &self,
            key: &str,
            r: &mut dyn std::io::Read,
            size: u64,
            mtime_ms: Option<u64>,
        ) -> Result<crate::entity::Entity, crate::error::Error> {
            self.inner.put_from(key, r, size, mtime_ms)
        }
        fn put_from_with(
            &self,
            key: &str,
            _r: &mut dyn std::io::Read,
            _size: u64,
            opts: crate::store::PutOpts,
        ) -> Result<crate::entity::Entity, crate::error::Error> {
            if opts.if_match_etag.is_some() || opts.if_none_match_star {
                return Err(crate::error::Error::PreconditionFailed(format!(
                    "lost race on {key}"
                )));
            }
            self.inner.put_from(key, _r, _size, opts.mtime_ms)
        }
        fn delete(&self, key: &str) -> Result<(), crate::error::Error> {
            self.inner.delete(key)
        }
    }

    #[cfg(unix)]
    #[test]
    fn walker_hint_does_not_promise_transfer_inclusion() {
        // R4-M1/W38: the skipped-symlink hint must not promise that
        // `--follow-symlinks` will include symlinks in transfers - follow is
        // inventory-only in v1 (push/pull Skip followed file symlinks). The
        // old text said "...to include".
        let dir = TempDir::new("vaultsync-cli-test");
        let outside = TempDir::new("vaultsync-cli-outside");
        std::fs::write(outside.join("secret.txt"), "s").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), dir.join("link.txt")).unwrap();
        let (code, _, err) = run(Command::status(dir.path().into()), &MemoryStore::new());
        assert_eq!(code, 0);
        assert!(
            !err.contains("to include"),
            "hint promises transfer inclusion: {err}"
        );
        assert!(
            err.contains("followed symlinks") || err.contains("status only"),
            "hint does not state inventory-only: {err}"
        );
    }

    #[test]
    fn run_check_exit_0_no_mock_label() {
        let (code, out, _) = run(Command::check(), &MemoryStore::new());
        assert_eq!(code, 0);
        assert!(out.contains("check: ok"));
        assert!(!out.contains("(mock)"), "(mock) marker removed: {out}");
    }

    #[test]
    fn parse_check_json_flag_carried() {
        // W53/B-L1: `check --json` must carry the flag into the Command so
        // dispatch can reject it like status/push/pull - today it is silently
        // dropped and check runs human output with exit 0.
        let mut args = a();
        args.push("check".into());
        args.push("--json".into());
        match parse_args(&args).unwrap() {
            Command::Check { json, .. } => assert!(json),
            other => panic!("expected Check, got {other:?}"),
        }
    }

    #[test]
    fn run_check_json_rejected_not_implemented() {
        // W53/B-L1: `check --json` must be rejected at dispatch (exit 1,
        // stderr names --json), mirroring status/push/pull - the one
        // subcommand that silently dropped the flag is closed.
        let (code, out, err) = run(
            Command::Check {
                config: None,
                verbose: 0,
                json: true,
            },
            &MemoryStore::new(),
        );
        assert_eq!(code, 1);
        assert!(err.contains("--json"), "err: {err}");
        assert!(
            !out.contains("check: ok"),
            "must not run human check: {out}"
        );
    }

    /// A store whose probe put is denied -> actionable failure path.
    struct CheckFailStore {
        inner: MemoryStore,
    }
    impl ObjectStore for CheckFailStore {
        fn list(&self, prefix: &str) -> Result<crate::store::Listing, crate::error::Error> {
            self.inner.list(prefix)
        }
        fn head(&self, key: &str) -> Result<crate::entity::Entity, crate::error::Error> {
            self.inner.head(key)
        }
        fn get_to(
            &self,
            key: &str,
            w: &mut dyn std::io::Write,
        ) -> Result<crate::entity::Entity, crate::error::Error> {
            self.inner.get_to(key, w)
        }
        fn put_from(
            &self,
            _k: &str,
            _r: &mut dyn std::io::Read,
            _s: u64,
            _m: Option<u64>,
        ) -> Result<crate::entity::Entity, crate::error::Error> {
            Err(crate::error::Error::Unauthorized("denied".to_string()))
        }
        fn delete(&self, key: &str) -> Result<(), crate::error::Error> {
            self.inner.delete(key)
        }
    }

    #[test]
    fn run_check_failure_actionable_message() {
        let store = CheckFailStore {
            inner: MemoryStore::new(),
        };
        let (code, _, err) = run(Command::check(), &store);
        assert_eq!(code, 1);
        assert!(
            err.to_lowercase().contains("credential") || err.to_lowercase().contains("permission"),
            "actionable hint missing: {err}"
        );
    }

    fn no_store_settings(vault: &std::path::Path) -> crate::config::Settings {
        // F4/W197 (PR 38 r1): helpers go through the real resolve path so the
        // (user, resolved) split can never be production-impossible. Absent
        // `[ignore]` => user empty, resolved = Obsidian six. Empty bucket
        // keeps the in-memory mock store path used by CLI unit tests.
        let cfg = crate::config::FileConfig {
            vault_root: Some(vault.to_path_buf()),
            ..Default::default()
        };
        crate::config::resolve_settings(&cfg, &crate::config::EnvSnapshot::default())
            .expect("default FileConfig must resolve")
    }

    fn settings_with_ignore(
        vault: &std::path::Path,
        patterns: Vec<&str>,
    ) -> crate::config::Settings {
        // F4/W197: real resolve with `[ignore].patterns` only; profile absent
        // => Obsidian default, user patterns extend (production semantics).
        let cfg = crate::config::FileConfig {
            vault_root: Some(vault.to_path_buf()),
            ignore: Some(crate::config::IgnoreConfig {
                profile: None,
                patterns: patterns.iter().map(|s| s.to_string()).collect(),
            }),
            ..Default::default()
        };
        crate::config::resolve_settings(&cfg, &crate::config::EnvSnapshot::default())
            .expect("valid ignore patterns in settings helpers must resolve")
    }

    fn settings_profile_none(
        vault: &std::path::Path,
        patterns: Vec<&str>,
    ) -> crate::config::Settings {
        // F4/W197: real resolve with `[ignore].profile = "none"` - the
        // escape hatch that disables the Obsidian built-ins (user patterns
        // still apply when non-empty).
        let cfg = crate::config::FileConfig {
            vault_root: Some(vault.to_path_buf()),
            ignore: Some(crate::config::IgnoreConfig {
                profile: Some("none".to_string()),
                patterns: patterns.iter().map(|s| s.to_string()).collect(),
            }),
            ..Default::default()
        };
        crate::config::resolve_settings(&cfg, &crate::config::EnvSnapshot::default())
            .expect("valid profile=none settings must resolve")
    }

    /// The issue #34 acceptance fixture vault: the Obsidian built-in profile
    /// must keep `notes/a.md` + `.obsidian/app.json` and hide the workspace
    /// session file, `.trash/`, `.git/`, and `.DS_Store`; `profile = "none"`
    /// must list everything.
    fn write_default_profile_vault(dir: &TempDir) {
        std::fs::create_dir_all(dir.join(".obsidian")).unwrap();
        std::fs::create_dir_all(dir.join(".trash")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("notes")).unwrap();
        std::fs::write(dir.join(".obsidian/app.json"), "{}").unwrap();
        std::fs::write(dir.join(".obsidian/workspace.json"), "{}").unwrap();
        std::fs::write(dir.join(".trash/x.md"), "x").unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main").unwrap();
        std::fs::write(dir.join("notes/a.md"), "hi").unwrap();
        std::fs::write(dir.join("notes/.DS_Store"), "").unwrap();
    }

    #[test]
    fn status_default_profile_hides_workspace() {
        // Issue #34 D3 e2e (acceptance checkbox, issue sketch exact name):
        // with NO `[ignore]` config the Obsidian built-in profile applies
        // end-to-end through the CLI - the workspace session file, `.trash/`,
        // `.git/`, and `.DS_Store` are pruned from the status plan while
        // `notes/a.md` and `.obsidian/app.json` stay listed. Mutation-checked:
        // `profile = "none"` settings make this RED (everything listed).
        let dir = TempDir::new("vaultsync-cli-test");
        write_default_profile_vault(&dir);
        let settings = no_store_settings(dir.path());
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_settings(
            Command::status(dir.path().into()),
            &settings,
            ProgressMode::Off,
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).unwrap();
        let out = String::from_utf8(out).unwrap();
        // exit 2: dirty plan (uploads pending) - the point is the run
        // happens with the default profile applied.
        assert_eq!(code, 2, "stderr: {err}");
        assert!(
            out.lines().any(|l| l.starts_with("U  notes/a.md")),
            "notes/a.md must be listed: {out}"
        );
        assert!(
            out.lines().any(|l| l.starts_with("U  .obsidian/app.json")),
            ".obsidian/app.json must be listed: {out}"
        );
        assert!(
            !out.contains("workspace.json"),
            "workspace session file must be ignored: {out}"
        );
        assert!(!out.contains(".trash/"), ".trash/ pruned: {out}");
        assert!(!out.contains(".git/"), ".git/ pruned: {out}");
        assert!(!out.contains(".DS_Store"), ".DS_Store pruned: {out}");
    }

    #[test]
    fn status_profile_none_lists_workspace() {
        // Issue #34 D3 escape hatch (issue sketch exact name):
        // `profile = "none"` disables the Obsidian built-ins, so the same
        // fixture vault lists everything - workspace session file, `.trash/`,
        // `.git/HEAD`, `notes/.DS_Store` - with no Phase 3 text.
        let dir = TempDir::new("vaultsync-cli-test");
        write_default_profile_vault(&dir);
        let settings = settings_profile_none(dir.path(), vec![]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_settings(
            Command::status(dir.path().into()),
            &settings,
            ProgressMode::Off,
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(code, 2, "dirty plan; stderr: {err}");
        assert!(!err.contains("Phase 3"), "no W25 text: {err}");
        for expected in [
            "U  .obsidian/workspace.json",
            "U  .obsidian/app.json",
            "U  .trash/x.md",
            "U  .git/HEAD",
            "U  notes/.DS_Store",
            "U  notes/a.md",
        ] {
            assert!(
                out.lines().any(|l| l.starts_with(expected)),
                "{expected} missing under profile=none: {out}"
            );
        }
    }

    #[test]
    fn status_profile_none_still_skips_reserved() {
        // Issue #34 reserved orthogonal: even under `profile = "none"` the
        // reserved vaultsync temp/probe namespace is still skipped (a
        // walker-level invariant independent of ignore patterns) - the
        // reserved name is absent from the plan and surfaced as a temp-file
        // skip warning, never as an ignore skip.
        let dir = TempDir::new("vaultsync-cli-test");
        write_default_profile_vault(&dir);
        std::fs::write(dir.join(".name.vaultsync-tmp-1-2"), "x").unwrap();
        let settings = settings_profile_none(dir.path(), vec![]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_settings(
            Command::status(dir.path().into()),
            &settings,
            ProgressMode::Off,
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(code, 2, "dirty plan; stderr: {err}");
        assert!(
            !out.contains(".name.vaultsync-tmp-1-2"),
            "reserved name must be absent from plan: {out}"
        );
        assert!(
            err.contains("temp/probe") && err.contains("vaultsync"),
            "temp skip warning expected: {err}"
        );
        assert!(
            !err.contains("local path(s) by ignore patterns"),
            "reserved skip is not an ignore skip: {err}"
        );
    }

    #[test]
    fn settings_with_ignore_helper_matches_resolve_split() {
        // F4/W197 (PR 38 r1): the helpers must not build a
        // production-impossible split (user non-empty, resolved empty). A
        // helper-fed `.trash/` must look exactly like a real resolve: raw
        // user field = supplied patterns, resolved = Obsidian six with the
        // repeated `.trash/` deduped (no seventh entry). RED today: the
        // helper leaves resolved empty.
        let dir = TempDir::new("vaultsync-cli-test");
        let s = settings_with_ignore(dir.path(), vec![".trash/"]);
        assert_eq!(s.ignore_patterns, vec![".trash/".to_string()]);
        assert!(
            !s.resolved_ignore_patterns.is_empty(),
            "helper must resolve defaults, not leave resolved empty"
        );
        let expected: Vec<String> = crate::config::OBSIDIAN_DEFAULT_IGNORE_PATTERNS
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            s.resolved_ignore_patterns, expected,
            "built-in .trash/ deduped; no seventh entry"
        );
    }

    #[test]
    fn push_with_ignore_patterns_applies() {
        // Issue #34 (D-wire + D3-extend): user patterns (union with the
        // Obsidian defaults) must actually prune the push plan - no W25
        // refusal (exit 0), the ignored key absent from the plan, and the
        // ignored file never reaches the store. The fixture uses user-only
        // `private/` (not one of the six Obsidian built-ins), so a
        // defaults-only matcher cannot mask a broken user union.
        // Mutation-checked: built-ins-only at the compile site makes this RED
        // (private/secret.md uploads).
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::create_dir_all(dir.join("private")).unwrap();
        std::fs::write(dir.join("a.md"), "hi").unwrap();
        std::fs::write(dir.join("private/secret.md"), "s").unwrap();
        let mut settings = settings_with_ignore(dir.path(), vec!["private/"]);
        settings.store.bucket = "b".to_string(); // store injected below; avoids the no-store refusal
        let store = MemoryStore::new();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_settings_store(
            Command::push(dir.path().into(), false),
            &settings,
            &store,
            ProgressMode::Off,
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(code, 0, "stderr: {err}");
        assert!(!err.contains("Phase 3"), "W25 refusal must be gone: {err}");
        assert!(
            out.lines().any(|l| l.starts_with("U  a.md")),
            "a.md planned: {out}"
        );
        assert!(
            !out.contains("private/secret.md"),
            "ignored key absent from plan: {out}"
        );
        assert!(store.head("a.md").is_ok(), "a.md uploaded");
        assert!(
            matches!(
                store.head("private/secret.md"),
                Err(crate::error::Error::NotFound(_))
            ),
            "ignored file must never reach the store"
        );
    }

    #[test]
    fn pull_with_ignore_patterns_applies() {
        // Issue #34 apply/pull equivalent: a remote-only ignored key
        // (`private/secret.md` under user pattern `private/`) must be
        // filtered from the pull plan - no Download row, nothing materialized
        // locally, and no W25 refusal (exit 0). The fixture uses user-only
        // `private/` (not one of the six Obsidian built-ins), so a
        // defaults-only matcher cannot mask a broken user union.
        let dir = TempDir::new("vaultsync-cli-test");
        let store = MemoryStore::new();
        let mut c1 = std::io::Cursor::new(b"a".to_vec());
        store.put_from("a.md", &mut c1, 1, Some(100)).unwrap();
        let mut c2 = std::io::Cursor::new(b"s".to_vec());
        store
            .put_from("private/secret.md", &mut c2, 1, Some(100))
            .unwrap();
        let mut settings = settings_with_ignore(dir.path(), vec!["private/"]);
        settings.store.bucket = "b".to_string(); // store injected below
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_settings_store(
            Command::pull(dir.path().into(), false),
            &settings,
            &store,
            ProgressMode::Off,
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(code, 0, "stderr: {err}");
        assert!(!err.contains("Phase 3"), "W25 refusal must be gone: {err}");
        assert!(
            out.lines().any(|l| l.starts_with("D  a.md")),
            "a.md download planned: {out}"
        );
        assert!(
            !out.contains("private/secret.md"),
            "ignored remote key absent from plan: {out}"
        );
        assert!(dir.join("a.md").exists(), "a.md materialized");
        assert!(
            !dir.join("private/secret.md").exists(),
            "ignored remote key must not be downloaded"
        );
    }

    #[test]
    fn status_with_ignore_patterns_applies() {
        // Issue #34 status half: user patterns prune the status plan (ignored
        // key absent) and the run proceeds with exit 2 (dirty plan - a.md
        // upload pending) and NO Phase 3 warning. The fixture uses user-only
        // `private/` (not one of the six Obsidian built-ins), so a
        // defaults-only matcher cannot mask a broken user union.
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::create_dir_all(dir.join("private")).unwrap();
        std::fs::write(dir.join("a.md"), "hi").unwrap();
        std::fs::write(dir.join("private/secret.md"), "s").unwrap();
        let settings = settings_with_ignore(dir.path(), vec!["private/"]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_settings(
            Command::status(dir.path().into()),
            &settings,
            ProgressMode::Off,
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).unwrap();
        let out = String::from_utf8(out).unwrap();
        // exit 2: the plan is dirty (a.md upload pending), which is the
        // success signal for status - the point is it RUNS (no W25 refuse)
        // with the ignored key pruned.
        assert_eq!(code, 2, "stderr: {err}");
        assert!(!err.contains("Phase 3"), "W25 warning must be gone: {err}");
        assert!(
            out.lines().any(|l| l.starts_with("U  a.md")),
            "a.md planned: {out}"
        );
        assert!(
            !out.contains("private/"),
            "ignored key absent from plan: {out}"
        );
    }

    #[test]
    fn push_delete_does_not_delete_remote_ignored_e2e() {
        // Issue #34 delete invariant, remote half (issue sketch exact name):
        // `push --delete` must never plan a `DeleteRemote` for a remote-only
        // key the ignore patterns drop (the ignored key is absent from the
        // plan entirely), while a non-ignored remote-only key still deletes.
        // Real execution against MemoryStore: the ignored workspace key
        // survives on the store, `orphan.md` is gone. Mutation-checked: an
        // empty `IgnoreSet` at the CLI compile site makes this RED (DR row
        // for the workspace key appears).
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::create_dir_all(dir.join("notes")).unwrap();
        std::fs::write(dir.join("notes/a.md"), "hi").unwrap();
        let store = MemoryStore::new();
        let mut c1 = std::io::Cursor::new(b"{}".to_vec());
        store
            .put_from(".obsidian/workspace.json", &mut c1, 2, Some(100))
            .unwrap();
        let mut c2 = std::io::Cursor::new(b"same".to_vec());
        store.put_from("notes/a.md", &mut c2, 4, Some(100)).unwrap();
        let mut c3 = std::io::Cursor::new(b"orphan".to_vec());
        store.put_from("orphan.md", &mut c3, 6, Some(100)).unwrap();
        let mut settings = no_store_settings(dir.path());
        settings.store.bucket = "b".to_string(); // store injected below
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_settings_store(
            Command::push(dir.path().into(), true),
            &settings,
            &store,
            ProgressMode::Off,
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(code, 0, "stderr: {err}");
        assert!(
            !out.lines()
                .any(|l| l.starts_with("DR .obsidian/workspace.json")),
            "no DeleteRemote for ignored workspace key: {out}"
        );
        assert!(
            out.lines().any(|l| l.starts_with("DR orphan.md")),
            "non-ignored remote-only key still deletes: {out}"
        );
        assert!(
            store.head(".obsidian/workspace.json").is_ok(),
            "ignored remote key survives on the store"
        );
        assert!(store.head("orphan.md").is_err(), "orphan deleted");
        assert!(store.head("notes/a.md").is_ok(), "a.md still present");
    }

    #[test]
    fn pull_delete_does_not_delete_local_ignored() {
        // Issue #34 delete invariant, local half (issue sketch exact name):
        // `pull --delete` must never delete a local-only path the ignore
        // patterns prune (it never enters the plan, so no `DL` row) while the
        // walk still sees everything else. Real execution: `.trash/x.md`
        // survives on disk, `notes/a.md` intact, and a non-ignored local-only
        // `extra.md` is still DeleteLocal'd (PR 41 F1 positive control -
        // the pin cannot pass with delete silently off).
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::create_dir_all(dir.join("notes")).unwrap();
        std::fs::create_dir_all(dir.join(".trash")).unwrap();
        std::fs::write(dir.join("notes/a.md"), "same").unwrap();
        std::fs::write(dir.join(".trash/x.md"), "x").unwrap();
        std::fs::write(dir.join("extra.md"), "extra").unwrap();
        let store = MemoryStore::new();
        let mut c = std::io::Cursor::new(b"same".to_vec());
        store.put_from("notes/a.md", &mut c, 4, Some(100)).unwrap();
        let mut settings = no_store_settings(dir.path());
        settings.store.bucket = "b".to_string(); // store injected below
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_settings_store(
            Command::pull(dir.path().into(), true),
            &settings,
            &store,
            ProgressMode::Off,
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(code, 0, "stderr: {err}");
        assert!(
            !out.lines().any(|l| l.starts_with("DL .trash/x.md")),
            "no DeleteLocal for ignored trash path: {out}"
        );
        assert!(
            dir.join(".trash/x.md").exists(),
            "ignored local file survives the pull --delete"
        );
        assert!(dir.join("notes/a.md").exists(), "a.md intact");
        assert!(
            out.lines().any(|l| l.starts_with("DL extra.md")),
            "non-ignored local-only key must DeleteLocal: {out}"
        );
        assert!(
            !dir.join("extra.md").exists(),
            "non-ignored local-only file must be removed by pull --delete"
        );
    }

    #[test]
    fn status_reports_skipped_ignored() {
        // Issue #34 D-report local half (issue sketch exact name): when the
        // walk prunes/skips > 0 paths by ignore patterns, the CLI always
        // prints the locked count line (not -v-only, no key dump). Fixture
        // counting (issue #32 D-report): each pruned dir counts 1 (`.trash/`,
        // `.git/`), each ignored file counts 1 (`workspace.json`,
        // `.DS_Store`) => exactly 4. RED today: `print_walk_warnings` has no
        // `skipped_ignored` branch.
        let dir = TempDir::new("vaultsync-cli-test");
        write_default_profile_vault(&dir);
        let settings = no_store_settings(dir.path());
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_settings(
            Command::status(dir.path().into()),
            &settings,
            ProgressMode::Off,
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).unwrap();
        let _out = String::from_utf8(out).unwrap();
        assert_eq!(code, 2, "stderr: {err}");
        assert!(
            err.contains("warning: ignored 4 local path(s) by ignore patterns"),
            "exact local ignore count line expected: {err}"
        );
        // the warning is count-only - no per-key dump of ignored names
        assert!(!err.contains("workspace.json"), "no key dump: {err}");
    }

    #[test]
    fn push_reports_remote_ignored_count() {
        // Issue #34 D-report remote half at the CLI: the #33 remote ignore
        // partition's `PlanReport.warnings` entry must surface as a
        // `warning: ...` stderr line - count-only (no key names in that
        // line). Already produced by `ignored_remote_drops_warning` + the
        // dispatch warning loop once W205 wired the real set; this pins the
        // CLI half.
        let dir = TempDir::new("vaultsync-cli-test");
        let store = MemoryStore::new();
        let mut c1 = std::io::Cursor::new(b"{}".to_vec());
        store
            .put_from(".obsidian/workspace.json", &mut c1, 2, Some(100))
            .unwrap();
        let mut c2 = std::io::Cursor::new(b"x".to_vec());
        store
            .put_from(".trash/x.md", &mut c2, 1, Some(100))
            .unwrap();
        let mut settings = no_store_settings(dir.path());
        settings.store.bucket = "b".to_string(); // store injected below
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_settings_store(
            Command::push(dir.path().into(), false),
            &settings,
            &store,
            ProgressMode::Off,
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).unwrap();
        let _out = String::from_utf8(out).unwrap();
        assert_eq!(code, 0, "stderr: {err}");
        // `.trash/` folder marker (MemoryStore lists ancestor folders) + the
        // two keys => 3 dropped.
        assert!(
            err.contains("warning: ignored 3 remote key(s) by ignore patterns"),
            "remote ignore count line expected: {err}"
        );
        assert!(
            !err.contains(".obsidian/workspace.json") && !err.contains(".trash/x.md"),
            "no per-key dump in the warning: {err}"
        );
    }

    #[test]
    fn status_no_local_ignore_warning_when_zero() {
        // Issue #34 D-report N==0: no local ignore line when nothing was
        // skipped by ignore patterns (`profile = "none"` + a clean vault).
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::create_dir_all(dir.join("notes")).unwrap();
        std::fs::write(dir.join("notes/a.md"), "hi").unwrap();
        let settings = settings_profile_none(dir.path(), vec![]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_settings(
            Command::status(dir.path().into()),
            &settings,
            ProgressMode::Off,
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).unwrap();
        let _out = String::from_utf8(out).unwrap();
        assert_eq!(code, 2, "dirty plan; stderr: {err}");
        assert!(
            !err.contains("local path(s) by ignore patterns"),
            "no ignore line when N == 0: {err}"
        );
    }

    #[test]
    fn status_user_patterns_extend_default_profile() {
        // Issue #34 D3-extend: user patterns UNION with the Obsidian
        // built-ins - `private/` (user pattern) and workspace.json (built-in)
        // are both pruned while notes/a.md stays listed.
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::create_dir_all(dir.join("private")).unwrap();
        std::fs::create_dir_all(dir.join(".obsidian")).unwrap();
        std::fs::create_dir_all(dir.join("notes")).unwrap();
        std::fs::write(dir.join("private/secret.md"), "s").unwrap();
        std::fs::write(dir.join(".obsidian/workspace.json"), "{}").unwrap();
        std::fs::write(dir.join("notes/a.md"), "hi").unwrap();
        let settings = settings_with_ignore(dir.path(), vec!["private/"]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_settings(
            Command::status(dir.path().into()),
            &settings,
            ProgressMode::Off,
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(code, 2, "dirty plan; stderr: {err}");
        assert!(
            out.lines().any(|l| l.starts_with("U  notes/a.md")),
            "notes/a.md kept: {out}"
        );
        assert!(!out.contains("private/"), "user pattern pruned: {out}");
        assert!(
            !out.contains("workspace.json"),
            "built-in pattern still pruned: {out}"
        );
    }

    #[test]
    fn check_with_ignore_patterns_does_not_refuse() {
        // Issue #34 W25 retirement + D-wire: `check` with non-empty user
        // ignore patterns must NOT be refused for Phase 3 - it falls through
        // to the store requirement (empty bucket => exit 1, `[store]`
        // message) with no Phase 3 ignore text.
        let dir = TempDir::new("vaultsync-cli-test");
        let settings = settings_with_ignore(dir.path(), vec![".trash/"]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_settings(
            Command::check(),
            &settings,
            ProgressMode::Off,
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).unwrap();
        assert_eq!(code, 1, "store requirement failure; stderr: {err}");
        assert!(!err.contains("Phase 3"), "no W25 refusal: {err}");
        assert!(
            err.to_lowercase().contains("store"),
            "failure must be the store requirement: {err}"
        );
        assert!(
            !String::from_utf8(out).unwrap().contains("check: ok"),
            "must not be a mock green check"
        );
    }

    #[test]
    fn status_absent_ignore_resolve_does_not_warn_phase3() {
        // F3/W196 (PR 38 r1) kept green after the W25 retire (issue #34
        // D-w25-retire): a real TOML-less FileConfig (absent `[ignore]`)
        // resolves to an empty raw user list + the six Obsidian built-ins;
        // status must stay silent (no `[ignore].patterns` / Phase-3 text -
        // the retired W25 strings must not reappear) and produce a normal
        // plan. The D-wire compile site reads `resolved_ignore_patterns`,
        // never the raw user field alone.
        let dir = TempDir::new("vaultsync-cli-test");
        let cfg = crate::config::FileConfig {
            vault_root: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let settings =
            crate::config::resolve_settings(&cfg, &crate::config::EnvSnapshot::default()).unwrap();
        assert!(
            settings.ignore_patterns.is_empty(),
            "raw user field must stay empty for absent [ignore]"
        );
        let expected: Vec<String> = crate::config::OBSIDIAN_DEFAULT_IGNORE_PATTERNS
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(settings.resolved_ignore_patterns, expected);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_settings(
            Command::status(dir.path().into()),
            &settings,
            ProgressMode::Off,
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(code, 0, "stderr: {err}");
        assert!(
            !err.contains("[ignore].patterns"),
            "retired W25 warning string must not reappear: {err}"
        );
        assert!(out.contains("plan:"), "plan produced: {out}");

        // section present but empty (`[ignore]` with no keys) resolves
        // identically (same as config W188).
        let cfg = crate::config::FileConfig {
            vault_root: Some(dir.path().to_path_buf()),
            ignore: Some(crate::config::IgnoreConfig::default()),
            ..Default::default()
        };
        let settings =
            crate::config::resolve_settings(&cfg, &crate::config::EnvSnapshot::default()).unwrap();
        assert!(settings.ignore_patterns.is_empty());
        assert_eq!(settings.resolved_ignore_patterns, expected);
        let mut out2 = Vec::new();
        let mut err2 = Vec::new();
        let code2 = run_with_settings(
            Command::status(dir.path().into()),
            &settings,
            ProgressMode::Off,
            &mut out2,
            &mut err2,
        );
        let err2 = String::from_utf8(err2).unwrap();
        assert_eq!(code2, 0, "stderr: {err2}");
        assert!(
            !err2.contains("[ignore].patterns"),
            "retired W25 warning string must not reappear for an empty section: {err2}"
        );
    }

    #[test]
    fn push_absent_ignore_resolve_does_not_trip_w25() {
        // F3/W196 kept green after the W25 retire (issue #34 D-w25-retire):
        // with absent `[ignore]` (raw user field empty, resolved = Obsidian
        // six), push must not emit the retired Phase-3 refusal - it falls
        // through to the store requirement and fails there (exit 1,
        // `[store]` message) because no bucket is configured.
        let dir = TempDir::new("vaultsync-cli-test");
        let cfg = crate::config::FileConfig {
            vault_root: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let settings =
            crate::config::resolve_settings(&cfg, &crate::config::EnvSnapshot::default()).unwrap();
        assert!(settings.ignore_patterns.is_empty());
        assert!(
            !settings.resolved_ignore_patterns.is_empty(),
            "resolved defaults must be present"
        );
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_settings(
            Command::push(dir.path().into(), false),
            &settings,
            ProgressMode::Off,
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).unwrap();
        assert_eq!(code, 1, "stderr: {err}");
        assert!(
            !err.contains("[ignore].patterns"),
            "retired W25 refusal string must not reappear: {err}"
        );
        assert!(
            err.contains("[store]") || err.contains("store.bucket"),
            "failure must be the store requirement: {err}"
        );
    }

    #[test]
    fn run_silent_on_configured_concurrency() {
        // I20-cli: `[transfer].concurrency` is live (issue 20); an explicitly
        // set value (8) must NOT warn - no "Phase 3"/"concurrency" text on
        // stderr - and the run proceeds. RED today: the W28/M6 warning fires
        // for a divergent explicit value.
        let dir = TempDir::new("vaultsync-cli-test");
        let mut settings = no_store_settings(dir.path());
        settings.concurrency = 8;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_settings(
            Command::status(dir.path().into()),
            &settings,
            ProgressMode::Off,
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).unwrap();
        assert_eq!(code, 0);
        assert!(
            !err.to_lowercase().contains("concurrency") && !err.to_lowercase().contains("phase 3"),
            "concurrency must be live and silent: {err}"
        );
    }

    #[test]
    fn requires_real_store_predicate() {
        // W5 (A-M2/B-L5): push/pull/check need a configured store; status and
        // help/version run against the mock.
        assert!(requires_real_store(&Command::push(
            PathBuf::from("."),
            false
        )));
        assert!(requires_real_store(&Command::pull(
            PathBuf::from("."),
            false
        )));
        assert!(requires_real_store(&Command::check()));
        assert!(!requires_real_store(&Command::status(PathBuf::from("."))));
        assert!(!requires_real_store(&Command::Help));
        assert!(!requires_real_store(&Command::Version));
    }

    #[test]
    fn run_pull_requires_configured_store() {
        // W5 (A-M2/B-L5) dispatch level: pull --delete with no [store] must
        // refuse (exit 1) and leave the temp vault untoched - this locks shut
        // the old hole where pull --delete planned DeleteLocal for every local
        // file and deleted them against the empty mock with exit 0.
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::write(dir.join("victim.md"), "precious").unwrap();
        let settings = no_store_settings(dir.path());
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_settings(
            Command::pull(dir.path().into(), true),
            &settings,
            ProgressMode::Off,
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).unwrap();
        assert_eq!(code, 1);
        assert!(
            err.contains("[store]") || err.to_lowercase().contains("store"),
            "expected store refusal: {err}"
        );
        // no data loss
        assert!(dir.join("victim.md").exists());
        assert_eq!(std::fs::read(dir.join("victim.md")).unwrap(), b"precious");
    }

    #[test]
    fn run_check_requires_configured_store() {
        // W5 (B-L5): check without a bucket must never be a green mock probe.
        let dir = TempDir::new("vaultsync-cli-test");
        let settings = no_store_settings(dir.path());
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_settings(
            Command::check(),
            &settings,
            ProgressMode::Off,
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).unwrap();
        assert_eq!(code, 1);
        assert!(
            err.to_lowercase().contains("store"),
            "expected store refusal: {err}"
        );
        assert!(
            !String::from_utf8(out).unwrap().contains("check: ok"),
            "must not be a mock green check"
        );
    }

    #[test]
    fn run_with_settings_applies_toml_tolerance() {
        // r2-M3 (W92): resolve_settings' tolerance output must reach the
        // plan end to end through the run_with_settings dispatch. Local
        // `a.md` is 4000 ms newer than the remote; with
        // `[transfer].mtime_tolerance_ms = 5000` (resolved from TOML) the
        // row is Skip, and the default Settings (1000) plans an Upload for
        // the same file/remote state. The store is injected via the W92
        // seam (run_with_settings builds its own store internally - S3
        // needing credentials, or an empty mock - so a naive test cannot
        // observe the tolerance). RED: the seam does not exist (compile
        // failure).
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::write(dir.join("a.md"), "same").unwrap();
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_004_000);
        std::fs::File::open(dir.join("a.md"))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(t))
            .unwrap();
        let cfg = crate::config::parse_config_str(
            "[store]\nbucket = \"b\"\n[transfer]\nmtime_tolerance_ms = 5000\n",
        )
        .unwrap();
        let settings =
            crate::config::resolve_settings(&cfg, &crate::config::EnvSnapshot::default()).unwrap();
        assert_eq!(settings.mtime_tolerance_ms, 5000, "TOML tolerance resolved");
        let store = MemoryStore::new();
        let mut cursor = std::io::Cursor::new(b"same".to_vec());
        store
            .put_from("a.md", &mut cursor, 4, Some(1_700_000_000_000))
            .unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_settings_store(
            Command::push(dir.path().into(), false),
            &settings,
            &store,
            ProgressMode::Off,
            &mut out,
            &mut err,
        );
        let out = String::from_utf8(out).unwrap();
        let err = String::from_utf8(err).unwrap();
        assert_eq!(code, 0, "stderr: {err}");
        assert!(
            !out.lines().any(|l| l.starts_with("U  a.md")),
            "expected Skip under TOML tolerance 5000: {out}"
        );
        // control: the default Settings (1000) plans an Upload for the same
        // file/remote state
        let mut defaults = no_store_settings(dir.path());
        defaults.store.bucket = "b".to_string(); // avoid the no-store refusal; the store is injected anyway
        let mut out2 = Vec::new();
        let mut err2 = Vec::new();
        let code2 = run_with_settings_store(
            Command::push(dir.path().into(), false),
            &defaults,
            &store,
            ProgressMode::Off,
            &mut out2,
            &mut err2,
        );
        let out2 = String::from_utf8(out2).unwrap();
        let err2 = String::from_utf8(err2).unwrap();
        assert_eq!(code2, 0, "stderr: {err2}");
        assert!(
            out2.lines().any(|l| l.starts_with("U  a.md")),
            "expected Upload under default tolerance: {out2}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cli_pull_delete_surfaces_cleanup_warning() {
        // r2-M4 (W93): the dispatch loop must print executor report warnings
        // to stderr (src/cli.rs:487). Pull --delete with the empty-dir
        // cleanup failing (parent `a` locked 0o555, mirroring
        // remove_empty_ancestor_dirs_reports_failures) must surface the
        // cleanup warning on stderr. Passes on landing (the loop exists);
        // its teeth are proven by deleting the loop and watching the test
        // fail. Non-fatal: exit stays 0.
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a/b/gone.md"), "bye").unwrap();
        let store = MemoryStore::new(); // empty remote: pull --delete plans DeleteLocal
        // lock the parent `a` (read+traverse, no write) so the file delete
        // works (write is on `a/b`) but remove_dir(a/b) fails EACCES
        std::fs::set_permissions(dir.join("a"), std::fs::Permissions::from_mode(0o555)).unwrap();
        let (code, _out, err) = run(Command::pull(dir.path().into(), true), &store);
        // restore perms so TempDir drop can remove the tree
        std::fs::set_permissions(dir.join("a"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(code, 0, "cleanup warning is non-fatal: {err}");
        assert!(
            err.contains("warning:") && (err.contains("a/b") || err.contains("remove")),
            "cleanup warning missing from stderr: {err}"
        );
    }

    /// A store whose `list` returns an injected advisory warning, to lock
    /// that `Listing.warnings` (H1/W99, the S3 dropped-folder-key channel)
    /// reaches stderr at CLI dispatch.
    struct WarnListStore {
        inner: MemoryStore,
        warning: String,
    }
    impl ObjectStore for WarnListStore {
        fn list(&self, prefix: &str) -> Result<crate::store::Listing, crate::error::Error> {
            let mut listing = self.inner.list(prefix)?;
            listing.warnings.push(self.warning.clone());
            Ok(listing)
        }
        fn head(&self, key: &str) -> Result<crate::entity::Entity, crate::error::Error> {
            self.inner.head(key)
        }
        fn get_to(
            &self,
            key: &str,
            w: &mut dyn std::io::Write,
        ) -> Result<crate::entity::Entity, crate::error::Error> {
            self.inner.get_to(key, w)
        }
        fn put_from(
            &self,
            key: &str,
            r: &mut dyn std::io::Read,
            size: u64,
            mtime: Option<u64>,
        ) -> Result<crate::entity::Entity, crate::error::Error> {
            self.inner.put_from(key, r, size, mtime)
        }
        fn delete(&self, key: &str) -> Result<(), crate::error::Error> {
            self.inner.delete(key)
        }
    }

    #[test]
    fn cli_listing_warning_reaches_stderr() {
        // H1 (W99) mutation check: a `Listing.warnings` entry (the S3
        // dropped-folder-key channel, W70/A-N2) must reach stderr at CLI
        // dispatch as a `warning: ...` line. Teeth proven by deleting the
        // dispatch print and watching this test fail.
        let dir = TempDir::new("vaultsync-cli-test");
        let store = WarnListStore {
            inner: MemoryStore::new(),
            warning: "ignoring remote object odd/ (10 bytes): keys ending in '/' are folder markers; rename it to sync".to_string(),
        };
        let (code, _out, err) = run(Command::status(dir.path().into()), &store);
        assert_eq!(code, 0, "warning must be non-fatal: {err}");
        assert!(
            err.contains("warning: ignoring remote object odd/ (10 bytes)")
                && err.contains("folder markers"),
            "listing warning missing from stderr: {err}"
        );
    }

    #[test]
    fn cli_reserved_namespace_warning_reaches_stderr() {
        // H1 (W99) mutation check: the reserved-namespace warning (W79/r9-L1)
        // must reach stderr at CLI dispatch as a `warning: ...` line instead
        // of an eprintln from library code. Teeth proven by deleting the
        // dispatch print and watching this test fail.
        let dir = TempDir::new("vaultsync-cli-test");
        let store = MemoryStore::new();
        store
            .put_from(
                ".vaultsync-check-1-2-3",
                &mut std::io::Cursor::new(b"x".to_vec()),
                1,
                None,
            )
            .unwrap();
        let (code, _out, err) = run(Command::status(dir.path().into()), &store);
        assert_eq!(code, 0, "warning must be non-fatal: {err}");
        assert!(
            err.contains(
                "warning: ignoring 1 remote object(s) under the reserved vaultsync namespace"
            ) && err.contains(".vaultsync-check-1-2-3"),
            "reserved-namespace warning missing from stderr: {err}"
        );
    }

    #[test]
    fn run_push_executes_uploads_exit_0() {
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::write(dir.join("a.md"), "hi").unwrap();
        let store = MemoryStore::new();
        let (code, out, _) = run(Command::push(dir.path().into(), false), &store);
        assert_eq!(code, 0);
        // store gains the file; stdout has a summary
        assert!(store.head("a.md").is_ok());
        assert!(out.contains("plan:"), "no summary: {out}");
        assert!(out.lines().any(|l| l.starts_with("U  a.md")));
    }

    #[test]
    fn cli_dispatch_uses_configured_tolerance() {
        // W2 (PR2 A-H2/B-M1): the resolved transfer tolerance reaches the plan
        // via the dispatch seam. Local `a.md` is 4000 ms newer than the remote;
        // with tolerance 5000 they are Equal (Skip), with the default 1000 the
        // row would be an Upload.
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::write(dir.join("a.md"), "same").unwrap();
        let local_ms = 1_700_000_004_000u64;
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_millis(local_ms);
        std::fs::File::open(dir.join("a.md"))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(t))
            .unwrap();
        let store = MemoryStore::new();
        let mut cursor = std::io::Cursor::new(b"same".to_vec());
        store
            .put_from("a.md", &mut cursor, 4, Some(1_700_000_000_000))
            .unwrap();
        let (code, out, _) = run_tol(Command::push(dir.path().into(), false), &store, 5000);
        assert_eq!(code, 0);
        assert!(
            !out.lines().any(|l| l.starts_with("U  a.md")),
            "expected Skip under tolerance 5000: {out}"
        );
        // sanity: same command under the default 1000 does plan an Upload
        let (_, out2, _) = run(Command::push(dir.path().into(), false), &store);
        assert!(
            out2.lines().any(|l| l.starts_with("U  a.md")),
            "expected Upload under default tolerance: {out2}"
        );
    }

    #[test]
    fn run_push_conflict_exit_2() {
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::write(dir.join("c.md"), "x").unwrap();
        let mt = std::fs::metadata(dir.join("c.md"))
            .unwrap()
            .modified()
            .unwrap();
        let ms = mt
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let store = MemoryStore::new();
        let mut cursor = std::io::Cursor::new(b"xx".to_vec());
        store.put_from("c.md", &mut cursor, 2, Some(ms)).unwrap();
        let (code, out, _) = run(Command::push(dir.path().into(), false), &store);
        assert_eq!(code, 2, "conflict -> exit 2");
        assert!(
            out.lines().any(|l| l.starts_with("*  c.md")),
            "conflict row: {out}"
        );
        // the conflicting key is NOT transferred (remote unchanged)
        assert_eq!(store.head("c.md").unwrap().size, 2);
    }

    #[test]
    fn run_push_transfer_failure_exit_1() {
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::write(dir.join("a.md"), "hi").unwrap();
        let store = FailPutStore {
            inner: MemoryStore::new(),
        };
        let (code, _, err) = run(Command::push(dir.path().into(), false), &store);
        assert_eq!(code, 1, "transfer failure -> exit 1");
        assert!(err.contains("a.md"), "stderr names key: {err}");
    }

    #[test]
    fn run_push_delete_removes_remote_exit_0() {
        let store = MemoryStore::new();
        let mut cursor = std::io::Cursor::new(b"x".to_vec());
        store
            .put_from("gone.md", &mut cursor, 1, Some(100))
            .unwrap();
        let dir = TempDir::new("vaultsync-cli-test");
        let (code, out, _) = run(Command::push(dir.path().into(), true), &store);
        assert_eq!(code, 0);
        assert!(out.lines().any(|l| l.starts_with("DR gone.md")));
        assert!(matches!(
            store.head("gone.md").unwrap_err(),
            crate::error::Error::NotFound(_)
        ));
    }

    #[test]
    fn run_pull_delete_removes_local_exit_0() {
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::write(dir.join("gone.md"), "bye").unwrap();
        let store = MemoryStore::new();
        let (code, out, _) = run(Command::pull(dir.path().into(), true), &store);
        assert_eq!(code, 0);
        assert!(out.lines().any(|l| l.starts_with("DL gone.md")));
        assert!(!dir.join("gone.md").exists(), "local extra removed");
    }

    #[test]
    fn run_pull_dry_run_mutates_nothing_exit_2() {
        // pull --dry-run with a remote-only key: plan printed (Download =
        // dirty -> exit 2), but nothing written to disk.
        let dir = TempDir::new("vaultsync-cli-test");
        let store = MemoryStore::new();
        let mut cursor = std::io::Cursor::new(b"x".to_vec());
        store.put_from("b.md", &mut cursor, 1, Some(100)).unwrap();
        let cmd = Command::Pull {
            vault: dir.path().into(),
            delete: false,
            dry_run: true,
            force_local: false,
            force_remote: false,
            json: false,
            config: None,
            verbose: 0,
            follow_symlinks: false,
        };
        let (code, out, _) = run(cmd, &store);
        assert_eq!(code, 2);
        assert!(out.contains("plan:"), "plan printed: {out}");
        assert!(out.lines().any(|l| l.starts_with("D  b.md")));
        assert!(!dir.join("b.md").exists(), "dry-run mutates nothing");
    }

    #[test]
    fn run_status_error_exit_1() {
        let (code, _, err) = run(
            Command::status(PathBuf::from("/nonexistent/vaultsync-zzz")),
            &MemoryStore::new(),
        );
        assert_eq!(code, 1);
        assert!(err.contains("error:"));
    }
}
