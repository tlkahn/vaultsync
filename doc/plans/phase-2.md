# Phase 2 - Real local FS + S3 (implementation plan)

**Status:** complete - all slices 0-10 implemented; offline suite green (217 lib + 7 env-gated); AWS matrix verified, R2 row pending
**Roadmap:** [doc/roadmap.md](../roadmap.md) Phase 2 + the full "Phase 2 checklist (deferred PR 1 review items)"
**Design refs:** [architecture.md](../architecture.md), [sync-model.md](../sync-model.md), [object-store.md](../object-store.md), [cli.md](../cli.md)
**Predecessor:** [phase-1.md](phase-1.md) (complete, 138 tests green)

---

## Goal

Turn the Phase 1 skeleton into a real one-way sync tool:

1. Local walker (exists) + real reader/writer (open, stream, write, delete, set mtime).
2. S3 backend behind the existing `ObjectStore` trait (list/get/put/delete, metadata mtime, prefix, path-style, custom endpoint).
3. TOML config + AWS credential chain.
4. Real `check` against a real bucket.
5. Real `push` / `pull` executor (streaming, verified, safe ordering, correct exit codes).
6. Every deferred Phase 2 checklist item from the roadmap either landed or explicitly re-deferred with a decision-log entry.

**Exit criteria (from roadmap):** push/pull a sample vault including nested folders and a binary attachment, verified on **AWS S3** and **Cloudflare R2**.

---

## Locked planning decisions (this plan)

Made with the user before writing this plan; record each in the roadmap decision log as it lands.

| ID | Decision | Choice |
| -- | -------- | ------ |
| P2-scope | Phase 2 scope | Roadmap 5 items **plus** the full deferred Phase 2 checklist (each item lands or is explicitly re-deferred with a log entry) |
| P2-d1 | S3 client spike order | Spike the **official stack only** (`aws-sdk-s3` + `aws-config` + `tokio`) as one unit. No parallel `rust-s3` spike. If the matrix fails or the user rejects weight on recorded metrics, **stop and re-open D1** - do not auto-import another S3 crate |
| P2-matrix | Manual test matrix | AWS S3 + Cloudflare R2 (S3-compatible row) |
| P2-integ | Real-bucket tests | Env-gated automated integration tests (`tests/`, off by default) **plus** a manual matrix checklist doc |
| P2-toml | Config parsing | `toml` + `serde` (derive) |
| P2-cli | CLI parser | Migrate to `clap` (per cli.md N3 note); gains pre-command global flags, `--flag=value`, `--` terminator |

### Dependency policy (Phase 2)

Minimize direct crates. Prefer the lightest option that clears the matrix; confirm with the user before adding anything not listed here.

| Crate | Status | Purpose |
| ----- | ------ | ------- |
| `clap` | **approved** | CLI parse (global flags, `--`, help) |
| `toml` | **approved** | config file |
| `serde` (derive) | **approved** | config structs; future `--json` |
| `aws-sdk-s3` | **approved for spike** | official S3 client (D1); one stack with the next two rows |
| `aws-config` | **approved for spike** | default credential/region chain; companion to `aws-sdk-s3`, not a second S3 library |
| `tokio` | **approved for spike** | runtime if the official stack wins (D2); async stays inside `store::s3` |
| `rust-s3` | **docs only** | named alternative in [object-store.md](../object-store.md); **not approved** to import. Re-open D1 with the user before any non-official S3 crate |
| anything else | **ask first** | per workspace Rust dependency policy |

S3 stack note: `aws-sdk-s3` + `aws-config` + `tokio` are one ecosystem unit. Omitting `aws-config` does not meaningfully shrink the client tree (smithy/hyper/tokio still come in via `aws-sdk-s3`); it only drops the default credential chain, which Phase 2 keeps (env, shared config/credentials, `AWS_PROFILE`; full chain via `aws-config`). Do not add a second S3 ecosystem alongside the official stack.

Feature minimalism (spike duty): record the smallest `tokio` / `aws-config` / `aws-sdk-s3` feature set that still clears the matrix, and use that shape in `Cargo.toml`. Do not pull in `tracing`, `anyhow`, `thiserror`, or other convenience crates just because SDK examples use them.

Async containment (locked, restating D2): `tokio` and all `async` code live inside the S3 backend. `ObjectStore` keeps its **sync** trait surface; `S3Store` owns a private `tokio::runtime::Runtime` and `block_on`s per call. The planner, executor, CLI, and all existing tests stay sync and runtime-free. No `async` keyword outside `store::s3` without a new decision-log entry.

---

## Method: strict fine-grained TDD

Same rules of engagement as Phase 1 (see [phase-1.md](phase-1.md) "Method"):

