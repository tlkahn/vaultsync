# Issue 32 plan: Apply ignore patterns in the local walker

**Status:** implemented (W195-W201 landed on this worktree: commits
`5d9048c` W195 .. `aa0e5d6` W200 + W201 docs; offline lib+bin tests 462
passed / 0 failed / 1 ignored, clippy `-D warnings` and fmt clean at each
commit)
**Issue:** https://github.com/tlkahn/vaultsync/issues/32 (OPEN; P1 of epic #9)
**Branch:** `worktree-apply-ignore-patterns-in-the-local-walker` (this worktree; cut from
`main` tip `b1af61c` - Issue 30 / PR 35 merged)
**Design refs:** issue #32 body (locked D-both-sides local half / D-prune /
D-report local half / D-library-seam), epic #9, [doc/plans/issue-30.md](issue-30.md)
(`IgnoreSet` matcher already on `main`), [sync-model.md](../sync-model.md)
(default ignore list), [roadmap.md](../roadmap.md) (D3, I30-matcher),
PR 35 r2 heads-up (basename vs dir-prefix sharpness deferred here)
**Verified baseline (recorded at plan time):** tip `b1af61c`. Gate on this
worktree:
`cargo test --offline --lib --bins` = 454 passed / 0 failed / 1 ignored;
`IgnoreSet` suite (9 tests) green; `src/ignore.rs` + `pub use ignore::IgnoreSet`
already present. Clippy/fmt assumed clean with tip (re-confirm on first commit).
**Blocker check:** #30 landed. Does **not** wire config/CLI (#31/#34) or remote
filter (#33). **Unblocks #10** (depth cap must sequence after this; both touch
`walk()`). Parallel with #31 and #33.

---

## Problem recap (from the issue, verified against the tree)

Epic #9 needs ignored paths absent from local inventory before `plan()` so
`--delete` never invents transfers for intentional local-only junk, and so a
nested `.git/objects` tree is never walked.

