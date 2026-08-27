//! CLI argument parsing and command dispatch.
//!
//! Hand-rolled argv over `&[String]` (no `clap` until flags grow in Phase 2+).
//! `run_with_io` threads `&mut dyn Write` so dispatch is testable without
//! spawning the binary.

use std::io::Write;
use std::path::PathBuf;

use crate::plan::{Mode, PlanOpts};
use crate::store::ObjectStore;
use crate::store::mock::MemoryStore;

/// Parsed top-level command.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Status { vault: PathBuf },
    Push { vault: PathBuf, delete: bool },
    Pull { vault: PathBuf, delete: bool },
    Check,
    Version,
    Help,
}

const USAGE: &str = "usage: vaultsync <command> [options]\n\ncommands:\n  status [--vault <path>]         show plan against mock store\n  push [--vault <path>] [--delete] print push plan (dry-run stub)\n  pull [--vault <path>] [--delete] print pull plan (dry-run stub)\n  check                           mock connectivity stub\n  version                         print version\n  help | --help | -h              show this help";

/// Parse argv (including the program name at `args[0]`).
pub fn parse_args(args: &[String]) -> Result<Command, String> {
    if args.is_empty() {
        return Ok(Command::Help);
    }
    let rest = &args[1..];
    if rest.is_empty() {
        return Ok(Command::Help);
    }

    match rest[0].as_str() {
        "version" => Ok(Command::Version),
        "help" | "--help" | "-h" => Ok(Command::Help),
        "check" => Ok(Command::Check),
        "status" => parse_vault_cmd(&rest[1..], None),
        "push" => parse_vault_cmd(&rest[1..], Some(Mode::Push)),
        "pull" => parse_vault_cmd(&rest[1..], Some(Mode::Pull)),
        other => Err(format!("unknown command: {other}\n{USAGE}")),
    }
}

/// Shared flag parser for status/push/pull.
fn parse_vault_cmd(tail: &[String], mode: Option<Mode>) -> Result<Command, String> {
    let mut vault = PathBuf::from(".");
    let mut delete = false;
    let mut i = 0;
    while i < tail.len() {
        match tail[i].as_str() {
            "--vault" => {
                i += 1;
                if i >= tail.len() {
                    return Err("--vault requires a path argument".to_string());
                }
                vault = PathBuf::from(&tail[i]);
            }
            "--delete" => delete = true,
            other => {
                return Err(format!(
                    "unknown flag for {}: {other}\n{USAGE}",
                    mode.map_or("status", |m| match m {
                        Mode::Status => "status",
                        Mode::Push => "push",
                        Mode::Pull => "pull",
                    })
                ));
            }
        }
        i += 1;
    }

    match mode {
        None => Ok(Command::Status { vault }),
        Some(Mode::Push) => Ok(Command::Push { vault, delete }),
        Some(Mode::Pull) => Ok(Command::Pull { vault, delete }),
        Some(Mode::Status) => unreachable!(),
    }
}