1. **RED** - named failing test first; confirm it fails for the right reason.
2. **GREEN** - smallest implementation that passes.
3. **REFACTOR** - behavior-preserving cleanup on green.
4. One logical behavior per cycle; full `cargo test` after every slice.
5. Test names describe behavior, not implementation.

Phase 2 additions to the method:

- **No network in the default test suite.** `cargo test` with no env vars set must pass offline using mock store + temp dirs only.
- **Env-gated integration tests are not RED/GREEN units.** They are written as `#[ignore]`-style gates (see Slice 10) and run on demand against real endpoints. Their *support code* (key mapping, metadata encode/decode, error mapping, config resolution) is pure and gets normal RED/GREEN unit tests first. Rule: **any logic that can be tested without a socket must be**; only the thinnest possible request/response shell is network-only.
- **The spike (Slice 0) is explicitly not TDD.** It is throwaway probe code with a written outcome. Production code starts TDD again in Slice 1.
- **Characterization tests are the safety net for planner amendments.** Phase 1 locked behavior with named tests (e.g. `plan_pull_remote_none_mtime_diff_size_skips_as_local_newer`). Phase 2 policy changes (Slices 4) land by: write the new-behavior RED test, flip/retire the old characterization test in the same commit, and add a decision-log entry. Never silently edit a characterization test.

### Cadence

```text
write test -> cargo test -- --nocapture <test_name>     # RED
minimal prod code -> same command                       # GREEN
cargo test                                              # full suite green
optional refactor -> cargo test
```

---

## Current baseline (confirm before coding)

```text
src/
  lib.rs        # build_plan (remote ingest validation), status_with_store,
                # format_plan_human, version()
  main.rs       # thin: cli::run_from_env -> exit
  cli.rs        # hand-rolled parse (subcommand-first flags only), stubs:
                #   push/pull print plan, mutate nothing, exit 0 always
                #   check prints "ok (mock)"; status exits 0/2
  entity.rs     # Entity, ensure_valid_key (rejects / \ ctl ws-only segments)
  error.rs      # Error::{NotFound, InvalidKey, Io, Other}
  local.rs      # LocalFs::list walker (skip symlinks/specials, loud on bad keys)
  plan/mod.rs   # pure plan(): Delta classify + mode/force matrix; folders Skip
  store/mod.rs  # ObjectStore trait (sync, streaming get_to/put_from)
  store/mock.rs # MemoryStore (content-derived FNV-1a etags)
Cargo.toml      # zero dependencies, edition 2024, no rust-version pin
```

`cargo test`: 138 tests green. Keep it green after every slice.

---

## Target module layout (end of Phase 2)

```text
src/
  lib.rs            # orchestration: build_plan, status_with_store, format_plan_human
  main.rs           # thin
  cli.rs            # clap-based parse + dispatch (real push/pull/check)
  config.rs         # TOML config structs + load/resolve/precedence (pure, tested)
  entity.rs         # unchanged surface; helpers as needed
  error.rs          # + Unauthorized/Timeout/Unavailable mapping per object-store.md
  local.rs          # walker + reader/writer/delete + key_to_local_path (single join site)
  exec.rs           # executor: apply plan (transfers, deletes, verify, report)
  plan/mod.rs       # + collision/mtime-None/case-collision amendments
  store/
    mod.rs          # trait (sync, streaming) - unchanged
    mock.rs         # unchanged
    s3.rs           # S3Store: owns tokio runtime, prefix mapping, mtime metadata
tests/
  s3_integration.rs # env-gated; not compiled into the default offline run
doc/
  test-matrix.md    # manual matrix checklist (AWS + R2)
```

Module-graph rules still hold: planner imports nothing IO; store imports no planner; CLI depends on everything, nothing depends on CLI.

---

## Work slices (ordered TDD)

Slices are ordered so every slice is buildable and green on its own. The S3 spike is deliberately first: D1/D2 gate the shape of `store::s3`, and deciding early de-risks everything downstream.

---

### Slice 0 - S3 client spike (D1/D2) - NOT TDD, throwaway

**Purpose:** validate the official S3 stack before any production backend code. Single candidate only: `aws-sdk-s3` + `aws-config` + `tokio`.

**Probe requirements** (scratch binary under `examples/` or a temp branch, deleted or kept as an example after the decision):

1. `list` (paginated `ListObjectsV2`), `head`, `get`, `put`, `delete` against a real bucket.
2. Write + read back a user-metadata mtime (`vaultsync-mtime`, decimal ms).
3. Prefix support (`myvault/` style).
4. Path-style addressing toggle.
5. Custom endpoint (R2) **and** AWS default endpoint.
6. Credentials from the default AWS chain via `aws-config` (env vars at minimum; confirm profile/shared-file path works).

**Spike exit criteria** (all must pass for the official stack to win D1):

