# Issue 17 plan: retire `s3_integ_list_paginates` CI skip under I20 head concurrency (+ deferred I20 gauge reliability)

**Status:** implemented on `fix/issue-17-paginate-concurrency` (Track A +
Track B landed at `d39729d`; P30 knobs landed at `1691251`). PR 23 review
round 1 fixes (issuecomment-5462873894) are tracked in
[pr-23-fix-5462873894.md](pr-23-fix-5462873894.md); tip at implementation
time: `5dd7061`.
**Issue:** https://github.com/tlkahn/vaultsync/issues/17 (OPEN;
`s3_integ_list_paginates` live slowness; also owns the three `#[ignore]`d I20
probes deferred from PR 22 r1/r2)
**Branch:** `fix/issue-17-paginate-concurrency` (cut from `main` tip `303b391`)
**Design refs:** [object-store.md](../object-store.md),
[test-matrix.md](../test-matrix.md), [roadmap.md](../roadmap.md)
(I6-trigger / I20-heads / I20-r1 / I20-r2 decision-log entries),
[issue-6.md](issue-6.md), [issue-20.md](issue-20.md),
[pr-22-fix-5462129107.md](pr-22-fix-5462129107.md)
**Verified baseline (recorded at implementation start, 2026-08-29):** tip
`303b391` (Issue 20 / PR 22 merged). Gate measured on this branch cut:
`cargo test --offline --lib --bins` = 403 passed / 0 failed / 3 ignored;
`cargo test --offline --test s3_integration -- --skip s3_integ_list_paginates`
= 12 passed / 0 failed / 1 filtered (live bucket `pdf-tmp-repo`, `us-west-1`,~17s);
`cargo clippy --all-targets -- -D warnings` clean; `cargo fmt --check` clean.
Phase 0 row A was NOT run as part of the baseline (logs land first).
**Blocker check:** resolved - product-path head concurrency landed in #20 /
PR 22 (`enrich_with_head_mtimes` + `S3Store::list` honor
`[transfer].concurrency`). This issue is the live proof + harness + CI
close-out, not another product concurrency design.

---

## Problem recap (from the issue, verified against the tree)

### What the test does today

`s3_integ_list_paginates` (`tests/s3_integration.rs:195-214`):

1. Seeds **N = 1050** small objects under `p/` via 16 threads (forces a second
   ListObjectsV2 page; S3 pages at 1000).
2. Calls `s.list("")` and asserts the non-folder entity count equals 1050.
3. Returns into `with_store`, which always ends by `store.list("")` again and
   per-key `store.delete` of every non-folder entity.

The test is a **pagination assertion**, not a convergence assertion. It
passes logically; it is impractically slow live.

### Cost shape (why it blows the 15-20 min budget)

Every `S3Store::list` pays the I15 N+1 shape (`list_prefix_objects` +
`enrich_with_head_mtimes` - `src/store/s3.rs:539-569`): one (or more)
ListObjectsV2 cycle **plus N HeadObject calls**. Since I20 the N heads fan
out through `crate::pool::run_bounded` capped by the store's
`concurrency` field. But:

| Phase | What runs today | Approx request shape |
| ----- | --------------- | -------------------- |
| seed | 16-thread puts | 1050 PutObject (already parallel) |
| body `list` | **`with_store` -> `with_store_conc(..., 1, ...)`** | 2 List pages + **1050 sequential heads** |
| harness cleanup `list` | same store, concurrency 1 | 2 List pages + **another 1050 sequential heads** |
| harness cleanup deletes | per-key `DeleteObject` loop | **1050 sequential deletes** |

