//! CLI argument parsing and command dispatch.
//!
//! Hand-rolled argv over `&[String]` (no `clap` until flags grow in Phase 2+).
//! `run_with_io` threads `&mut dyn Write` so dispatch is testable without
//! spawning the binary.

use std::ffi::OsString;
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

/// Convert `args_os()` output into `String`s, failing loud on non-UTF8
/// arguments instead of letting `std::env::args()` panic (M1). The parser
/// below works on `&[String]`; this seam is the only place OsString meets it.
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
    if args.is_empty() {
        return Ok(Command::Help);
    }
    let rest = &args[1..];
    if rest.is_empty() {
        return Ok(Command::Help);
    }

    match rest[0].as_str() {
        "version" => {
            reject_trailing(&rest[1..], "version")?;
            Ok(Command::Version)
        }
        "help" | "--help" | "-h" => {
            reject_trailing(&rest[1..], "help")?;
            Ok(Command::Help)
        }
        "check" => {
            reject_trailing(&rest[1..], "check")?;
            Ok(Command::Check)
        }
        "status" => parse_vault_cmd(&rest[1..], Mode::Status),
        "push" => parse_vault_cmd(&rest[1..], Mode::Push),
        "pull" => parse_vault_cmd(&rest[1..], Mode::Pull),
        other => Err(format!("unknown command: {other}\n{USAGE}")),
    }
}

/// Reject trailing tokens after a command that takes no arguments.
fn reject_trailing(rest_tail: &[String], cmd: &str) -> Result<(), String> {
    if let Some(tok) = rest_tail.first() {
        return Err(format!("unexpected argument for {cmd}: {tok}\n{USAGE}"));
    }
    Ok(())
}