- All 6 probe items work on AWS S3.
- Items 1-5 work on R2 with a custom endpoint (document any R2 quirks, e.g. metadata case, checksum enforcement).
- Dependency weight is acceptable. **Mandatory metrics** in the spike notes (user judges "too heavy" from these, not a fixed threshold):
  - `cargo tree` size (full tree line count and/or `cargo tree -i aws-sdk-s3`)
  - clean release build time on the spike machine
  - stripped release binary size delta vs Phase 1 baseline
  - intended `Cargo.toml` feature set (smallest `tokio` / `aws-config` / `aws-sdk-s3` features that still pass the matrix)

**Outcome:** decision-log entries closing **D1** (client stack) and **D2** (async runtime) in [roadmap.md](../roadmap.md), plus spike notes in `doc/spikes/phase-2-s3.md` (or inline in the log if short).

**If the official stack fails the matrix or the user rejects weight:** stop. Re-open D1 with the user. Do **not** import `rust-s3` or any other S3 crate in the same slice/PR. Alternatives named in [object-store.md](../object-store.md) (`rust-s3`, hand-rolled SigV4) stay documentation-only until a new explicit approval.

**Tests:** none permanent. This is the only non-TDD slice.

---

### Slice 1 - CLI migration to clap

**RED tests** (parse-level, pure; port all 138-test-relevant existing parse tests, then extend):

| Test | Input | Expect |
| ---- | ----- | ------ |
| `parse_global_vault_before_subcommand` | `--vault /v status` | Status, vault `/v` |
| `parse_vault_equals_form` | `status --vault=/v` | Status, vault `/v` (P1r5 escape hatch) |
| `parse_vault_dash_name_via_equals` | `status --vault=-foo` | vault `-foo` reachable |
| `parse_double_dash_terminator` | `status -- --weird` | treated as positional/error per spec, not a flag |
| `parse_config_flag` | `--config /c.toml status` | config path captured |
| `parse_push_force_local` | `push --force-local` | force_local true |
| `parse_pull_force_remote` | `pull --force-remote` | force_remote true |
| `parse_both_forces_accepted_planner_cancels` | `push --force-local --force-remote` | parses; planner cancel semantics (P1r-both-forces) unchanged |
| `parse_dry_run_flag` | `push --dry-run` | dry_run true |
| `parse_verbose_repeatable` | `-vv status` | verbosity 2 |
| `parse_repeated_vault_still_errors` | `status --vault a --vault b` | error (P1r4-vault-value) |
| `parse_repeated_delete_still_errors` | `push --delete --delete` | error (P1r7-delete-repeat) |
| `parse_unknown_flag_errors_with_usage` | `status --bogus` | error + usage |
| `parse_help_per_subcommand` | `push --help` | help text, exit 0 |
| existing suite | all Phase 1 parse/dispatch tests | still green (ported to clap surface) |

**Locks:**

- Global flags (`--config`, `--vault`, `--json`, `-v/--verbose`) accepted **before or after** the subcommand (clap `global = true`).
- `--json` parses in Phase 2 but dispatch still rejects it with "not implemented" (schema stability is Phase 3) - one test locks this.
- `--yes` / `--max-delete` / `--concurrency` parse or are rejected? **Lock: rejected as unknown** until Phase 3 (delete-safety rails are Phase 3; `--delete` in Phase 2 still has no confirmation prompt - document loudly in help text).
- Every clap error message keeps the Phase 1 invariant of ending with usage (P1r7-parse-usage); clap does this natively - one test locks it.

**GREEN:** rewrite `src/cli.rs` on clap derive or builder (pick per test ergonomics; keep `Command` enum shape). `main.rs` unchanged.

**REFACTOR:** keep `parse_args` -> `Command` and `run_with_io` seams so dispatch tests stay process-free.

---

### Slice 2 - Config TOML + resolution

**RED tests** (`config` module, pure):

| Test | Asserts |
| ---- | ------- |
| `config_parse_full_example` | the cli.md example TOML parses into structs (vault_root, store.*, ignore.patterns, transfer.*) |
| `config_parse_minimal` | only `[store]` bucket+region -> defaults elsewhere |
| `config_missing_file_default_search_ok` | no config anywhere -> defaults, no error |
| `config_explicit_missing_file_errors` | `--config /nope.toml` -> loud error |
| `config_search_order` | `./.vaultsync.toml` beats `~/.config/vaultsync/config.toml` (temp-dir injected search paths) |
| `config_rejects_unknown_store_type` | `type = "azure"` -> error |
| `config_requires_bucket` | missing `bucket` -> error |
| `config_prefix_normalized_trailing_slash` | `prefix = "notes"` stored as `notes/`; `"notes/"` unchanged |
| `config_cli_vault_overrides_config` | `--vault` beats `vault_root` |
| `config_mtime_tolerance_default_1000` | unset -> 1000 |
| `config_invalid_toml_reports_line` | parse error message includes line info |

