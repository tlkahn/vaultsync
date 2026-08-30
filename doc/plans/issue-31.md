# Issue 31 plan: [ignore] profile + config resolution (Obsidian default)

**Status:** implemented (branch `worktree-ignore-profile-config-resolution-obsidian-default`;
W186-W195 landed, all commits bisectable-green)
**Issue:** https://github.com/tlkahn/vaultsync/issues/31 (OPEN; P0 of epic #9)
**Branch:** `worktree-ignore-profile-config-resolution-obsidian-default` (this worktree; cut from
`main` tip `b1af61c` = Issue 30 / PR 35 merged)
**Design refs:** issue #31 body (locked D3-profile / D-config-surface /
sequencing note), epic #9, issue #30 (IgnoreSet matcher, landed), issue #34
(CLI wire-up / W25-retire, blocked by this), [sync-model.md](../sync-model.md)
(D3 default ignore list), [cli.md](../cli.md) (`[ignore]` still Phase 3 / W25),
[roadmap.md](../roadmap.md) (D3), [doc/plans/issue-30.md](issue-30.md)
**Verified baseline (recorded at plan time):** tip `b1af61c`. Gate on this
worktree: `cargo test --offline --lib --bins` = 454 passed / 0 failed / 1
ignored; `cargo clippy --all-targets -- -D warnings` clean expected;
`cargo fmt --check` clean expected.
**Blocker check:** depends on #30 (landed on this tip as `IgnoreSet` in
`src/ignore.rs`). Does **not** by itself unblock walk prune (#32), remote
filter (#33), or W25 retirement (#34). Soft-combine with #34 is an explicit
non-choice for this worktree (see D-w25-seq below).

**Implementation discipline:** **strict fine-grained TDD** for every behavior
change. For each work item: write the failing pin(s) first, run them, observe
RED (record the failing assertion), implement the minimum to reach GREEN, then
refactor under green. Characterization pins for *existing* behavior (W25 keep
green, Settings constructor sites) are green-on-arrival with a mutation check
(temporarily break, observe RED, revert) so they stay load-bearing. Doc /
roadmap / rustdoc-only commits are not behavior changes and land under the
all-green gate only. Every pushed commit stays bisectable and green; never
push a RED state.

---

## Problem recap (from the issue, verified against the tree)

Epic #9 needs config/profile resolution before CLI wire-up (#34) can activate
defaults end-to-end. Matcher (#30) is pure and already landed; walk/remote
application (#32/#33) consume a resolved list they do not interpret.

Today:

| Piece | State | File |
| ----- | ----- | ---- |
| `[ignore].patterns` | parses into `Settings.ignore_patterns: Vec<String>` (user list only; no profile) | `src/config.rs` |
| `[ignore].profile` | **does not exist** | `src/config.rs` `IgnoreConfig` |
| Built-in Obsidian list | hard-coded only inside `ignore_set_default_profile_fixture` test | `src/ignore.rs` tests |
| Resolution | `resolve_settings` clones `cfg.ignore.patterns` or `vec![]` | `src/config.rs` |
| Validation | none at config layer (matcher validates only when `IgnoreSet::from_patterns` is called) | - |
| W25/M3 gate | non-empty `settings.ignore_patterns` -> exit 1 on push/pull/check; status warns | `src/cli.rs` |
| Matcher | `IgnoreSet::from_patterns(&[String]) -> Result<IgnoreSet, Error>` | `src/ignore.rs` |

Critical hazard (issue sequencing note): if this issue injects the six
built-ins into the same field W25 reads, every push/pull/check refuses with
no user config. That must not land alone.

---

## Locked decisions (owned by #31; do not reopen in implementation)

| ID | Lock | Choice |
| -- | ---- | ------ |
| D3-profile | Default profile | Built-in Obsidian set is the default when `[ignore]` is absent **or** `profile` is absent. Name: `obsidian`. Escape hatch: `profile = "none"` disables built-ins. User `patterns` **extend** the active profile (union), they do not replace it. Replacement = `profile = "none"` + explicit full list. |
| D-config-surface | TOML shape | `IgnoreConfig` gains `profile: Option<String>` (`deny_unknown_fields` stays). No CLI `--profile` / `--exclude` / `--include` (epic non-goal). |
| D-w25-seq | Sequencing vs W25 | **Option (2)** from the issue: introduce the resolved list on a **new field** `Settings.resolved_ignore_patterns` while leaving `Settings.ignore_patterns` as **user-only, still W25-gated**. Do **not** soft-combine with #34 on this branch (keeps the PR reviewable and scoped to config). #34 renames/collapses at wire-up (clear note in rustdoc + roadmap row). Rejected here: (1) same-PR-as-#34; (3) feature-flag code path. |
| D-field-split | Field roles until #34 | `ignore_patterns` = raw user `[ignore].patterns` only (empty when section/key absent). W25 continues to key off this field unchanged. `resolved_ignore_patterns` = profile built-ins (+) user patterns, deduped, validated. Rest of program after #34 should read only the resolved field; until then only tests + future callers touch it. |
| D-resolve-order | Algorithm | In `resolve_settings` (via a private `resolve_ignore` helper): (1) resolve profile -> built-in list (`obsidian` set or empty); (2) append user `patterns` (stable order: built-ins first, user next; **exact string** dedup, first wins); (3) validate the full resolved list via `IgnoreSet::from_patterns` (loud `Error::Other` naming the bad pattern - reuse matcher messages); (4) store user list + resolved list on `Settings`. |
| D-profile-values | Allowed set | Exact codepoint match, case-sensitive: `"obsidian"` \| `"none"`. `None` / key absent -> `obsidian`. Unknown value (incl. `""`, `"Obsidian"`, `" obsidian"`) -> loud error naming the value **and** the allowed set. No trim. |
| D-constant | Single source of truth | `pub const OBSIDIAN_DEFAULT_IGNORE_PATTERNS: &[&str]` in `src/config.rs` with the six exact vault-relative strings from the issue (order locked below). Docs and tests cite this constant; do not re-list ad hoc in production code. |
| D-validate-seam | How to validate | Call `crate::IgnoreSet::from_patterns(&resolved)` and discard the `Ok` value (or `let _ = ...`). Do **not** reimplement pattern rules in config. Do **not** add a new `Error` variant. Prefer keeping the matcher message intact (already `invalid ignore pattern {pat:?}: {reason}`); do not invent a parallel vocabulary. Optional thin prefix only if a pin needs the config key named - default is no prefix. |
| D-dedup | Exact string | Dedup by `resolved.contains(p)` (exact `String` equality). Built-in first means a user entry that repeats a built-in is dropped. Order of first occurrence is preserved. Not path-semantic dedup (`.git` vs `.git/` stay distinct if both present). |
| D-scope | Non-goals | No walk prune (#32). No remote filter (#33). No W25 retire / CLI apply / docs rewrite of live behavior (#34). No `Cargo.toml` dep changes. No CLI flags. No change to matcher semantics. |

### Built-in list (normative; constant order)

```text
.git/
.trash/
.DS_Store
.obsidian/workspace
.obsidian/workspace.json
.obsidian/workspace-mobile.json
```

Rationale (D3, reaffirmed): sync `.obsidian/` **except** workspace session
files - "settings yes, ephemeral no". Exact three names Obsidian ships today
(not a single `.obsidian/workspace*` glob). `.git/`, `.trash/`, `.DS_Store`
are vault conventions; `node_modules/` etc. stay user-added only.

### Resolution truth table (pins must lock these)

| Input | `ignore_patterns` (user) | `resolved_ignore_patterns` |
| ----- | ------------------------ | -------------------------- |
| no `[ignore]` section | `[]` | the six built-ins |
| `[ignore]` present, no `profile`, no `patterns` | `[]` | the six built-ins |
| `profile` absent, `patterns = ["private/"]` | `["private/"]` | six built-ins + `private/` |
| `profile = "obsidian"`, no patterns | `[]` | the six built-ins |
| `profile = "none"`, no patterns | `[]` | `[]` |
| `profile = "none"`, `patterns = ["private/"]` | `["private/"]` | `["private/"]` |
| `profile = "obsidian"`, user repeats `.git/` | `[".git/"]` | six built-ins once (no dup) |
| `profile = "weird"` | (error, no Settings) | - |
| any resolved entry invalid under #30 rules | (error naming pattern) | - |

W25 interaction under D-w25-seq: absent `[ignore]` does **not** trip W25
(user field empty) even though resolved is non-empty. User-supplied non-empty
`patterns` still trip W25 exactly as today.

---

## Design

### Types / API surface

```rust
// src/config.rs

/// Built-in Obsidian default ignore profile (issue #31 / roadmap D3).
/// Vault-relative; exact strings; single source of truth for docs + resolve.
pub const OBSIDIAN_DEFAULT_IGNORE_PATTERNS: &[&str] = &[
    ".git/",
    ".trash/",
    ".DS_Store",
    ".obsidian/workspace",
    ".obsidian/workspace.json",
    ".obsidian/workspace-mobile.json",
];

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IgnoreConfig {
    /// Optional profile name. Absent key => `"obsidian"` at resolve time.
    /// Allowed: `"obsidian"` | `"none"`.
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub patterns: Vec<String>,
}

pub struct Settings {
    // ... existing fields ...
    /// Raw user `[ignore].patterns` only (no profile injection).
    /// W25/M3 still gates on this field until #34.
    pub ignore_patterns: Vec<String>,
    /// Fully resolved ignore list: active profile built-ins, then user
    /// patterns, exact-string deduped, validated via `IgnoreSet`.
    /// Unused by CLI until #34 wire-up; present so defaults can land without
    /// tripping W25 (see issue #31 sequencing note option 2).
    pub resolved_ignore_patterns: Vec<String>,
}
```

Private helper (name flexible; keep next to `resolve_retry`):

```rust
fn resolve_ignore(
    ignore: Option<&IgnoreConfig>,
) -> Result<(Vec<String> /* user */, Vec<String> /* resolved */), Error>
```

### Error message shapes (stable enough to pin)

| Failure | Message must contain |
| ------- | -------------------- |
| unknown profile | the raw value (Debug/`{value:?}` preferred) **and** both allowed names `obsidian` and `none` (and ideally the key `ignore.profile`) |
| bad pattern | the bad pattern string (as `IgnoreSet` already does via `{pat:?}`) and a reason token (`empty`, `leading`, `**`, `!`, etc.) |

Do not soft-default unknown profiles. Do not clamp.

### Call sites that construct `Settings` literally (must compile after field add)

| Site | File | Update |
| ---- | ---- | ------ |
| `resolve_settings` Ok arm | `src/config.rs` | real resolution |
| offline / no-store helpers | `src/cli.rs` (~874, ~1699) | `resolved_ignore_patterns: Vec::new()` (or match user field) |
| W25 test helper `settings_with_ignore` | `src/cli.rs` (~1718) | set **user** `ignore_patterns` only; leave resolved empty **or** mirror user list - either is fine for W25 tests because the gate reads user field only. Prefer `resolved_ignore_patterns: Vec::new()` to make the split obvious in tests. |

No production CLI read of `resolved_ignore_patterns` in this issue.

### Files touched (expected)

| File | Change |
| ---- | ------ |
| `src/config.rs` | constant; `IgnoreConfig.profile`; `Settings.resolved_ignore_patterns`; `resolve_ignore`; `resolve_settings` wiring; unit tests listed below |
| `src/cli.rs` | mechanical `Settings { ... }` field init only (no W25 logic change) |
| `src/ignore.rs` | optional: point `ignore_set_default_profile_fixture` at `OBSIDIAN_DEFAULT_IGNORE_PATTERNS` so one source of truth (keep-green refactor under green; not required for acceptance) |
| `doc/roadmap.md` | decision-log row only (I31-profile); no cli.md live-behavior rewrite (#34) |
| `doc/plans/issue-31.md` | this plan; flip Status to implemented on land |

**Do not touch:** `src/local.rs`, plan/build_plan paths, matcher semantics, `Cargo.toml` deps, W25 branch logic, user-facing cli.md ignore section (beyond zero or a one-line "resolution lands in #31, apply in #34" only if already editing - prefer roadmap only).

---

## TDD work items (RED -> GREEN -> refactor)

Numbering continues the epic series after #30's W185.

Each behavior commit: RED pins in the same commit as GREEN impl is OK **only
if** RED was observed locally before the impl was written and the commit
message records that; preferred workflow is edit test -> run -> see RED ->
impl -> GREEN -> commit once green. Never push RED.

Gate on every commit:

```text
cargo fmt --check
cargo clippy --all-targets -- --deny warnings
cargo test --offline --lib --bins
```

---

### W186 - scaffold: `resolved_ignore_patterns` field (compile + characterization)

**Goal:** make room for the split without changing observable resolve behavior
yet. Not a user-behavior change; keeps the tree compiling.

**Steps:**

1. Add `pub resolved_ignore_patterns: Vec<String>` to `Settings` with rustdoc
   stating D-w25-seq / "unused until #34".
2. In `resolve_settings`, temporarily set
   `resolved_ignore_patterns: ignore_patterns.clone()` (current user list only;
   still empty when absent). This is the pre-injection baseline.
3. Update every literal `Settings { ... }` in `src/cli.rs` (and any other
   breakage) to include the new field (`Vec::new()` or clone of user list).
4. Gate green. No new behavioral pins required yet; existing tests must stay
   green (including W25 trio).

**Commit:** `refactor: [31] add Settings.resolved_ignore_patterns field (W186)`

---

### W187 - RED/GREEN: `config_parse_ignore_profile_field` (serde + deny_unknown)

**RED:** add test `config_parse_ignore_profile_field` in `src/config.rs` tests:

1. TOML with

   ```toml
   [ignore]
   profile = "obsidian"
   patterns = [".git/"]
   ```

   parses; `cfg.ignore.as_ref().unwrap().profile.as_deref() == Some("obsidian")`;
   patterns len 1.

2. TOML with `[ignore]` and only `patterns = []` (no profile key):
   `profile` is `None`.

3. Unknown key under `[ignore]` (e.g. `profil = "obsidian"` or `foo = 1`)
   fails parse and the error names the unknown key (`deny_unknown_fields`).

Expected RED on (1) until `IgnoreConfig` gains `profile` (serde will reject
unknown field `profile` today).

**GREEN:** 

```rust
#[serde(default)]
pub profile: Option<String>,
```

on `IgnoreConfig`. Update the section rustdoc (no longer "patterns only /
unused"; say profile+patterns, resolution in `resolve_settings`, apply in
#34). No resolve behavior change yet.

**Commit:** `feat: [31] IgnoreConfig.profile field (W187)`

---

### W188 - RED/GREEN: `resolve_default_profile_is_obsidian`

**RED:** test `resolve_default_profile_is_obsidian`:

- `parse_config_str("")` / empty `FileConfig` -> `resolve_settings`.
- Assert `s.ignore_patterns.is_empty()` (user-only; W25 safe).
- Assert `s.resolved_ignore_patterns` equals the six built-ins **in constant
  order** (compare to `OBSIDIAN_DEFAULT_IGNORE_PATTERNS` mapped to `String`,
  or to the constant directly).

Also pin: empty `[ignore]` section (section present, no keys) yields the same
resolved six and empty user list.

Expected RED: resolved is empty today (W186 clone of user list).

**GREEN:** introduce `OBSIDIAN_DEFAULT_IGNORE_PATTERNS` and `resolve_ignore`:

- absent section / absent profile -> built-in list into `resolved_*`;
- user list still empty.

Do **not** yet implement user extend / none / errors if tests for those are
not in this commit - minimum to green this pin. (Implementing the full helper
in one go is fine if W189+ pins are written in the same sitting; still commit
as small as practical.)

**Commit:** `feat: [31] default ignore profile resolves to Obsidian built-ins (W188)`

---

### W189 - RED/GREEN: `resolve_profile_none_plus_user`

**RED:** test `resolve_profile_none_plus_user`:

```toml
[ignore]
profile = "none"
patterns = ["private/"]
```

- `ignore_patterns == ["private/"]`
- `resolved_ignore_patterns == ["private/"]` (no built-ins)

Also pin: `profile = "none"` with no patterns -> both empty.

**GREEN:** branch in `resolve_ignore` for `"none"` -> empty builtin slice;
append user patterns.

**Commit:** `feat: [31] profile=none escape hatch (W189)`

---

### W190 - RED/GREEN: `resolve_user_extends_obsidian`

**RED:** test `resolve_user_extends_obsidian`:

```toml
[ignore]
patterns = ["private/"]
```

(no profile key -> default obsidian)

- user field `["private/"]`
- resolved = six built-ins followed by `private/`

Also pin explicit `profile = "obsidian"` + `patterns = ["private/"]` same
resolved list.

**GREEN:** append user patterns after built-ins.

**Commit:** `feat: [31] user patterns extend active profile (W190)`

---

### W191 - RED/GREEN: dedup exact string duplicates

**RED:** test `resolve_ignore_dedup_exact_string` (name flexible; acceptance
says "Dedup"):

```toml
[ignore]
profile = "obsidian"
patterns = [".git/", "private/", ".git/"]
```

- resolved contains `.git/` **once**, then other built-ins, and `private/`
  once;
- order: full built-in sequence first (with `.git/` only at its built-in
  index), then `private/` (user dup of `.git/` dropped; second user `.git/`
  dropped).

Pin: user-only dup under `profile = "none"`:
`patterns = ["a/", "b/", "a/"]` -> `["a/", "b/"]`.

**GREEN:** skip push when `resolved.contains(p)`.

**Commit:** `feat: [31] exact-string dedup on resolved ignore list (W191)`

---

### W192 - RED/GREEN: `resolve_unknown_profile_errors`

**RED:** test `resolve_unknown_profile_errors` table-driven:

| profile value | notes |
| ------------- | ----- |
| `"git"` | unknown name |
| `""` | empty string |
| `"Obsidian"` | case-sensitive |
| `"none "` | trailing space; no trim |
| `"obsidian\n"` | if representable in TOML basic string - optional |

For each: `resolve_settings` is `Err`; `format!("{err}")` contains the raw
value (or a clear substring) **and** mentions `obsidian` and `none`
(allowed set). Prefer also containing `ignore.profile`.

**GREEN:** match arm on unknown -> `Error::Other(...)`.

**Commit:** `feat: [31] unknown ignore.profile is a loud error (W192)`

---

### W193 - RED/GREEN: `resolve_bad_pattern_errors`

**RED:** test `resolve_bad_pattern_errors` table-driven via resolve (not only
raw `IgnoreSet`):

| patterns (with `profile = "none"` to isolate) | must name pattern / reason |
| --------------------------------------------- | -------------------------- |
| `[""]` | empty |
| `["/abs"]` | leading `/` |
| `["a/**/b"]` | `**` |
| `["!foo"]` | `!` |
| (optional) `["foo?"]`, `["a//b"]` | `?` / empty segment |

Also pin: under default profile, a bad **user** pattern still fails even
when built-ins are present (`patterns = ["private/", ""]` or leading `/`).

For each: err Display contains the bad pattern text.

**GREEN:** after building `resolved`, `IgnoreSet::from_patterns(&resolved)?;`
propagate err. Built-ins alone must pass (constant is valid).

**Commit:** `feat: [31] validate resolved ignore patterns via IgnoreSet (W193)`

---

### W194 - keep-green: W25 still keys off user field only

**Characterization (green on arrival once W188+ land):**

1. Existing CLI tests remain green without edits to their assertions:
   - `push_with_ignore_patterns_errors_loudly`
   - `pull_with_ignore_patterns_errors_loudly`
   - `status_with_ignore_patterns_warns_but_runs`
2. New pin `resolve_absent_ignore_does_not_populate_user_field` (may already
   be implied by W188): empty config -> `ignore_patterns.is_empty()` AND
   `!resolved_ignore_patterns.is_empty()`. Documents the sequencing invariant
   in config tests so a future mistaken merge of fields fails loudly.

**Mutation check:** temporarily point W25 at `resolved_ignore_patterns`
instead of `ignore_patterns`, run
`cargo test --offline --lib resolve_absent_ignore` / a small CLI harness if
needed - the new invariant pin should still pass at config layer; document in
commit message that CLI W25 is intentionally unchanged. Revert any scratch
mutation.

No production CLI change.

**Commit:** `test: [31] lock W25 user-field vs resolved-field split (W194)`

---

### W195 - hygiene: rustdoc, constant reuse, roadmap, plan status

No RED required.

1. Rustdoc on `OBSIDIAN_DEFAULT_IGNORE_PATTERNS`, `IgnoreConfig`, `Settings`
   fields, and `resolve_settings` ignore paragraph: cite issue #31, D3,
   D-w25-seq ("#34 reads `resolved_ignore_patterns` and retires W25").
2. Optional DRY: `ignore_set_default_profile_fixture` builds its list from
   `OBSIDIAN_DEFAULT_IGNORE_PATTERNS` (import via `crate::config::...`).
   Keep-green; mutation-check by swapping one constant entry temporarily only
   if useful.
3. `doc/roadmap.md` decision-log row, dated, e.g.:

   > I31-profile | Issue #31: `[ignore].profile` (`obsidian` default / `none`
   > escape); user patterns extend profile; resolved list on
   > `Settings.resolved_ignore_patterns` validated via `IgnoreSet`;
   > `ignore_patterns` stays user-only for W25 until #34. Constant
   > `OBSIDIAN_DEFAULT_IGNORE_PATTERNS` is the D3 source of truth. Plan:
   > doc/plans/issue-31.md.

4. Flip this plan's **Status** line to `implemented` when landing.
5. Full gate once more.

**Commit:** `docs: [31] ignore profile rustdoc + roadmap decision log (W195)`

---

## Sequencing (commits on the branch)

```text
W186 field scaffold
  -> W187 serde profile field
  -> W188 default obsidian resolve   (constant + helper starts here)
  -> W189 profile=none
  -> W190 user extends
  -> W191 dedup
  -> W192 unknown profile errors
  -> W193 IgnoreSet validation
  -> W194 W25 split pins
  -> W195 docs / decision log
```

Rationale:

- W186 first so the tree compiles with the new field before behavior pins.
- W187 before resolve pins that set `profile = ...` in TOML (otherwise parse
  fails for the wrong reason and RED is ambiguous).
- W188 before W189/W190 so the default path exists; none/extend are deltas.
- W191 after append exists (dedup needs both sources).
- W192/W193 can swap order if desired; both are independent error paths.
  Prefer unknown-profile before pattern validation only for readability.
- W194 after resolution is real (otherwise the absent-ignore invariant is
  vacuous).
- W195 last.

Each arrow is a separate commit with the full offline gate green.

Alternative acceptable squash: W189+W190+W191 as one "resolve union"
commit **if and only if** all three RED pins were observed before the shared
impl - still prefer split when diffs stay small.

---

## Acceptance mapping (issue checkboxes -> work items)

| Acceptance | Work item |
| ---------- | --------- |
| `resolve_default_profile_is_obsidian` | W188 |
| `resolve_profile_none_plus_user` | W189 |
| `resolve_user_extends_obsidian` | W190 |
| `resolve_unknown_profile_errors` | W192 |
| `resolve_bad_pattern_errors` | W193 |
| Dedup: user repeating a built-in does not duplicate | W191 |
| W25 interaction handled per sequencing note | D-w25-seq + W186 + W194 |
| `config_parse_ignore_profile_field` | W187 |
| fmt/clippy/offline tests green | every commit gate |

Use the issue's test names literally so the issue checkboxes can be ticked
against `rg fn resolve_ default_profile` etc.

---

## Explicit non-goals (refuse scope creep while implementing)

- Retire W25 / rewrite CLI apply path / live cli.md ignore section (#34)
- Soft-combine this branch with #34
- Walker prune, `WalkReport.skipped_ignored` (#32)
- Remote listing filter / delete invariant (#33)
- Writing `resolved_ignore_patterns` into the field W25 reads
- CLI `--exclude` / `--include` / `--profile`
- New crate dependency
- New `Error` variant
- Changing `IgnoreSet` match semantics or pattern language
- NFC / case-fold profile names
- Built-in expansion of `node_modules/`, `.venv/`, or `.obsidian/workspace*`
- Planner `Skip(ignored)` rows

---

## Risk notes

| Risk | Mitigation |
| ---- | ---------- |
| Default injection trips W25 on every push | D-w25-seq option 2; W188+W194 pins assert user field empty when absent config |
| Accidentally merge fields at wire-up prep | rustdoc on both fields; roadmap row; W194 invariant test |
| Dedup changes user intent for "repeat means louder" | exact-string first-wins locked; no semantic merge |
| Validation double-reports / message drift | single seam `IgnoreSet::from_patterns`; pin substrings not full strings |
| Literal `Settings` sites forgotten | W186 compile gate; `rg 'Settings \{'` before push |
| Constant drift vs ignore fixture | W195 optional DRY to `OBSIDIAN_DEFAULT_IGNORE_PATTERNS` |
| Scope creep into #34 | file touch list; review diff for `cli.rs` logic zero beyond struct init |

---

## Post-landing (not this plan's commits)

- Open PR against `main`; PR body states: config resolution only; W25 still
  in force; absent `[ignore]` does not refuse push; `resolved_ignore_patterns`
  unused by CLI until #34.
- Close #31 after merge (or when acceptance is green on `main`).
- Unblock #34 (and any soft-dep from #32/#33 that wants the resolved list
  from Settings rather than a side channel).
- #34 rename note: collapse to a single field or rename
  `resolved_ignore_patterns` -> `ignore_patterns` once W25 is gone - record
  in #34 plan, not here.

---

## Implementation checklist (copy into PR body)

- [ ] W186 `resolved_ignore_patterns` field scaffold
- [ ] W187 `IgnoreConfig.profile` serde + `config_parse_ignore_profile_field`
- [ ] W188 `resolve_default_profile_is_obsidian` + constant
- [ ] W189 `resolve_profile_none_plus_user`
- [ ] W190 `resolve_user_extends_obsidian`
- [ ] W191 exact-string dedup
- [ ] W192 `resolve_unknown_profile_errors`
- [ ] W193 `resolve_bad_pattern_errors` via `IgnoreSet`
- [ ] W194 W25 user-vs-resolved split pins (CLI W25 untouched)
- [ ] W195 rustdoc + roadmap I31-profile row + plan status
- [ ] No W25 logic change; no walk/remote/apply wiring
- [ ] `Cargo.toml` deps unchanged
- [ ] Full gate green on tip