/// Shared flag parser for status/push/pull. The mode is known at the call
/// site, so it is passed directly (no `Option<Mode>` + `unreachable!()` arm).
fn parse_vault_cmd(tail: &[String], mode: Mode) -> Result<Command, String> {
    let mut vault = PathBuf::from(".");
    let mut vault_seen = false;
    let mut delete = false;
    let mut delete_seen = false;
    let mut i = 0;
    while i < tail.len() {
        match tail[i].as_str() {
            "--vault" => {
                if vault_seen {
                    return Err(format!("repeated --vault flag\n{USAGE}"));
                }
                vault_seen = true;
                i += 1;
                if i >= tail.len() {
                    return Err(format!("--vault requires a path argument\n{USAGE}"));
                }
                let tok = &tail[i];
                if tok.is_empty() || tok.starts_with('-') {
                    return Err(format!(
                        "--vault requires a path argument, got {tok:?}\n{USAGE}"
                    ));
                }
                vault = PathBuf::from(tok);
            }
            "--delete" => {
                if mode == Mode::Status {
                    // Status mode never emits Delete rows; accepting then silently
                    // discarding the flag is worse than rejecting it outright.
                    return Err(format!(
                        "--delete is only valid for push/pull, not status\n{USAGE}"
                    ));
                }
                if delete_seen {
                    // Repeated `--delete` is a parse error (fail loud over
                    // silent idempotence), matching repeated `--vault`
                    // (P1r4-vault-value). USAGE per the uniform rule (P1r7-parse-usage).
                    return Err(format!("repeated --delete flag\n{USAGE}"));
                }
                delete_seen = true;
                delete = true;
            }
            other => {
                return Err(format!(
                    "unknown flag for {}: {other}\n{USAGE}",
                    match mode {
                        Mode::Status => "status",
                        Mode::Push => "push",
                        Mode::Pull => "pull",
                    }
                ));
            }
        }
        i += 1;
    }

    match mode {
        Mode::Status => Ok(Command::Status { vault }),
        Mode::Push => Ok(Command::Push { vault, delete }),
        Mode::Pull => Ok(Command::Pull { vault, delete }),
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
    let args: Vec<OsString> = std::env::args_os().collect();
    let args = match os_args_to_strings(args) {
        Ok(a) => a,
        Err(msg) => {
            // Fail loud on non-UTF8 argv (M1): a clear `error:` line and
            // exit 1, consistent with parse-error handling below - never the
            // `args()` panic (exit 101).
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
    fn parse_version_rejects_trailing_token() {
        let mut args = a();
        args.push("version".into());
        args.push("--json".into());
        let msg = parse_args(&args).unwrap_err();
        assert!(
            msg.contains("unexpected") || msg.contains("unknown"),
            "clear hint missing: {msg}"
        );
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
    fn parse_vault_rejects_flag_like_value() {
        // `--vault` must not swallow a following flag-looking token as its
        // path value (R3/B2: `--delete` was silently consumed as a path).
        let mut args = a();
        args.push("push".into());
        args.push("--vault".into());
        args.push("--delete".into());
        let msg = parse_args(&args).unwrap_err();
        assert!(msg.contains("--vault"), "msg: {msg}");

        let mut args = a();
        args.push("status".into());
        args.push("--vault".into());
        args.push("-o".into());
        assert!(parse_args(&args).is_err());

        // missing value still errors
        let mut args = a();
        args.push("push".into());
        args.push("--vault".into());
        assert!(parse_args(&args).is_err());
    }

    #[test]
    fn parse_repeated_vault_flag_errors() {
        // Repeated `--vault` is a parse error (fail loud over silent
        // last-wins).
        let mut args = a();
        args.push("push".into());
        args.push("--vault".into());
        args.push("/a".into());
        args.push("--vault".into());
        args.push("/b".into());
        let msg = parse_args(&args).unwrap_err();
        assert!(
            msg.contains("repeated") || msg.contains("duplicate"),
            "msg: {msg}"
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
    fn parse_status_rejects_delete_flag() {
        let mut args = a();
        args.push("status".into());
        args.push("--delete".into());
        let msg = parse_args(&args).unwrap_err();
        assert!(msg.contains("--delete"), "msg: {msg}");
        assert!(
            msg.contains("push") || msg.contains("pull") || msg.contains("not valid for status"),
            "hint missing: {msg}"
        );
    }

    #[test]
    fn parse_repeated_delete_flag_errors() {
        // Repeated `--delete` is a parse error (fail loud over silent
        // idempotence), matching repeated `--vault` (P1r4-vault-value).
        let mut args = a();
        args.push("push".into());
        args.push("--delete".into());
        args.push("--delete".into());
        let msg = parse_args(&args).unwrap_err();
        assert!(msg.contains("repeated"), "msg: {msg}");
        assert!(msg.contains("--delete"), "msg: {msg}");
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
    fn parse_push_vault_and_delete() {
        let mut args = a();
        args.push("push".into());
        args.push("--vault".into());
        args.push("/tmp/v".into());
        args.push("--delete".into());
        assert_eq!(
            parse_args(&args).unwrap(),
            Command::Push {
                vault: PathBuf::from("/tmp/v"),
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
    fn parse_check_rejects_trailing_token() {
        let mut args = a();
        args.push("check".into());
        args.push("bogus".into());
        let msg = parse_args(&args).unwrap_err();
        assert!(
            msg.contains("unexpected") || msg.contains("unknown"),
            "clear hint missing: {msg}"
        );
    }

    #[test]
    fn parse_help_rejects_trailing_token() {
        let mut args = a();
        args.push("help".into());
        args.push("extra".into());
        let msg = parse_args(&args).unwrap_err();
        assert!(
            msg.contains("unexpected") || msg.contains("unknown"),
            "clear hint missing: {msg}"
        );
    }

    #[test]
    fn parse_errors_always_include_usage() {
        // Uniform rule (P1r7-parse-usage): every `parse_args` error message
        // ends with the USAGE block, matching the unknown-command / unknown-
        // flag / trailing-token precedent.
        let cases: Vec<Vec<&str>> = vec![
            vec!["vaultsync", "foo"],                // unknown command
            vec!["vaultsync", "status", "--json"],   // unknown flag
            vec!["vaultsync", "status", "extra"],    // positional
            vec!["vaultsync", "status", "--delete"], // status delete
            vec!["vaultsync", "version", "--json"],  // trailing token
            vec!["vaultsync", "push", "--vault", "/a", "--vault", "/b"], // repeated vault
            vec!["vaultsync", "push", "--vault"],    // missing value
            vec!["vaultsync", "push", "--delete", "--delete"], // repeated delete
        ];
        for args in cases {
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let msg = parse_args(&args).unwrap_err();
            assert!(
                msg.contains("usage: vaultsync"),
                "case {args:?} missing usages: {msg:?}"
            );
        }
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

    // --- argv conversion seam ---

    #[cfg(unix)]
    #[test]
    fn cli_args_reject_non_utf8() {
        // A non-UTF8 argument must fail the conversion seam with a clear
        // message (M1): `std::env::args()` panics on such argv; the seam is
        // the tested boundary between `args_os()` and the String-based parser.
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
        // Happy path through the seam (M1): valid argv converts and feeds the
        // existing parser unchanged. Locks the no-regression property that
        // A2's wiring cannot break the normal path.
        let args = os_args_to_strings(vec!["vaultsync".into(), "status".into()]).unwrap();
        assert_eq!(
            parse_args(&args).unwrap(),
            Command::Status {
                vault: PathBuf::from(".")
            }
        );
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
        let dir = TempDir::new("vaultsync-cli-test");
        let (code, out, _) = run(
            Command::Status {
                vault: dir.path().into(),
            },
            &MemoryStore::new(),
        );
        assert_eq!(code, 0);
        assert!(out.contains("plan:"));
    }

    #[test]
    fn run_status_dirty_exit_2() {
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::write(dir.join("a.md"), "hi").unwrap();
        let (code, out, _) = run(
            Command::Status {
                vault: dir.path().into(),
            },
            &MemoryStore::new(),
        );
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
    fn run_push_stub_conflict_exit_0_placeholder() {
        // Characterization lock (P1r6 / L2): the Phase 1 dry-run stub returns
        // exit 0 even when the plan contains a conflict. This is a deliberate
        // placeholder, not an endorsement - Phase 2 must define executor exit
        // codes (conflict -> non-zero per sync-model) before real push ships.
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
        let (code, out, _) = run(
            Command::Push {
                vault: dir.path().into(),
                delete: false,
            },
            &store,
        );
        assert_eq!(code, 0);
        assert!(
            out.lines().any(|l| l.starts_with("*  c.md")),
            "conflict row missing: {out}"
        );
    }

    #[test]
    fn run_push_stub_prints_plan_no_store_mutation() {
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::write(dir.join("a.md"), "hi").unwrap();
        let store = MemoryStore::new();
        let (code, out, _) = run(
            Command::Push {
                vault: dir.path().into(),
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
        let dir = TempDir::new("vaultsync-cli-test");
        let (code, out, _) = run(
            Command::Push {
                vault: dir.path().into(),
                delete: true,
            },
            &store,
        );
        assert_eq!(code, 0);
        assert!(out.lines().any(|l| l.starts_with("DR gone.md")));
    }

    #[test]
    fn run_pull_delete_stub_prints_delete_local() {
        let dir = TempDir::new("vaultsync-cli-test");
        std::fs::write(dir.join("gone.md"), "bye").unwrap();
        let store = MemoryStore::new();
        let (code, out, _) = run(
            Command::Pull {
                vault: dir.path().into(),
                delete: true,
            },
            &store,
        );
        assert_eq!(code, 0);
        assert!(out.lines().any(|l| l.starts_with("DL gone.md")));
        // dry-run stub must not mutate the local file
        assert!(dir.join("gone.md").exists());
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
}
