# Issue 27 plan: live sync progress on TTY (tqdm-style bar for push/pull)

**Status:** implemented (landed as PR #27 - all cycles green; baseline gate
recorded below; acceptance mapping + manual TTY smoke in the PR description)
**Issue:** https://github.com/tlkahn/vaultsync/issues/27 (OPEN,
enhancement; tqdm-style progress for the binary)
**Branch:** worktree-live-sync-progress-on-tty
**Design refs:** [issue #27](https://github.com/tlkahn/vaultsync/issues/27),
[architecture.md](../architecture.md) (executor: "streams progress events to
the CLI"), [cli.md](../cli.md) (push/pull, `--json`), [roadmap.md](../roadmap.md)
(Phase 4 polish)
**Verified baseline:** `9de24a8` - rerun the gate at implementation start:
`cargo test --offline` (408 lib + 4 non-skipped integration-support tests at
baseline; full `s3_integ_*` remains env-gated per #6/#17), then
`cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`.

**Implementation record (cycle 0/9 bookkeeping):** branch point
`9de24a8`, baseline gate green (408 lib + 16 integration, clippy/fmt clean),
`execute_plan(` call sites = 62 (cli 1, exec 52, lib 2, integration 7) - all
stay source-compatible via the wrapper (I27-api). Final gate after cycle 9:
434 lib + 16 integration, clippy/fmt clean. New tests landed:
`progress_event_variants_carry_fields`, `no_progress_accepts_events`,
`progress_is_send_sync` (C1); `execute_plan_with_progress_emits_pass_and_key_events_push`,
`..._emits_failure_event_and_run_end_counts`, `..._skips_empty_passes` (C2);
`..._events_complete_under_concurrency`, `..._key_done_bytes_match_plan` (C3,
verification cycle - passed on the C2 implementation, landed as regression
pins); `execute_plan_wrapper_matches_with_progress_report`,
`execute_plan_wrapper_emits_no_events` (C4); `progress_line_renders_pass_progress`,
`progress_line_bar_width_and_fill`, `progress_line_shows_rate_and_eta_after_first_byte`,
`progress_line_truncates_long_keys`, `progress_line_zero_total_pass_renders_nothing` (C5);
`tty_renderer_refreshes_single_line_and_clears`,
`tty_renderer_final_line_contains_counts_and_100`, `quiet_renderer_writes_nothing`,
`renderer_flushes_writer` (C6); `progress_mode_auto_uses_stderr_is_terminal`,
`run_seam_defaults_progress_off_for_captured_io` (C7);
`push_always_progress_writes_bar_frames_to_stderr`,
`push_off_progress_writes_nothing`, `pull_always_progress_writes_download_frames`,
`dry_run_and_status_emit_no_progress`, `progress_does_not_change_exit_codes` (C8).
**Sibling issues:** #24 (SIGINT drain - land order flexible, note interaction
below), #12 (`--json` schema stability - progress must never touch stdout).

---

## Problem recap (from the issue, verified against the tree)

- `dispatch_plan` (`src/cli.rs:479`) prints the plan summary to stdout, then
  calls `crate::exec::execute_plan(...)` (`src/cli.rs:508-509`) with no
  progress callback. Warnings/failures print only after the run
  (`src/cli.rs:510-515`). On a large first-time push the binary looks dead.
- `execute_plan` (`src/exec.rs:53`) returns only an aggregate `ExecReport`
  (`executed` / `failed` / `warnings`); there is no event stream.
  `doc/architecture.md` already sketches "streams progress events to the
  CLI" as the executor's contract, but it was never wired.
- The bounded pool (`src/pool.rs`) finishes keys out of order under
  `[transfer].concurrency > 1`; progress must be completion-driven, not
  plan-order-driven.
- Existing offline suite captures stdout/stderr as `Vec<u8>` via
  `run_with_io` seams (`src/cli.rs:295`); any TTY behavior must be
  test-injectable, and default test behavior must stay silent (so the 408
  existing tests keep their captured stderr contracts without churn).

## Locked decisions

| ID | Decision | Choice |
| -- | -------- | ------ |
| I27-shape | Output channel | **stderr only, always.** Progress never writes to stdout: stdout stays the plan text today and the `--json` stream later (#12). Human progress lines/bars go to stderr; the issue's sample block lives on stderr. |
| I27-tty | When it shows | **Auto: TTY only.** Real binary renders progress when `std::io::stderr().is_terminal()`; piped/redirected stderr stays quiet. No `--progress` flag in this issue (a future `--progress=always/never` can ride on the same seam without changing this behavior). |
| I27-json | `--json` | **No progress under `--json`.** The flag is rejected as "not implemented" today (Phase 3, #12), but the lock stands for when it lands: machine stdout must never see progress bytes. |
| I27-api | Library seam | **`&dyn Progress` parameter on a new public `execute_plan_with_progress`, with `execute_plan` kept as the thin wrapper that passes `&NoProgress`.** No new default type-parameter on `execute_plan` (that changes its type for every caller and reads worse in docs); the wrapper keeps the ~50 existing offline/test call sites (`src/exec.rs` tests, `src/lib.rs` tests, `tests/s3_integration.rs`) compiling unchanged and preserves today's exact signature for library consumers. |
| I27-events | Event model | **Coarse, pass-scoped events emitted on completion**, not per-byte: `PassStart { kind: PassKind, total_keys: u32, total_bytes: u64 }`, `KeyDone { kind, key, bytes: u64, ok: bool }`, `PassEnd { kind }`, and one terminal `RunEnd { executed: u32, failed: u32 }` after the final pass fold. No per-chunk byte streaming this issue (multipart is post-v1 anyway). Emission is completion-driven (correct under the pool); key order in events may interleave with concurrency > 1 and is not asserted as plan-ordered. |
| I27-thread | Concurrency safety | **`pub trait Progress: Send + Sync`** with a no-arg `fn event(&self, ev: ProgressEvent)`. Worker threads call it directly under `run_bounded`; implementors serialize internally (the CLI renderer holds a `Mutex<ProgressState>`). The closure stays `Sync` per the existing `run_bounded` bound (`src/pool.rs:21`). |
| I27-home | Module | **New `src/progress.rs`, `pub mod progress;` in `src/lib.rs`.** It owns the event types, the `Progress` trait + `NoProgress`, the pure `ProgressLine` state/formatter, the TTY/non-TTY renderers, and the `ProgressMode` seam. `exec` emits, `cli` renders; neither depends on the other's rendering. |
| I27-render | Bar content | **One line per active pass, refreshed with `\r` + ANSI clear-to-EOL (`\x1b[K`) on TTY.** Content: `Uploading  847/1204  [=========>--------]  70%  12.4 MB/s  ETA 0:01:12` (pass verb, done/total keys, bar, percent, smoothed byte rate, ETA). Passes with `total_keys == 0` render nothing. On pass end the line is finalized with a newline. Non-TTY renderer writes nothing. |
| I27-rate | Rate/ETA | **Cumulative rate only: `bytes_done / elapsed`** for the current pass; ETA = `bytes_remaining / rate`. No sliding window, no exponential smoothing this issue (documented accepted simplification; jittery ETA on a mixed-size vault is acceptable v1). Rate/ETA are `None` until at least one byte-complete event and a non-zero elapsed, so the line shows no fake numbers at pass start. |
| I27-width | Bar width | **Fixed 20-cell bar** (10 chars `[` + 20 cells + `]`). No terminal-width detection this issue (std-only has no `ioctl` TIOCGWINSZ; a future `terminal_size`/`indicatif` add would need dependency-policy confirmation). Long keys are truncated in the line. |
| I27-bytes | Byte totals | **Planned bytes from the `Plan` actions:** Upload/Download use the source-side entity size; deletes contribute 0 bytes but still advance key counts. `total_bytes` is informational; key counts are the primary progress signal. |
| I27-verbose | `-v` interplay | **No special per-key mode under `-v` in this issue.** `-v` already lists Skip/Conflict rows in the plan summary; progress stays the aggregate bar regardless of verbosity. (A future `-vv` per-key stream can layer on the same event feed.) |
| I27-test | Test injection | **`ProgressMode` enum (`Auto` / `Off` / `Always`) carried on `PlanFlags` / the run seam.** `Auto` = real `is_terminal()` check (used by the real binary path); tests default to `Off` so captured-stderr contracts are untouched; CLI progress tests pass `Always` with a captured stderr buffer and assert on the byte stream (including `\r` / `\x1b[K` frames). No global-state hacks. |
| I27-deps | Dependencies | **Std-only.** Manual bar + `std::time::Instant` + `std::io::IsTerminal`. No `indicatif`/`terminal_size`/etc. in this issue (Rust dependency policy); revisit only with explicit user confirmation. |
| I27-sigint | #24 interplay | **No interaction implemented here.** The renderer only reads executor events; when #24 (SIGINT drain) lands it gets the same events for free (a drained run simply emits fewer `KeyDone`s and the final `RunEnd`). This issue must not write anything that assumes the run completes. |

## Method: strict fine-grained TDD

Same rules of engagement as the issue-8/15/20 plans
([issue-8.md](issue-8.md), [issue-15.md](issue-15.md),
[issue-20.md](issue-20.md)):

1. **RED** - named failing test first, exercising a production API; confirm it
   fails for the right reason. Compile-RED is a legitimate RED for the
   `Progress` trait / `execute_plan_with_progress` signature (precedent:
   I20 cycle 1 supertraits).
2. **GREEN** - smallest implementation that passes.
3. **REFACTOR** - behavior-preserving cleanup on green.
4. One logical behavior per cycle; per-cycle gate:
   `cargo test --offline`
   `cargo clippy --all-targets -- -D warnings`
   `cargo fmt --check`
5. **No network in the default suite.** All behavior is pinnable offline:
   executor events via mock store + temp vaults + a `RecordingProgress`
   test double; rendering via a captured `Vec<u8>` stderr writer; the only
   "TTY-ness" is the injected `ProgressMode`, so no pseudo-terminal harness
   is needed.

## Cycles

### Cycle 0 - pre-flight

- Baseline gate green on the branch point (record the commit + counts at the
  top of this file when implementation starts).
- Confirm the `execute_plan` call-site list is stable
  (`grep -rn "execute_plan(" src/ tests/`): ~50 sites, all expected to stay
  source-compatible via the wrapper (I27-api).

### Cycle 1 - progress module scaffold: events, trait, `NoProgress`

- **RED** (compile-RED + tiny behavior tests in `src/progress.rs`):
  - `progress_event_variants_carry_fields` - construct each
    `ProgressEvent` variant (`PassStart`/`KeyDone`/`PassEnd`/`RunEnd`) and
    assert field values round-trip (locks the event shape).
  - `no_progress_accepts_events` - `NoProgress.event(..)` is a callable
    no-op for every variant (locks the default sink).
  - `progress_is_send_sync` - `fn assert_ss<T: ?Sized + Send + Sync>() {}`
    over `dyn Progress` (locks I27-thread).
  - Fails to compile (no `progress` module) = RED.
- **GREEN**: create `src/progress.rs` with `PassKind`, `ProgressEvent`,
  `pub trait Progress: Send + Sync { fn event(&self, ev: ProgressEvent); }`,
  `pub struct NoProgress` + impl; `pub mod progress;` in `src/lib.rs`.
  Keep it free of rendering this cycle.
- **REFACTOR**: none expected.

### Cycle 2 - executor emits pass/key events (sequential path first)

- **RED** (in `src/exec.rs` tests, using a `RecordingProgress` test double
  with a `Mutex<Vec<ProgressEvent>>`):
  - `execute_plan_with_progress_emits_pass_and_key_events_push` - a 3-upload
    plan at `concurrency = 1` against the mock store: events are exactly
    `PassStart(Upload, total_keys=3, total_bytes=sum)` then three
    `KeyDone { kind: Upload, ok: true, .. }` (one per key, order asserted
    only as a set under concurrency 1), then `PassEnd(Upload)`, then
    `RunEnd { executed: 3, failed: 0 }`.
  - `execute_plan_with_progress_emits_failure_event_and_run_end_counts` -
    a 2-upload plan where one key fails (e.g. a `FailPutStore`-style
    wrapper): the failing key's `KeyDone` has `ok: false`, and
    `RunEnd { executed: 1, failed: 1 }` matches the `ExecReport`.
  - `execute_plan_with_progress_skips_empty_passes` - a pure-download pull
    plan emits no `PassStart(Upload)`/`PassEnd(Upload)` (zero-item passes
    render nothing, I27-render).
  - RED because `execute_plan_with_progress` does not exist yet
    (compile-RED).
- **GREEN**: add
  `pub fn execute_plan_with_progress(local, store, plan, mode, opts,
  concurrency, progress: &dyn Progress) -> ExecReport`;
  move the current `execute_plan` body into it; thread `progress` through
  each pass: emit `PassStart` before `run_bounded`, wrap the per-key closure
  so each completed item emits `KeyDone` (bytes = the action's planned byte
  size; `ok` from the result), emit `PassEnd` after the fold; emit `RunEnd`
  once after the final pass (and after the empty-dir post-pass, with counts
  from `rep`). Keep the `mode == Mode::Status` early-return silent (no
  events). `execute_plan` becomes a one-line wrapper:
  `execute_plan_with_progress(.., &NoProgress)`.
- **REFACTOR**: factor the repeated per-pass event emission into a small
  local helper if the four passes duplicate more than a couple of lines;
  keep behavior identical.

### Cycle 3 - event correctness under the bounded pool

- **RED**:
  - `execute_plan_with_progress_events_complete_under_concurrency` - 16-key
    push at `concurrency = 4` with an adversarial mock store (reverse-sleep
    like `run_bounded_results_in_input_order`): exactly one `PassStart`,
    exactly 16 `KeyDone`s with 16 distinct keys, exactly one `PassEnd`,
    `RunEnd { executed: 16, failed: 0 }`. Completion order is not asserted.
  - `execute_plan_with_progress_key_done_bytes_match_plan` - sum of `bytes`
    over `KeyDone`s equals the `PassStart.total_bytes` for a mixed-size
    plan.
  - Both are expected to pass on the cycle-2 implementation *or* fail on a
    real race (e.g. if emission was accidentally tied to plan-order fold);
    either way they are the RED/GREEN gate for pool-safety - if they pass
    immediately, they land as regression pins and the cycle is a
    verification cycle (documented in the cycle notes, same as I20's
    cycle-2 spike outcome).
- **GREEN**: if a race surfaces, fix emission to be completion-driven (emit
  inside the per-item closure or immediately on slot fill, not from the
  plan-order fold) while keeping `ExecReport` assembly plan-ordered and
  unchanged.
- **REFACTOR**: none expected.

### Cycle 4 - `execute_plan` wrapper stays behavior-identical

- **RED**:
  - `execute_plan_wrapper_matches_with_progress_report` - run the same plan
    through `execute_plan` and through `execute_plan_with_progress(..,
    &NoProgress)`; assert identical `ExecReport`s and identical store state
    (the I20 `exec_report_is_deterministic_under_pool` shape, reused).
  - `execute_plan_wrapper_emits_no_events` - tautological but pins that the
    wrapper uses `NoProgress` (guard via a compile-time type check that the
    wrapper body references `NoProgress`, or simply review-lock; prefer the
    report-equality test above as the real pin).
- **GREEN**: already green from cycle 2's wrapper; land the equality test as
  the regression pin.
- **REFACTOR**: none expected.

### Cycle 5 - pure progress-line state and formatting

- **RED** (in `src/progress.rs`):
  - `progress_line_renders_pass_progress` - feed a scripted event sequence
    into a `ProgressLine` state machine (no IO) and assert the rendered
    string at a few checkpoints: correct `done/total`, percent, bar fill
    count, and verb per `PassKind` (`Uploading`/`Downloading`/`Deleting
    remote`/`Deleting local`).
  - `progress_line_bar_width_and_fill` - 20-cell bar: 0%, 50%, 100% render
    exact cell counts; partial cell at >0% uses the `>` head.
  - `progress_line_shows_rate_and_eta_after_first_byte` - before any byte
    `KeyDone`, no rate/ETA text; after scripted byte events at scripted
    timestamps (inject a `now: Instant` or elapsed-ms into the state machine
    so the test is deterministic), cumulative rate and ETA appear and match
    hand-computed values.
  - `progress_line_truncates_long_keys` - a very long key is truncated to a
    fixed column budget with no overflow.
  - `progress_line_zero_total_pass_renders_nothing` - `total_keys == 0`
    yields no line (matches cycle 2's skip-empty-pass behavior).
- **GREEN**: implement `ProgressLine` as a pure state machine
  (`on_event(ProgressEvent, now)` mutating counters; `render() -> String`)
  with a small human-bytes formatter (`B`/`KiB`/`MiB`/`GiB`, one decimal)
  and `hh:mm:ss`/`m:ss` ETA formatting. Deterministic: time is a parameter.
- **REFACTOR**: extract the byte/ETA formatters into small pure fns with
  their own unit tests if they grow branches.

### Cycle 6 - renderers: TTY line refresh + quiet non-TTY

- **RED**:
  - `tty_renderer_refreshes_single_line_and_clears` - drive a
    `TermProgress<ProgressLine>` over a captured `Vec<u8>` writer with a
    scripted event stream; assert the byte stream contains `\r` and
    `\x1b[K` refreshes, never `\n` mid-pass, and ends the pass with a
    newline; assert the final visible line equals the `ProgressLine`
    render.
  - `tty_renderer_final_line_contains_counts_and_100` - after `PassEnd`
    the last frame shows `total/total` and `100%`.
  - `quiet_renderer_writes_nothing` - the non-TTY renderer receives the same
    event stream and writes zero bytes.
  - `renderer_flushes_writer` - each refresh is flushed (assert on a
    counting writer that records flush calls, so a buffered cursor can't
    freeze the bar).
- **GREEN**: implement `TermProgress` (wraps a `&mut dyn Write` +
  `ProgressLine`; on `KeyDone`/`PassStart` re-render with
  `\r{line}\x1b[K`, on `PassEnd` append `\n`) and a `QuietProgress` no-op
  writer; both behind the `Progress` trait (renderer itself holds the
  writer, so `event()` serializes via its own internal `Mutex` to satisfy
  `Sync`).
- **REFACTOR**: none expected.

### Cycle 7 - `ProgressMode` selection + real-binary TTY check

- **RED**:
  - `progress_mode_auto_uses_stderr_is_terminal` - a tiny seam
    (`fn progress_mode_for(auto: ProgressMode, is_tty: bool) ->
    ResolvedMode`) unit-tested: `Auto`+tty -> render, `Auto`+not-tty ->
    quiet, `Off` -> quiet, `Always` -> render. (The `is_tty` bool is the
    injectable stand-in for `stderr().is_terminal()`.)
  - `run_seam_defaults_progress_off_for_captured_io` - existing
    `run_with_io`-based CLI tests still see empty progress on their captured
    stderr unless they opt in (this is a no-change assertion guarding
    I27-test).
- **GREEN**: add `ProgressMode { Auto, Off, Always }` and the resolution
  seam; wire the real binary path to compute `stderr().is_terminal()` once
  in `run`/`main` and carry the resolved mode; the library `run_with_io`
  seam gains a `ProgressMode` parameter (defaulted by callers, compile-driven).
- **REFACTOR**: none expected.

### Cycle 8 - CLI wiring: push/pull render progress on the real path

- **RED** (CLI-level, `run_with_io` + `ProgressMode::Always` + captured
  stderr `Vec<u8>`):
  - `push_always_progress_writes_bar_frames_to_stderr` - a push run with
    several uploads: captured stderr contains at least one `\r` frame with
    `Uploading` and a final `n/n` frame; stdout still contains the plan
    text and no progress bytes.
  - `push_off_progress_writes_nothing` - same run with `ProgressMode::Off`:
    captured stderr has no `\r`/bar frames (warnings/failures only), stdout
    unchanged. Locks I27-test default silence for the existing suite.
  - `pull_always_progress_writes_download_frames` - a pull run shows
    `Downloading` frames.
  - `dry_run_and_status_emit_no_progress` - `--dry-run` push and `status`
    produce no progress frames even under `ProgressMode::Always` (executor
    not run / `Mode::Status` early-return).
  - `progress_does_not_change_exit_codes` - same runs assert the existing
    0/1/2 exit-code table is unchanged (conflict -> 2, failure -> 1).
- **GREEN**: in `dispatch_plan`, resolve the mode, build the renderer
  (`TermProgress` over real stderr when rendering, else a quiet sink), and
  call `execute_plan_with_progress(.., renderer)` instead of
  `execute_plan`. Ensure `--json` runs never construct a renderer (I27-json;
  today they exit before dispatch anyway, but keep the guard explicit).
- **REFACTOR**: keep the renderer construction in one small helper so a
  future `--progress=` flag lands in one place.

### Cycle 9 - docs + decision-log + acceptance sweep

- Update `doc/cli.md` push/pull sections with a short "Progress" note
  (stderr, TTY-only, `--json` clean).
- Update `doc/architecture.md` only if the "streams progress events" bullet
  needs a pointer to `src/progress.rs` (it is now accurate).
- Roadmap: add a dated decision-log row for I27 (shape/tty/json/api/deps
  locks, e.g. `I27-progress` summarizing the table above).
- Close the loop on #27: confirm each Acceptance box from the issue maps to
  a landed test (list the test names in the PR description).
- Final gate: `cargo test --offline`, `cargo clippy --all-targets --
  -D warnings`, `cargo fmt --check`, plus one manual TTY smoke on a scratch
  vault (document the observed bar in the PR, since CI has no TTY).

## Out of scope (explicitly)

- Per-byte / multipart chunk progress (post-v1 with multipart).
- Terminal-width-aware bars, unicode block chars, colors, multi-line or
  per-file nested bars.
- A `--progress=` / `--no-progress` CLI flag (the `ProgressMode` seam is the
  extension point).
- Sliding-window / smoothed rates, spdlog-style throttling of refresh rate
  (a simple "render on every event" is fine at vault scale; if a pathological
  vault shows flicker, add a time-based refresh throttle as a follow-up).
- Any change to `ExecReport` semantics, exit codes, plan ordering, or the
  retry/concurrency knobs.
- Any new external dependency.

## Risks / watch-items

- **Captured-stderr contract churn:** the whole existing CLI suite asserts on
  captured stderr; defaulting tests to `ProgressMode::Off` (I27-test) keeps
  them green without edits. Verify cycle 7/8 introduce no incidental writes.
- **Concurrent emission ordering:** events interleave under the pool; tests
  must assert set/count semantics, never exact cross-key ordering (cycle 3).
- **Renderer `Sync`:** `TermProgress` owns a `&mut dyn Write`; wrap it in a
  `Mutex` inside the `Progress` impl so worker-thread `event()` calls are
  serialized (pin via a compile-time `Send + Sync` assertion).
- **Time in tests:** never wall-clock; `ProgressLine` takes time as a
  parameter (cycle 5) so rate/ETA assertions are deterministic.
- **#24 ordering:** if #24 (SIGINT drain) lands first, re-verify the
  `RunEnd`-on-partial-run semantics still hold; if this lands first, note in
  the #24 plan that a drained run emits a final `RunEnd` with partial counts.
