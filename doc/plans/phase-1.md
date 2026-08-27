# Phase 1 - Skeleton (implementation plan)

**Status:** complete (2026-08-27)  
**Roadmap:** [doc/roadmap.md](../roadmap.md) Phase 1  
**Design refs:** [architecture.md](../architecture.md), [sync-model.md](../sync-model.md), [object-store.md](../object-store.md), [cli.md](../cli.md)

---

## Goal

Ship a single-crate skeleton where:

1. Library modules exist: `entity`, `plan`, `local`, `store` (trait + in-memory mock).
2. The planner is pure and fully unit-tested against fixture entity trees (no network).
3. CLI stubs exist for `status`, `push`, `pull`, `check`, `version`, exercising plans against a mock store.

**Exit criteria (from roadmap):**

- `cargo test` green
- `vaultsync status` against a mock store in a temp vault produces a correct plan (human text on stdout)

Phase 1 does **not** include: real S3, TOML config loading, executor transfers, ignore profiles beyond a hard-coded minimal set (or none), `--delete` safety rails, lock files, or JSON schema stability work reserved for later phases.

---

## Method: strict fine-grained TDD

Every behavior-bearing change follows one cycle:

1. **RED** - write the named test first; run it; confirm it fails for the *right* reason (missing type/fn, assertion, etc.), not because of a compile typo in the test harness itself.
2. **GREEN** - write the smallest implementation that makes that test pass. No speculative helpers, no second feature.
3. **REFACTOR** - clean names/structure with the suite still green. Refactors do not add behavior.

Rules of engagement:

- One logical behavior per cycle. Prefer many small cycles over one "module dump".
- Tests name the behavior, not the implementation (`plan_upload_when_local_only`, not `test_plan_1`).
- Do not expand production code without a failing test that demands it (except pure wiring that the next RED cannot compile without - keep that wiring minimal and covered immediately after).
- After each vertical slice below, run `cargo test` (full suite) before starting the next slice.
- No network in any Phase 1 test. Mock store + temp dirs only.
- When a cycle needs a new external crate: **stop and confirm with the user first** (Rust dependency policy). Phase 1 target is **std-only**.

### Suggested red/green cadence

```text
write test -> cargo test -p vaultsync -- --nocapture <test_name>   # RED
minimal prod code -> same command                                  # GREEN
cargo test                                                         # full suite still green
optional refactor -> cargo test
```

Binary/CLI smoke can use `cargo run -- <args>` for manual exit-criteria checks; automate those as lib-level or `#[test]` process-free tests first. Prefer testing CLI parsing and command dispatch as library functions so tests do not have to spawn the binary (spawning is allowed later if needed; not required for Phase 1).

---

## Current baseline (confirm before coding)

```text
vaultsync/                 # single package (locked)
  Cargo.toml               # edition 2024, zero dependencies
  src/lib.rs               # version() only
  src/main.rs              # prints "vaultsync {version}"
  doc/                     # Phase 0 design (done)
  doc/plans/               # this file
```

`cargo test` currently runs 0 tests and passes. Keep it green after every slice.

---

## Dependency policy (Phase 1)

| Need | Choice | Rationale |
| ---- | ------ | --------- |
| CLI parse | hand-rolled argv over `&[String]` / `std::env::args` | avoid `clap` until flags grow (Phase 2+) |
| Temp dirs in tests | `std::fs` + `std::env::temp_dir` with unique suffix, or test-local `std::sync::atomic` counter | avoid `tempfile` crate |
| Assertions | `assert_eq!` / `assert!` | std |
| Errors | small `thiserror`-free enum + `Display` + `std::error::Error` | avoid `thiserror`/`anyhow` until error surface hurts |
| Serialize plan JSON | **out of scope** unless a tiny hand-built JSON string is needed for a stub; prefer human text only in Phase 1 | `serde` is Phase 2+ with config |
| Async / S3 SDK | none | Phase 2 spike |

If implementation pain strongly argues for one small crate, document the reason in the decision log and ask before adding it.

---

## Target module layout

