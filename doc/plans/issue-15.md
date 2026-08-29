# Issue 15 fix plan: list() falls back to LastModified; status/pull never converge

**Status:** implemented (W111-W114 landed; review round 1 follow-ups:
doc/plans/pr-16-fixes-5459427610-5459427837.md)
**Issue:** https://github.com/tlkahn/vaultsync/issues/15 (OPEN, reported by tlkahn)
**Design refs:** [object-store.md](../object-store.md), [sync-model.md](../sync-model.md), [test-matrix.md](../test-matrix.md), [roadmap.md](../roadmap.md)
**Verified baseline:** `cargo test --offline` green at `5e0526a` (352 lib + 8 env-gated
integration, self-skipping without `VAULTSYNC_TEST_S3_BUCKET`);
`cargo clippy --all-targets -- -D warnings` clean; `cargo fmt --check` clean.

---

## Problem recap (from the issue, verified against the tree)

`S3Store::put_from` stamps the true local mtime into user metadata
`vaultsync-mtime` (src/store/s3.rs, `MTIME_KEY`). `S3Store::list` uses
ListObjectsV2, which cannot return user metadata, so listing entities carry the
object's **upload** `LastModified` instead (`list_prefix_objects`). `head` /
`get_to` prefer the metadata, so a pulled file gets the *original* (earlier)
mtime restored on disk.

Net effect: after a successful `push`, every local mtime (original) compares
older than the listing's upload time, so the planner classifies every key
`RemoteNewer` and `status` / `pull` plan (and re-download) everything forever.
Download-direction incrementality does not exist.

## Locked decisions (made with the user before writing this plan)

| ID | Decision | Choice |
| -- | -------- | ------ |
| I15-approach | Fix approach | **Option 1: per-object `head` in `list`.** Enrich each listed object's mtime (and etag) via HeadObject, which reads `vaultsync-mtime`. Preserves exact-mtime restoration and the stateless design. Rejects Option 2 (local state cache; cuts against statelessness) and Option 3 (converges but sacrifices exact-mtime restoration, a verified feature). |
| I15-errors | HEAD failure policy | **Fail-closed.** A `NotFound` head (object deleted between LIST and HEAD) drops the row silently - a true concurrent-delete race, and planning against a vanished object would be worse. Any *other* head error fails the whole listing, matching the W61 fail-closed ethos (never plan against a knowingly-degraded remote view; `pull --delete` safety). |
| I15-concurrency | Request shape | **Sequential heads for now.** One `block_on` per head, reusing the existing per-call pattern (async containment D2 unchanged). Bounded concurrency / batching is deferred to Phase 3's request-pool work, recorded in the roadmap decision log. |

Cost (documented, accepted): every `status` / `push` / `pull` plan now costs
1+ ListObjectsV2 page requests **plus N HeadObject requests** (N = remote
objects under the prefix). Latency grows by roughly N x RTT; request volume is
the deliberate v1 trade for correctness without local state. Phase 3 revisits
with a request pool.

## Method: strict fine-grained TDD

Same rules of engagement as Phase 1/2 ([phase-1.md](phase-1.md),
[phase-2.md](phase-2.md)):

1. **RED** - named failing test first; confirm it fails for the right reason.
   Every behavior change first lands a failing test that exposes/replicates
   the reported defect through a production API.
2. **GREEN** - smallest implementation that passes.
3. **REFACTOR** - behavior-preserving cleanup on green.
4. One logical behavior per cycle; full per-commit gate:
   `cargo test --offline` + `cargo clippy --all-targets -- -D warnings` +
   `cargo fmt --check`.
5. **No network in the default suite.** The bug's mechanism (listing reports
   upload-time mtimes while head/get report metadata mtimes) is replicated
   offline with a test double; only the thinnest request/response shell is
   network-only. The env-gated S3 test is the literal issue replication and
   acceptance check - it is written first (RED against real S3 before the
   wiring lands, GREEN after), but the fine-grained RED/GREEN cycles are the
   offline tests.
6. Docs-only changes have no RED; they land under the all-green gate.
7. Work items continue the project W-series (W111+). One commit per item.
   Characterization tests are never silently edited; the one retired
   doc-comment limitation (s3.rs module doc) flips in the same commit as the
   wiring (W113).

## Design (what lands)

No trait changes. `ObjectStore` signatures are untouched; the mock is
untouched (its `list` already reports true mtimes, so it converges today).

**New production helper, sync, generic, offline-testable** - in
`src/store/mod.rs`:

```rust
/// Enrich a listing's object entities with per-object head() results so
/// plans compare client mtimes, not upload times (issue #15, I15-approach).
/// Folder views are skipped (not objects; head would NotFound by contract).
/// A NotFound head drops the row (concurrent-delete race); any other head
/// error fails the listing (I15-errors, fail-closed). Entity order (sorted)
/// and warnings are preserved; only `mtime_ms` and `etag` are overridden.
pub(crate) fn enrich_with_head_mtimes<S: ObjectStore + ?Sized>(
    store: &S,
    listing: Listing,
) -> Result<Listing, Error>
```

Implementation shape: iterate `listing.entities`; skip `is_folder()`; call
`store.head(&e.key)`; on `Ok(h)` set `e.mtime_ms = h.mtime_ms; e.etag = h.etag;`
(size stays from the listing - a mid-list rewrite race is out of scope and
`plan()` is already tolerant of either value); on `Err(Error::NotFound(_))`
drop the entity; on any other `Err` return it.

**S3 wiring** - `S3Store::list` (src/store/s3.rs) passes its converted listing
through `enrich_with_head_mtimes(self, ...)` before returning. Each head is
one `block_on`, exactly like today's standalone `head` (async containment
unchanged; no `async` outside `store::s3` - the helper itself is sync trait
code).

**Offline test double** - `src/testutil` (cfg(test) only): `S3LikeListStore`
wrapping `MockStore`. Its `list` delegates to the mock but rewrites every
object entity's `mtime_ms` to a fixed later "upload time" (simulating the
ListObjectsV2 `LastModified` fallback), then calls the production
`enrich_with_head_mtimes` - mirroring the W113 S3 wiring so the lib-level
convergence test exercises the real production path. `head` / `get_to` /
`put_from` / `delete` delegate unchanged (mock `head` reports the metadata
mtime, exactly like S3). A `fail_head: Option<Error>` knob (or a tiny separate
stub) forces head failures for the error-policy tests.