fn is_clean(p: &crate::plan::Plan) -> bool {
    p.stats.upload == 0
        && p.stats.download == 0
        && p.stats.delete_local == 0
        && p.stats.delete_remote == 0
        && p.stats.conflict == 0
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
            let _ = writeln!(out, "{USAGE}");
            0
        }
        Command::Check => {
            let _ = writeln!(out, "check: ok (mock)");
            0
        }
        Command::Status { vault } => {
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
        Command::Push { vault, delete } => {
            let opts = PlanOpts {
                delete,
                ..Default::default()
            };
            dispatch_plan_stub(&vault, store, Mode::Push, &opts, out, err)
        }
        Command::Pull { vault, delete } => {
            let opts = PlanOpts {
                delete,
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
    let args: Vec<String> = std::env::args().collect();
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
    use crate::plan::PlanOpts;
    use crate::store::mock::MemoryStore;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn a() -> Vec<String> {
        vec!["vaultsync".to_string()]
    }

    fn temp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("vaultsync-cli-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    // --- parse ---

    #[test]
    fn parse_version() {
        let mut args = a();
        args.push("version".into());
        assert_eq!(parse_args(&args).unwrap(), Command::Version);
    }

    #[test]
    fn parse_help() {
        let mut args = a();
        args.push("--help".into());
        assert_eq!(parse_args(&args).unwrap(), Command::Help);
    }

    #[test]
    fn parse_status_default_vault() {
        let mut args = a();
        args.push("status".into());
        assert_eq!(
            parse_args(&args).unwrap(),
            Command::Status {
                vault: PathBuf::from(".")
            }
        );
    }

    #[test]
    fn parse_status_vault_flag() {
        let mut args = a();
        args.push("status".into());
        args.push("--vault".into());
        args.push("/tmp/v".into());
        assert_eq!(
            parse_args(&args).unwrap(),
            Command::Status {
                vault: PathBuf::from("/tmp/v")
            }
        );
    }

    #[test]
    fn parse_push_delete() {
        let mut args = a();
        args.push("push".into());
        args.push("--delete".into());
        assert_eq!(
            parse_args(&args).unwrap(),
            Command::Push {
                vault: PathBuf::from("."),
                delete: true
            }
        );
    }

    #[test]
    fn parse_pull() {
        let mut args = a();
        args.push("pull".into());
        assert_eq!(
            parse_args(&args).unwrap(),
            Command::Pull {
                vault: PathBuf::from("."),
                delete: false
            }
        );
    }

    #[test]
    fn parse_check() {
        let mut args = a();
        args.push("check".into());
        assert_eq!(parse_args(&args).unwrap(), Command::Check);
    }

    #[test]
    fn parse_unknown_command() {
        let mut args = a();
        args.push("foo".into());
        assert!(parse_args(&args).unwrap_err().contains("unknown command"));
    }

    #[test]
    fn parse_unknown_flag() {
        let mut args = a();
        args.push("status".into());
        args.push("--json".into());
        assert!(parse_args(&args).unwrap_err().contains("unknown flag"));
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
    fn run_status_clean_exit_0() {
        let dir = temp_dir();
        let (code, out, _) = run(Command::Status { vault: dir }, &MemoryStore::new());
        assert_eq!(code, 0);
        assert!(out.contains("plan:"));
    }

    #[test]
    fn run_status_dirty_exit_2() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.md"), "hi").unwrap();
        let (code, out, _) = run(Command::Status { vault: dir }, &MemoryStore::new());
        assert_eq!(code, 2);
        assert!(out.lines().any(|l| l.starts_with("U  a.md")));
    }

    #[test]
    fn run_check_stub_exit_0() {
        let (code, out, _) = run(Command::Check, &MemoryStore::new());
        assert_eq!(code, 0);
        assert!(out.contains("check: ok (mock)"));
    }

    #[test]
    fn run_push_stub_prints_plan_no_store_mutation() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.md"), "hi").unwrap();
        let store = MemoryStore::new();
        let (code, out, _) = run(
            Command::Push {
                vault: dir,
                delete: false,
            },
            &store,
        );
        assert_eq!(code, 0);
        assert!(out.contains("dry-run (phase 1 stub)"));
        assert!(out.lines().any(|l| l.starts_with("U  a.md")));
        // store must remain empty
        assert!(store.list("").unwrap().is_empty());
    }

    #[test]
    fn run_push_delete_stub_prints_delete_remote() {
        let store = MemoryStore::new();
        let mut cursor = std::io::Cursor::new(b"x".to_vec());
        store
            .put_from("gone.md", &mut cursor, 1, Some(100))
            .unwrap();
        let dir = temp_dir();
        let (code, out, _) = run(
            Command::Push {
                vault: dir,
                delete: true,
            },
            &store,
        );
        assert_eq!(code, 0);
        assert!(out.lines().any(|l| l.starts_with("DR gone.md")));
    }

    #[test]
    fn run_status_error_exit_1() {
        let (code, _, err) = run(
            Command::Status {
                vault: PathBuf::from("/nonexistent/vaultsync-zzz"),
            },
            &MemoryStore::new(),
        );
        assert_eq!(code, 1);
        assert!(err.contains("error:"));
    }

    // silence unused-PlanOpts import in non-Status paths if any
    #[allow(dead_code)]
    fn _opts() -> PlanOpts {
        PlanOpts::default()
    }
}