```text
src/
  lib.rs              # re-exports / module tree; version()
  main.rs             # thin: args -> cli::run -> exit code
  error.rs            # core Error enum used by store/local/plan orchestration
  entity.rs           # Entity, Side, key helpers
  plan/
    mod.rs            # Plan, Action, ActionKind, PlanOpts, Mode, plan()
  local.rs            # LocalFs: walk (required); read/write/delete stubs or thin real FS for walk only
  store/
    mod.rs            # ObjectStore trait + ObjectMeta (or re-use Entity)
    mock.rs           # MemoryStore
  cli.rs              # parse + dispatch; status builds plan via local + mock
```

Notes:

- Single package, lib + bin (roadmap decision). No workspace split.
- `ObjectMeta` vs `Entity`: prefer **one** public file descriptor type. Recommended: `Entity` is the planner-facing type; the store trait speaks `Entity` (or a thin alias) so mock list results plug straight into `plan()`. If a separate `ObjectMeta` is introduced, provide a lossless conversion and test it - do not keep two divergent shapes.
- Planner **must not** import CLI. Store **must not** import planner. Local **must not** import store. Enforce via module graph (and code review), not a workspace boundary yet.
- Streaming trait methods (`get_to` / `put_from`) are normative from day one ([object-store.md](../object-store.md)). Mock implements them. Buffered helpers may wrap streams for tests.

---

## Public type sketches (normative for Phase 1)

Adjust names to Rust style as tests demand; keep semantics aligned with design docs.

### Entity

```rust
/// Vault-relative path. No leading '/'. Folders end with '/'. Separators are '/'.
pub struct Entity {
    pub key: String,
    pub size: u64,              // 0 for folders
    pub mtime_ms: Option<u64>,  // client-visible mtime when known
    pub etag: Option<String>,   // remote opaque token when known
}
```

Helpers (test-driven):

- `fn is_folder(&self) -> bool`
- `fn ensure_valid_key(key: &str) -> Result<()>` - reject leading `/`, backslash, empty (except decide: empty key invalid)
- construction helpers for fixtures: `file(key, size, mtime_ms)`, `folder(key)`

### Plan

```rust
pub enum Mode { Status, Push, Pull }

pub struct PlanOpts {
    pub mtime_tolerance_ms: u64,  // default 1000
    pub delete: bool,             // maps to --delete
    pub force_local: bool,
    pub force_remote: bool,
}

pub enum ActionKind {
    Upload,
    Download,
    DeleteLocal,
    DeleteRemote,
    Skip,
    Conflict,
}

pub struct Action {
    pub key: String,
    pub kind: ActionKind,
    pub reason: &'static str, // or small enum later; &'static str is fine for v1 skeleton
    pub local: Option<Entity>,
    pub remote: Option<Entity>,
}

pub struct PlanStats {
    pub upload: u32,
    pub download: u32,
    pub delete_local: u32,
    pub delete_remote: u32,
    pub skip: u32,
    pub conflict: u32,
    // bytes_in/out optional in Phase 1; can stay 0
}

pub struct Plan {
    pub actions: Vec<Action>,
    pub stats: PlanStats,
}

pub fn plan(local: &[Entity], remote: &[Entity], mode: Mode, opts: &PlanOpts) -> Plan;
```

Mode filtering (sync-model):

| Mode | Emits executable intent for |
| ---- | --- |
| `Status` | all classifications as report actions (upload/download/delete_* as *would-be*, plus skip/conflict) |
| `Push` | Upload (+ DeleteRemote if `opts.delete`); Skip/Conflict still listed |
| `Pull` | Download (+ DeleteLocal if `opts.delete`); Skip/Conflict still listed |

Phase 1 choice (lock here): **`plan()` always reports the full classification**; mode + `delete` control which action kinds are *selected* for execution semantics. `status` displays the status-oriented view. `push`/`pull` stubs call `plan` with their mode so printed plans match what a future executor would run.

Recommended reason strings (stable enough for tests):

| Situation | reason |
| --------- | ------ |
| both equal | `equal` |
| local only | `local_only` |
| remote only | `remote_only` |
| local mtime newer | `local_newer` |
| remote mtime newer | `remote_newer` |
| same mtime (within tol), different size | `conflict_mtime_size` |
| folder both sides | `equal` (skip) |