**Lock (credentials):** the TOML file **never** carries secrets (cli.md). `S3Store` is constructed with region/endpoint/bucket/prefix/path_style from config; credentials come from the AWS default chain (`aws-config`): env `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`/`AWS_SESSION_TOKEN`, `AWS_PROFILE`, shared config files, IMDS. Unit-testable part: a pure `resolve_settings(config, cli, env-snapshot) -> Result<Settings>` function with injected env maps; the actual chain call is thin and covered by Slice 10 integration tests.

**GREEN:** `src/config.rs` with serde derives; `resolve_settings`; wire `--config` into dispatch (still mock store until Slice 6).

---

### Slice 3 - Local reader/writer + single join site (R2.2, P1r7 TOCTOU)

**RED tests:**

| Test | Asserts |
| ---- | ------- |
| `key_to_local_path_joins_under_root` | `key_to_local_path(vault, "notes/a.md")` -> `vault/notes/a.md` |
| `key_to_local_path_rejects_traversal` | `..`, absolute, control-char keys rejected **before** join (single validation site; walker, executor download, and delete all route through it) |
| `key_to_local_path_rejects_folder_key_for_file_ops` | trailing `/` -> `InvalidKey` for read/write paths |
| `local_read_streams_bytes` | read a known file through the reader API |
| `local_open_rechecks_type_not_symlink` | swap a listed file for a symlink between `list` and open -> open fails loud (no follow); test via direct symlink at open time (TOCTOU window itself is not timing-tested) |
| `local_write_creates_parents_and_bytes` | write `n/deep/b.md` creates dirs, bytes round-trip |
| `local_write_sets_mtime` | written file mtime matches requested ms (within FS granularity) |
| `local_write_atomic_tmp_rename` | write goes through a temp sibling + rename; no partial file visible at final path on injected failure |
| `local_write_stays_under_root` | a `key_to_local_path`-validated path whose parent is replaced by an out-of-vault symlink is detected: canonicalize parent, confirm prefix under canonicalized vault root, refuse otherwise (P1r7 download half) |
| `local_delete_file` | file removed; missing -> `NotFound` |
| `local_remove_empty_dirs_bottom_up` | post-pass helper removes now-empty dirs children-first, stops at non-empty, never removes the root (feeds Slice 5 R2.1) |

**GREEN:** extend `src/local.rs`. Reader: `open_verified(path, expected_size) -> Result<fs::File>` that stats the **opened** file descriptor (`f.metadata()`, not a second path stat) and rejects size mismatch / non-file. Writer: temp-file-then-rename + `filetime`-free mtime set (std `File::set_modified`, stable since 1.75).

**Lock:** all vault path construction in the codebase routes through `key_to_local_path`. One test greps-enforced? No - enforced by code review + the executor only seeing that function (documented).

---

### Slice 4 - Planner Phase 2 amendments (pure, fixture trees)

Four independent behavior changes, each its own RED/GREEN cycle and decision-log entry. `plan()` stays pure; preflights live in `build_plan`.

#### 4a - File/folder path collision (P1r-type-collision)

| RED test | Asserts |
| -------- | ------- |
| `plan_file_vs_folder_key_conflicts` | local file `K`, remote folder `K/` (or remote child `K/x`) -> Conflict `path_collision`, not Upload+Download |
| `plan_folder_vs_file_key_conflicts` | mirror direction -> Conflict `path_collision` |
| `path_collision_survives_all_modes` | Status/Push/Pull all Conflict; forces do **not** resolve type collisions (test locks) |

Lock: type collisions are never force-resolvable; executor never touches them (Slice 5 test).

#### 4b - Unknown-mtime policy (P1r-mtime-none, P1r5-mtime-pull, P1r6-mtime-zero)

**Locked policy (amends Phase 1 `None -> 0`):** when both sides exist and **either** mtime is `None`:

- sizes differ -> **Conflict** `conflict_mtime_unknown` (all modes; forces apply per the existing mode-aware force table)
- sizes equal -> **Skip** `equal_unknown_mtime` (visible row, zero overwrite risk)

Pre-epoch `Some(0)` stays indistinguishable from "real epoch mtime" but no longer aliases `None`; the `None -> 0` classifier rule is deleted.

