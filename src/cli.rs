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
use std::path::PathBuf;

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
    disable_help_subcommand = true
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
pub fn run_with_io(
    cmd: Command,
    store: &dyn ObjectStore,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    match cmd {
        Command::Version => {
            let _ = writeln!(out, "vaultsync {}", crate::version());
            0
        }
        Command::Help => {
            let _ = write!(out, "{}\n", Cli::command().render_help());
            0
        }
        Command::Check { config: _c, verbose: _v } => {
            let _ = writeln!(out, "check: ok (mock)");
            0
        }
        Command::Status {
            vault,
            json,
            config: _c,
            verbose: _v,
        } => {
            if json {
                return reject_json(err);
            }
            let opts = PlanOpts::default();
            match crate::status_with_store(&vault, store, &opts) {
                Ok(plan) => {
                    let _ = write!(out, "{}", crate::format_plan_human(&plan));
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
            verbose: _v,
        } => {
            if json {
                return reject_json(err);
            }
            let _ = dry_run;
            let opts = PlanOpts {
                delete,
                force_local,
                force_remote,
                ..Default::default()
            };
            dispatch_plan_stub(&vault, store, Mode::Push, &opts, out, err)
        }
        Command::Pull {
            vault,
            delete,
            dry_run,
            force_local,
            force_remote,
            json,
            config: _c,
            verbose: _v,
        } => {
            if json {
                return reject_json(err);
            }
            let _ = dry_run;
            let opts = PlanOpts {
                delete,
                force_local,
                force_remote,
                ..Default::default()
            };
            dispatch_plan_stub(&vault, store, Mode::Pull, &opts, out, err)
        }
    }
}

fn dispatch_plan_stub(
    vault: &PathBuf,
    store: &dyn ObjectStore,
    mode: Mode,
    opts: &PlanOpts,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let local = crate::local::LocalFs::new(vault);
    match crate::build_plan(&local, store, mode, opts) {
        Ok(plan) => {
            let _ = writeln!(out, "dry-run (phase 1 stub)");
            let _ = write!(out, "{}", crate::format_plan_human(&plan));
            0
        }
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            1
        }
    }
}

/// Entry point used by `main`: args from env, mock store, real stdout/stderr.
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
    let store = MemoryStore::new();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    match parse_args(&args) {
        Ok(cmd) => run_with_io(cmd, &store, &mut out, &mut err),
        Err(msg) => {
            let _ = writeln!(err, "{msg}");
            1
        }
    }
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
        assert_eq!(parse_args(&args).unwrap(), Command::status(PathBuf::from(".")));
    }

    #[test]
    fn parse_global_vault_before_subcommand() {
        // Global flags accepted before the subcommand (clap global=true).
        let mut args = a();
        args.push("--vault".into());
        args.push("/v".into());
        args.push("status".into());
        assert_eq!(parse_args(&args).unwrap(), Command::status(PathBuf::from("/v")));
    }

    #[test]
    fn parse_vault_equals_form() {
        // `--vault=<path>` escape hatch (P1r5).
        let mut args = a();
        args.push("status".into());
        args.push("--vault=/v".into());
        assert_eq!(parse_args(&args).unwrap(), Command::status(PathBuf::from("/v")));
    }

    #[test]
    fn parse_vault_dash_name_via_equals() {
        // A vault literally named `-foo` is reachable via the equals form.
        let mut args = a();
        args.push("status".into());
        args.push("--vault=-foo".into());
        assert_eq!(parse_args(&args).unwrap(), Command::status(PathBuf::from("-foo")));
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
        assert!(err.contains(&expected_debug), "offending bytes not shown: {err}");
    }

    #[test]
    fn cli_args_valid_utf8_roundtrip() {
        let args = os_args_to_strings(vec!["vaultsync".into(), "status".into()]).unwrap();
        assert_eq!(parse_args(&args).unwrap(), Command::status(PathBuf::from(".")));
    }

    // --- dispatch ---

    fn run(cmd: Command, store: &MemoryStore) -> (i32, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_io(cmd, store, &mut out, &mut err);
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
        };
        let (code, _, err) = run(cmd, &MemoryStore::new());
        assert_eq!(code, 1);
        assert!(err.contains("--json"), "err: {err}");
    }

    #[test]
    fn run_check_stub_exit_0() {
        let (code, out, _) = run(Command::check(), &MemoryStore::new());
        assert_eq!(code, 0);
        assert!(out.contains("check: ok (mock)"));
    }

    #[test]
    fn run_push_stub_conflict_exit_0_placeholder() {
        // Characterization lock (P1r6 / L2): the Phase 1 dry-run stub returns
        // exit 0 even with a conflict. Phase 2 Slice 6 defines real exit codes.
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
        assert_eq!(code, 0);
        assert!(out.lines().any(|l| l.starts_with("*  c.md")), "conflict row missing: {out}");
    }

    #[test]
    fn run_push_stub_prints_plan_no_store_mutation() {
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::write(dir.join("a.md"), "hi").unwrap();
        let store = MemoryStore::new();
        let (code, out, _) = run(Command::push(dir.path().into(), false), &store);
        assert_eq!(code, 0);
        assert!(out.contains("dry-run (phase 1 stub)"));
        assert!(out.lines().any(|l| l.starts_with("U  a.md")));
        assert!(store.list("").unwrap().is_empty());
    }

    #[test]
    fn run_push_delete_stub_prints_delete_remote() {
        let store = MemoryStore::new();
        let mut cursor = std::io::Cursor::new(b"x".to_vec());
        store.put_from("gone.md", &mut cursor, 1, Some(100)).unwrap();
        let dir = TempDir::new("vaultsync-cli-test");
        let (code, out, _) = run(Command::push(dir.path().into(), true), &store);
        assert_eq!(code, 0);
        assert!(out.lines().any(|l| l.starts_with("DR gone.md")));
    }

    #[test]
    fn run_pull_delete_stub_prints_delete_local() {
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::write(dir.join("gone.md"), "bye").unwrap();
        let store = MemoryStore::new();
        let (code, out, _) = run(Command::pull(dir.path().into(), true), &store);
        assert_eq!(code, 0);
        assert!(out.lines().any(|l| l.starts_with("DL gone.md")));
        assert!(dir.join("gone.md").exists());
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