### ObjectStore

```rust
pub trait ObjectStore {
    fn list(&self, prefix: &str) -> Result<Vec<Entity>, Error>;
    fn head(&self, key: &str) -> Result<Entity, Error>;
    fn get_to(&self, key: &str, w: &mut dyn std::io::Write) -> Result<Entity, Error>;
    fn put_from(
        &self,
        key: &str,
        r: &mut dyn std::io::Read,
        size: u64,
        mtime_ms: Option<u64>,
    ) -> Result<Entity, Error>;
    fn delete(&self, key: &str) -> Result<(), Error>;
}
```

Prefix: mock may ignore backend bucket prefix (planner keys are vault-relative). `list("")` lists all. `list("notes/")` filters by key prefix.

### Mock store

In-memory `HashMap<String, MockObject { bytes, mtime_ms, etag }>`:

- files only in the map; folders synthesized on `list` from key prefixes (trailing `/` entities) OR stored explicitly when put of folder marker - Phase 1 default: **synthesize folders from file key parents**, matching "no folder objects" remote default
- `put_from` reads `size` bytes (or to EOF if easier - pick one and test it; prefer honoring `size`)
- `get_to` writes bytes; `NotFound` if missing
- `delete` removes or `NotFound`
- etag: simple counter or hash of contents (std only: e.g. length+mtime debug string is enough; optional `Xx` hex of a cheap checksum)

### Local

```rust
pub struct LocalFs {
    root: PathBuf,
}

impl LocalFs {
    pub fn new(root: impl Into<PathBuf>) -> Self;
    /// Walk files (and folders) under root into vault-relative Entity keys.
    pub fn list(&self) -> Result<Vec<Entity>, Error>;
    // Phase 1: read/write/delete may be thin real implementations or `todo!` -
    // only `list` is required for status exit criteria.
}
```

Walk rules:

- keys relative to root, `/` separators, no leading `/`
- folders end with `/`
- skip the root itself as an entity
- mtime from `fs::metadata` modified time -> ms since epoch
- size from metadata; folders size 0
- do not follow symlinks in v1 (test: symlink to file outside is skipped or error - pick **skip symlinks** and test it)
- empty directories appear as folder entities (local can round-trip empty folders; remote mock may not - document in plan output tests)

### Error

```rust
pub enum Error {
    NotFound(String),
    InvalidKey(String),
    Io(std::io::Error),
    Other(String),
}
```

Map mock/local failures into this. CLI prints `Display` on stderr and exits 1.

### CLI

```rust
pub enum Command {
    Status { vault: PathBuf },
    Push { vault: PathBuf, delete: bool },
    Pull { vault: PathBuf, delete: bool },
    Check,
    Version,
    Help,
}

pub fn parse_args(args: &[String]) -> Result<Command, String>;
pub fn run(cmd: Command) -> i32; // exit code
```

Phase 1 argv (minimal subset of [cli.md](../cli.md)):

```text
vaultsync version
vaultsync help | --help | -h
vaultsync status [--vault <path>]
vaultsync push [--vault <path>] [--delete]
vaultsync pull [--vault <path>] [--delete]
vaultsync check
```

Defaults: `--vault` defaults to `.` (cwd). No `--config`, no `--json` required in Phase 1 (may stub "not implemented" if flagged - prefer reject unknown flags with usage).

`status` / `push` / `pull` wiring in Phase 1:

1. `LocalFs::list` on vault path
2. Build a **process-local mock store** seeded empty **or** via an env/test seam

**Mock injection seam (required for exit criteria):** production `status` against a real user vault cannot see a mock unless we inject one. Phase 1 approach:

- Library function:

  ```rust
  pub fn status_with_store(
      vault: &Path,
      store: &dyn ObjectStore,
      opts: &PlanOpts,
  ) -> Result<Plan, Error>;
  ```

- CLI `status` for Phase 1 uses `MemoryStore::new()` empty by default and prints the plan (everything local_only uploads). That satisfies "against mock store".
- Tests call `status_with_store` with a pre-seeded mock + temp vault for richer diffs.
- Optional: `VAULTSYNC_MOCK_FIXTURE=1` is **not** required if tests stay at lib level; prefer lib-level tests over env magic.

