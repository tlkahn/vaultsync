# Plan: push `s3_integ_list_paginates` below 30s total

**Status:** done (A+B landed under 30s; C/D/E not needed)
**Parent:** PR 23 / issue #17 close-out
([issuecomment-5462688729](https://github.com/tlkahn/vaultsync/pull/23#issuecomment-5462688729);
[issue-17.md](issue-17.md) Track A landed at tip `d39729d`)
**Branch:** `fix/paginate-sub-30s` (cut from #17 tip)
**Design refs:** [object-store.md](../object-store.md),
[test-matrix.md](../test-matrix.md), [roadmap.md](../roadmap.md)
(I17-paginate-closeout / I20-heads / I20-r1 / P30-paginate-sub-30s),
[issue-17.md](issue-17.md), [issue-20.md](issue-20.md)
**Verified baseline (Phase 0 on tip `d39729d`):** offline lib/bins green;
clippy -D warnings green; fmt --check green. Live paginate baseline below.

**Aim:** `s3_integ_list_paginates` wall clock **< 30s** on the same
`us-west-1` path used for #17 Phase 0 (today ~49.8-54s), without giving up
N = 1050 or the enriched-`list` I20-heads scale proof, unless a later
measurement cycle explicitly unlocks a coverage split (Cycle E, last
resort).

**Non-aim:** shrinking the rest of the integ suite, changing
`DEFAULT_CONCURRENCY` / production `ObjectStore::list` semantics, or
re-opening the #17 CI skip.

---

## Problem recap (post-#17)

### What #17 already fixed

| Phase | Before (#17 row A, conc 1) | After (#17 row D, conc 32) | Boost |
| ----- | -------------------------- | -------------------------- | ----- |
| Body list | stalled >380s (1050 sequential heads) | ~13s | ~30x |
| Cleanup list | ~12s (second enriched N-head pass) | ~1.6s (`list_object_keys`) | ~8x |
| Cleanup deletes | ~393-410s (sequential) | ~11s (`delete_keys_bounded`) | ~36x |
| Test total | >900s (never completed in CI) | **49.8-54s** | ~18x |

Product path is correct: `with_store_conc("paginate", 32, ...)`, harness
cleanup uses unenriched `S3Store::list_object_keys` + parallel
`delete_keys_bounded`, CI runs the test on every PR. The remaining cost is
**not a missing pool** - it is RTT arithmetic at the knobs #17 chose.

### Cost shape today (row D, `us-west-1`)

| Phase | ~ms | Request shape | Binding constraint |
| ----- | --- | ------------- | ------------------ |
| Seed | 24-27s | 1050 PutObject via **16 hardcoded** threads | under-parallel vs body K; plus per-put temp-file tax in `put_from` |
| Body `list` | 12-16s | 2 List pages + 1050 HeadObject @ K=32 | near `(N/K)*RTT` floor for K=32 |
| Cleanup list | 1.6-6s | unenriched ListObjectsV2 only | already cheap; variance only |
| Cleanup deletes | 11s | 1050 DeleteObject @ K=32 | same floor as heads; still 1 RTT per key |
| **Total** | **~50-54s** | | **~77% of full integ suite (~65s)** |

Effective RTT on the measured path is ~360-400ms
(`body_list_ms * K / N ≈ 13s * 32 / 1050 ≈ 396ms`). Every further second has
to come from **more in-flight ops**, **fewer round trips**, or **less local
work per op** - not from another wiring fix of the #17 kind.

### Root-cause reflection

```
paginate ~50s
  |- (S) seed: 16-thread puts, each put_from = temp file + PutObject
  |      dominates wall clock; independent of store concurrency
  |- (L) body list: enriched list at K=32 already near RTT floor for that K
  |      this IS the I20-heads live proof - keep it unless Cycle E unlocks
  |- (C) cleanup list: unenriched, residual
  '- (D) cleanup deletes: still N single-key DeleteObject @ K
         DeleteObjects batch deferred in I17-scope-out
```

Thesis to confirm or revise in Phase 0: **seed fan-out + higher K move the
total under 30s without product API changes; DeleteObjects and a small-body
put fast path are the structural backups if the RTT floor at higher K is
still above budget.**

---

## Locked decisions

| ID | Decision | Choice |
| -- | -------- | ------ |
| P30-aim | Success bar | **`s3_integ_list_paginates` `[ok] paginate` total < 30s** on the same live bucket/region used for #17 Phase 0, three consecutive green runs (no single lucky sample). Full integ suite stays green; offline gate untouched in spirit. |
| P30-N | Seed size | **Keep N = 1050.** Pagination proof (second ListObjectsV2 page) and the I20-heads scale proof both depend on it. N = 1001 is last-resort only and must be decision-logged if used. |
| P30-proof | Body assertion | **Keep enriched `s.list("")` + `files.len() == 1050`.** Do not switch the body to `list_object_keys` in Cycles A-D - that would silently drop the I20-heads live proof #17 was sold on. Coverage split is Cycle E only, measurement-gated. |
| P30-method | How we pick knobs | **Exploring while fixing.** Every cycle that changes a knob re-runs the Phase 0 timing table (seed / body_list / cleanup_list / cleanup_delete / total) before the next cycle. No knob locked from theory alone. |
| P30-logs | Instrumentation | Re-introduce temporary `[p30]` phase-timing `eprintln!`s in `with_store_conc` + test-local seed/list timings in `s3_integ_list_paginates` for the working branch only. **Strip before merge** (same fate as I17-logs-fate); keep one `[ok] {name} {total_ms}ms` line. |
| P30-K | Body/cleanup concurrency | Start from landed K = 32. Cycle B sweeps **64, then 128** for the paginate test only (`with_store_conc("paginate", K, ...)`). Library callers stay uncapped; `MAX_CONCURRENCY = 256` is config-layer only (I20-r1/F2) and is **not** raised. Stop at the first K that is past diminishing returns or that trips sustained `SlowDown`/503. |
| P30-seed | Seed fan-out | Cycle A raises seed workers from **16 -> match K, then try 64**. Seed concurrency is test-local (the `std::thread::scope` loop in `s3_integ_list_paginates`), independent of store K, but the Phase 0 table should record both numbers. |
| P30-delete | Cleanup delete shape | Cycle C (only if deletes still dominate after A+B): add **`S3Store::delete_object_keys`** (name exact; public on `S3Store` only, not on the trait) backed by S3 `DeleteObjects` (max 1000 keys/request). Harness `with_store_conc` cleanup switches from per-key `delete_keys_bounded` to the bulk path. Partial-failure reporting stays loud on stderr (W104). No `ObjectStore::delete` signature change. |
| P30-put | Small-body put path | Cycle D (only if seed still dominates after A+B+C): thresholded `put_from` fast path - small bodies buffer in memory / `ByteStream::from(bytes)`; large bodies keep the temp-file path (P1r-put-size intact above the threshold). Threshold locked in-cycle from a microbench, not guessed. Std/SDK only; no new crate. |
| P30-split | Coverage split | Cycle E last resort only. If A-D cannot land < 30s without weakening the 1050-head proof: split into (1) pagination via `list_object_keys` at N=1050 and (2) a smaller enriched head-scale test (N=128-256 @ K). Requires an explicit decision-log entry and updated test-matrix row. |
| P30-infra | Region / RTT | Out of band, not a code cycle: one CI-runner HEAD/Put timing sample vs local `us-west-1`. If CI RTT is materially worse than local, file an infra note (bucket/runner colocated region) rather than coding around it. Do not block code cycles on infra. |
| P30-deps | Dependencies | **None.** std + existing AWS SDK only. |
| P30-scope-out | Explicit non-goals | Re-skipping paginate in CI; changing production `ObjectStore::list` to optionally skip enrichment; raising/lowering `DEFAULT_CONCURRENCY` / `MAX_CONCURRENCY`; skipping cleanup in favor of lifecycle expiry; sharing seeded fixtures across tests; multipart upload; changing I15 fail-closed enrichment semantics. |

---

## Method: exploring while fixing

Same TDD spine as issue-8/15/17/20, with an explicit **measure -> change one
knob -> remeasure** outer loop:

1. **RED / baseline** - Phase 0 table row filled from tip; aim bar stated.
2. **GREEN** - smallest change that moves the targeted phase.
3. **REFACTOR** - behavior-preserving cleanup on green.
4. One logical lever per cycle; per-cycle gate:

   ```bash
   cargo test --offline --lib --bins
   cargo clippy --all-targets -- -D warnings
   cargo fmt --check
   # live, when the cycle touches seed/K/delete/put:
   cargo test --locked --test s3_integration s3_integ_list_paginates -- --nocapture --exact
   ```

5. **No network in the default suite** for new unit pins (DeleteObjects
   helper pure chunking, put fast-path size branch, etc.). Live S3 stays
   env-gated.
6. **Stop condition:** three consecutive paginate totals < 30s on the
   measured path, full integ suite green, logs stripped, roadmap row
   written. Do not keep sweeping K "for fun" past the bar.

### Phase timing contract (temporary)

Mirror #17 Phase 0 labels so rows stay comparable:

| Field | Where | Meaning |
| ----- | ----- | ------- |
| `seed_ms` | test body | wall time of the seed `thread::scope` |
| `body_list_ms` | test body | wall time of `s.list("")` |
| `cleanup_list_ms` | `with_store_conc` | wall time of `list_object_keys("")` |
| `cleanup_delete_ms` | `with_store_conc` | wall time of delete fan-out / bulk delete |
| `total_ms` | `with_store_conc` | already emitted as `[ok] paginate Nms` |

Prefix working logs `[p30]` so they are greppable and obviously temporary.

---

## Phase 0 - re-baseline on the landed tree (Cycle 0)

**Goal:** confirm row D still holds on the branch tip this plan cuts from,
and capture seed worker count explicitly (16 today).

1. Land temporary `[p30]` phase timers (behavior-neutral).
2. Run paginate three times; fill:

| Run | seed_workers | K | seed_ms | body_list_ms | cleanup_list_ms | cleanup_delete_ms | total_ms |
| --- | ------------ | - | ------- | ------------ | --------------- | ----------------- | -------- |
| D' local (tip) | 16 | 32 | | | | | |
| D' x2 | 16 | 32 | | | | | |
| D' x3 | 16 | 32 | | | | | |

3. Thesis check: seed ≈ half of total; body list ≈ `(1050/32)*RTT`;
   deletes ≈ body list. If the shape has drifted, revise Cycles A-D order
   before coding.
4. **Commit:** `test: [p30] phase-timing logs for paginate sub-30s diagnosis`
   (or keep uncommitted until Cycle A if the branch prefers one squash -
   either is fine; logs must not merge).

**Exit:** table filled; go/no-go on Cycle A (seed) vs jumping to B (K) if
seed is no longer dominant.

---

## Track A - test-only knob sweeps (no product API)

### Cycle A - raise seed fan-out (W-p30-seed)

**Why first:** seed is ~25s of ~50s and still hardcodes 16 threads while
body already runs at 32.

- **RED:** none required for a pure harness knob; optional pin that seed
  worker count is a named constant (not a magic 16) if that helps review.
- **GREEN:** in `s3_integ_list_paginates`, set seed workers to **32**
  (match landed K). Re-time. Then try **64**. Pick the lowest worker count
  that is within ~10% of the best total (diminishing-returns stop).
- Watch for put `SlowDown` / error rate; on sustained throttle step back one
  notch.
- **Commit:** `test: [p30] list_paginates seed fan-out matches scale knobs`

**Expected:** seed 25s -> ~12-13s (32) or ~6-8s (64). Total ~35-45s.

### Cycle B - raise paginate K (W-p30-K)

**Why second:** body list + deletes are near the K=32 RTT floor; library
path allows higher K for this test.

- **GREEN:** `with_store_conc("paginate", 64, ...)` then `128`. Cleanup
  deletes already use store concurrency, so they ride along. Seed workers
  may stay at Cycle A's pick or be re-matched - record both.
- Stop conditions per K step:
  - total improved < 10% vs previous K -> stop, keep previous
  - visible `SlowDown`/5xx storm -> step back
  - total already < 30s with margin -> stop, do not chase
- **Commit:** `test: [p30] list_paginates with_store_conc(K) after K sweep`
  (K named in the message)

**Expected at K=64:** body ~6-7s, deletes ~5-6s. Combined with Cycle A,
total often lands in the **20-28s** band - which may **end the plan early**
(skip C/D/E). That is success, not incomplete work.

### Cycle A+B exit gate

If three consecutive totals are < 30s:

- strip `[p30]` logs (keep `[ok] ... ms`)
- full live suite green
- roadmap decision-log row
- **stop; do not start Track B**

If still >= 30s, continue.

---

## Track B - structural backups (product/harness API, measurement-gated)

### Cycle C - `DeleteObjects` bulk cleanup (W-p30-delete)

**Precondition:** after A+B, `cleanup_delete_ms` is still a top-2 phase.

- **RED (offline):** pure helper pins for key chunking
  (`chunks of 1000`, remainder chunk, empty input) and a test-double or
  SDK-shaped partial-failure mapping if introduced as a pure function.
  Compile-RED on the new `S3Store` method is acceptable.
- **API (locked name):**

  ```rust
  impl S3Store {
      /// Bulk-delete object keys via S3 DeleteObjects (max 1000 keys per
      /// request). Keys only; no head enrichment. Intended for harness
      /// cleanup / bulk discovery paths where per-key DeleteObject RTT
      /// dominates. Not on ObjectStore - trait delete stays single-key.
      pub fn delete_object_keys(&self, keys: &[String]) -> Result<DeleteKeysReport, Error> { ... }
  }
  ```

  Exact `DeleteKeysReport` shape is in-cycle (at minimum: deleted count +
  per-key errors for W104 stderr). Quiet success when `keys` is empty.
- **GREEN:** implement with SDK `delete_objects`, chunk at 1000, map
  per-key errors without failing closed on a single bad key (cleanup must
  keep draining - same ethos as today's `delete_keys_bounded`). Point
  `with_store_conc` cleanup at it; retire or keep `delete_keys_bounded` as
  a fallback only if something still needs single-key fan-out (prefer
  delete the dead path).
- **Live re-time.** Expected: cleanup deletes 5-11s -> ~1-2s (2 RTTs for
  1050 keys).
- **Commit:** `feat: [p30] S3Store::delete_object_keys via DeleteObjects`
  then `test: [p30] with_store cleanup uses delete_object_keys`

**Scope guard:** do not plumb bulk delete into `execute_plan` /
`DeleteRemote` in this plan. Planner/executor stay per-key.

### Cycle D - small-body `put_from` fast path (W-p30-put)

**Precondition:** after A+B(+C), `seed_ms` is still a top-2 phase.

- **Why:** every 1-byte seed put still does create-temp -> copy ->
  `sync_all` -> `ByteStream::from_path` -> unlink (`src/store/s3.rs`
  `put_from`). Local IO x 1050 sits on top of PutObject RTT.
- **RED:** offline pins:
  - small size takes the in-memory/`ByteStream::from` branch (instrument
    with a test-only counter or temp-dir side effect assertion)
  - large size still uses the temp-file path (P1r-put-size / streaming put
    integ test remains green at 8 MiB)
  - short-read and ceiling rejection unchanged
- **Threshold:** lock from a quick seed microbench (candidates: 256 KiB,
  1 MiB, 8 MiB). Document the picked value and rationale in the commit body.
- **GREEN:** branch in `put_from`; no new deps; owner-only temp path
  preserved for the large branch.
- **Live re-time** of paginate seed.
- **Commit:** `perf: [p30] put_from small-body skips temp file`

### Cycle E - coverage split (last resort only)

**Precondition:** A-D cannot land < 30s **or** higher K is unstable and
bulk delete + put fast path still leave total >= 30s while keeping 1050
enriched heads.

- Split:

  | Test | Proves | Shape |
  | ---- | ------ | ----- |
  | `s3_integ_list_paginates` | second List page | N=1050, `list_object_keys` (or unenriched count), no head tax |
  | `s3_integ_list_enrich_heads_scales` (name flexible) | I20-heads at scale | enriched `list`, N=128-256, K from Cycle B |

- Update [test-matrix.md](../test-matrix.md) and roadmap decision log:
  state explicitly that the single-test "1050 heads live proof" is replaced
  by the pair.
- **Commit:** `test: [p30] split pagination proof from head-scale proof`

Prefer not to reach E. If A-D land under 30s with N=1050 enriched list,
E is permanently out of scope for this plan.

---

## Track C - hygiene + close-out (always)

### Cycle F - strip logs, docs, suite gate

1. Remove all `[p30]` phase timers; keep `[ok] {name} {total_ms}ms`.
2. Full live suite:

   ```bash
   cargo test --locked --test s3_integration -- --nocapture
   ```

   Record final table row + full-suite total. Paginates must be < 30s x3.
3. Offline gate + clippy + fmt clean.
4. `doc/roadmap.md` decision-log row `P30-paginate-sub-30s` summarizing:
   final K, seed workers, whether DeleteObjects / put fast path / split
   landed, final timing row, and the aim bar.
5. If an issue was filed, close it against the PR.

**Commit:** `test: [p30] strip diagnosis logs; record sub-30s close-out`
(may fold docs into the same commit).

---

## Phase timing table (fill as cycles land)

| Run | seed_workers | K | seed_ms | body_list_ms | cleanup_list_ms | cleanup_delete_ms | total_ms | Notes |
| --- | ------------ | - | ------- | ------------ | --------------- | ----------------- | -------- | ----- |
| #17 D (ref) | 16 | 32 | 24473 | 12297 | 6477 | 11010 | 54429 | PR 23 / issuecomment-5462688729 |
| 0 baseline | 16 | 32 | 23968 | 13800 | 1678 | 10329 | 49953 | Cycle 0 run 1 |
| 0 baseline x2 | 16 | 32 | 23884 | 13809 | 1428 | 11082 | 50337 | Cycle 0 run 2 |
| 0 baseline x3 | 16 | 32 | 28209 | 12559 | 1797 | 10475 | 53180 | Cycle 0 run 3 |
| A seed=32 | 32 | 32 | 13521 | 13393 | 1846 | 11694 | 40625 | |
| A seed=32 x2 | 32 | 32 | 12320 | 12680 | 1609 | 10929 | 37676 | |
| A seed=64 | 64 | 32 | 14115 | 15396 | 2388 | 11058 | 43132 | variance high |
| A seed=64 x2 | 64 | 32 | 7512 | 13755 | 1837 | 11772 | 35011 | keep seed=64 |
| B K=64 | 64 | 64 | 7550 | 7427 | 1748 | 4950 | **21859** | under 30s |
| B K=64 x2 | 64 | 64 | 8496 | 7335 | 1657 | 5280 | **22905** | under 30s |
| B K=64 x3 | 64 | 64 | 7844 | 6781 | 5771 | 5826 | **26357** | under 30s; 4 delete_errs (W104) |
| B K=64 stab | 64 | 64 | - | - | - | - | 32794 | over bar (variance); step to 128 |
| B K=128 x1 | 64 | 128 | - | - | - | - | **22729** | |
| B K=128 x2 | 64 | 128 | - | - | - | - | **24012** | |
| B K=128 x3 | 64 | 128 | - | - | - | - | **17835** | |
| B K=128 x4 | 64 | 128 | - | - | - | - | **18372** | |
| B K=128 x5 | 64 | 128 | - | - | - | - | **18167** | |
| C DeleteObjects | - | - | - | - | - | - | - | not needed |
| D put fast path | - | - | - | - | - | - | - | not needed |
| E split | - | - | - | - | - | - | - | not needed |
| Final knobs | 64 | 128 | - | - | - | - | **18-24s** | merge bar met with margin; stop |

Optimistic ceiling if A+B+C all help and E is avoided:

| Phase | Today | Optimistic |
| ----- | ----- | ---------- |
| Seed | 25s | 6-12s |
| Body list | 13s | 3-7s |
| Cleanup list | 2-6s | 1-2s |
| Cleanup deletes | 11s | 1-2s |
| **Total** | **~50s** | **~15-25s** |

---

## Risks and fallbacks

| Risk | Fallback |
| ---- | -------- |
| Higher K trips S3 `SlowDown` / shared retry quota (I8) | step K back; lean on Cycle C for deletes; do not disable retries |
| Seed@64 saturates disk on temp-file creates | Cycle D put fast path; or cap seed workers below K |
| `DeleteObjects` partial failure semantics surprise cleanup | per-key error list + W104 stderr; never invent success; offline pins on chunk edges |
| Small-body put path regresses streaming / large put | keep 8 MiB `s3_integ_streaming_put_large` green; threshold well below that |
| CI RTT >> local RTT so local <30s but CI is not | infra note (colocate bucket/runner); loosen aim only with decision-log evidence, do not re-skip the test |
| Cycle E weakens the #17 narrative | require roadmap entry that names the replacement head-scale test; do not silently shrink N inside the original test |
| Scope creep into executor bulk delete | hard stop - harness/`S3Store` only this plan |
| Chasing sub-15s after bar is met | stop at <30s x3 with margin; further polish is a different plan |

---

## Acceptance criteria

- [ ] Phase 0 baseline table filled on the branch tip.
- [ ] `s3_integ_list_paginates` total **< 30s** on three consecutive live
      runs (same bucket/region class as #17 Phase 0).
- [ ] N = 1050 preserved **or** Cycle E decision-logged with replacement
      head-scale coverage.
- [ ] Body still exercises enriched `list` at scale **or** Cycle E pair is
      green and documented.
- [ ] Full `s3_integration` suite green; no CI `--skip` reintroduced.
- [ ] `[p30]` diagnosis logs stripped before merge; `[ok] ... ms` retained.
- [ ] Offline `cargo test --offline --lib --bins` green.
- [ ] `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`
      clean.
- [ ] Roadmap decision-log entry `P30-paginate-sub-30s` written.
- [ ] If Cycle C landed: `delete_object_keys` is on `S3Store` only (not the
      trait), chunked at 1000, W104-loud on partial failure, offline chunk
      pins present.
- [ ] If Cycle D landed: large-body temp-file path + streaming integ test
      still green; threshold documented.

---

## Suggested commit order (summary)

1. `test: [p30] phase-timing logs for paginate sub-30s diagnosis` (Cycle 0)
2. `test: [p30] list_paginates seed fan-out matches scale knobs` (A)
3. `test: [p30] list_paginates with_store_conc(K) after K sweep` (B)
4. *(stop here if < 30s)*
5. `feat: [p30] S3Store::delete_object_keys via DeleteObjects` (C, if needed)
6. `test: [p30] with_store cleanup uses delete_object_keys` (C)
7. `perf: [p30] put_from small-body skips temp file` (D, if needed)
8. `test: [p30] split pagination proof from head-scale proof` (E, last resort)
9. `test: [p30] strip diagnosis logs; record sub-30s close-out` (F)

---

## Out of scope / already landed

- I20 head/transfer pool and `with_store_conc` (issue #20 / PR 22).
- #17 Track A harness cleanup (`list_object_keys`, parallel single-key
  deletes, CI skip retirement, job merge) and Track B overlap gauges.
- SDK retry/backoff (issue #8).
- I15 fail-closed enrichment semantics.
- Production bulk delete in `execute_plan`.
- Changing `DEFAULT_CONCURRENCY` / `MAX_CONCURRENCY`.
- Multipart upload (post-v1).
- Making `ObjectStore::list` optionally unenriched for production callers.