| RED test | Asserts |
| -------- | ------- |
| `plan_none_mtime_diff_size_conflicts` | local mtime set, remote `None`, sizes differ -> Conflict `conflict_mtime_unknown` (fixes silent-pull hole, P1r5-mtime-pull) |
| `plan_none_mtime_same_size_skips_visible` | -> Skip `equal_unknown_mtime` |
| `plan_both_none_mtime_diff_size_conflicts` | both `None`, sizes differ -> Conflict |
| `plan_local_none_remote_set_diff_size_conflicts` | symmetric direction |
| `plan_pre_epoch_real_zero_still_compares` | `Some(0)` vs `Some(5000)` -> `remote_newer` as before |
| retired characterization tests | `plan_pull_remote_none_mtime_*` and `plan_status_remote_none_mtime_diff_size_uploads_as_local_newer` flipped to the new expectations **in the same commit**, each with a comment citing this decision |

#### 4c - Key identity + case-collision preflight (A2/B4)

**Lock:** v1 key identity is case-sensitive, codepoint-exact, **no NFC normalization** (preserve bytes; documented). Preflight in `build_plan` detects **case-only collisions** within either side's entity list (e.g. `Note.md` vs `note.md` on one side) and within the local/remote pairing:

| RED test | Asserts |
| -------- | ------- |
| `build_plan_case_collision_same_side_conflicts` | local has `Note.md` and `note.md` -> both keys get Conflict `case_collision` rows |
| `build_plan_case_collision_cross_side_conflicts` | local `Note.md`, remote `note.md` (different content/mtime) -> Conflict `case_collision`, never auto-paired as Equal |
| `case_collision_not_executed` | (Slice 5) executor skips Conflict rows |

#### 4d - Etag / local-hash policy (P1r7 etag, complements 4b)

**Lock (decision only, no planner code):** Phase 2 does **not** compare etags and never hashes local files. Real-S3 etags are MD5 only for single-part uploads and provider-dependent (R2 included), so no cross-store etag equality is portable. `--checksum` content comparison stays post-v1 per the roadmap table. Consequence: the 4b policy is the sole guard for zero-evidence pairs, and `Entity::etag` remains an opaque remote token. Decision-log entry required; one test locks that `plan()` ignores etag fields entirely.

---

### Slice 5 - Executor (`src/exec.rs`)

Applies a plan against `LocalFs` + `dyn ObjectStore`. Fully TDD against the mock store and temp vaults - no S3 needed.

**Locks (from sync-model.md "Execution order" + checklist):**

- Order: **transfers first (downloads then uploads within mode), destination deletes last**; parents-before-children on create, children-before-parents on delete.
- Folder actions are always Skip (P1 folders); no folder transfers ever execute.
- Conflict/Skip rows never mutate anything.
- **R3.3:** upload re-verifies the opened file against the planned size **and** mtime (`open_verified` from Slice 3); mismatch -> per-key error, key recorded failed, run continues, exit 1 at end.
- **R2.1 folder delete policy: option (a)** - after local deletes, `local_remove_empty_dirs_bottom_up` cleans orphan empty dirs outside the plan; remote has no folder objects so nothing to do; the characterization Skip tests stay.
- Per-key failures are isolated: one bad key never aborts the run; report collects `(key, error)`.
- Download writes go through atomic temp+rename and set mtime from the remote entity (sync-model "apply mtime after download").

**RED tests** (mock store + temp vault):

| Test | Asserts |
| ---- | ------- |
| `exec_upload_creates_remote_bytes_and_mtime` | push Upload: mock receives bytes, size, mtime |
| `exec_download_writes_file_and_mtime` | pull Download: local file bytes + mtime set |
| `exec_push_delete_removes_remote_extras` | DeleteRemote executed after uploads |
| `exec_pull_delete_removes_local_and_cleans_empty_dirs` | DeleteLocal + bottom-up empty-dir cleanup (R2.1a) |
| `exec_deletes_run_after_transfers` | instrumented mock records call order |
| `exec_conflict_and_skip_untouched` | Conflict/Skip keys: no store/local mutation |
| `exec_upload_restated_size_mismatch_fails_key` | file grows between plan and open -> key error, others continue (R3.3) |
| `exec_download_missing_remote_errors_key` | remote vanished mid-run -> key error |
| `exec_report_counts_and_failures` | report: executed counts + failure list |
| `exec_path_collision_never_executes` | 4a rows untouched |
| `exec_status_mode_mutates_nothing` | Status plan through executor = no-op (belt and braces) |

**GREEN:** `execute_plan(local, store, plan, opts) -> ExecReport`.

---

### Slice 6 - Real push/pull dispatch + exit codes (P1r-stub-exit)

**Locks:**

- `status`: exit 0 clean, 2 dirty, 1 error (unchanged).
- `push`/`pull`: execute the plan; exit **0** if all selected actions succeeded and no conflicts, **2** if the plan contained any Conflict rows (non-conflict actions still execute), **1** on any transfer failure or fatal error. Retires `run_push_stub_conflict_exit_0_placeholder` in the same commit.
- `--dry-run`: prints plan, mutates nothing, exits like `status` (2 if dirty/conflicts).
- Help text for `--delete` carries a "no confirmation yet; permanent" warning until Phase 3 rails land.