`push` / `pull` stubs: build the same plan with `Mode::Push` / `Mode::Pull`, print it, **do not** mutate store or disk. Label output `dry-run (phase 1 stub)` so users are not surprised.

`check` stub: print `check: ok (mock)` and exit 0. Real connectivity is Phase 2.

`version`: print `vaultsync {version}` exit 0.

Exit codes for `status` (align with cli.md early):

- `0` - clean (no upload/download/delete/conflict; skips only)
- `2` - dirty (any non-skip action or conflict)
- `1` - error

---

## Work slices (ordered TDD)

Each slice lists **RED tests first**, then the production surface those tests force. Do not skip ahead.

---

### Slice 0 - Test harness hygiene

**RED/GREEN:**

1. Add a smoke unit test on `version()` in `src/lib.rs` (or `tests` module): assert `version()` equals `env!("CARGO_PKG_VERSION")` / `"0.1.0"`.
2. Confirm `cargo test` runs 1 test green.

No new modules yet.

---

### Slice 1 - `error` + `entity`

**RED tests** (`entity` module tests):

| Test | Asserts |
| ---- | ------- |
| `entity_file_helpers` | `file("a.md", 10, Some(1000))` fields |
| `entity_folder_helper_trailing_slash` | `folder("notes")` and `folder("notes/")` both yield key `notes/` |
| `entity_is_folder` | `notes/` true; `notes/a.md` false |
| `entity_reject_leading_slash` | `ensure_valid_key("/a")` err |
| `entity_reject_backslash` | `ensure_valid_key("a\\b")` err |
| `entity_reject_empty` | `ensure_valid_key("")` err |

**GREEN:** implement `src/error.rs`, `src/entity.rs`; `mod` from `lib.rs`.

**REFACTOR:** keep helpers small; no planner yet.

---

### Slice 2 - `store` trait + `MemoryStore`

**RED tests** (`store::mock` tests):

| Test | Asserts |
| ---- | ------- |
| `mock_put_get_roundtrip` | `put_from` then `get_to` bytes equal; `head` size/mtime |
| `mock_get_missing_not_found` | `get_to` missing -> `Error::NotFound` |
| `mock_delete_removes` | delete then get NotFound |
| `mock_delete_missing` | delete missing -> NotFound (or Ok idempotent - **lock: NotFound**) |
| `mock_list_all_and_prefix` | put `a.md`, `notes/b.md`; `list("")` both; `list("notes/")` one |
| `mock_list_synthesizes_folder_prefixes` | after `notes/b.md`, list contains `notes/` folder entity (size 0) |
| `mock_overwrite_put` | second put replaces bytes and updates mtime/etag |

**GREEN:** `src/store/mod.rs`, `src/store/mock.rs` with `MemoryStore`.

**REFACTOR:** share a test helper `put_str(store, key, body, mtime)`.

Implement `ObjectStore` for `MemoryStore`. Interior mutability: `MemoryStore` uses `RefCell`/`Mutex` if trait methods take `&self` (preferred, matches sketch). Lock: **`&self` + `std::sync::Mutex`** so the trait object is usable from CLI without `mut` gymnastics.

---

### Slice 3 - Planner core (pure, fixture trees)

No IO. Fixtures are hand-built `Vec<Entity>`.

**Default opts for tests:** `mtime_tolerance_ms = 1000`, `delete = false`, forces false unless named.

**RED tests** (`plan` module) - table-driven preferred:

| Test | Local | Remote | Mode | Expect |
| ---- | ----- | ------ | ---- | ------ |
| `plan_both_empty` | [] | [] | Status | stats all 0; actions empty |
| `plan_local_only_file_status` | `a.md` | [] | Status | 1 Upload, reason `local_only` |
| `plan_remote_only_file_status` | [] | `a.md` | Status | 1 Download, reason `remote_only` |
| `plan_equal_files_skip` | same size+mtime | same | Status | Skip `equal` |
| `plan_mtime_tolerance_skip` | mtime 1000 | mtime 1500, tol 1000 | Status | Skip (diff 500 <= 1000) |
| `plan_local_newer_upload` | mtime 5000 | mtime 1000, same size | Status | Upload `local_newer` |
| `plan_remote_newer_download` | mtime 1000 | mtime 5000 | Status | Download `remote_newer` |
| `plan_conflict_same_mtime_diff_size` | size 1 mtime 1000 | size 2 mtime 1000 | Status | Conflict `conflict_mtime_size` |
| `plan_folders_both_sides_skip` | `n/` | `n/` | Status | Skip |
| `plan_push_mode_filters_download` | remote only | - | Push | no Download action (or Skip report - **lock below**) |
| `plan_pull_mode_filters_upload` | local only | - | Pull | no Upload action |
| `plan_push_delete_remote_only` | [] | `gone.md` | Push, delete=true | DeleteRemote |
| `plan_pull_delete_local_only` | `gone.md` | [] | Pull, delete=true | DeleteLocal |
| `plan_push_without_delete_keeps_remote_only_as_skip_or_absent` | [] | `r.md` | Push, delete=false | **lock: Skip** with reason `remote_only` (visible in status-like report without deleting) |
| `plan_stats_counts` | mix | mix | Status | stats match action kinds |
| `plan_actions_sorted_by_key` | unsorted inputs | - | Status | actions sorted by key ascending (stable UX/tests) |

**Mode filtering lock (Phase 1):**

- `Status`: Upload / Download / Delete* never emitted for deletes unless we want status to *show* would-be deletes - **status shows would-be deletes only when `opts.delete` is true**; otherwise remote-only on push-ish report is Download (pull direction interest) and local-only is Upload. Simpler rule used by rsync-like tools:

  **Final lock for `plan()`:**

  1. Classify every key into a *delta*: `Equal`, `LocalOnly`, `RemoteOnly`, `LocalNewer`, `RemoteNewer`, `Conflict`.
  2. Map delta -> `ActionKind` by mode:

     | Delta | Status | Push | Pull |
     | ----- | ------ | ---- | ---- |
     | Equal | Skip | Skip | Skip |
     | LocalOnly | Upload | Upload | DeleteLocal if delete else Skip |
     | RemoteOnly | Download | DeleteRemote if delete else Skip | Download |
     | LocalNewer | Upload | Upload | Skip (or Conflict? no: local newer on pull means keep local - **Skip** reason `local_newer`) |
     | RemoteNewer | Download | Skip reason `remote_newer` | Download |
     | Conflict | Conflict | Conflict unless force_local->Upload / force_remote->Download | same with forces |

  3. Forces only apply to `Conflict` rows in Phase 1.

**GREEN:** `src/plan/mod.rs` implementing `plan()`.

**REFACTOR:** extract `classify_pair(local, remote, tol) -> Delta` pure helper with its own unit tests if the main table gets noisy.

Coverage target: every cell of the mode mapping table above has at least one test before leaving the slice.

---

### Slice 4 - `local` walker

**RED tests** (temp dir fixtures built in-test):

| Test | Setup | Asserts |
| ---- | ----- | ------- |
| `local_list_empty_dir` | empty temp root | `Ok([])` or only nothing |
| `local_list_files_and_nested` | `a.md`, `n/b.md` | keys `a.md`, `n/`, `n/b.md` (folder present) |
| `local_keys_use_slash_not_backslash` | nested | no `\` in keys |
| `local_mtime_and_size_populated` | write known bytes | size matches; mtime_ms `Some` |
| `local_skips_symlinks` | symlink file | not present in list (or only the link skipped) |
| `local_missing_root_errors` | path does not exist | `Err` |

**GREEN:** `src/local.rs` walk via `std::fs::read_dir` recursion (stack/queue). No external walkdir crate.

**REFACTOR:** normalize path to key in one function; test that function directly if useful.

Windows note: develop on macOS; keep key normalization logic explicit so Windows later still emits `/`.

---

### Slice 5 - Orchestration helpers

**RED tests:**

| Test | Asserts |
| ---- | ------- |
| `status_with_store_local_only` | temp vault with `a.md`, empty mock -> plan has Upload `a.md` |
| `status_with_store_matches_seeded_remote` | vault `a.md` equal to mock seed -> Skip |
| `status_with_store_remote_only_download` | empty vault, mock has `b.md` -> Download |
| `format_plan_human_contains_stats_line` | formatter includes `plan:` and counts |
| `format_plan_human_marks_actions` | lines with `U ` / `D ` / `* ` / prefixes per kind |

**GREEN:**

```rust
// e.g. src/lib.rs or src/ops.rs
pub fn build_plan(
    local: &LocalFs,
    store: &dyn ObjectStore,
    mode: Mode,
    opts: &PlanOpts,
) -> Result<Plan, Error>;

