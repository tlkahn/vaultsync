# Issue 33 plan: Apply ignore patterns to remote listings in `build_plan`

**Status:** implemented (W195-W202 landed on this worktree)

Checklist reconciled (`- [x]`) under review plan `pr-39-review-5467770600.md` W207.
**Issue:** https://github.com/tlkahn/vaultsync/issues/33 (OPEN; P1 of epic #9)
**Branch:** `worktree-apply-ignore-patterns-to-remote-listings-in-build_plan` (this worktree; cut from
`main` tip `b1af61c` = Issue 30 / PR 35 merged)
**Design refs:** issue #33 body (locked D-both-sides remote half, delete
invariant remote half, D-report remote half, D-library-seam), epic #9,
[sync-model.md](../sync-model.md), [roadmap.md](../roadmap.md) (D3),
sibling plans [issue-30.md](issue-30.md) (matcher landed), parallel #31/#32
(out of scope here)
**Verified baseline (recorded at plan time):** tip `b1af61c`. Gate on this
worktree:
`cargo test --offline --lib --bins` = 454 passed / 0 failed / 1 ignored;
`cargo clippy --all-targets -- -D warnings` clean;
`cargo fmt --check` clean.
**Blocker check:** depends on #30 (landed on `main`). Does **not** require
#31 or #32. Does **not** retire W25 or touch CLI ignore wiring (#34). Blocks
#34 e2e (delete invariant end-to-end needs this remote half).

---

## Problem recap (from the issue, verified against the tree)

Epic #9 D-both-sides requires ignored keys to be **absent** from both local
and remote entity lists before `plan()`, so `--delete` never plans a delete
for an ignored remote-only (or local-only) key. #30 shipped the pure
`IgnoreSet` matcher. This issue owns the **remote half** only.

Today (`src/lib.rs` `build_plan`):

| Step | Behavior |
| ---- | -------- |
| `store.list("")` | full remote listing + store warnings |
| drop empty keys | R4-M2 exact-prefix marker |
| `partition_reserved_remote_keys` | drops `.vaultsync-check-*` / tmp-sibling leftovers; W79 warning |
| `ensure_valid_key` | fail-closed on remaining keys |
| `plan()` | pure pairing; filter-agnostic |
| IgnoreSet | **not consulted** |

