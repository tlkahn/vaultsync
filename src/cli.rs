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
//! - `--yes` / `--max-delete` / `--concurrency` are rejected as unknown until
//!   Phase 3 (delete-safety rails are Phase 3).
//! - Every parse error includes usage (clap does this natively).

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::builder::ArgAction;
use clap::{Args, CommandFactory, Parser, Subcommand};

use crate::plan::{Mode, PlanOpts};
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
            Some(Commands::Check) => Command::Check { config, verbose },
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

/// Dispatch a command against a store, writing to `out`/`err`. Returns exit code.
/// `tolerance_ms` is the resolved `transfer.mtime_tolerance_ms` threaded into
/// every `PlanOpts` (W2, PR2 A-H2/B-M1).
pub fn run_with_io(
    cmd: Command,
    store: &dyn ObjectStore,
    tolerance_ms: u64,
    out: &mut dyn Write,
    err: &mut dyn Write,
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
        } => match crate::check_store(store) {
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
        },
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
                mtime_tolerance_ms: tolerance_ms,
                ..Default::default()
            };
            let local = crate::local::LocalFs::with_follow(&vault, follow_symlinks);
            match crate::build_plan(&local, store, Mode::Status, &opts) {
                Ok(plan) => {
                    print_walk_warnings(&local, follow_symlinks, err);
                    let _ = write!(out, "{}", crate::format_plan_human_verbose(&plan, verbose));
                    if is_clean(&plan) { 0 } else { 2 }
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
                mtime_tolerance_ms: tolerance_ms,
                delete,
                force_local,
                force_remote,
            };
            let flags = PlanFlags {
                dry_run,
                follow_symlinks,
                verbose,
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
                mtime_tolerance_ms: tolerance_ms,
                delete,
                force_local,
                force_remote,
            };
            let flags = PlanFlags {
                dry_run,
                follow_symlinks,
                verbose,
            };
            dispatch_plan(&vault, store, Mode::Pull, &opts, &flags, out, err)
        }
    }
}

/// Print walk-report warnings to stderr (Slice 9): out-of-vault followed
/// symlink skips, and the default-mode skipped-symlink count hint.
fn print_walk_warnings(local: &crate::local::LocalFs, follow: bool, err: &mut dyn Write) {
    let rep = local.report();
    for w in &rep.warnings {
        let _ = writeln!(err, "warning: {w}");
    }
    if rep.skipped_symlinks > 0 && !follow {
        let _ = writeln!(
            err,
            "warning: skipped {} symlink(s); use --follow-symlinks to include",
            rep.skipped_symlinks
        );
    }
}

/// Push/pull value flags bundled so `dispatch_plan` stays under clippy's
/// 7-argument limit (W18/B-L9) while keeping the dry-run/verbosity/symlink
/// plumbing in one place.
struct PlanFlags {
    dry_run: bool,
    follow_symlinks: bool,
    verbose: u8,
}