Whole test under concurrency 1: ~2100 sequential heads + 1050 sequential
deletes, each a full HTTP RTT. Observed (PR 16 review / W124 live gate,
`us-west-1`): stall past ~400s in the first `list` alone ("seeded; starting
list" printed, "list returned" never reached); full run exceeded 900s.

### What is already fixed (not this issue's product work)

- I20-heads: `enrich_with_head_mtimes(store, listing, concurrency)` fans out
  at `concurrency > 1`; sequential short-circuit restored at `<= 1`
  (I20-r1/F1, W145/W146).
- `S3Store` carries `self.concurrency` and passes it into enrichment.
- `with_store_conc(name, concurrency, f)` exists and is used by
  `s3_integ_parallel_{pull,push}_correct` (N=16 / conc=4) - **transfer**
  fan-out proof, not large-N list enrichment.
- CI PR-gate deliberately `--skip s3_integ_list_paginates`
  (`.github/workflows/ci.yml:115-120`); nightly full suite "may be red while
  #17 is open" (`integration-nightly`).

### What is NOT done (this issue)

1. `s3_integ_list_paginates` still calls `with_store` (= concurrency **1**),
   so the live suite never exercises parallel head enrichment at the scale
   that motivated I20-heads.
2. Harness cleanup always re-lists through the enriched path (second N-head
   tax) and deletes one-at-a-time.
3. CI still skips the test on the PR gate; jobs stay split.
4. Three `#[ignore]`d I20 probes were parked here (overlap gauges + F3
   probabilistic net) - same issue home, secondary track.

---

## Root-cause reflection (before changing behavior)

The filing-time root cause (sequential N heads after I15) is **product-fixed
but proof-blocked**. The remaining failure mode is a stack of three harness /
wiring gaps, not a missing pool:

```
live timeout
  |- (A) test wiring: list_paginates -> with_store -> concurrency 1
  |      even though S3Store/enrich already honor concurrency > 1
  |- (B) cleanup list: with_store always store.list("") (full enrichment)
  |      doubles the head tax regardless of (A)
  '- (C) cleanup delete: sequential per-key DeleteObject
         dominates once (A)+(B) are fixed, if RTT is non-trivial
```

Secondary / deferred (do not block skip retirement if still flaky after a
honest attempt):

```
#[ignore] reliability
  |- (D) exec_parallel_downloads_overlap / enrich_heads_bounded_parallel
  |      yield_now overlap gauges; scheduler-sensitive under full suite
  |      (F3 dir_create_lock staggers create-alloc; store gauge flaked on
  |      the pristine I20 baseline too)
  '- (E) exec_parallel_shared_parent_cleanup_no_spurious_failures
         probabilistic F3 net; residual interleavings outside create-alloc;
         fix (dir_create_lock) stays; net is honesty-deferred
```

**Working thesis to confirm with logs (not re-derive from first principles):**

- Body list wall-clock under conc=1 is ~O(N x RTT_head); under conc=K it
  drops toward O((N/K) x RTT_head) plus list-page overhead.
- Cleanup list under today's harness is the same order as body list.
- Cleanup deletes are ~O(N x RTT_delete) sequential; unenriched list is
  ~O(pages x RTT_list) only.
- Seed is already parallel and should be a small fraction of the total once
  heads are concurrent.

If live timings refute the thesis (e.g. body list still ~N x RTT at conc=32,
or deletes alone exceed the job budget even when parallelized), **stop and
reassess** before dropping the CI skip - do not "fix" CI around a still-red
test.

---

## Phase 0 - diagnostic instrumentation (logs first, no behavior change)

Goal: get a phase-breakdown on a real bucket so the fix order is
evidence-driven and the post-fix acceptance number is known. Logs are
temporary, labeled, and removed (or reduced to a single `[ok] name Ns`
summary) once the close-out is confirmed.

### Where

All logs go to **stderr** via `eprintln!` (matches existing `[ok]` / `[skip]`
/ `cleanup:` convention in `tests/s3_integration.rs`; CI runs
`--nocapture`). Label every line `[17]` so they are greppable and obviously
temporary.

### What to log

1. **`with_store_conc`** (shared harness - every integ test benefits):
   - on entry: `name`, `concurrency`, `prefix`, `bucket`, `region`
   - after `S3Store::new`: `store_ready_ms`
   - after `f(&store)` returns: `body_ms`, `body_ok=bool`
   - cleanup list: start; on success `cleanup_list_ms`, `entities`,
     `files`, `folders`; on err the existing cleanup message plus elapsed
   - cleanup deletes: `cleanup_delete_ms`, `deleted`, `delete_errs`
   - total: `total_ms`
2. **`s3_integ_list_paginates` body** (test-local, finer grain):
   - `seed_start` / `seed_ms` / `seeded=N`
   - `list_start` / `list_ms` / `files` / `folders` / `warnings`
3. **Optional one-shot product-path counter** (only if harness logs leave
   body-list time unexplained): a `thread_local!` / `AtomicU64` head-call
   counter behind `#[cfg(test)]` in `enrich_with_head_mtimes`, printed once
   at the end of the test body. Prefer NOT adding this unless Phase 0
   timings are ambiguous - keep production code clean.

### How to run Phase 0

Against the dedicated test bucket (same env as CI / local):

```bash
# baseline (concurrency 1, current code) - expect long runtime; use a
# personal run, not CI, and be ready to Ctrl-C after the phase you care
# about if it is clearly stuck past several minutes
cargo test --locked --test s3_integration s3_integ_list_paginates -- --nocapture --exact

# after the first code change (conc=K only), re-run the same command and
# diff the [17] lines
```

Record the numbers in this plan under **Phase 0 results** (fill in at
implementation time):

Phase 0 results (recorded 2026-08-29, live `us-west-1` bucket `pdf-tmp-repo`):

- Row A (conc 1): body list stalled past ~380s (1050 sequential heads, ~360ms
  RTT each) and was aborted per the plan's Ctrl-C guidance; seed_ms=24737.
- Row B (conc 32): body_list_ms=13229 (I20-heads fan-out proven live at
  scale); cleanup_list_ms=12196 (still-enriched second N-head pass);
  cleanup_delete_ms=410279 sequential deletes (~390ms RTT each) - now the
  dominant cost. total_ms=459682 (7.7 min).

Thesis confirmed: body/cleanup list wall-clock under conc=K drops toward
O((N/K) x RTT) + page overhead; sequential cleanup deletes are ~O(N x RTT);
unenriched list is ~O(pages x RTT) only. No surprise in the cost shape; the
fix order (conc -> unenriched cleanup list -> parallel deletes) is validated.
Cycle A4 is REQUIRED (410s sequential deletes), not optional.

| Run | conc | seed_ms | body_list_ms | cleanup_list_ms | cleanup_delete_ms | total_ms |
| --- | ---- | ------- | ------------ | --------------- | ----------------- | -------- |
| A baseline (pre-fix) | 1 | 24737 | aborted after ~380s in list (stalled; 1050 sequential heads) | n/a (never reached) | n/a | n/a |
| B conc only | 32 | 23804 | 13229 | 12196 | 410279 | 459682 |
| C + unenriched cleanup list | 32 | 26984 | 15578 | 1641 | 393224 | 437591 |
| D + parallel cleanup deletes | 32 | 24473 | 12297 | 6477 | 11010 | 54429 |

Acceptance target for skip retirement: **total_ms comfortably under the
integration job `timeout-minutes: 20`** with margin (aim total < 120s on
`us-west-1` CI; hard fail if still > 10 min). Tune K / delete parallelism
from the table, not from guesswork.

### Phase 0 exit criteria

- Table rows A (and B if conc change lands in the same working session)
  filled from a real run.
- Thesis confirmed or revised in one short paragraph under the table.
- No CI skip dropped yet; no product API change required for logging-only
  commits (harness-only diff is fine as its own commit).

---

## Locked decisions

| ID | Decision | Choice |
| -- | -------- | ------ |
| I17-tracks | Work split | **Track A (blocking close-out):** paginate live proof + harness cleanup + CI skip retirement. **Track B (best-effort, same PR if green; else thin follow-up):** the three `#[ignore]`d I20 probes. Track B must not hold Track A hostage. |
| I17-conc | Paginates concurrency | **`with_store_conc("paginate", K, ...)` with K = 32** as the first live try (issue suggestion). Floor is `DEFAULT_CONCURRENCY` (4); cap is `MAX_CONCURRENCY` (256). If Phase 0 row B at 32 is already under budget, keep 32; if SDK/thread cost shows up, step down to 16 then 8 before giving up. Do **not** go through `with_store` (hardcoded 1). |
| I17-N | Seed size | **Keep N = 1050.** It is the pagination proof (second page). Shrinking to 1001 is a last-resort lever only if 1050 cannot fit the budget after conc + cleanup fixes; document if used. |
| I17-cleanup-list | Cleanup list shape | **Do not re-enrich.** Add a public `S3Store` API that returns object keys (or raw unenriched rows) via the existing private `list_prefix_objects` path - **no** `enrich_with_head_mtimes`, **no** folder-view synthesis required. Harness cleanup uses this API, then deletes. Production `ObjectStore::list` semantics stay untouched (still enriched). |
| I17-cleanup-api | API shape | **`S3Store::list_object_keys(&self, prefix: &str) -> Result<Vec<String>, Error>`** (name exact). Public on `S3Store` only (not a trait method) - integration tests live in the external `tests/` crate and need visibility; trait surface stays planner-facing and enrichment-honest. Doc-comment states: keys only, no head enrichment, no folder views, intended for bulk discovery / harness cleanup where mtime/etag are not needed. Filters out keys ending in `/` (nonempty folder marker leftovers) so delete never targets a folder view. |
| I17-cleanup-delete | Cleanup delete shape | **Measurement-driven.** After unenriched list lands, read Phase 0 row C. If `cleanup_delete_ms` is already small vs budget, leave the sequential loop. If it dominates, fan out deletes in the harness with `std::thread::scope` and a worker count equal to the store concurrency (or a dedicated `cleanup_concurrency` constant matching K). **No** `DeleteObjects` batch API in this issue (optional future; new SDK surface + partial-failure semantics). **No** production `ObjectStore::delete` signature change. |
| I17-ci | CI trigger split | **Drop `--skip s3_integ_list_paginates` on the PR-gate job once live total is under budget.** Then **merge `integration` and `integration-nightly` into one job** (same steps, no `if: event_name` split) - the skip was the only I6-trigger reason for the split (issue-6.md I6-trigger). Keep `timeout-minutes: 20` unless Phase 0 shows a tighter honest bound worth encoding. Revisit the job-name comment that cites #17. |
| I17-logs-fate | Instrumentation fate | Phase 0 verbose `[17]` phase lines are **removed before merge** (or reduced to one optional `eprintln!("[ok] {name} {total_ms}ms")` if useful). No permanent `tracing` dependency; no product-path logging. |
| I17-gauges | Track B approach | Prefer a **deterministic rendezvous** over `yield_now`: the gauged `head`/`get_to` blocks (Condvar / barrier) until `in_flight` has been observed `>= 2` (or `>= min(concurrency, n)`) **or** every worker has entered, then proceeds. That proves overlap without scheduler luck. If a clean deterministic rewrite still flakes under full-suite load, leave `#[ignore]` and open a thin follow-up rather than weakening the assertion. The F3 probabilistic net stays probabilistic by nature; either keep ignored with an updated reason or drop it and rely on the lock + the existing deterministic exec tests (`exec_parallel_failure_isolation`, `exec_report_is_deterministic_under_pool`, `exec_parallel_guards_hold`). |
| I17-deps | Dependencies | **None.** std only (existing project policy). |
| I17-scope-out | Explicit non-goals | Product concurrency redesign; `DeleteObjects` multi-key API; changing I15 fail-closed enrichment semantics; changing default `DEFAULT_CONCURRENCY`; SDK retry policy (#8 done); making `ObjectStore::list` optionally unenriched for production callers. |

---

## Method: strict fine-grained TDD

Same rules of engagement as issue-8/15/20 plans:

1. **RED** - named failing test first, exercising a production API (or the
   integ assertion that is currently skipped); confirm it fails for the
   right reason. Compile-RED is legitimate for new public signatures.
2. **GREEN** - smallest implementation that passes.
3. **REFACTOR** - behavior-preserving cleanup on green.
4. One logical behavior per cycle; per-cycle gate:
   ```
   cargo test --offline --lib --bins
   cargo test --offline --test s3_integration -- --skip s3_integ_list_paginates
   cargo clippy --all-targets -- -D warnings
   cargo fmt --check
   ```
   The paginate skip stays in the per-cycle gate until Cycle A5 (CI
   retirement) - same pattern issue-8/20 used while #17 was open. After A5
   the skip disappears from the gate too.
5. **No network in the default suite.** New `list_object_keys` behavior that
   is pure (filter `/`-terminated keys out of a raw row list) is pinned
   offline via a small `pub(crate)` helper if one is extracted; the S3
   round-trip itself is env-gated. Concurrency overlap stays gauge-based,
   never wall-clock (I20 rule), except Phase 0 live timing which is a probe
   table, not a regression pin.
6. Docs-only / CI-YAML-only cycles have no RED; they land under the all-green
   gate.
7. Work items continue the project W-series (**W151+**). One commit per
   cycle unless a cycle is explicitly a docs+code pair that must land
   atomic (call out when so).
8. **TDD exception (harness hygiene):** pure cleanup-path rewiring inside
   `with_store_conc` with no new assertion surface follows the W73/W104
   precedent (test-only hygiene, no RED). Prefer still adding an offline
   pin when a helper is extracted (Cycle A2).

Phase 0 logging commits are ordered **before** the TDD cycles and do not
need RED/GREEN.

---

## Cycles

### Cycle 0 - pre-flight + Phase 0 logs

- Cut branch from `main`; record tip SHA and gate result at the top of this
  file.
- Confirm the verified facts with grep (no surprises since PR 22):
  - `s3_integ_list_paginates` calls `with_store(` (conc 1).
  - `with_store` is `with_store_conc(name, 1, f)`.
  - `S3Store::list` calls `enrich_with_head_mtimes(..., self.concurrency)`.
  - CI PR gate has `--skip s3_integ_list_paginates`.
  - Three `#[ignore]` reasons still cite issue #17.
- Land harness `[17]` phase logs in `with_store_conc` + test-local seed/list
  logs in `s3_integ_list_paginates` (behavior-neutral).
- Run Phase 0 row A against real S3; paste numbers into the table above.
- **Commit:** `test: [17] phase-timing logs for list_paginates diagnosis`
  (or equivalent). No CI change.

---

### Track A - paginate close-out (blocking)

### Cycle A1 - point `s3_integ_list_paginates` at `with_store_conc` (W151)

- **RED (live, env-gated):** not a new offline test - the existing
  `s3_integ_list_paginates` body already asserts `files.len() == 1050`.
  Characterization: under conc=1 it is correct-but-too-slow. The RED for
  this cycle is the **Phase 0 row A timeout / multi-minute body_list_ms**;
  the cycle's job is to make row B green on wall-clock without weakening
  the count assertion.
- **GREEN:** change the test to
  `with_store_conc("paginate", 32, |s| { ... })` (K from I17-conc; comment
  cites issue #17 / I20-heads live proof). Keep N = 1050 and the files-len
  assertion. Do not touch cleanup yet.
- **REFACTOR:** none expected.
- Re-run Phase 0 row B; record numbers. Expect `body_list_ms` to drop by
  roughly K (with real-world overhead). If it does not, debug before
  continuing (head counter optional; confirm `S3Store::new` received 32).
- **Commit:** `test: [17] list_paginates uses with_store_conc(32) (I20 heads live proof)`

### Cycle A2 - `S3Store::list_object_keys` unenriched path (W152)

- **RED (offline):** extract a tiny pure helper (keeps S3 I/O out of the
  unit test), e.g. in `src/store/s3.rs`:
  ```rust
  pub(crate) fn object_keys_from_raw(
      raw: Vec<(String, u64, Option<u64>)>,
  ) -> Vec<String> { /* drop trailing-/ keys; keep order */ }
  ```
  Tests:
  - `object_keys_from_raw_drops_folder_markers` - input mix of file keys +
    `notes/`-style rows; output is only file keys, stable order.
  - `object_keys_from_raw_preserves_all_files` - 3 file rows round-trip.
  - (Optional compile/docs pin) a unit test that `list_object_keys` is
    reachable - not required if the helper carries the logic.
- **GREEN:**
  - implement `object_keys_from_raw`;
  - implement
    `pub fn list_object_keys(&self, prefix: &str) -> Result<Vec<String>, Error>`
    as `Ok(object_keys_from_raw(self.list_prefix_objects(prefix)?))`;
  - rustdoc as locked in I17-cleanup-api;
  - no change to `ObjectStore::list`.
- **REFACTOR:** if `list` / `list_object_keys` share more than the one
  `list_prefix_objects` call, leave them side-by-side (clarity over DRY);
  do not route `list` through `list_object_keys`.
- Gate green offline. No live run required for this cycle alone.
- **Commit:** `feat: [17] S3Store::list_object_keys skips head enrichment`

### Cycle A3 - harness cleanup uses unenriched keys (W153)

- **RED:** harness hygiene exception (W73/W104 precedent) - no production
  assertion changes. Optional live characterization: Phase 0 row B
  `cleanup_list_ms` still ~ body head cost.
- **GREEN:** in `with_store_conc` cleanup, replace `store.list("")` with
  `store.list_object_keys("")`; delete each returned key (still sequential
  this cycle). Keep the W104 stderr reporting on list/delete failure
  (never fail the test outcome from cleanup). Folders no longer appear, so
  the `e.is_folder()` continue is gone with the Entity loop.
- **REFACTOR:** none.
- Re-run Phase 0 row C; expect `cleanup_list_ms` to collapse to ~pages x
  RTT (two pages for 1050 keys). Record numbers.
- **Commit:** `test: [17] with_store cleanup lists keys without head enrichment`

### Cycle A4 - parallel cleanup deletes if row C says so (W154, conditional)

- **Decision gate:** if row C `cleanup_delete_ms` is acceptable (e.g. < 30s
  and total already under target), **skip this cycle** and note "not needed
  per Phase 0 row C" here. Otherwise proceed.
- **RED:** harness hygiene; live characterization is the RED.
- **GREEN:** fan-out deletes in `with_store_conc` via `std::thread::scope`:
  workers = `min(concurrency, keys.len()).max(1)`; each worker pulls indices
  from an `AtomicUsize` (same shape as `run_bounded`, inlined in the harness
  - `crate::pool` is `pub(crate)` and invisible to the external test crate,
  do not widen pool visibility just for cleanup). Preserve per-key error
  `eprintln!`. Order of deletes does not matter; counts do.
- **REFACTOR:** if the inline pull loop is non-trivial, a test-local
  `fn delete_keys_bounded(store, keys, concurrency)` helper next to
  `with_store_conc` is fine.
- Re-run Phase 0 row D; record numbers.
- **Commit:** `test: [17] with_store cleanup deletes in parallel under store concurrency`

### Cycle A5 - retire CI skip + merge integration jobs (W155)

Preconditions: Phase 0 final row shows total well under `timeout-minutes: 20`
with margin; local full `cargo test --locked --test s3_integration --
--nocapture` green against the real bucket including paginate.

- **No RED** (YAML + comment edits).
- **GREEN:**
  - `.github/workflows/ci.yml`:
    - PR-gate `integration` job: drop `--skip s3_integ_list_paginates`; run
      the full `s3_integration` suite with `--nocapture`.
    - Remove the `integration-nightly` job and the `if: github.event_name !=
      'schedule'` guard on `integration`, **or** keep one job that runs on
      `push/pr/schedule` with identical steps (preferred: single job, three
      triggers - matches I6-trigger "merge when #17 lands").
    - Update comments that cite "#17" / "excluded until" / "may be red".
    - Keep `timeout-minutes: 20` unless evidence supports a lower bound.
  - `doc/roadmap.md`: decision-log entry for I17 close-out; flip any "skip
    until #17" present-tense wording in Phase 3 item 6 / I6-trigger notes.
  - `doc/object-store.md` / `doc/test-matrix.md`: only if they still describe
    sequential cleanup or the CI skip as current - update to match.
  - Module doc on `tests/s3_integration.rs` if it mentions the skip.
- **REFACTOR:** strip Phase 0 verbose `[17]` lines (I17-logs-fate); keep a
  single `[ok] {name}` (existing) or `[ok] {name} {total_ms}ms` if desired.
- Full offline gate **without** the paginate skip:
  ```
  cargo test --offline --lib --bins
  cargo test --offline --test s3_integration
  cargo clippy --all-targets -- -D warnings
  cargo fmt --check
  ```
- **Commit:** `ci: [17] run s3_integ_list_paginates on PR gate; merge integration jobs`

### Cycle A6 - docs + issue close-out checklist (W156)

- Roadmap decision-log final wording (if not fully done in A5).
- Issue #17 body is the source of truth for "what landed"; add a short
  closing comment via `gh` at PR merge time (not in this plan commit).
- Confirm Track B disposition in the PR description (landed / follow-up
  filed / left ignored with updated reason).

---

### Track B - deferred I20 gauge reliability (best-effort)

Do **after** Track A is green live, in the same PR only if the cycles stay
small; otherwise file a follow-up and stop.

### Cycle B1 - deterministic overlap gauge for enrichment heads (W157)

- **RED:** un-`#[ignore]` `enrich_heads_bounded_parallel` and replace the
  `yield_now` inside `GaugedHeadStore::head` with a rendezvous:
  - on entry: `in_flight += 1`; update `max_in_flight`;
  - wait until `max_in_flight >= 2` **or** a "all entered" count hits
    `n_workers` (Condvar + Mutex, std only), with a generous
    `Duration` timeout that fails the test loudly on deadlock;
  - then call inner head; `in_flight -= 1`.
  - Assert `max_in_flight > 1` and `<= 4`; enriched listing equals conc=1.
  - Run under full `cargo test --offline --lib` repeatedly (e.g. 20x) to
    confirm no flake before calling it green.
- **GREEN:** the rendezvous implementation above; remove the `#[ignore]` and
  its issue-17 reason.
- **REFACTOR:** share the rendezvous helper with B2 if both need it
  (`testutil` or a small fn in the test module).

### Cycle B2 - deterministic overlap gauge for exec downloads (W158)

- Same pattern on `GaugedGetStore::get_to` for
  `exec_parallel_downloads_overlap`.
- Un-`#[ignore]`; full-suite flake check as in B1.
- Note: F3 `dir_create_lock` serializes create-alloc; the gauge must measure
  **get_to** overlap (bytes streaming), not tmp allocation overlap. The
  rendezvous belongs in `get_to`, after or around the inner store call as
  today, not inside `LocalFs`.

### Cycle B3 - F3 probabilistic net disposition (W159)

Pick one, based on evidence after B1/B2:

| Option | When | Action |
| ------ | ---- | ------ |
| B3-keep | still flakes on fixed tree | leave `#[ignore]`; rewrite the reason to drop "deferred to #17" and point at a new follow-up issue; document that `dir_create_lock` remains the fix |
| B3-drop | redundant with deterministic exec tests | delete the test; rely on lock + `exec_parallel_*` deterministic pins; decision-log note |
| B3-hook | only if A+B still want a hard pin | test-only hook inside `tmp_path_for` (previously declined as production-surface pollution) - **default no**; requires an explicit user override to reopen |

Default recommendation: **B3-keep** with a follow-up issue if still flaky, or
**B3-drop** if the team prefers a smaller suite over a flaky net. Decide at
implementation time; do not block Track A.

---

## Risk register

| Risk | Mitigation |
| ---- | ---------- |
| conc=32 still slow body list (shared SDK retry quota / current-thread runtime surprise) | Phase 0 row B catches it; I20 W48 spike already proved concurrent `block_on` overlaps on the current-thread runtime - if live disagrees, capture head-counter + AWS latency before redesigning |
| unenriched cleanup deletes a folder marker key | `object_keys_from_raw` drops trailing `/`; pinned offline in A2 |
| cleanup races with a still-running body (should not - body joined) | no change in sequencing; `f(&store)` completes before cleanup |
| parallel cleanup delete errors become silent | keep per-key `eprintln!` (W104); never promote cleanup failure to test failure |
| dropping CI skip while total still > timeout | A5 precondition is the Phase 0 final row; do not merge A5 on hope |
| Track B flakes on CI full suite | keep ignored; Track A still closes #17 primary scope (update issue text / split follow-up) |
| `list_object_keys` misused by future production callers to skip mtime enrichment | rustdoc warns; not on the trait; planner path stays on `ObjectStore::list` |
| Nightly-only leak tripwire lost when jobs merge | single job still runs on `schedule`; leak tripwire preserved |

---

## Acceptance criteria (issue close-out)

Track A (required):

- [ ] `s3_integ_list_paginates` uses `with_store_conc` with K >= 4 (target 32)
      and still asserts 1050 non-folder entities.
- [ ] Harness cleanup does not call enriched `list` (no second N-head pass).
- [ ] Phase 0 final timing row: total comfortably under integration
      `timeout-minutes: 20` (aim < 120s on CI `us-west-1`).
- [ ] CI PR gate runs `s3_integ_list_paginates` (no `--skip`).
- [ ] `integration` / `integration-nightly` split retired (or equivalent single
      job on push/pr/schedule).
- [ ] Offline gate green without the paginate skip flag.
- [ ] `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` clean.
- [ ] Verbose `[17]` diagnosis logs removed or reduced per I17-logs-fate.
- [ ] Roadmap decision-log entry written.

Track B (optional for #17 close):

- [ ] `enrich_heads_bounded_parallel` reliable without `#[ignore]`, or follow-up filed.
- [ ] `exec_parallel_downloads_overlap` reliable without `#[ignore]`, or follow-up filed.
- [ ] F3 net disposition recorded (keep ignored / drop / follow-up).

---

## Suggested commit order (summary)

1. `test: [17] phase-timing logs for list_paginates diagnosis` (Cycle 0)
2. `test: [17] list_paginates uses with_store_conc(32)` (A1)
3. `feat: [17] S3Store::list_object_keys skips head enrichment` (A2)
4. `test: [17] with_store cleanup lists keys without head enrichment` (A3)
5. `test: [17] with_store cleanup deletes in parallel ...` (A4, conditional)
6. `ci: [17] run s3_integ_list_paginates on PR gate; merge integration jobs` (A5)
7. docs / log strip / roadmap (A6, may fold into A5)
8. Track B commits only if green (B1-B3)

---

## Out of scope / already landed

- Bounded transfer + head concurrency itself (issue #20 / PR 22).
- SDK retry/backoff on transient S3 errors (issue #8).
- I15 fail-closed enrichment semantics (NotFound drops row; other head
  errors fail the listing).
- `DeleteObjects` multi-key batch API.
- Production `ObjectStore` trait changes / unenriched production `list`.
- Raising or lowering `DEFAULT_CONCURRENCY` / `MAX_CONCURRENCY`.