**RED tests** (`run_with_io` level, mock store + temp vault):

| Test | Asserts |
| ---- | ------- |
| `run_push_executes_uploads_exit_0` | mock store gains the file; stdout summary |
| `run_push_conflict_exit_2` | conflict fixture -> exit 2, conflict key not transferred |
| `run_push_transfer_failure_exit_1` | injected mock failure -> exit 1 + stderr names key |
| `run_pull_dry_run_mutates_nothing_exit_2` | disk unchanged, plan printed |
| `run_check_mock_removed` | `check` no longer prints "(mock)" when a real store is configured (Slice 7 fills real path) |

---

### Slice 7 - S3 backend (`src/store/s3.rs`)

Post-spike, TDD everything pure; env-gate the rest.

**Pure RED tests (no network):**

| Test | Asserts |
| ---- | ------- |
| `s3_key_mapping_applies_prefix` | `notes/a.md` -> `myvault/notes/a.md`; `list` strips the prefix back off |
| `s3_prefix_empty_ok` | no prefix -> identity mapping |
| `s3_mtime_metadata_roundtrip` | encode `Some(ms)` / `None` to headers; parse back; garbage metadata -> fall back to LastModified (sync-model mtime policy 2) |
| `s3_list_synthesizes_folders` | keys-to-entities conversion synthesizes `notes/` folder views, sorted, trailing `/` (same shape as mock) |
| `s3_error_mapping` | 404 -> `NotFound`, 403 -> `Unauthorized` (new `Error` variant per object-store.md), timeout -> `Timeout`, other -> `Other` |
| `s3_rejects_invalid_key_before_request` | `put_from`/`head` validate with `ensure_valid_key` before any client call (trait doc N1; testable via a construction that would panic on request build, or by factoring the validation into a pure helper) |
| `s3_put_rejects_folder_key` | trailing `/` -> `InvalidKey` (P1r3-put-folder-key) |
| `s3_head_response_key_validated` | head/list ingest validates keys (R2.2 completion) |
| `error_new_variants_display` | `Unauthorized`/`Timeout`/`Unavailable` Display strings |

**Env-gated integration tests** (compiled always, **skip at runtime** unless `VAULTSYNC_TEST_S3_*` is set; see Slice 10 for the harness):

| Test | Asserts |
| ---- | ------- |
| `s3_integ_put_get_head_delete_roundtrip` | bytes + mtime metadata round-trip |
| `s3_integ_list_paginates` | seed > 1000 keys (small), list returns all |
| `s3_integ_prefix_isolation` | objects under another prefix invisible |
| `s3_integ_path_style_toggle` | path-style on works against the endpoint |
| `s3_integ_streaming_put_large` | 8 MiB streamed from a counting reader: prove no full buffering (reader pulled incrementally) - R1/P1r-put-size for the real backend |

**GREEN:** `S3Store` implementing `ObjectStore` with an owned tokio runtime; construction from `Settings` (Slice 2); async fully contained (P2-d1/D2 lock).

---

### Slice 8 - Real `check` + credential chain

**Lock:** `check` = write a tiny probe object under `<prefix>.vaultsync-check-<pid>`, read it back, delete it; report ok/fail with actionable text (wrong region, bad bucket, expired credentials per object-store.md errors section). Falls back to head-bucket-only if write is denied? **No** - lock: probe failure is a failure; message explains.

**RED tests:**

| Test | Asserts |
| ---- | ------- |
| `check_probe_key_under_prefix` | pure: probe key shape correct |
| `check_success_exit_0` | mock store through the same probe path -> 0 |
| `check_failure_actionable_message` | injected 403 -> stderr mentions credentials/permissions, exit 1 |
| `resolve_settings_env_overrides_config_region` | `AWS_REGION` vs config precedence (pure, injected env) |

**Env-gated:** `check` against real bucket exit 0; against bad credentials exit 1.

---

### Slice 9 - Symlink policy (P1r4-symlink)

**Lock:** default remains **skip all symlinks below the root** (symlinked root still followed, P1r6-root-symlink). Phase 2 adds:

1. `--follow-symlinks` flag (off by default): when on, walker follows symlinks; loops guarded by canonical-path visited set; a symlink escaping the vault root is **still skipped with a warning** (never sync out-of-vault content silently).
2. When off (default), skipped symlinks surface as a **walk warning count** printed on stderr (`warning: skipped N symlink(s); use --follow-symlinks to include`), not as plan Skip rows.

**RED tests:**