/// Build a plan and dispatch push/pull execution.
///
/// Exit codes (P1r-stub-exit, retired in Slice 6): `0` all selected actions
/// succeeded and no conflict rows; `2` the plan contained any Conflict rows
/// (non-conflict actions still execute); `1` any transfer/fatal error.
///
/// With `--dry-run`: print the plan, mutate nothing, exit like status
/// (2 if dirty/conflicts, else 0).
fn dispatch_plan(
    vault: &PathBuf,
    store: &dyn ObjectStore,
    mode: Mode,
    opts: &PlanOpts,
    flags: &PlanFlags,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let local = crate::local::LocalFs::with_follow(vault, flags.follow_symlinks);
    match crate::build_plan(&local, store, mode, opts) {
        Ok(plan) => {
            print_walk_warnings(&local, flags.follow_symlinks, err);
            let _ = write!(
                out,
                "{}",
                crate::format_plan_human_verbose(&plan, flags.verbose)
            );
            if flags.dry_run {
                if is_clean(&plan) { 0 } else { 2 }
            } else {
                let report = crate::exec::execute_plan(&local, store, &plan, mode, opts);
                for w in &report.warnings {
                    let _ = writeln!(err, "warning: {w}");
                }
                for f in &report.failed {
                    let _ = writeln!(err, "error: {}\n  {}", f.key, f.message);
                }
                if !report.failed.is_empty() {
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
        | Command::Check { config, .. } => config.as_deref(),
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
        other => other,
    }
}

/// True when a command mutates or probes a real store and therefore must not
/// silently run (or delete) against the in-memory mock (A-M2/B-L5, W5).
fn requires_real_store(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::Push { .. } | Command::Pull { .. } | Command::Check { .. }
    )
}

/// Stable command name used in the no-store refusal message.
fn command_name(cmd: &Command) -> &'static str {
    match cmd {
        Command::Push { .. } => "push",
        Command::Pull { .. } => "pull",
        Command::Check { .. } => "check",
        _ => "this command",
    }
}

/// Dispatch a fully-resolved command + settings. W5 (A-M2/B-L5): push/pull/
/// check refuse loudly when no `[store]` is configured (the in-memory mock is
/// `status`-only); `status` keeps the mock for offline play.
fn run_with_settings(
    cmd: Command,
    settings: &crate::config::Settings,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    // W28/M6: `[transfer].concurrency` is a Phase 3 feature (inert until the
    // pool exists). An explicitly-set value that DIVERGES from the default is
    // warned every run; an explicit copy of the default is silent (R5-M3 - it
    // is behaviorally indistinguishable from omitting the key, so the
    // documented example config must not warn on every run).
    if settings.concurrency_explicitly_set
        && settings.concurrency != crate::config::DEFAULT_CONCURRENCY
    {
        let _ = writeln!(
            err,
            "warning: [transfer].concurrency = {} is a Phase 3 feature and is not yet applied; transfers stay sequential",
            settings.concurrency
        );
    }
    // W25/M3: `[ignore].patterns` is a Phase 3 feature (parsed but not yet
    // applied). A mutating command that would silently not apply it must
    // refuse loudly; `status` (read-only) warns and proceeds with the plan.
    if !settings.ignore_patterns.is_empty() {
        match &cmd {
            Command::Push { .. } | Command::Pull { .. } | Command::Check { .. } => {
                let _ = writeln!(
                    err,
                    "error: [ignore].patterns is a Phase 3 feature and is not yet applied; refusing {} ({:?} ignored). Remove the [ignore] section or use `status` to preview.",
                    command_name(&cmd),
                    settings.ignore_patterns
                );
                return 1;
            }
            _ => {
                let _ = writeln!(
                    err,
                    "warning: [ignore].patterns is a Phase 3 feature and is not yet applied ({:?} ignored)",
                    settings.ignore_patterns
                );
            }
        }
    }
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

    // Build the store: a real `[store]` bucket -> S3Store; otherwise the mock.
    let store: Box<dyn ObjectStore> = if settings.store.bucket.is_empty() {
        Box::new(MemoryStore::new())
    } else {
        match crate::store::s3::S3Store::new(&settings.store) {
            Ok(s) => Box::new(s),
            Err(e) => {
                let _ = writeln!(err, "error: {e}");
                return 1;
            }
        }
    };
    run_with_io(cmd, store.as_ref(), settings.mtime_tolerance_ms, out, err)
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
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();

    let cmd = match parse_args(&args) {
        Ok(c) => c,
        Err(msg) => {
            let _ = writeln!(err, "{msg}");
            return 1;
        }
    };

    // help/version need no config and must not fail on a bad config file.
    if matches!(cmd, Command::Help | Command::Version) {
        // help/version need no tolerance; the value is unused for these.
        return run_with_io(cmd, &MemoryStore::new(), 0, &mut out, &mut err);
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
        match crate::config::resolve_settings(&cfg, &crate::config::Cli::default(), &envsnap) {
            Ok(s) => s,
            Err(e) => {
                let _ = writeln!(err, "error: {e}");
                return 1;
            }
        }
    };
    run_with_settings(cmd, &settings, &mut out, &mut err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::mock::MemoryStore;
    use crate::testutil::TempDir;

    fn a() -> Vec<String> {
        vec!["vaultsync".to_string()]
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
        // `--yes` / `--max-delete` / `--concurrency` are Phase 3 rails; until
        // then they are unknown flags.
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

    fn run(cmd: Command, store: &dyn ObjectStore) -> (i32, String, String) {
        run_tol(cmd, store, crate::config::DEFAULT_MTIME_TOLERANCE_MS)
    }

    fn run_tol(cmd: Command, store: &dyn ObjectStore, tolerance_ms: u64) -> (i32, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_io(cmd, store, tolerance_ms, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
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

    /// A store whose `put_from` always fails, to inject a transfer failure.
    struct FailPutStore {
        inner: MemoryStore,
    }
    impl ObjectStore for FailPutStore {
        fn list(&self, prefix: &str) -> Result<Vec<crate::entity::Entity>, crate::error::Error> {
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

    #[test]
    fn run_check_exit_0_no_mock_label() {
        let (code, out, _) = run(Command::check(), &MemoryStore::new());
        assert_eq!(code, 0);
        assert!(out.contains("check: ok"));
        assert!(!out.contains("(mock)"), "(mock) marker removed: {out}");
    }

    /// A store whose probe put is denied -> actionable failure path.
    struct CheckFailStore {
        inner: MemoryStore,
    }
    impl ObjectStore for CheckFailStore {
        fn list(&self, prefix: &str) -> Result<Vec<crate::entity::Entity>, crate::error::Error> {
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
        crate::config::Settings {
            vault_root: vault.to_path_buf(),
            store: crate::config::StoreSettings {
                bucket: String::new(),
                region: None,
                endpoint: None,
                prefix: String::new(),
                path_style: false,
            },
            mtime_tolerance_ms: 1000,
            concurrency: 4,
            ignore_patterns: Vec::new(),
            concurrency_explicitly_set: false,
        }
    }

    fn settings_with_ignore(
        vault: &std::path::Path,
        patterns: Vec<&str>,
    ) -> crate::config::Settings {
        let mut s = no_store_settings(vault);
        s.ignore_patterns = patterns.iter().map(|s| s.to_string()).collect();
        s
    }

    #[test]
    fn push_with_ignore_patterns_errors_loudly() {
        // W25/M3: `[ignore].patterns` is a Phase 3 feature; a mutating command
        // that would silently not apply it must refuse loudly, naming the key
        // and the phase.
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::write(dir.join("a.md"), "hi").unwrap();
        let settings = settings_with_ignore(dir.path(), vec![".trash/"]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_settings(
            Command::push(dir.path().into(), false),
            &settings,
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).unwrap();
        assert_eq!(code, 1);
        assert!(
            err.to_lowercase().contains("ignore") && err.contains("Phase 3"),
            "expected ignore/Phase-3 refusal: {err}"
        );
    }

    #[test]
    fn pull_with_ignore_patterns_errors_loudly() {
        let dir = TempDir::new("vaultsync-cli-test");
        let settings = settings_with_ignore(dir.path(), vec![".trash/"]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_settings(
            Command::pull(dir.path().into(), false),
            &settings,
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).unwrap();
        assert_eq!(code, 1);
        assert!(
            err.to_lowercase().contains("ignore") && err.contains("Phase 3"),
            "expected ignore/Phase-3 refusal: {err}"
        );
    }

    #[test]
    fn status_with_ignore_patterns_warns_but_runs() {
        // W25/M3: `status` is read-only, so ignore patterns warn on stderr but
        // the plan is still produced (exit 0 on a clean vault).
        let dir = TempDir::new("vaultsync-cli-test");
        let settings = settings_with_ignore(dir.path(), vec![".trash/"]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_settings(
            Command::status(dir.path().into()),
            &settings,
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(code, 0);
        assert!(
            err.to_lowercase().contains("ignore") && err.contains("Phase 3"),
            "expected ignore/Phase-3 warning: {err}"
        );
        assert!(out.contains("plan:"), "plan produced: {out}");
    }

    #[test]
    fn run_does_not_warn_on_explicit_default_concurrency() {
        // R5-M3: copying `concurrency = 4` (== DEFAULT_CONCURRENCY) into the
        // config is behaviorally indistinguishable from omitting the key, so
        // it must be silent - otherwise the documented example warns on every
        // run including `status`. Fails today (warns on any explicitly-set
        // value regardless of whether it diverges).
        let dir = TempDir::new("vaultsync-cli-test");
        let mut settings = no_store_settings(dir.path());
        settings.concurrency_explicitly_set = true;
        settings.concurrency = crate::config::DEFAULT_CONCURRENCY;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_settings(
            Command::status(dir.path().into()),
            &settings,
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).unwrap();
        assert_eq!(code, 0);
        assert!(
            !err.to_lowercase().contains("concurrency"),
            "explicit-default concurrency warned: {err}"
        );
    }

    #[test]
    fn run_warns_on_configured_concurrency() {
        // W28/M6 amended for R5-M3: an explicitly-set value that DIVERGES from
        // the default (`concurrency = 8`) is inert until Phase 3 (the pool
        // does not exist); warn on stderr every run, exit 0. An explicit copy
        // of the default stays silent (see
        // run_does_not_warn_on_explicit_default_concurrency).
        let dir = TempDir::new("vaultsync-cli-test");
        let mut settings = no_store_settings(dir.path());
        settings.concurrency_explicitly_set = true;
        settings.concurrency = 8;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_settings(
            Command::status(dir.path().into()),
            &settings,
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).unwrap();
        assert_eq!(code, 0);
        assert!(
            err.to_lowercase().contains("concurrency") && err.to_lowercase().contains("phase 3"),
            "expected inert-concurrency warning: {err}"
        );
    }

    #[test]
    fn run_does_not_warn_without_configured_concurrency() {
        // W28/M6: only an explicitly-set value warns; the default must not.
        let dir = TempDir::new("vaultsync-cli-test");
        let settings = no_store_settings(dir.path()); // flag false
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_settings(
            Command::status(dir.path().into()),
            &settings,
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).unwrap();
        assert_eq!(code, 0);
        assert!(
            !err.to_lowercase().contains("concurrency"),
            "spurious concurrency warning: {err}"
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
        let code = run_with_settings(Command::check(), &settings, &mut out, &mut err);
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