pub fn format_plan_human(plan: &Plan) -> String;
```

Human format (from cli.md, Phase 1 subset):

```text
plan: 3 upload, 1 download, 0 delete_local, 0 delete_remote, 2 skip, 0 conflict
U  notes/a.md
D  notes/b.md
*  notes/c.md    conflict_mtime_size
```

Keep columns simple; exact spacing tested loosely (`contains`) where brittle.

---

### Slice 6 - CLI parse + dispatch

**RED tests** (`cli` module, pure parse):

| Test | Input | Expect |
| ---- | ----- | ------ |
| `parse_version` | `["vaultsync","version"]` | `Command::Version` |
| `parse_help` | `["vaultsync","--help"]` | Help |
| `parse_status_default_vault` | `status` | vault `.` |
| `parse_status_vault_flag` | `status --vault /tmp/v` | path |
| `parse_push_delete` | `push --delete` | delete true |
| `parse_pull` | `pull` | delete false |
| `parse_check` | `check` | Check |
| `parse_unknown_command` | `foo` | Err with usage hint |
| `parse_unknown_flag` | `status --json` | Err unknown flag (Phase 1) |

**RED tests** (dispatch, lib-level):

| Test | Asserts |
| ---- | ------- |
| `run_version_exit_0` | exit code 0 (capture output via `format` injection or test `version` path only) |
| `run_status_clean_exit_0` | empty vault + empty store -> 0 |
| `run_status_dirty_exit_2` | file in vault + empty store -> 2 |
| `run_check_stub_exit_0` | 0 |
| `run_push_stub_prints_plan_no_store_mutation` | mock remains empty after push stub with local file |

Output capture approach (std-only): thread `run` through a small trait or pass `&mut dyn Write` for stdout/stderr:

```rust
pub fn run_with_io(cmd: Command, store: &dyn ObjectStore, out: &mut dyn Write, err: &mut dyn Write) -> i32;
```

CLI `main` uses `MemoryStore::new()`, `stdout()`/`stderr()`, real vault path from args.

**GREEN:** `src/cli.rs`; thin `main.rs`:

```rust
fn main() {
    let code = vaultsync::cli::run_from_env();
    std::process::exit(code);
}
```

**REFACTOR:** keep parse separate from execute.

---

### Slice 7 - Exit criteria manual + automated glue

**Automated:**

- Full `cargo test` green (all slices).
- One integration-style unit test that mirrors the roadmap sentence:

  `phase1_exit_status_against_mock_in_temp_vault`

  - create temp vault with `notes/hello.md`
  - seed mock with nothing
  - `status_with_store` -> assert Upload of `notes/hello.md` and folder `notes/` handling consistent with planner rules (folder local-only -> Upload or Skip? **Folders:** local-only folder with no remote: Status maps LocalOnly -> Upload. Empty folder upload is a no-op on real S3 later; Phase 1 mock may accept `put_from` of zero-byte folder key or planner may Skip folders for transfer kinds.

**Folder transfer lock (Phase 1):**

- Classification still emits folder entities.
- For **file** keys only, Upload/Download carry transfer meaning.
- Folder-only actions: **Skip** with reason `folder` (or treat folder LocalOnly as Skip always), because empty folders do not round-trip to S3 by default ([sync-model.md](../sync-model.md)).

Add tests:

| Test | Expect |
| ---- | ------ |
| `plan_local_only_folder_skips_transfer` | local `n/` only -> Skip `folder` (Status/Push/Pull) |
| `plan_remote_only_folder_skips_transfer` | remote `n/` only -> Skip `folder` |

File under folder still uploads the file; synthesized remote folders from mock list remain Skip.

**Manual (document in checklist):**

```text
cargo build
./target/debug/vaultsync version
./target/debug/vaultsync help
TMP=$(mktemp -d)
echo hi > "$TMP/note.md"
./target/debug/vaultsync status --vault "$TMP"
# expect dirty plan with U note.md, exit 2
./target/debug/vaultsync push --vault "$TMP"
# expect phase-1 stub plan text, no crash
./target/debug/vaultsync check
# expect mock ok
```

---

## Out of scope (do not sneak into Phase 1)

| Item | Where it belongs |
| ---- | ---------------- |
| `aws-sdk-s3` / real S3 | Phase 2 |
| TOML config + credential chain | Phase 2 |
| Executor concurrency, real put/get files | Phase 2 (execute) / Phase 3 (limits) |
| `--json` stable schema | Phase 3 hardening (may stub reject until then) |
| `--yes` / `--max-delete` confirmations | Phase 3 |
| Ignore globs / Obsidian profile | Phase 3 (Phase 1 walk lists everything under vault) |
| Lock file | Phase 3 |
| Bidirectional `sync` | post-v1 |
| Encryption | post-v1 |

If a test seems to need an out-of-scope feature, narrow the test.

---

## Implementation order checklist

Use this as the working board; tick in PRs or locally.

- [x] Slice 0 - `version` smoke test
- [x] Slice 1 - `error` + `entity` + key validation tests
- [x] Slice 2 - `ObjectStore` + `MemoryStore` roundtrip/list/prefix/delete tests
- [x] Slice 3 - pure `plan()` mode mapping + tolerance + conflict + delete flags
- [x] Slice 4 - `LocalFs::list` temp-dir tests
- [x] Slice 5 - `build_plan` / `status_with_store` / human formatter
- [x] Slice 6 - argv parse + `run_with_io` exit codes + push/pull stubs
- [x] Slice 7 - exit-criteria test + manual smoke
- [x] `cargo test` green (67 tests)
- [x] `cargo run -- status --vault <temp>` dirty plan demo
- [x] Update root `README.md` status blurb to "Phase 1 complete" when exit criteria pass (done, 2026-08-27)
- [x] Optionally tick Phase 1 boxes in `doc/roadmap.md` when done (done)

---

## Definition of done

Phase 1 is done when all of the following hold:

1. Module tree matches the target layout (names may vary slightly; responsibilities must not).
2. `cargo test` is green with planner fixture coverage for every mode-mapping cell and mock/local tests as listed.
3. No non-std dependency added without an explicit decision-log entry and user OK.
4. `vaultsync status --vault <temp>` prints a human plan against the in-memory mock and exits 2 when the vault has files.
5. `push` / `pull` print plans and do not mutate disk or mock contents.
6. `check` and `version` exit 0 with stub/version text.
7. No network sockets opened by library code or tests.

---

## Risk notes

| Risk | Mitigation |
| ---- | ---------- |
| Over-building executor in Phase 1 | Stubs print plans only; no transfer loop |
| Planner mode matrix confusion | Lock table in Slice 3; one test per cell |
| Folder/file action noise | Folder-only -> Skip `folder`; files carry transfer actions |
| CLI grows flags early | Reject unknown flags; hand-parse minimal set |
| `edition = "2024"` surprises | Already compiling on rustc 1.95; do not change unless broken |
| Temptation to add `clap`/`serde` | Std-only policy; ask first |

---

## First commit suggestion (after Slice 0-1)

Keep commits aligned to slices so review stays bisectable:

1. `test: version smoke + entity key rules`
2. `feat: MemoryStore ObjectStore mock`
3. `feat: pure planner with fixture tests`
4. `feat: LocalFs list walker`
5. `feat: status plan orchestration + human format`
6. `feat: CLI stubs status/push/pull/check/version`

Each commit must leave `cargo test` green (TDD: tests land in the same commit as the green implementation, or as a preceding RED commit only on branches that tolerate red middles - prefer **single commit per completed slice** after GREEN).