Why the double replicates the bug honestly: pre-fix, its raw listing produces
exactly the planner input real S3 produces today (local mtime < upload time on
every key -> all `RemoteNewer` -> Download-everything). The convergence
assertion is RED before `enrich_with_head_mtimes` exists (compile failure -
the project's established RED form for a new function) and GREEN after the
helper + wiring land.

---

## Work items

### W111 - core fix: `enrich_with_head_mtimes` + offline convergence proof

**RED (three tests, same commit):**

1. `enrich_overrides_listing_mtime_with_head_mtime` (src/store/mod.rs tests):
   hand-built `Listing` whose object entities carry upload-time mtimes; the
   double's `head` reports the true (earlier) metadata mtimes. Assert the
   enriched listing carries the head mtimes (and head etags), folder entities
   pass through untouched (mtime stays `None`), order stays sorted, and
   `warnings` are preserved verbatim.
2. `enrich_drops_row_when_head_not_found` : a listed key whose `head` answers
   `NotFound` (concurrent delete race) is dropped from the enriched listing;
   sibling rows are unaffected.
3. `enrich_fails_closed_on_head_error` : a non-NotFound head error (e.g.
   `Error::Unavailable`, the throttling/transient class) propagates - the
   listing fails rather than degrading to LastModified (I15-errors).

Confirm all three fail (compile failure: the function does not exist).

**GREEN:** implement `enrich_with_head_mtimes` in src/store/mod.rs exactly as
shaped above. Gate.

**RED (lib-level replication of the issue, offline):**

4. `build_plan_status_converges_after_push_with_s3like_listing` (src/lib.rs
   tests): `TempDir` vault with 2-3 files at fixed old mtimes (nested folder
   included); push through `build_plan` + `execute_plan` into
   `S3LikeListStore`; then `build_plan(&local, &store, Mode::Status, ...)`.
   Assert zero Upload / zero Download / zero Conflict (folders may Skip).
   Pre-fix sanity (documented in the test comment): planning against the
   double's *raw* degraded listing yields Download-everything - that is the
   bug from issue #15 reproduced without a socket.
5. `pull_into_fresh_dir_then_status_converges_with_s3like_listing` : pull the
   double's contents into a fresh `TempDir`, assert byte-identical files and
   exact mtimes restored (existing feature, must not regress), then
   `build_plan(Status)` on the fresh dir plans 0 mutating actions. This pair
   maps one-to-one onto the issue's acceptance bullets.

**GREEN:** these pass once the double's `list` wires
`enrich_with_head_mtimes` (the production helper). If the double wiring is
considered part of the test harness rather than the fix, that is fine - the
wiring that matters ships in W113 and is covered there by the env-gated
replication. Gate.

**REFACTOR:** none expected; the helper is a single loop. Gate.

### W112 - env-gated issue replication (written first, RED on real S3)

Add to tests/s3_integration.rs (env-gated like the rest of the suite; skips
cleanly offline):

1. `s3_integ_status_converges_after_push` : seed a `TestDir` vault with files
   at a fixed old mtime (reuse the file/mtime helpers of
   `s3_integ_e2e_push_pull`); push via `build_plan` + `execute_plan`; then
   `build_plan(Status)` against the same store. Assert `stats.upload == 0`,
   `stats.download == 0`, `stats.conflict == 0`. RED on real S3 today (plans
   N downloads - the exact issue report); GREEN after W113.
2. `s3_integ_pull_then_status_converges` : wipe local, pull into a fresh dir,
   assert exact mtimes restored (abs_diff < 2000 ms, as the e2e test does),
   then `build_plan(Status)` on the fresh dir: zero mutating actions. RED
   today, GREEN after W113.

These are the acceptance harness, not the TDD units (per the Phase 2 method:
env-gated tests are not RED/GREEN units; the offline cycles in W111 are).
They must be run at least once against real AWS S3 before the fix (observe
RED) and after (observe GREEN), and the run recorded in the PR description.

### W113 - S3 wiring: `S3Store::list` enriches via head

**RED:** none offline beyond W111 (the wiring is one call inside the
network-only shell); W112's env-gated pair is the RED for this item on real
S3.

**GREEN:** in `S3Store::list`, wrap the converted listing:
`enrich_with_head_mtimes(self, Listing { entities, warnings })`. Confirm the
existing offline s3.rs unit tests (`convert_listed`-level, pure) still pass
unchanged - they test below the wiring and must not move.

**Same commit (doc flip, characterization-comment amendment):** rewrite the
s3.rs module doc's "documented limitation" paragraph (lines ~14-18): `list`
now surfaces client mtimes via per-object head (I15-approach), cost is N
extra HeadObject requests per plan, sequential until Phase 3 (I15-concurrency),
head failures are fail-closed except the NotFound drop (I15-errors).

**Live verification:** run W112's two tests plus the existing
`s3_integ_e2e_push_pull` against real AWS S3; all GREEN. Re-run the issue's
original repro shape (push 4 files; `status` -> 0 actions, exit 0; pull into
fresh dir; `status` -> 0 actions) as the final acceptance pass.

**REFACTOR:** none. Gate.

### W114 - docs + roadmap decision log (docs-only, all-green gate)

1. doc/test-matrix.md: delete the "Known limitation" section (lines ~27-33)
   and add matrix rows for the two convergence checks (status-after-push
   no-op; status-after-pull no-op; exact mtimes preserved), marked done with
   the verifying test names.
2. doc/object-store.md: operations-mapping table - `list` row notes the
   per-object HeadObject mtime enrichment (N+1 request shape; sequential
   until Phase 3); Metadata section unchanged (`vaultsync-mtime` stays the
   source of truth).
3. doc/roadmap.md decision log: new entry recording I15-approach /
   I15-errors / I15-concurrency (option 1 chosen over the state cache and
   the LastModified-restoration sacrifice; bounded concurrency explicitly
   deferred to Phase 3's request-pool work).
4. doc/cli.md: only if it states request-count or limitation claims about
   `status`/`pull` (check; otherwise untouched).

## Acceptance mapping (issue #15 bullets)

| Issue acceptance | Covered by |
| ---------------- | ---------- |
| After `push`, `status` on the same vault plans 0 actions (exit 0) | W111 test 4 (offline) + W112 test 1 + W113 live repro |
| After `pull` into a fresh dir, `status` there plans 0 actions | W111 test 5 (offline) + W112 test 2 + W113 live repro |
| Exact-mtime restoration on pull is preserved | W111 test 5 + W112 test 2 (abs_diff < 2000 ms assertions) + existing `s3_integ_e2e_push_pull` stays green |
| Offline test suite stays green without network | Per-commit gate `cargo test --offline`; env-gated tests self-skip; no new dependencies |

## Risks / notes

- **Request volume.** N extra HEADs per plan is the accepted cost
  (I15-approach). Throttling (429/5xx) maps to `Error::Unavailable` and fails
  the listing loudly (I15-errors) - acceptable for v1; Phase 3's retry/pool
  work is the mitigation, and the roadmap log entry (W114.3) makes the
  deferral explicit so it is not lost.
- **Folder entities** are never head-enriched (not objects; `head` on a folder
  key is NotFound by contract). Synthesized folder mtimes stay `None`;
  planner folder handling is unchanged.
- **Out-of-band objects without `vaultsync-mtime`** degrade exactly as today:
  `head` falls back to `LastModified` (`decode_mtime`), so the enrichment is a
  no-op for them. No regression for foreign objects.
- **Mock divergence watch:** the mock keeps true mtimes in `list`, so mock-only
  planner tests cannot see this bug class. The `S3LikeListStore` double (W111)
  is the standing guard; keep it in `testutil` for future backend-behavior
  regressions.
- **No trait break, no new dependencies** (dependency policy: nothing added).