Today (post-#30):

| Piece | State | File |
| ----- | ----- | ---- |
| `IgnoreSet` pure matcher | live (`from_patterns` / `matches`) | `src/ignore.rs` |
| `LocalFs` constructors | `new(root)`, `with_follow(root, bool)` only | `src/local.rs` |
| `walk` / `handle_followed_symlink` | reserved names, specials, symlinks only; no ignore | `src/local.rs` |
| `WalkReport` | `skipped_symlinks`, `skipped_temp_files`, `followed_files`, `warnings` | `src/local.rs` |
| CLI / `build_plan` | construct `LocalFs` without ignore; W25 still gates non-empty patterns | `src/cli.rs`, `src/lib.rs` |

`build_plan` already consumes `local.list_report()` and never re-filters local
keys for user ignores. Once the walker omits ignored entities, the planner
sees absence automatically (D-both-sides local half; no planner `Skip` rows).

Identity model reused from #30 / walker:

- Vault-relative keys from `path_to_key` / `Entity.key`: `/`-separated, no
  leading `/`, folders carry trailing `/`, case-sensitive, codepoint-exact.
- Dir patterns are **path-prefix** (`.git/` does **not** match `nested/.git/`
  or `foo.git`); basename patterns are final-segment-anywhere (`.DS_Store`
  matches `notes/.DS_Store`). Locked in #30; #32 must not invent
  basename-dir behavior.
- `IgnoreSet::matches` does not re-validate keys; the walker still builds keys
  via `path_to_key` + `ensure_valid_key` before matching (W193).

---

## Locked decisions (owned by #32; do not reopen in implementation)

| ID | Lock | Choice |
| -- | ---- | ------ |
| D-both-sides (local) | Where filter runs | Apply **during** `walk`. Ignored local paths are **absent** from `local_entities` (not planner `Skip` rows). Shared type: existing `IgnoreSet`. |
| D-prune | Directory patterns | When a directory **key** (trailing `/`) matches: do **not** emit the folder entity; do **not** recurse; count **1** ignored (not every unvisited descendant). File matches skip only that file (count 1). |
| D-report (local) | `WalkReport` | Add `skipped_ignored: u32`. Counting rules above; field rustdoc states the rule so #33/#34 wording stays consistent. CLI printing is **#34 only** - this issue populates the field. |
| D-library-seam | `LocalFs` API | `LocalFs` holds an `IgnoreSet`. Empty set = today's walk behavior (modulo reserved-namespace etc.). Prefer non-breaking constructors: keep `new` / `with_follow` defaulting to empty; add a chainable `with_ignore(self, IgnoreSet) -> Self` (or equivalent) so tests and later #34 can set it without a combinatorial constructor explosion. |
| D-filter-order | Precedence | **Reserved-namespace first / independent.** A reserved temp leftover is counted in `skipped_temp_files` and never re-labeled as ignored, even if a user pattern would also match its name. Symlink skip/follow policy stays as today and runs before dir/file ignore checks on non-symlink arms; followed-symlink arms apply the same ignore checks on the vault-relative key of the link entry (and of children under that link path). |
| D-match-subject | What string is matched | Vault-relative entity key of the **walked path** only (symlink name / child key under the symlink path). **Not** the canonical target's path. Consequence (document, do not "fix"): `alias -> .git` yields keys under `alias/...`, which do **not** match pattern `.git/` (D-match-key from #30). The follow+ignore test must plant a link whose **own key** matches (e.g. symlink named `.git`, or a path prefix pattern covering the link). |
| D-walk-shape | Depth-cap hygiene | Do **not** restructure `walk()` for depth-cap here. Leave #10 a clean follow-up on the post-ignore shape. Thread `IgnoreSet` through existing recursion; no new walker abstraction. |
| D-scope | Non-goals | No remote filter (#33). No config profile injection (#31). No W25 retire / CLI count line / user docs (#34). No planner changes. No new crate. No `matches_dir` on `IgnoreSet` (folder keys already end with `/`). |

### Basename vs dir-prefix (PR 35 r2 heads-up, locked sharp here)

| Pattern | Matches | Does not match |
| ------- | ------- | -------------- |
| `.git/` | `.git/`, `.git/objects/aa/bb` | `nested/.git/`, `.gitignore`, `foo.git`, `git/` |
| `.git` (basename, no slash) | final segment `.git` including `.git/` and `nested/.git/` | `.gitignore` |
| `.trash/` | `.trash/`, `.trash/foo.md` | `not-trash.md`, `foo.trash`, `.trashfile`, `notes/.trash/` |
| `.DS_Store` | `.DS_Store`, `notes/.DS_Store` | `DS_Store.bak`, `notes/.DS_Store.bak` |

Default profile (owned by #31) uses **dir-prefix** `.git/` / `.trash/` plus basename
`.DS_Store`. #32 tests pin both shapes so walk behavior cannot drift from
`IgnoreSet` semantics.

---

## Method: strict fine-grained TDD

Same rules as [issue-30.md](issue-30.md) / phase-1/2:

1. **RED** - named failing test first; confirm it fails for the right reason
   (missing field, entity still present, wrong count, etc.).
2. **GREEN** - smallest implementation that passes that test (and keeps the
   prior suite green).
3. **REFACTOR** - behavior-preserving cleanup only on green.
4. One logical behavior per cycle; full per-commit gate:
   `cargo test --offline --lib --bins` +
   `cargo clippy --all-targets -- -D warnings` +
   `cargo fmt --check`.
5. Characterization / back-compat tests are never silently weakened.
6. Docs-only items have no RED; they land under the all-green gate.
7. Work items continue the project W-series at **W195+**. One commit per item
   unless noted. Prefer the issue's exact test names so acceptance checkboxes
   map 1:1.

Mutation-check discipline (as used on #30): after GREEN, temporarily break
the new branch (e.g. comment out the dir-prune `continue`, or stop
incrementing `skipped_ignored`) and confirm the new test goes RED; revert.
Record "mutation-checked" in the commit body when non-obvious.

---

## Design (what lands in the tree)

### `WalkReport` (`src/local.rs`)

```rust
#[derive(Debug, Default, Clone, PartialEq)]
pub struct WalkReport {
    pub skipped_symlinks: u32,
    pub skipped_temp_files: u32,
    /// Local paths skipped by the configured [`IgnoreSet`] (issue #32).
    /// Counting rule (D-report): each ignored **file** counts 1; each
    /// **pruned directory** counts 1 (the directory key matched; unvisited
    /// descendants are **not** counted). Reserved-namespace skips stay in
    /// `skipped_temp_files` and are not double-counted here. CLI printing of
    /// this count is issue #34.
    pub skipped_ignored: u32,
    pub followed_files: std::collections::BTreeSet<String>,
    pub warnings: Vec<String>,
}
```

`Default` / `PartialEq` continue to derive; existing tests that only assert
other fields stay valid (`skipped_ignored` defaults to 0).

### `LocalFs` plumbing

```rust
pub struct LocalFs {
    root: PathBuf,
    follow_symlinks: bool,
    ignore: IgnoreSet, // NEW; empty by default
    report: std::sync::Mutex<WalkReport>,
    root_canon: std::sync::OnceLock<PathBuf>,
    dir_create_lock: std::sync::Mutex<()>,
}

impl LocalFs {
    pub fn new(root: impl Into<PathBuf>) -> Self { /* ignore: empty */ }
    pub fn with_follow(root: impl Into<PathBuf>, follow_symlinks: bool) -> Self { /* empty */ }

    /// Attach a compiled ignore set (issue #32). Empty set preserves pre-ignore
    /// walk behavior. Chain after `new` / `with_follow`.
    pub fn with_ignore(mut self, ignore: IgnoreSet) -> Self {
        self.ignore = ignore;
        self
    }
}
```

Empty construction: `IgnoreSet::from_patterns(&[]).unwrap()` (or a private
helper). Prefer **not** adding `Default` on `IgnoreSet` in this issue unless
it removes real noise - keep #30's API surface stable.

`list_report` threads `&self.ignore` into `walk`.

### `WalkOpts` + `walk` / `handle_followed_symlink`

```rust
struct WalkOpts<'a> {
    follow_symlinks: bool,
    ignore: &'a IgnoreSet,
}
```

Lifetime the one existing unit test that builds `WalkOpts` inline
(`walk_skips_vanished_subdir`) to pass an empty `&IgnoreSet`.

Pseudo-order inside the non-symlink dir/file arms (after `file_type` and
path/`rel` are known):

**Directory**

1. `key = format!("{}/", path_to_key(rel)?)` (still fail-loud on invalid names).
2. If `opts.ignore.matches(&key)`: `report.skipped_ignored += 1`; `continue`
   (no `folder_entity`, no recurse).
3. Else: emit folder entity if present; recurse `walk(...)`.

**File**

1. If `is_reserved_vaultsync_name(file_name)`: `skipped_temp_files += 1`;
   do **not** consult ignore; do **not** increment `skipped_ignored`.
2. Else: `key = path_to_key(rel)?`.
3. If `opts.ignore.matches(&key)`: `skipped_ignored += 1`; skip emit.
4. Else: `file_entity` + push.

**Followed symlink** (`handle_followed_symlink`)

After locality / dangling / cycle guards (unchanged), on the dir arm and file
arm apply the **same** ignore checks against the symlink entry's
vault-relative key (dir key with trailing `/`, file key without). Dir match:
count + no emit + no recurse into the link path. File match: count + no emit
(+ do not insert into `followed_files`). Reserved check on followed file
symlinks stays **before** ignore, matching today's reserved-first file arm.

Pass `ignore` into the recursive `walk` call that currently rebuilds
`WalkOpts { follow_symlinks: true }`.

### Call sites outside `local.rs`

| Site | This issue |
| ---- | ---------- |
| `src/cli.rs` (`LocalFs::with_follow`) | **untouched** (empty ignore via default) |
| `src/lib.rs` (`LocalFs::new` / tests) | **untouched** |
| `src/ignore.rs` | **untouched** unless a tiny compile fix is forced (prefer zero) |
| `src/plan/**`, `exec.rs`, `config.rs` | **untouched** |

#34 will chain `.with_ignore(resolved_set)` at the CLI boundary.

### Files explicitly out of scope (must stay behavior-equivalent)

- Remote listing filter / `PlanReport` ignore warning (#33)
- `[ignore].profile` resolution (#31)
- W25 gate removal, stderr count line for `skipped_ignored`, cli.md (#34)
- Walker depth cap (#10)
- New dependencies

Allowed docs touch: `doc/roadmap.md` decision-log one-liner + this plan file.

---

## Work items

### W195 - `skipped_ignored` field + empty-ignore seam (compile/behavior RED -> GREEN)

**RED:**

1. Add test `walk_empty_ignore_set_unchanged` in `src/local.rs` tests:
   - Tempdir with a small fixed tree: `note.md`, `sub/a.md`, folder `sub/`.
   - `let fs = LocalFs::new(dir.path());` inventory keys + report.
   - `let fs2 = LocalFs::new(dir.path()).with_ignore(IgnoreSet::from_patterns(&[]).unwrap());`
   - Assert **same sorted keys** and `rep2.skipped_ignored == 0`,
     `skipped_temp_files` / `skipped_symlinks` unchanged vs `fs`.
2. Also assert `WalkReport::default().skipped_ignored == 0` (field exists).

Confirm RED: `with_ignore` / `skipped_ignored` missing (compile fail) or field
always absent.

**GREEN:**

- Add `skipped_ignored: u32` to `WalkReport` with the D-report rustdoc
  (counting rule text required even if increments land later - prevents #34
  from guessing).
- Add `ignore: IgnoreSet` field; `new` / `with_follow` initialize empty;
  `with_ignore(self, IgnoreSet) -> Self`.
- Thread `&self.ignore` through `list_report` -> `WalkOpts` / `walk` /
  `handle_followed_symlink` **without** applying matches yet (or apply with
  no behavioral change because tests only use empty sets). Update
  `walk_skips_vanished_subdir`'s manual `WalkOpts` construction.

Do **not** implement prune/skip logic in this commit if it can be deferred
cleanly; empty-set path must be observationally identical to tip.

**Commit:** `feat: [32] LocalFs ignore seam + WalkReport.skipped_ignored (W195)`

---

### W196 - D-prune: directory patterns prune emit + recurse

**RED:** test `walk_prunes_git_dir`:

```text
tempdir/
  note.md
  .git/objects/aa/bb   (file "bb" with any bytes)
  .git/HEAD            (file)
```

- Patterns: `[".git/"]`
- `LocalFs::new(dir).with_ignore(set).list_report()`
- Assert:
  - keys contain `note.md`
  - keys do **not** contain `.git/`, `.git/HEAD`, `.git/objects/`,
    `.git/objects/aa/`, `.git/objects/aa/bb` (entity absence is the
    acceptance bar; optional: `skipped_ignored >= 1`)
- Stronger optional pin (same test or helper): after walk, the only way we
  prove "did not enumerate objects" for acceptance is entity absence of the
  nested keys; do not require FS-level read hooks.

Also pin false friends in the same test or a sibling
`walk_dir_prefix_no_false_friends` (issue acceptance bullet):

| pattern | tree entries that must **remain** listed |
| ------- | ---------------------------------------- |
| `.trash/` | `not-trash.md`, `foo.trash` (file), and ideally `.trashfile` if easy to plant |

Confirm RED (today `.git/...` still lists).

**GREEN:** in the `ft.is_dir()` arm, after building `key`, if
`opts.ignore.matches(&key)` then `skipped_ignored += 1` and `continue`.
Mutation-check: remove the `continue` and confirm nested keys reappear.

**Commit:** `feat: [32] prune ignored directories during local walk (W196)`

---

### W197 - file skip (basename + nested)

**RED:** test `walk_skips_ignored_file` (issue name) and/or
`walk_skips_basename_ds_store` (sketch name - prefer **issue acceptance name**
`walk_skips_ignored_file`, with basename coverage inside):

```text
tempdir/
  note.md
  .DS_Store
  notes/.DS_Store
  notes/keep.md
```

- Patterns: `[".DS_Store"]`
- Assert absent: `.DS_Store`, `notes/.DS_Store`
- Assert present: `note.md`, `notes/`, `notes/keep.md`
- `skipped_ignored == 2` (optional here if W198 owns counts; prefer at least
  absence pins here)

Confirm RED.

**GREEN:** in the file arm, after reserved check and key build, if
`matches(&key)` then count++ and skip emit.

**Commit:** `feat: [32] skip ignored files during local walk (W197)`

---

### W198 - D-report counts (pruned dirs + skipped files)

**RED:** test `walk_counts_skipped_ignored`:

Plant a tree that exercises **both** rules in one walk:

```text
note.md                 # kept
.DS_Store               # file skip -> +1
.trash/a.md             # under pruned dir (not individually counted)
.trash/b/c.md           # same
nested/.DS_Store        # file skip -> +1
```

Patterns: `[".trash/", ".DS_Store"]`

Assert:

- `skipped_ignored == 3` exactly: pruned `.trash/` (1) + two `.DS_Store` files
  (2). Descendants under `.trash/` must **not** inflate the count.
- Entity keys: `note.md` present; no `.trash/` prefix keys; no `.DS_Store`
  keys.

Confirm RED (count 0 or wrong total before increments on both arms - if W196/
W197 already increment, this may go GREEN immediately; if so, treat as
characterization pin and still mutation-check by zeroing increments).

**Strict TDD note:** if W196/W197 already increment, write this test in the
same commit as the first increment that makes a partial count, **or** land
increments only in W198 and keep W196/W197 as absence-only. **Preferred
discipline for this issue:**

- W196 GREEN: prune + increment on dir match.
- W197 GREEN: skip + increment on file match.
- W198 RED/GREEN: the **exact combined count** pin (may start GREEN if prior
  increments are correct - then mutation-check is mandatory and the commit is
  `test: [32] lock skipped_ignored counting rules (W198)`).

**Commit:** `test: [32] lock skipped_ignored dir+file counting (W198)`
(or `feat:` if increments move here)

---

### W199 - reserved orthogonal to ignore

**RED:** test `walk_ignore_does_not_override_reserved` (issue) /
`walk_ignore_orthogonal_to_reserved` (sketch - use **issue acceptance name**):

```text
note.md
.note.md.vaultsync-tmp-123-4     # reserved crash leftover
.vaultsync-check-1-2-3           # reserved probe leftover
```

- Patterns: either empty, or a deliberate broad pattern that would match the
  leftover names if consulted (e.g. basename glob `*` or exact/basename
  patterns equal to the leftover final segments). Prefer a pattern that
  **would** match the reserved basenames so the precedence is real:
  `[".note.md.vaultsync-tmp-123-4", ".vaultsync-check-1-2-3"]` **or**
  a single basename pattern equal to one leftover plus the other planted.
- Assert:
  - `note.md` listed
  - leftovers **not** listed
  - `skipped_temp_files == 2` (or 1 if only one leftover planted - plant both
    and expect 2)
  - `skipped_ignored == 0` for those leftovers (they must not be counted only
    as ignored, and under reserved-first must not be double-counted)

Also useful: same tree with pattern `.DS_Store` and one real `.DS_Store` plus
one reserved leftover - expect `skipped_temp_files >= 1`, `skipped_ignored == 1`.

Confirm RED if implementation checks ignore before reserved or double-counts.

**GREEN:** keep reserved branch first; no ignore consult on that arm.

**Commit:** `fix: [32] reserved-namespace stays ahead of ignore (W199)`

---

### W200 - follow-symlink path applies the same ignore checks

**RED:** `#[cfg(unix)]` test `walk_follow_symlink_into_ignored_dir`:

Plant (keys must match patterns under D-match-subject):

```text
realdir/child.md
.git -> realdir          # symlink named `.git` (in-vault target)
keep.md
```

- `LocalFs::with_follow(dir, true).with_ignore(IgnoreSet::from_patterns(&[".git/".into()]).unwrap())`
- Assert:
  - `keep.md` present
  - `realdir/child.md` still present (real target walked under its own key
    unless separately ignored)
  - **no** `.git/` entity, **no** `.git/child.md`
  - `skipped_ignored >= 1`
  - must not hang (cycle/visited unchanged)

Second case in same test or `walk_follow_skips_ignored_file_symlink`:

```text
real.md
.DS_Store -> real.md     # file symlink
```

- follow=true, pattern `.DS_Store`
- `.DS_Store` absent from entities and from `followed_files`
- `skipped_ignored >= 1`
- `real.md` still listed

Confirm RED.

**GREEN:** apply ignore checks in `handle_followed_symlink` dir/file arms;
thread `ignore` into recursive walk. Do not match on canonical target path.

**Commit:** `feat: [32] apply ignore on followed-symlink walk arms (W200)`

---

### W201 - hygiene: rustdoc, module notes, roadmap decision log, plan status

No RED.

1. Touch up `LocalFs` / `with_ignore` / `WalkReport.skipped_ignored` rustdoc:
   cite issue #32, D-prune / D-report rules, reserved precedence, pointer that
   CLI printing is #34 and remote half is #33.
2. Brief note on basename vs dir-prefix in the `with_ignore` or walk rustdoc
   (PR 35 r2 heads-up closed here).
3. `doc/roadmap.md` decision-log row, dated, e.g.:

   > I32-local-walk | Issue #32: local walker applies `IgnoreSet` (D-prune dir
   > no-emit/no-recurse; file skip; `WalkReport.skipped_ignored` counts pruned
   > dirs + skipped files only; reserved-namespace stays first; empty
   > `IgnoreSet` via `LocalFs::with_ignore` default-compatible). Match subject
   > remains vault-relative walk keys (symlink target path not re-matched).
   > CLI count line / W25 retire stay #34; remote filter #33; depth cap #10
   > unblocked for sequencing. Plan: doc/plans/issue-32.md.

4. Flip this plan's **Status** to `implemented` when landing.
5. Full gate once more.

**Commit:** `docs: [32] local ignore walk rustdoc + roadmap decision log (W201)`

---

## Sequencing (commits on the branch)

```text
W195 seam + WalkReport.skipped_ignored + with_ignore (empty behavior)
  -> W196 dir prune (D-prune) + false-friend pins
  -> W197 file skip (basename / nested)
  -> W198 exact skipped_ignored counting lock (test-only if increments done)
  -> W199 reserved orthogonal
  -> W200 follow-symlink ignore arms (unix)
  -> W201 docs / decision log / plan status
```

Rationale:

- Seam first keeps every later RED about behavior, not API shape.
- Dir prune before file skip matches the issue's performance motivation
  (`.git/objects` must never be walked) and unblocks a meaningful count test.
- Reserved precedence after basic skip/prune so the "would have matched
  ignore" setup is realistic.
- Follow arms last: depends on both dir and file ignore logic existing.
- Each arrow = separate commit; full offline gate green.

---

## Acceptance mapping (issue checkboxes -> work items)

| Acceptance | Work item |
| ---------- | --------- |
| `walk_prunes_git_dir` - planted `.git/objects/aa/bb` absent; no need to enumerate objects | W196 |
| `walk_skips_ignored_file` - `.DS_Store` / `notes/.DS_Store` absent | W197 |
| `walk_counts_skipped_ignored` - dirs + files per D-report | W198 (increments in W196/W197) |
| `walk_empty_ignore_set_unchanged` - empty set = pre-ignore inventory | W195 |
| `walk_ignore_does_not_override_reserved` - temp leftovers = `skipped_temp_files` | W199 |
| Trailing-slash dir pattern no false positive on `not-trash.md` / `foo.trash` | W196 (false-friend pins) |
| Followed-symlink path applies same checks | W200 |
| fmt / clippy / offline tests green | every commit gate |

Issue test sketch names (use these literals where they match acceptance):

| Test name | Locks |
| --------- | ----- |
| `walk_prunes_git_dir` | D-prune |
| `walk_skips_ignored_file` (covers sketch `walk_skips_basename_ds_store`) | file skip |
| `walk_counts_skipped_ignored` | D-report |
| `walk_empty_ignore_set_unchanged` | back-compat |
| `walk_ignore_does_not_override_reserved` (sketch: `walk_ignore_orthogonal_to_reserved`) | reserved precedence |
| `walk_follow_symlink_into_ignored_dir` | follow + ignore |

---

## Explicit non-goals (refuse scope creep while implementing)

- Remote listing filter / delete invariant / `PlanReport` ignore warning (#33)
- `[ignore].profile` / built-in Obsidian list injection / `profile = "none"` (#31)
- Retire W25, CLI wiring of `IgnoreSet`, printing `skipped_ignored`, cli.md /
  sync-model user wording (#34)
- Planner `Skip(ignored)` rows
- Walker depth cap (#10) - only keep `walk()` shape friendly for a follow-up
- Matching ignore against absolute paths or canonical symlink targets
- `IgnoreSet::matches_dir` or any #30 API change
- New crate (`globset` / gitignore engines)
- Changing reserved-namespace, special-node, or symlink escape/cycle policy
  beyond threading ignore into existing arms

---

## Risk notes

| Risk | Mitigation |
| ---- | ---------- |
| Dir-prefix false friends (`.trash/` vs `not-trash.md` / `foo.trash`) | Rely on #30 matcher; W196 pins FS-level absence/presence |
| Basename `.git` vs dir-prefix `.git/` confusion in tests/docs | Explicit table in this plan + rustdoc in W201; default-profile patterns stay dir-prefix at #31 |
| Double-count reserved leftovers as ignored | Reserved arm first; W199 pins `skipped_ignored == 0` for those entries |
| Follow symlink `alias -> .git` still walks objects under `alias/` | Accepted under D-match-key; document; test uses symlink **named** `.git` so key matches `.git/` |
| Accidentally restructuring walk for #10 | D-walk-shape; review diff for unrelated control-flow churn |
| CLI/build_plan call sites break | `with_ignore` chain; `new`/`with_follow` defaults empty; no required call-site edits |
| `WalkOpts` lifetime churn / test compile breaks | Update the one manual `walk(...)` test in W195; keep opts construction local |
| Counting descendants under pruned dirs | Only increment on the matched directory key; W198 exact `== 3` pin |
| Performance regret (building keys for every entry before match) | Required for `IgnoreSet` subject = entity key; same as emit path today; prune still saves full subtree IO |

---

## Post-landing (not this plan's commits)

- Open PR against `main`; description: local walk half of D-both-sides only;
  empty ignore default; no user-visible CLI change; W25 still in force until
  #34; unblocks sequencing of #10.
- Close #32 after merge (or when acceptance is green on `main`).
- #10 may start after merge; #33 remains parallel; #34 waits on #31+#32+#33.

---

## Implementation checklist (copy into PR body)

- [ ] W195 `LocalFs::with_ignore` + `WalkReport.skipped_ignored` + empty-set pin
- [ ] W196 `walk_prunes_git_dir` + dir false-friend pins
- [ ] W197 `walk_skips_ignored_file`
- [ ] W198 `walk_counts_skipped_ignored` exact D-report counts
- [ ] W199 `walk_ignore_does_not_override_reserved`
- [ ] W200 `walk_follow_symlink_into_ignored_dir` (+ file symlink case)
- [ ] W201 rustdoc + roadmap I32 row + plan status `implemented`
- [ ] No edits to `cli.rs` / `config.rs` / `plan/**` / remote `build_plan` filter
- [ ] No new Cargo dependencies
- [ ] `cargo fmt` / `clippy -D warnings` / offline lib+bin tests green