`IgnoreSet` lives in `src/ignore.rs` (`from_patterns` + `matches`). CLI still
refuses non-empty `[ignore].patterns` on push/pull/check (W25/M3 in
`src/cli.rs`). `build_plan` call sites: `src/cli.rs` (2), `status_with_store`,
and many unit tests in `src/lib.rs` / `src/exec.rs` (all empty-ignore until
#34).

Precedent to mirror: `partition_reserved_remote_keys` +
`reserved_drops_warning` - pure partition, order-preserving, warning is a
separate pure string helper pushed into `PlanReport.warnings`.

---

## Locked decisions (owned by #33; do not reopen in implementation)

| ID | Lock | Choice |
| -- | ---- | ------ |
| D-both-sides (remote) | When | Filter **after** empty-key drop + `partition_reserved_remote_keys`, **before** `ensure_valid_key` / `plan()`. |
| D-both-sides (remote) | Effect | An ignored remote key is **absent** from `remote_entities` (no plan row of any kind - not `Skip(ignored)`). |
| D-both-sides (remote) | Matcher | Shared `IgnoreSet` only - no second matcher, no re-parse. |
| Delete invariant (remote) | `push --delete` | Must **not** plan `DeleteRemote` for a remote-only key that matches ignore (it never enters the remote list, so it is not "remote-only"). Non-ignored remote-only still plans `DeleteRemote`. |
| D-report (remote) | Warning | When dropped count `N > 0`, push one `PlanReport` warning. Locked wording (pin in tests via substring): `ignored {N} remote key(s) by ignore patterns`. Count only; **do not** dump key names (reserved warning still names first 5; ignore stays count-only per issue sketch). |
| D-report (remote) | Empty set | `N == 0` (empty `IgnoreSet`, or no matches): no ignore warning. |
| D-library-seam | `build_plan` | Accepts `ignore: &IgnoreSet`. `plan()` stays pure and filter-agnostic. |
| D-library-seam | Default | Call sites that do not care pass `&IgnoreSet::empty()` (new inherent; see Design). Empty set = today's behavior. |
| D-library-seam | Helper | Pure `partition_ignored_remote_keys(entities, &IgnoreSet) -> (kept, dropped)`, `pub(crate)`, order-preserving both sides - same shape as reserved partition. |
| D-ordering | Reserved first | Reserved partition runs first. A key that is both reserved and would match ignore is dropped **only** as reserved (reserved warning only; not counted in ignore `N`). |
| D-scope | Non-goals | No local walk prune (#32). No profile/config resolution (#31). No CLI wiring / W25 retire / user docs (#34). No remote list optimization (delimiter/prefix exclusion). No `plan()` changes. No Cargo.toml deps. Do not retire W25 or change config defaults. |

### Signature (normative)

```rust
pub fn build_plan(
    local: &LocalFs,
    store: &dyn ObjectStore,
    mode: Mode,
    opts: &PlanOpts,
    ignore: &IgnoreSet,
) -> Result<PlanReport, Error>
```

Rationale vs alternatives (locked here so implementation does not thrash):

| Alternative | Why not |
| ----------- | ------- |
| `Option<&IgnoreSet>` (`None` = empty) | Two ways to spell "no ignores"; every call site still changes; slightly less honest at the type level. |
| Field on `PlanOpts` | Couples filter policy into opts that `plan()` already takes; blurs "plan stays filter-agnostic". |
| Separate `build_plan_with_ignore` | Two entry points drift; issue accepts a single signature break (Phase 3, CLI is the product consumer). |

`status_with_store` also gains `ignore: &IgnoreSet` and forwards it (same default story in tests).

### Pipeline order inside `build_plan` (normative)

```text
store.list("")
  -> aggregate listing.warnings
  -> drop empty keys
  -> partition_reserved_remote_keys   // may push reserved warning
  -> partition_ignored_remote_keys    // NEW; may push ignore warning
  -> ensure_valid_key on kept         // ignored invalid keys never reach here
  -> plan() + existing post-passes (case collision, followed-symlink)
```

### Warning helper (normative)

```rust
pub(crate) fn ignored_remote_drops_warning(dropped_count: usize) -> String {
    format!("ignored {dropped_count} remote key(s) by ignore patterns")
}
```

Take a count (not `&[Entity]`) to make "no key dump" structural. Only call when
`dropped_count > 0`.

---

## Method: strict fine-grained TDD

Same discipline as issue-30:

1. **RED** - named failing test first; confirm it fails for the right reason
   (compile failure for a missing type/fn is an accepted RED form; once the
   symbol exists, assertion failures are the RED form).
2. **GREEN** - smallest implementation that passes that cycle's tests.
3. **Refactor** only while green; no behavior change without a new RED.
4. One logical behavior per work item. Prefer separate commits per item (or
   RED+GREEN pair collapsed only when RED is compile-fail on a brand-new
   symbol and GREEN is the first body - still prefer separate when practical).
5. After each GREEN: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`,
   and the focused test(s) plus a full
   `cargo test --offline --lib --bins` before the next RED.
6. **Mutation-check** on filter/order/delete pins: temporarily break one
   assertion or swap kept/dropped, confirm RED, revert.
7. Work items continue the project W-series at **W195+**.
8. **Do not** edit W25 characterization tests, walk tests, config defaults,
   or `plan()` pairing logic. Touch `cli.rs` only to pass
   `&IgnoreSet::empty()` at the two `build_plan` call sites (behavior
   unchanged under W25).

### Mutation-check habit (required on filter pins)

After each GREEN that locks absence / delete / ordering / warning:

- Flip one fixture key from ignored to non-ignored (or invert the assert) and
  confirm the test goes RED for the expected reason.
- For the delete invariant: temporarily remove the ignore filter call and
  confirm `push_delete_does_not_delete_remote_ignored` goes RED
  (`DeleteRemote` appears for the ignored key).
- Revert; leave the suite green.

---

## Design (what lands in the tree)

### `src/ignore.rs` - small seam addition

```rust
impl IgnoreSet {
    /// Empty matcher (matches nothing). Equivalent to `from_patterns(&[])`
    /// without the `Result`. Preferred default at `build_plan` call sites.
    pub fn empty() -> Self {
        IgnoreSet { rules: Vec::new() }
    }
}
```

No pattern-language changes. Optional one-line rustdoc note that #33
consumes this at the remote ingest boundary.

### `src/lib.rs` - partition + warning + wire

```rust
/// Split remote entities into `(kept, dropped)` by [`IgnoreSet::matches`]
/// on `Entity.key`. Pure and unit-testable offline. Both output lists
/// preserve input order. Runs *after* [`partition_reserved_remote_keys`]
/// so a reserved leftover is never double-counted as an ignore drop.
pub(crate) fn partition_ignored_remote_keys(
    entities: Vec<Entity>,
    ignore: &IgnoreSet,
) -> (Vec<Entity>, Vec<Entity>) {
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for e in entities {
        if ignore.matches(&e.key) {
            dropped.push(e);
        } else {
            kept.push(e);
        }
    }
    (kept, dropped)
}

pub(crate) fn ignored_remote_drops_warning(dropped_count: usize) -> String {
    format!("ignored {dropped_count} remote key(s) by ignore patterns")
}
```

Wire in `build_plan` immediately after the reserved block:

```rust
let (remote_entities, reserved_dropped) = partition_reserved_remote_keys(remote_entities);
if !reserved_dropped.is_empty() {
    warnings.push(reserved_drops_warning(&reserved_dropped));
}
let (remote_entities, ignored_dropped) =
    partition_ignored_remote_keys(remote_entities, ignore);
if !ignored_dropped.is_empty() {
    warnings.push(ignored_remote_drops_warning(ignored_dropped.len()));
}
// then ensure_valid_key + plan() unchanged
```

### Call-site blast radius

Every `build_plan(...)` / `status_with_store(...)` call gains
`&IgnoreSet::empty()` (or a real set in new tests). Grep anchors at plan
time (must all compile after W196):

- `src/lib.rs` - definition + `status_with_store` + lib tests
- `src/cli.rs` - status path + mutating path (still empty under W25)
- `src/exec.rs` - executor unit tests

No production behavior change until a caller passes a non-empty set (#34).
Library consumers / tests can exercise the seam immediately.

### Files explicitly out of scope (must stay behavior-untouched)

| File | Why |
| ---- | --- |
| `src/plan/mod.rs` | `plan()` stays filter-agnostic |
| `src/local.rs` | local prune is #32 |
| `src/config.rs` | profile resolution is #31 |
| `src/cli.rs` logic beyond empty-set arg | W25 retire / wiring is #34 |
| `src/store/**` | no list-side ignore optimization |
| `Cargo.toml` | no new deps |
| W25 tests in `cli.rs` | must keep refusing non-empty patterns |

---

## Work items

### W195 - pure `partition_ignored_remote_keys` (order + passthrough)

**RED** (in `src/lib.rs` `tests` module, next to reserved partition tests):

1. `partition_ignored_remote_keys_preserves_order`
   - Input entities in order: `a.md`, `.obsidian/workspace.json`, `notes/a.md`,
     `b.md` (use `entity::file`).
   - `IgnoreSet` from `[".obsidian/workspace.json"]`.
   - Assert `kept` keys `["a.md", "notes/a.md", "b.md"]` (order preserved).
   - Assert `dropped` keys `[".obsidian/workspace.json"]`.

2. `partition_ignored_empty_set_passthrough`
   - Same four entities (or any non-empty list) + `IgnoreSet::empty()` (or
     `from_patterns(&[])` if `empty` not yet landed - prefer landing `empty`
     in this item).
   - Assert `kept` is the full input order; `dropped` is empty.

3. `partition_ignored_dir_prefix_nested` (helper-level pin for `.git/`)
   - Entities: `.git/objects/aa`, `notes/a.md`, `.git/HEAD` (file shapes).
   - Pattern `.git/`.
   - Assert dropped = both `.git/...` keys in input order; kept = `notes/a.md`.

Confirm RED: missing `partition_ignored_remote_keys` (compile) or wrong split.

**GREEN:**

- Implement `partition_ignored_remote_keys` as specified.
- Add `IgnoreSet::empty()` if not already present (trivial; tested by
  passthrough).

**Mutation-check:** swap kept/dropped push arms; confirm order test RED;
revert.

**Commit:** `feat: [33] partition_ignored_remote_keys pure helper (W195)`

---

### W196 - `build_plan` / `status_with_store` signature seam (empty = no-op)

**RED:**

- Change the two public signatures to take `ignore: &IgnoreSet`.
- Do **not** wire the partition yet (or wire it but only so empty is no-op -
  wiring with correct helper is fine because empty drops nothing).
- Update **all** call sites to pass `&IgnoreSet::empty()`.
- Add characterization test `build_plan_empty_ignore_matches_today`:
  - Stub/MemoryStore with a normal remote-only `ok.md`; empty local; empty
    ignore; `Mode::Pull`.
  - Assert a Download (or appropriate) row for `ok.md` still appears - proves
    the new arg default does not break ingest.
- Full suite must compile; any missed call site is the RED form.

**GREEN:**

- All call sites updated.
- Optional: wire `partition_ignored_remote_keys` + warning now (empty path
  never warns). Prefer wiring here so W197+ only add tests, not plumbing -
  still keep W197's filter test as the first **behavior** pin for non-empty.

**Gate:** full offline lib/bins green (454+; count will rise with new tests).

**Commit:** `feat: [33] build_plan takes &IgnoreSet (empty default seam) (W196)`

---

### W197 - `build_plan_filters_remote_ignored` (acceptance)

**RED:** test name **exactly** `build_plan_filters_remote_ignored` (issue
checkbox):

```text
local: empty vault (TempDir + LocalFs)
store: StubStore listing
  - .obsidian/workspace.json  (file)
  - notes/a.md                (file)
ignore: IgnoreSet::from_patterns([".obsidian/workspace.json"])
mode: Pull (or Status - either is fine; Pull makes remote-only -> Download)
opts: default
```

Assert:

- No action row whose `key == ".obsidian/workspace.json"`.
- Some action row whose `key == "notes/a.md"` (Download under Pull).

Confirm RED if filter not wired (workspace key plans as remote_only).

**GREEN:** ensure W196 wired the partition; fix if not.

**Mutation-check:** remove the workspace pattern; confirm workspace row
reappears (RED against the assert); restore.

**Commit:** `test: [33] build_plan_filters_remote_ignored (W197)`

(If GREEN required a code fix beyond W196, use `feat:` and note in body.)

---

### W198 - `push_delete_does_not_delete_remote_ignored` (delete invariant)

**RED:** test name **exactly** `push_delete_does_not_delete_remote_ignored`:

```text
local: empty
store: StubStore listing
  - .obsidian/workspace.json   (ignored remote-only)
  - orphan.md                  (non-ignored remote-only)
ignore: [".obsidian/workspace.json"]
mode: Push
opts: PlanOpts { delete: true, ..Default::default() }
```

Assert:

- **No** action with `kind == DeleteRemote` and `key == ".obsidian/workspace.json"`.
- **Some** action with `kind == DeleteRemote` and `key == "orphan.md"`.
- Stronger pin: `plan.stats.delete_remote == 1` (only orphan).

Confirm RED without filter (both would be DeleteRemote).

**GREEN:** filter already sufficient if W197 green; this locks the delete
mapping interaction with absence (not a Skip).

**Mutation-check:** force-filter off; confirm both DeleteRemote; restore.

**Commit:** `test: [33] push_delete_does_not_delete_remote_ignored (W198)`

---

### W199 - `build_plan_ignore_after_reserved` (ordering vs W79)

**RED:** test name **exactly** `build_plan_ignore_after_reserved`:

```text
local: empty
store: StubStore listing
  - .vaultsync-check-1-2-3          (reserved leftover)
  - .git/objects/aa                 (ignored by .git/)
  - notes/a.md                      (kept)
ignore: [".git/", ".vaultsync-check-1-2-3"]
  // second pattern would also match the reserved key IF ignore ran first
  // or IF reserved did not drop it - locks reserved-first + no double count
mode: Pull
```

Assert:

- No plan row for the reserved key or the `.git/...` key.
- Plan row for `notes/a.md`.
- Exactly one warning containing `reserved vaultsync namespace` and naming
  `.vaultsync-check-1-2-3`.
- Exactly one warning containing `ignored 1 remote key(s) by ignore patterns`
  (the `.git/objects/aa` only - reserved must **not** inflate ignore N to 2).
- No warning claims ignored count 2.

Confirm RED if ignore runs first / double-counts, or if reserved warning lost.

**GREEN:** pipeline order as locked (reserved then ignore).

**Commit:** `test: [33] build_plan_ignore_after_reserved ordering (W199)`

---

### W200 - `build_plan_dir_prefix_drops_nested_remote` (acceptance)

**RED:** test name **exactly** `build_plan_dir_prefix_drops_nested_remote`
(issue sketch name; maps to acceptance "Remote prefix under ignored dir"):

```text
local: empty
store: StubStore listing
  - .git/objects/aa
  - notes/a.md
ignore: [".git/"]
mode: Pull
```

Assert:

- No row for `.git/objects/aa`.
- Row for `notes/a.md`.
- Warning `ignored 1 remote key(s) by ignore patterns`.

(Helper-level coverage already in W195; this locks the full `build_plan`
path including warning.)

**GREEN:** should already pass if W197-W199 wired; add if gap.

**Commit:** `test: [33] build_plan_dir_prefix_drops_nested_remote (W200)`

---

### W201 - warning only when N > 0 + multi-drop count

**RED:**

1. `build_plan_ignore_warning_when_dropped`
   - Two ignored remote keys + one kept under a multi-pattern or dir-prefix
     set; assert exactly one ignore warning and it contains
     `ignored 2 remote key(s) by ignore patterns`.
   - Assert warning does **not** embed the raw key strings (count-only lock).

2. `build_plan_no_ignore_warning_when_none_dropped`
   - Non-empty `IgnoreSet` whose patterns match nothing in the listing
     (e.g. pattern `.trash/` but remote only has `notes/a.md`).
   - Assert no warning contains `ignore patterns`.

**GREEN:** `ignored_remote_drops_warning` + `if !ignored_dropped.is_empty()`
guard (may already exist from W196/W197).

**Commit:** `test: [33] ignore drop warning count pins (W201)`

---

### W202 - rustdoc + roadmap decision-log row + plan status

No RED (docs-only), same as W185 on #30.

- `build_plan` rustdoc: document the `ignore` parameter, filter order
  (reserved then ignore), absence semantics, and that `plan()` stays
  filter-agnostic.
- `partition_ignored_remote_keys` / `ignored_remote_drops_warning` rustdoc
  cross-links.
- `IgnoreSet` module rustdoc one-liner: remote filter applied in `build_plan`
  (#33); local prune still #32; CLI wiring #34.
- `doc/roadmap.md` decision-log row, e.g. `I33-remote-ignore`, summarizing:
  post-list `IgnoreSet` filter in `build_plan`; delete invariant remote half;
  count-only PlanReport warning; reserved-first ordering.
- This plan file: set **Status** to `implemented (W195-W202 landed on this
  worktree)` when done.

**Commit:** `docs: [33] remote ignore filter rustdoc + roadmap log (W202)`

---

## Sequencing (commits on the branch)

```text
W195 pure partition helper + IgnoreSet::empty
  -> W196 build_plan signature + call-site empty default (+ wire filter)
  -> W197 build_plan_filters_remote_ignored
  -> W198 push_delete_does_not_delete_remote_ignored
  -> W199 build_plan_ignore_after_reserved
  -> W200 build_plan_dir_prefix_drops_nested_remote
  -> W201 warning count pins
  -> W202 docs / decision log / plan status
```

Rationale: helper before signature keeps the first RED pure and offline;
signature before behavior pins avoids rewriting call sites twice; delete
invariant after basic filter so failure mode is clear; reserved-ordering
after both warnings exist; docs last.

Each arrow is a separate commit with the full offline gate green.

---

## Acceptance mapping (issue checkboxes -> work items)

| Acceptance | Work item |
| ---------- | --------- |
| `build_plan_filters_remote_ignored` | W197 |
| `push_delete_does_not_delete_remote_ignored` | W198 |
| `partition_ignored_preserves_order` / non-ignored passthrough | W195 (`partition_ignored_remote_keys_preserves_order`, `partition_ignored_empty_set_passthrough`) |
| Reserved partition still runs first; no double-count as ignored only | W199 |
| Remote prefix under ignored dir (`.git/objects/aa` via `.git/`) | W195 helper + W200 `build_plan` path |
| Warning emitted when N > 0 remote ignores | W200/W201 |
| fmt / clippy / offline tests green | every commit gate |

Issue test sketch names (use these names so the issue can be checked off
literally):

| Issue sketch | Plan test name |
| ------------ | -------------- |
| `build_plan_filters_remote_ignored` | same (W197) |
| `push_delete_does_not_delete_remote_ignored` | same (W198) |
| `partition_ignored_remote_keys_unit` | `partition_ignored_remote_keys_preserves_order` + `partition_ignored_empty_set_passthrough` + `partition_ignored_dir_prefix_nested` (W195) |
| `build_plan_ignore_after_reserved` | same (W199) |
| `build_plan_dir_prefix_drops_nested_remote` | same (W200) |

---

## Explicit non-goals (refuse scope creep while implementing)

- `[ignore].profile` / built-in injection / `profile = "none"` (#31)
- Walker prune, `WalkReport.skipped_ignored`, local delete invariant (#32)
- Retire W25, CLI passes real `IgnoreSet` from settings, cli.md rewrite (#34)
- Planner `Skip(ignored)` rows (absence only)
- Remote listing optimization (delimiter / prefix exclusion / server-side)
- Changing reserved-namespace rules or `reserved_drops_warning` text
- Dumping ignored key names in the ignore warning (count-only)
- `plan()` / `PlanOpts` changes for ignore
- New crate dependency
- Filtering local entities inside `build_plan` (local half is walk-time #32)
- E2E CLI tests for `--delete` + ignores (belongs to #34 once W25 is gone)

---

## Risk notes

| Risk | Mitigation |
| ---- | ---------- |
| Mass call-site miss after signature change | W196 full-suite compile gate; rg `build_plan\(` / `status_with_store\(` before commit |
| Ignore runs before reserved -> double-count / wrong warning | W199 pins reserved-first + ignore N excludes reserved key |
| `DeleteRemote` still planned via some other path | W198 stats + kind assert; mutation-check by disabling filter |
| Ignored invalid remote key fail-closed vs drop | Locked: ignore filter **before** `ensure_valid_key`, so ignored junk is dropped silently (same class as reserved). Do not add a test that requires InvalidKey on an ignored evil key. |
| Empty `IgnoreSet` still emits a warning | W201 `build_plan_no_ignore_warning_when_none_dropped`; guard on `!dropped.is_empty()` |
| CLI accidentally starts applying patterns | Out of scope; only pass `empty()`; leave W25 tests green |
| Parallel #32/#31 merge conflicts on `IgnoreSet::empty` / rustdoc | Keep `empty()` trivial and docs additive; prefer rebase after #30-only base (already) |
| Folder-form remote keys under dir prefix | Matcher already treats dir prefix; optional extra fixture `folder(".git")` + child file if a gap appears - add under W200, do not invent new matcher rules |

---

## Test implementation notes (fixtures)

Reuse existing lib-test tools:

- `StubStore { listed: Vec<Entity> }` already in `src/lib.rs` tests - preferred
  for pure remote-ingest pins (no put/head).
- `entity::file(key, size, mtime_ms)` / `entity::folder(key)`.
- `TempDir` + `LocalFs::new` for empty local side.
- `PlanOpts { delete: true, ..Default::default() }` for W198.
- Build ignore sets with:

```rust
IgnoreSet::from_patterns(&[".obsidian/workspace.json".to_string()]).unwrap()
// or
IgnoreSet::empty()
```

Do not pull in S3 or live network. Offline lib tests only.

---

## Post-landing (not this plan's commits)

- Open PR titled around `P3-7d` / issue 33; body checklist from below.
- Close #33 after PR merge (or when acceptance is green on `main`).
- Unblocks #34 remote half of delete-invariant e2e; still needs #31+#32 for
  full epic exit.
- PR description must state: remote filter library seam only; W25 still in
  force; CLI still passes empty `IgnoreSet`; no user-visible change until #34.

---

## Implementation checklist (copy into PR body)

- [x] W195 `partition_ignored_remote_keys` + `IgnoreSet::empty`
- [x] W196 `build_plan` / `status_with_store` take `&IgnoreSet`; call sites empty
- [x] W197 `build_plan_filters_remote_ignored`
- [x] W198 `push_delete_does_not_delete_remote_ignored`
- [x] W199 `build_plan_ignore_after_reserved` (reserved first, no double-count)
- [x] W200 `build_plan_dir_prefix_drops_nested_remote`
- [x] W201 ignore warning N>0 / N==0 pins (count-only text)
- [x] W202 rustdoc + roadmap decision log + plan status
- [x] `Cargo.toml` deps unchanged
- [x] W25 tests still green; no CLI apply of real patterns
- [x] `plan()` untouched; filter-agnostic
- [x] `cargo fmt` / `clippy -D warnings` / `cargo test --offline --lib --bins` green
