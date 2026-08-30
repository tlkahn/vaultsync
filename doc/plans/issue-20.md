# Issue 20 plan: bounded transfer concurrency (std::thread worker pool; `[transfer].concurrency` goes live)

**Status:** planned
**Issue:** https://github.com/tlkahn/vaultsync/issues/20 (OPEN, P3-6b;
part of #14 Phase 3; split from #8)
**Branch:** worktree-bounded-transfer-cncrrncy-std-thrd-wrkr-pl-trnsfr-cncrrncy-gs-lv
**Design refs:** [object-store.md](../object-store.md), [cli.md](../cli.md),
[roadmap.md](../roadmap.md) (Phase 3 item 3 + decision log), W48 / W28-M6 /
W82 / W13 / W62 / W119 / R3.3
**Verified baseline:** `a2fca0a` (Issue 8 merged, #21) - rerun the gate at
implementation start:
`cargo test --offline -- --skip s3_integ_list_paginates`
(367+ lib tests, paginate test skipped per #17);
`cargo clippy --all-targets -- -D warnings`; `cargo fmt --check`.
**Blocker check:** resolved - #8 (retries) is CLOSED/merged at `a2fca0a`.

---

## Problem recap (from the issue, verified against the tree)

- `[transfer].concurrency` is parsed (`src/config.rs:77`, default
  `DEFAULT_CONCURRENCY = 4`) but inert; every run with an explicitly-set
  divergent value prints the W28/M6 "Phase 3 feature, transfers stay
  sequential" warning (`src/cli.rs:669-681`, tests at 1487-1540).
- `execute_plan` (`src/exec.rs:42`) runs all four passes sequentially:
  downloads, uploads, `DeleteRemote`, `DeleteLocal` (+ the sequential
  empty-dir post-pass). Per-key guards live inside each per-key fn
  (`check_destination` W13/W22, `open_verified` R3.3, head-before-delete
  W62/W119) and are already pass-order-safe.
- `enrich_with_head_mtimes` (`src/store/mod.rs`) issues N sequential
  `head()` calls per list (the ~N x RTT stall from #15, visible on
  `s3_integ_list_paginates`-scale vaults). It is `pub(crate)`, called from
  `S3Store::list` (W113 wiring) and the `S3LikeListStore` test double
  (`src/lib.rs:406`).
- `ObjectStore` has no `Send + Sync` supertraits; `S3Store` holds a
  current-thread `tokio::runtime::Runtime` + aws `Client` (W48,
  `src/store/s3.rs:92-97`). `LocalFs` is already Mutex-based for exactly
  this (W82, `src/local.rs:32-34`).
- Test doubles that will break under the supertraits (compile-driven fix
  list, verified by grep): `S3LikeListStore` (`RefCell<Vec>` head_log,
  `src/lib.rs`), `HeadFailStore` / `FlakyHeadStore` (`Cell<usize>`,
  `src/store/mod.rs`). `MemoryStore` (`Mutex`), `RecordingStore` (`Mutex`),
  and the cli.rs stub doubles (`FailPutStore`, `CheckFailStore`,
  `WarnListStore`) look fine but are compile-verified in cycle 1.

## Locked decisions

| ID | Decision | Choice |
| -- | -------- | ------ |
| I20-mech | Pool mechanism | **`std::thread::scope` + `AtomicUsize` work pull**, std-only (no new deps, per dependency policy). Workers = `min(concurrency, items)`; each worker `fetch_add`s the next item index; per-item results stored by index; report assembled in input order after `scope` joins. Worker panic propagates out of `scope` exactly as a panic in today's sequential loop would (no swallowing). The helper's home is locked: **`crate::pool` (`src/pool.rs`) from cycle 3**, `pub(crate)` - cycle 5's parallel heads consume it from `store::mod`, and `store` must not depend on `exec`, so it is never parked under `exec` first. |
| I20-one | `concurrency = 1` | **Dedicated sequential path.** `concurrency <= 1` runs the existing loop body verbatim on the caller thread - byte-for-byte today's behavior (acceptance criterion), no threads spawned. Pinned by a same-thread-id test + report-equality tests. |
| I20-traits | Supertraits | `pub trait ObjectStore: Send + Sync`. Compile-time assertion tests pin `S3Store`, `MemoryStore`, `LocalFs`, and `dyn ObjectStore` (`fn _assert<T: Send + Sync>()`-style). Test doubles migrate `Cell`/`RefCell` -> `Mutex`/`AtomicUsize` (behavior-neutral harness change). |
| I20-runtime | W48 runtime | **Spike first (cycle 2), branch on evidence.** Offline probe: N threads concurrently `block_on` futures that await tokio timers + `tokio::spawn` chains on one current-thread runtime; assert (a) all complete correctly, (b) wall-clock proves real overlap (two 400ms sleeps finish well under 800ms) - the failure mode to rule out is the current-thread core serializing concurrent `block_on` calls. If it passes: keep current-thread, decision-log entry extends W48 with the concurrency evidence. If it fails/serializes: switch to `new_multi_thread().worker_threads(concurrency).enable_all()` and amend W48 - and re-add tokio's `rt-multi-thread` feature to `Cargo.toml` in the same commit (W74 trimmed the features to `["rt"]` for the current-thread runtime; `Builder::new_multi_thread` does not compile without it). Either way the decision log is updated (acceptance criterion). |
| I20-plumb | Plumbing | `execute_plan(.., opts, concurrency: u32)`; `enrich_with_head_mtimes(store, listing, concurrency: u32)`; `S3Store::new(settings, retry, concurrency: u32)` (heads cap + runtime worker count if the multi-thread branch lands). CLI dispatch passes `settings.concurrency`. All call sites updated compile-driven; tests pass `1` unless exercising the pool. |
| I20-heads | Head cap | **Same `[transfer].concurrency` cap** (no dedicated head cap - the issue allows either; one knob keeps the config surface at its locked D-config-only size). Heads fan out with the same pool shape; fail-closed-on-first-non-NotFound semantics and entity order are preserved exactly (results reassembled by index; vanished-key warning built in listing order). **Error ordering locked:** with several non-NotFound head failures in one listing, the returned error is the first one in **listing order** (deterministic, independent of completion order); a hard error still fails the whole listing - no partial entities, no vanished warning (the warning is built on the success path only) - and in-flight heads are not cancelled (extra completed requests accepted, documented in the function doc). |
| I20-deletes | Pass 3 scope | All four kinds fan out: downloads, uploads, `DeleteRemote`, `DeleteLocal` (local deletes are cheap but uniform; the empty-dir post-pass stays sequential after, fed by `deleted_keys` in deterministic plan-index order). Guards stay per-key inside the worker: R3.3, W13/W22, W62/W119, W39 - untouched code, now called concurrently. |
| I20-config | Validation | `concurrency >= 1` enforced loudly in `resolve_settings` (W56 ethos, `Error::Other` naming `transfer.concurrency`), matching the I8 retry validation shape. `0` rejects; `1` is valid (sequential). |
| I20-cli | Warning removal | W28/M6 warning block deleted; its tests (`run_warns_on_configured_concurrency`, `run_does_not_warn_on_explicit_default_concurrency`, `run_does_not_warn_without_configured_concurrency`) replaced by one no-warning pin. `concurrency_explicitly_set` is deleted with it (its only consumer is the warning; compile-driven). `--concurrency` stays rejected as unknown (cli.rs:1106 test untouched). D2 untouched: no async leaks out of `store::s3`. |

## Method: strict fine-grained TDD

Same rules of engagement as the issue-8/15 plans
([issue-8.md](issue-8.md), [issue-15.md](issue-15.md)):

1. **RED** - named failing test first, exercising a production API; confirm
   it fails for the right reason. Compile-RED is a legitimate RED for
   signature/supertrait changes (precedent: W52/W61, issue-8 cycle 2).
2. **GREEN** - smallest implementation that passes.
3. **REFACTOR** - behavior-preserving cleanup on green.
4. One logical behavior per cycle; per-cycle gate (note the #17 skip flag):
   `cargo test --offline -- --skip s3_integ_list_paginates`
   `cargo clippy --all-targets -- -D warnings`
   `cargo fmt --check`
5. **No network in the default suite.** The pool, enrichment, config, and
   CLI behavior are all pinnable offline (mock store + temp vaults +
   instrumented test stores). Real-S3 parallel correctness lands only in the
   env-gated suite (cycle 9). **Concurrency tests must use gauges
   (max-in-flight atomic counters, completion-order shuffles), never
   wall-clock assertions** - the single allowed exception is the cycle-2
   W48 spike, which uses generous margins (>2x headroom) and is explicitly a
   probe, not a regression pin.

## Cycles

### Cycle 0 - pre-flight

- Baseline gate green on the branch point (record result at the top of this
  file when implementation starts).
- Confirm the compile-driven fix list: `grep -rn "impl.*ObjectStore for"`
  and `grep -n "RefCell\|Cell" src/ tests/` - expected: `S3LikeListStore`,
  `HeadFailStore`, `FlakyHeadStore` need `Mutex`/`Atomic` migration.

### Cycle 1 - `Send + Sync` supertraits + compile-time assertions

- **RED**: assertion tests (compile-RED):
  - `store::tests::object_store_is_send_sync` - `fn assert_ss<T: ?Sized +
    Send + Sync>() {}; assert_ss::<dyn ObjectStore>();`
  - `store::s3::tests::s3_store_is_send_sync` and
    `store::mock::tests::memory_store_is_send_sync`
    (`assert_ss::<S3Store>(); assert_ss::<MemoryStore>();`).
  - `local::tests::local_fs_is_send_sync` (locks W82's intent).
  - Fails to compile today (trait lacks the supertraits) = RED.
- **GREEN**: `pub trait ObjectStore: Send + Sync`; fix the fallout:
  `S3LikeListStore.head_log` RefCell -> `Mutex` (keep `head_log()` API),
  `HeadFailStore`/`FlakyHeadStore` `Cell<usize>` -> `AtomicUsize`
  (`Ordering::Relaxed` is fine - only exact counts are asserted). Verify the
  exec/cli/lib stub doubles compile unchanged.
- **REFACTOR**: none expected; keep harness diffs minimal.
- Note: this cycle alone proves the aws `Client` + `Runtime` are Send/Sync
  as the issue predicts - the `S3Store` assertion IS the verification; if it
  fails, stop and reassess before cycle 2.

### Cycle 2 - W48 runtime spike (evidence, then decision)

- **RED-as-probe** (new tests in `src/store/s3.rs`, gated to the current
  runtime shape):
  - `concurrent_block_on_completes` - 8 threads `rt.block_on` futures that
    each `tokio::spawn` a chain of timer awaits; all 8 results correct.
  - `concurrent_block_on_overlaps` - 2 threads `block_on` a 400ms timer
    future each; wall-clock < 600ms proves overlap (serial execution would
    be >= 800ms; >2x margin both directions).
- **GREEN / decision**:
  - If both pass on the current-thread runtime: keep W48; tests stay as
    regression pins (they now guard a load-bearing property).
  - If serialized or broken: switch `S3Store::new` to
    `Builder::new_multi_thread().worker_threads(concurrency as
    usize).enable_all()` (the `concurrency` param arrives in cycle 5 - spike
    with a hardcoded small number, e.g. 4, and reconcile) **and re-add
    `rt-multi-thread` to the tokio features in `Cargo.toml` in the same
    commit** (W74 trimmed to `features = ["rt"]`, which only covers the
    current-thread runtime; `Builder::new_multi_thread` does not compile
    without it); the probe tests must then pass against the multi-thread
    runtime.
- Draft the decision-log entry text in this cycle (final placement in
  cycle 8): either "W48 confirmed under concurrent `block_on` (I20-runtime
  evidence)" or "W48 amended: multi-thread runtime, worker count =
  `[transfer].concurrency`, because concurrent `block_on` on a
  current-thread runtime serializes on the core" (the amend text also
  records the `rt-multi-thread` feature re-add, reversing the W74 trim).

### Cycle 3 - bounded pool helper (`src/pool.rs`, `pub(crate)` as `crate::pool`)

- **RED**:
  - `run_bounded_caps_in_flight` - instrumented closure records a
    max-concurrent gauge (AtomicUsize inc/dec around a yield) over 32 items
    at concurrency 4: gauge never exceeds 4; every index processed exactly
    once.
  - `run_bounded_results_in_input_order` - closure sleeps inversely to the
    index (adversarial reverse completion); result vector is in input order.
  - `run_bounded_concurrency_1_runs_on_caller_thread` - every item executes
    on `std::thread::current().id()` == the caller's (pins I20-one).
  - `run_bounded_error_isolation` - per-item `Result`s returned
    individually; one `Err` does not drop neighbors or reorder.
- **GREEN**: `pub(crate) fn run_bounded<T, R>(concurrency: u32, items: &[T], f:
  impl Fn(&T) -> R + Sync) -> Vec<R>` in `src/pool.rs` (locked location -
  `store::mod` consumes it in cycle 5, so it must not live under `exec`)
  where `R: Send`: `concurrency <= 1` ->
  plain `items.iter().map(f).collect()`; else `std::thread::scope` +
  `AtomicUsize` pull into a pre-sized `Vec<Mutex<Option<R>>>` (or per-slot
  `OnceLock`), unwrapped into order after the scope.
- **REFACTOR**: extract the slot-vector dance into the smallest readable
  shape; clippy clean.

### Cycle 4 - executor passes fan out (`src/exec.rs`)

- **RED** (all through `execute_plan`, mock store + temp vaults):
  - `exec_parallel_downloads_overlap` - 16-key pull with a
    `Mutex`-gauged store wrapper (`get_to` inc/dec + `yield_now`): max
    in-flight > 1 at concurrency 4, all bytes/mtimes correct, report equals
    the concurrency-1 report exactly.
  - `exec_report_is_deterministic_under_pool` - same plan executed twice at
    concurrency 8 with completion-order-shuffling store (sleep keyed on
    hash of key): identical `ExecReport` both runs, and identical to
    concurrency 1 (`failed` in plan order).
  - `exec_parallel_failure_isolation` - one poisoned key (store fails its
    `get_to`) among 15 healthy at concurrency 4: exactly one `failed` entry
    naming that key, 15 executed, message identical to the sequential run.
  - `exec_parallel_guards_hold` - at concurrency 4: a post-plan local edit
    still fails the upload key via R3.3; a post-plan remote replacement
    still fails the `DeleteRemote` key via W62/W119; neighbors unaffected.
  - `exec_concurrency_1_byte_for_byte` - existing-suite safety: run the
    recording-store ordering test (`exec_deletes_run_after_transfers` shape)
    at concurrency 1 and 4; the op log shows transfers strictly before
    deletes in both.
- **GREEN**: add `concurrency: u32` to `execute_plan`; each of the four
  passes maps its filtered action slice through `run_bounded`; report
  assembly (executed count, `fail()` pushes, `deleted_keys`) happens after
  each pass in index order. Compile-driven update of all existing
  `execute_plan` call sites (`concurrency: 1`).
- **REFACTOR**: the four passes share the collect-then-assemble shape; keep
  the W62/W119 head-before-delete and W32/W77 cleanup blocks textually
  intact where possible (review surface).

### Cycle 5 - parallel enrichment heads (`src/store/mod.rs` + s3 wiring)

- **RED**:
  - `enrich_heads_bounded_parallel` - 32-object listing, gauged store:
    max in-flight heads > 1 and <= 4 at concurrency 4; enriched entities
    identical (order included) to the concurrency-1 result.
  - `enrich_parallel_vanished_warning_order_stable` - vanished keys
    interleaved with healthy ones: the bounded warning names them in listing
    order, identical across runs.
  - `enrich_parallel_fails_closed` - one non-NotFound head error fails the
    listing with that error (fail-closed preserved); NotFound drops still
    per-row. Two non-NotFound errors on different keys return the error of
    the key **earlier in listing order** (not completion order), and the
    failed listing carries no partial entities and no vanished warning
    (the I20-heads ordering lock, pinned).
  - `enrich_concurrency_1_unchanged` - concurrency 1 produces exactly
    today's entities/warnings (the existing suite already pins most of this;
    assert head attempt order == listing order).
- **GREEN**: `enrich_with_head_mtimes(store, listing, concurrency: u32)`;
  heads for non-folder rows fan out through the same pool shape (reuse
  `crate::pool::run_bounded` from cycle 3 as-is - the placement is locked
  there, so this cycle is wiring only, no relocation); results merged by
  index. `S3Store::new(settings, retry, concurrency)` and
  `S3Store::list` pass it through; `S3LikeListStore` passes `1` (existing
  lib tests unchanged in behavior). Compile-driven call-site updates
  (`tests/s3_integration.rs`, s3 unit tests: `1` or `DEFAULT_CONCURRENCY`).
- **REFACTOR**: doc updates on `enrich_with_head_mtimes`
  (single-attempt-per-head from I8 unchanged - the SDK retries underneath,
  the pool only widens in-flight count; deterministic listing-order error
  selection and the no-partial/no-vanished-warning failure shape per
  I20-heads).

### Cycle 6 - config validation (`src/config.rs`)

- **RED**:
  - `resolve_settings_rejects_zero_concurrency` - `concurrency = 0` =>
    `Err` naming `transfer.concurrency`.
  - `resolve_settings_allows_concurrency_1` - resolves to `1`.
- **GREEN**: validation in `resolve_settings` next to the I8 retry
  validation, `Error::Other` loud message.
- **REFACTOR**: none expected.

### Cycle 7 - CLI wiring + W28/M6 warning removal (`src/cli.rs`)

- **RED**:
  - Replace the three W28 warning tests with
    `run_silent_on_configured_concurrency` - `concurrency = 8` explicitly
    set: no "Phase 3"/"concurrency" text on stderr, run proceeds.
    (RED today: the warning fires.)
- **GREEN**: delete the warning block and `concurrency_explicitly_set`
  (compile-driven: `Settings` literals in cli.rs tests lose the field);
  dispatch passes `settings.concurrency` into `execute_plan` and
  `S3Store::new`. Update the module doc (line ~11) so
  "`--concurrency` rejected as unknown" no longer implies the config key is
  inert.
- **REFACTOR**: none expected.
- Untouched: the `--concurrency` unknown-flag rejection test (line ~1106).

### Cycle 8 - docs + decision log (not TDD)

- `doc/roadmap.md`: decision-log entry/entries - **I20-pool**
  (std::thread scoped pool, same-cap heads, deterministic report,
  concurrency=1 sequential path) and the **W48 resolution** drafted in
  cycle 2 (confirm or amend). Update Phase 3 item 3: concurrency landed
  under #20.
- `doc/cli.md`: `[transfer].concurrency` documented as live (default 4,
  `>= 1`, 1 = sequential; caps transfer passes AND list-enrichment heads;
  no CLI flag). Remove any "inert/Phase 3" wording.
- `doc/object-store.md`: `ObjectStore: Send + Sync` contract; enrichment
  heads bounded by the same cap; D2 restated (async still contained in
  `store::s3`; runtime flavor per the W48 outcome).
- `README.md`: re-check the Phase 3 inert-feature note (~line 81) -
  remove `concurrency` from the inert list.

### Cycle 9 - integration coverage + final gate

- New env-gated tests in `tests/s3_integration.rs` (same
  `VAULTSYNC_TEST_S3_*` gating, no new deps):
  - `s3_integ_parallel_pull_correct` - seed 16 objects, pull at
    concurrency 4, verify bytes + applied mtimes key-for-key.
  - `s3_integ_parallel_push_correct` - 16 local files, push at
    concurrency 4, `head` each: size + `vaultsync-mtime` metadata intact.
  - Keep them small (16 objects, tiny bodies) so the gated suite runtime is
    unaffected; coordinate with #17 before unskipping
    `s3_integ_list_paginates` (out of scope here).
- Full gate + gated suite; walk the issue #20 acceptance checklist.

## Acceptance mapping (issue #20 checklist -> cycles)

| Issue criterion | Cycle |
| --------------- | ----- |
| Transfers + enrichment heads capped at `[transfer].concurrency`; `concurrency = 1` byte-for-byte sequential | 3, 4, 5, 7 (I20-one pins) |
| Deterministic `ExecReport`; unchanged per-key failure isolation | 3, 4 |
| Compile-time Send/Sync assertions for `S3Store`, `LocalFs`, `dyn ObjectStore` | 1 |
| W48 runtime question resolved with decision-log entry either way | 2, 8 |
| Integration coverage on the gated suite; coordinate with #17 | 9 |
| W28/M6 warning removed; decision log, cli.md, object-store.md updated | 7, 8 |
| `concurrency >= 1` validation | 6 |

## Non-goals (restated from the issue)

- No async executor, no D2 amendment (async stays inside `store::s3`).
- No retry work (#8 landed; the SDK `RetryConfig` composes underneath).
- No `--concurrency` CLI flag (D-config-only; stays rejected as unknown).
- No dedicated head cap, no per-retry/per-worker logging.
- `--yes` / `--max-delete` remain with issue #3.
- No new dependencies (std::thread only; dependency policy).

## Commit strategy

One commit per cycle (`Issue 20: <cycle subject>`), each leaving the gate
green. Natural squash groupings at PR time if a shorter history is
preferred: {1, 2} (traits + runtime resolution), {3, 4, 5} (pool + executor
+ enrichment), {6, 7} (config + CLI), {8, 9} (docs + integration). Branch:
`worktree-bounded-transfer-cncrrncy-std-thrd-wrkr-pl-trnsfr-cncrrncy-gs-lv`.