| Test | Asserts |
| ---- | ------- |
| `walk_symlink_skipped_counted_by_default` | count in walk report; entities unchanged from Phase 1 |
| `walk_follow_symlinks_includes_in_vault_target` | flag on -> target file listed |
| `walk_follow_symlinks_skips_escaping_target_with_warning` | out-of-vault target never emitted |
| `walk_follow_symlinks_loop_safe` | a -> b -> a dir cycle terminates |
| `cli_parse_follow_symlinks` | flag wired (Slice 1 surface) |

---

### Slice 10 - Integration harness, manual matrix, polish, exit criteria

**Env-gated harness** (`tests/s3_integration.rs`):

- Skips (passes trivially with a printed note) unless `VAULTSYNC_TEST_S3_BUCKET` is set; optional `VAULTSYNC_TEST_S3_ENDPOINT`, `VAULTSYNC_TEST_S3_REGION`, `VAULTSYNC_TEST_S3_PREFIX`, `VAULTSYNC_TEST_S3_PATH_STYLE=1`.
- Credentials come from the ambient AWS chain (documented; tests never read secret values themselves).
- Uses a unique run prefix (`vaultsync-itest-<ts>/`) and cleans up in a `Drop`/finally pattern.
- Covers the Slice 7/8 env-gated tables plus end-to-end: build a temp vault (nested folders + binary attachment), `push`, wipe local, `pull`, byte-compare and mtime-compare. **This test is the automated half of the exit criteria.**

**Polish items (each one RED test):**

- R3 low: `format_plan_human` hides `S` rows by default; `-v` shows them (formatter gains a verbosity param).
- MSRV: pin `rust-version` in `Cargo.toml` to the oldest version actually required by the final dep tree (measure with the spike tree); CI workflow stays Phase 3.
- `--json` still rejected (Slice 1 lock) - schema stability is Phase 3.

**Manual matrix** (`doc/test-matrix.md`), executed once per endpoint row (**AWS S3**, **Cloudflare R2**):

| # | Scenario | Verify |
| - | -------- | ------ |
| 1 | `check` | exit 0, actionable failure modes tested with bad creds |
| 2 | push sample vault: nested folders + `.png` binary + unicode filename | bytes + mtimes on remote (via `aws s3 cp` back or pull into fresh dir) |
| 3 | pull into empty dir | tree identical (`diff -r` + mtime spot check) |
| 4 | modify local, push again | only changed keys transferred |
| 5 | modify remote (console), pull | remote wins per planner rules |
| 6 | push/pull with `--delete` | extras removed on the destination side only |
| 7 | conflict case | exit 2, nothing clobbered |
| 8 | prefix + path-style (R2 row) | objects land under prefix only |

**Roadmap/docs bookkeeping (final commit):**

- Decision-log entries for: D1, D2, P2-scope, P2-matrix, P2-integ, P2-toml, P2-cli, 4a/4b/4c/4d locks, R2.1(a), exit-code lock, symlink lock, etag policy.
- Tick the Phase 2 checklist boxes in [roadmap.md](../roadmap.md) that landed; explicitly re-defer any that did not (with reasons).
- README status blurb -> "Phase 2 complete".
- Walker depth cap and CI remain Phase 3 (already noted there).

---

## Deferred-checklist coverage map

Every item from the roadmap's "Phase 2 checklist" lands in exactly one place:

| Checklist item | Slice |
| -------------- | ----- |
| File-vs-folder path collision (P1r-type-collision) | 4a |
| Unknown-mtime policy (P1r-mtime-none, P1r5-mtime-pull) | 4b |
| Etag-aware equality decision (P1r7) | 4d |
| Real push/pull exit codes (P1r-stub-exit) | 6 |
| Force-flag CLI surface (P1r-both-forces) | 1 |
| Real backend streaming put (P1r-put-size) | 7 (`s3_integ_streaming_put_large`) |
| Folder + `--delete` policy (R2.1) | 5 (option a) |
| Remote ingest: head validation + `key_to_local_path` single join (R2.2) | 3, 7 |
| Key identity / case-collision preflight (A2/B4) | 4c |
| Symlink policy (P1r4-symlink) | 9 |
| Symlink-swap TOCTOU (P1r7) | 3 |
| Folder mtime asymmetry constraint (P1r4-folder-mtime) | constraint only; 4b tests must not compare folder mtimes |
| Walker depth cap | **stays Phase 3** (noted in roadmap; no code) |
| MSRV pin | 10 (CI stays Phase 3) |
| Executor re-stat after read (R3.3) | 3 + 5 |
| Skip-row output policy (R3 low) | 10 |
| `--vault=-foo` / `--` escape hatch (P1r5) | 1 |

---

## Out of scope (do not sneak into Phase 2)

| Item | Where it belongs |
| ---- | ---------------- |
| `--yes` / `--max-delete` / confirm prompts | Phase 3 (delete safety) |
| Ignore patterns / Obsidian profile | Phase 3 |
| Concurrency limits, retries/backoff | Phase 3 (executor is sequential in Phase 2) |
| Multipart upload | post-v1 (note in `store::s3` docs if size limits hit) |
| `--json` schema | Phase 3 |
| Lock file | Phase 3 |
| Walker depth cap | Phase 3 |
| CI workflow | Phase 3 (MSRV pin lands here) |
| `--checksum` / local hashing | post-v1 (4d lock) |
| Conflict copies, encryption, bidirectional sync | post-v1 |

If a test seems to need an out-of-scope feature, narrow the test.

---

## Implementation order checklist

- [x] Slice 0 - S3 spike; D1/D2 decision-log entries; spike notes
- [x] Slice 1 - clap migration; global flags; escape hatches; parse suite ported green
- [x] Slice 2 - `config.rs` TOML + `resolve_settings` precedence
- [x] Slice 3 - `key_to_local_path` single join; reader/writer/delete; TOCTOU guards
- [x] Slice 4a - path-collision Conflict
- [x] Slice 4b - unknown-mtime policy (characterization flips + log entry)
- [x] Slice 4c - case-collision preflight
- [x] Slice 4d - etag policy decision logged (no code)
- [x] Slice 5 - executor with ordering, isolation, R3.3 verify, R2.1(a) cleanup
- [x] Slice 6 - real push/pull dispatch + exit codes (placeholder test retired)
- [x] Slice 7 - `S3Store`: pure parts unit-tested; env-gated integration tests
- [x] Slice 8 - real `check` probe
- [x] Slice 9 - `--follow-symlinks` + skip warnings
- [x] Slice 10 - integration harness, manual matrix (AWS + R2) executed, polish, MSRV pin, docs/decision-log updates, roadmap boxes ticked
- [ ] `cargo test` green offline throughout; env-gated suite green on AWS and R2

---

## Definition of done

1. `cargo test` (offline) green; all Phase 1 tests still green except characterization tests explicitly flipped with decision-log citations.
2. Env-gated integration suite green against **AWS S3** and **Cloudflare R2**; `doc/test-matrix.md` manual rows checked off for both.
3. `vaultsync push` / `pull` against a real bucket move a sample vault (nested folders + binary attachment) byte-identically with mtimes preserved.
4. Exit codes: status 0/2/1, push/pull 0/2/1 as locked in Slice 6.
5. Only approved crates in `Cargo.toml`; async confined to `store::s3`.
6. Every roadmap Phase 2 checklist item landed or explicitly re-deferred with a decision-log entry.
7. No network access in the default build/test path; no secrets in the repo, config schema, or test code.

---

## Risk notes

| Risk | Mitigation |
| ---- | ---------- |
| aws-sdk dependency tree / compile time blows up | Slice 0 records tree size, release build time, binary delta, and minimal feature set; user judges before D1 close. Rejection re-opens D1 - no auto-fallback to `rust-s3` or a second S3 stack |
| Async leaks out of the backend | D2 containment lock; code review; no `async` outside `store::s3` without a log entry |
| Accidental convenience crates ride in with the SDK | Spike/Cargo.toml review: no `tracing`/`anyhow`/`thiserror`/etc. unless explicitly approved; prefer SDK defaults already in-tree |
| Flaky env-gated tests mask real breakage | Gates are explicit and loud (skipped tests print why); matrix doc requires a clean manual pass per endpoint before ticking boxes |
| mtime-None policy flip breaks Phase 1 characterization tests | Same-commit flips with citations (Slice 4b); never silent edits |
| R2 quirks (metadata, checksums, path-style) | Slice 0 probes R2 early; quirks documented in spike notes and test-matrix.md |
| Executor data-loss bugs | Mock-first TDD; ordering locks; atomic download writes; conflicts never execute; per-key failure isolation |
| Scope creep (full checklist is large) | Slices are independently shippable; if cut is needed, cut from the end (9, then 10 polish), never from 3-7 |
| clap migration churns the parse suite | Port existing tests first, extend second; behavior parity locked before new flags |

---

## First commit suggestion (per slice, green throughout)

1. `spike: official S3 stack probe (aws-sdk-s3 + aws-config + tokio)` + D1/D2 log entries
2. `refactor: migrate CLI to clap (behavior parity)`
3. `feat: TOML config + settings resolution`
4. `feat: local reader/writer with single key join + TOCTOU guards`
5. `feat: planner path-collision, unknown-mtime, case-collision policies`
6. `feat: executor for push/pull plans`
7. `feat: real push/pull/check dispatch + exit codes`
8. `feat: S3 ObjectStore backend`
9. `feat: --follow-symlinks policy`
10. `test: env-gated S3 integration suite + docs: test matrix, roadmap updates`
