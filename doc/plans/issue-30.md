# Issue 30 plan: IgnoreSet matcher (pure pattern language)

**Status:** implemented (W178-W185 landed on this worktree)
**Issue:** https://github.com/tlkahn/vaultsync/issues/30 (OPEN; P0 of epic #9)
**Branch:** `worktree-ignoreset-matcher-pure-pattern-language` (this worktree; cut from
`main` tip `25b88a5`)
**Design refs:** issue #30 body (locked D-match-key / D-match-semantics /
D-library-seam), epic #9, [sync-model.md](../sync-model.md) (default ignore
list), [cli.md](../cli.md) (`[ignore]` still Phase 3 / W25),
[roadmap.md](../roadmap.md) (D3), workspace Rust dependency policy in
project AGENTS / issue body
**Verified baseline (recorded at plan time):** tip `25b88a5` (Issue 27 / PR 28
merged). Gate on this worktree:
`cargo test --offline --lib --bins` = 445 passed / 0 failed / 1 ignored;
`cargo clippy --all-targets -- -D warnings` clean;
`cargo fmt --check` clean.
**Blocker check:** none. No upstream deps. Does **not** by itself unblock #10
(walk untouched) or retire W25 (CLI untouched). Blocks #31 #32 #33 #34.

---

## Problem recap (from the issue, verified against the tree)

Epic #9 needs a shared ignore matcher before profile resolution (#31), local
walk prune (#32), remote filter (#33), or W25 retirement (#34) can land.

Today:

| Piece | State | File |
| ----- | ----- | ---- |
| `[ignore].patterns` | parses into `Settings.ignore_patterns: Vec<String>` | `src/config.rs` |
| W25/M3 gate | non-empty list -> exit 1 on push/pull/check; status warns | `src/cli.rs` |
| Matcher type | **does not exist** | - |
| Local walk filters | reserved names, specials, symlinks only | `src/local.rs` |
| Remote filters | reserved-namespace partition only | `src/lib.rs` |
| Ignore crates | none direct | `Cargo.toml` |

Precedent for dual-sided pure filters: `is_reserved_vaultsync_key_name` +
`partition_reserved_remote_keys` in `src/lib.rs`. User ignores must eventually
mirror that shape, but **this issue ships only the pure matcher**.

Identity model already locked elsewhere and reused here:

- Vault-relative keys from `path_to_key` / `Entity.key` (`src/local.rs`,
  `src/entity.rs`): `/`-separated, no leading `/`, folders carry trailing `/`,
  case-sensitive, codepoint-exact (no NFC fold).
- `ensure_valid_key` rejects empty, leading `/`, `\`, controls, `.` / `..` /
  empty segments. Matcher assumes callers pass planner-shaped keys; it does
  not re-run full key validation on every `matches` call.

---

## Locked decisions (owned by #30; do not reopen in implementation)

| ID | Lock | Choice |
| -- | ---- | ------ |
| D-match-key | Match subject | Vault-relative `Entity.key` only. Never absolute host paths or store-prefix-qualified keys. Case/codepoint exact. Reserved-namespace skipping stays independent and always-on (not part of `IgnoreSet`). |
| D-match-semantics | Engine | Hand-rolled, small explicit matcher. **No new crate** (`globset` / gitignore engines) unless implementation discovers hand-rolled is genuinely more code+risk - then **stop and confirm** before adding. |
| D-library-seam | API | Pure `IgnoreSet::from_patterns` + `IgnoreSet::matches`. Construct once; rest of program never re-parses. `plan()` stays filter-agnostic (callers filter entity lists before `plan()` in later issues). |
| D-module | Placement | Top-level `src/ignore.rs` + `pub mod ignore` in `src/lib.rs`. Default over a `local` submodule so `lib` / `cli` / `local` can share without layering weirdness. |
| D-error | Validation errors | Reuse `crate::error::Error::Other(String)` with a stable message that **names the offending pattern** (Debug/`{pat:?}`) and the reason. Do **not** add an `Error` variant in this issue (keeps the public error surface stable; #31 can re-wrap into config-shaped messages). Do **not** overload `InvalidKey`. |
| D-api-min | Convenience | Ship `matches` only. No `matches_dir` in v1 of this type - folder keys already carry trailing `/`, so dir prune (#32) can call `matches(dir_key)`. Add later only if a caller proves need. |
| D-scope | Non-goals | No profile / built-in lists (#31). No walk prune (#32). No remote filter (#33). No CLI / W25 retire / docs polish beyond a one-line roadmap decision-log row (#34 owns user-facing docs). No `config.rs` resolution changes. |

### Pattern language (normative table)

| Pattern shape | Compiles to | Matches | Non-matches (pins) |
| ------------- | ----------- | ------- | ------------------ |
| `name` (no `/`, no metacharacters) | basename-anywhere | final segment equals `name` (after stripping one trailing `/` on the key, same final-segment idea as reserved-key handling) | `DS_Store.bak`; `name2`; differently cased `Name` |
| `path/to/file` (has `/`, no trailing `/`, no metacharacters) | exact key equality | only that exact key string | any other path; folder form `path/to/file/` |
| `path/to/dir/` (trailing `/`, no metacharacters) | dir key + string prefix | `path/to/dir/` itself **and** every key with that literal prefix | `not-path...`; `foo.dir`; sibling prefix false friends like `.trash/` vs `foo.trash` / `not-trash.md` / `.trashfile` |
| segment with `*` | per-segment glob (only metacharacter in v1) | `*` = any run of non-`/` chars, **including empty**; does not cross `/` | other directories; non-matching sibling names |
| empty string | reject | - | - |
| leading `/` | reject | - | - |
| `**` | reject | - | - |
| `!` anywhere | reject | - | - |
| char class `[` or `]` | reject | - | - |
| escape `\` | reject | - | - |
| other glob metachar `?` | reject | - | - (v1 is not "literal `?`"; unknown metacharacters are loud) |

Rationale (from issue, kept): basename-anywhere lets `.DS_Store` hit nested junk
without `**`; no gitignore crate keeps MSRV/review surface small and avoids
anchored/`**`/`!` subtleties vs `--delete`.

### Match composition

- Multiple patterns: **OR** (any compiled pattern match => `matches` true).
- Zero patterns: `from_patterns(&[])` succeeds; `matches` always false.
- Patterns are **not** trimmed; whitespace is significant (codepoint-exact).
- Duplicate patterns are allowed (redundant OR).
- First-match short-circuit is an impl detail; observable behavior is OR.

### Star / segment rules (fine print locked here so tests can pin them)

1. Split the pattern on `/`. A single trailing empty segment (the pattern ended
   with `/`) sets `dir_prefix = true` and is not a match segment. Any **other**
   empty segment (`a//b`, or a pattern that is only `/` after the leading-`/`
   reject) is a loud reject ("empty path segment").
2. A segment is a **glob segment** iff it contains at least one `*`. Otherwise
   it is exact (byte/codepoint equality).
3. Slash-free pattern + no `*` => basename-anywhere exact.
4. Slash-free pattern + `*` => basename-anywhere glob on the final key segment.
5. Slash-bearing, non-dir, no `*` => exact full-key equality (cheaper than
   segment zip; equivalent for well-formed keys).
6. Slash-bearing, non-dir, has `*` => segment-count equality + per-segment
   match (exact or glob).
7. Dir form (`dir_prefix`): key matches if
   - key segment list is **at least** as long as the pattern's segment list,
     and
   - each pattern segment matches the corresponding key segment in order
   - for the no-`*` common case this **must** be observationally identical to
     `key == pat || key.starts_with(pat)` with `pat` carrying its trailing `/`
     (pins false friends: `.trash/` must not match `.trashfile`).
8. Within one glob segment, `*` matching is the usual non-crossing glob:
   split the segment pattern on `*`, require the literal parts to appear in
   order; leading/trailing `*` allow free prefix/suffix; interior `*` allow
   free gaps; consecutive `**` inside one segment is still two empty literals
   around stars (allowed) - the **pattern-level** `**` reject fires only when
   the raw pattern string contains the two-char substring `**` (issue table),
   before segment compile. Practical effect: `**` never reaches the glob
   engine; a user wanting "two stars" cannot spell it without hitting the
   reject. That matches "not in v1".
9. Final key segment for basename rules: if `key.ends_with('/')`, strip
   **one** trailing `/` then take the substring after the last `/` (or the
   whole remainder if none). So folder key `.DS_Store/` has final segment
   `.DS_Store`. File key `notes/.DS_Store` has final segment `.DS_Store`.

### Public API (normative)

```rust
// src/ignore.rs
pub struct IgnoreSet { /* private compiled patterns */ }

impl IgnoreSet {
    /// Compile `patterns` into a reusable matcher.
    /// Empty input is valid (matches nothing).
    /// On invalid pattern: `Err(Error::Other(...))` naming the pattern.
    pub fn from_patterns(patterns: &[String]) -> Result<Self, crate::error::Error>;

    /// True when `key` (vault-relative entity key) is ignored by any pattern.
    pub fn matches(&self, key: &str) -> bool;
}
```

- Pure: no IO, no FS, no `Settings`, no `&Path`.
- `IgnoreSet` is `Debug`, `Clone`; equality is optional (not required).
- `pub use` from `lib.rs` only if a caller outside the module needs it in this
  issue - default: `pub mod ignore` is enough; re-export `IgnoreSet` at crate
  root if tests/examples benefit (`pub use ignore::IgnoreSet` is fine and
  cheap). Prefer `pub use ignore::IgnoreSet` so later issues can write
  `vaultsync::IgnoreSet` without churn.

### Complexity / dep tripwire

If the compiled matcher + tests grow into something that is clearly larger
and riskier than depending on a small glob crate, **stop**. Do not silently
add `globset` / `glob`. Open the question with the measured LOC and the
failing edge cases. Default remains hand-rolled.

---

## Method: strict fine-grained TDD

Same rules of engagement as Phase 1/2 and recent issue plans
([phase-1.md](phase-1.md), [issue-15.md](issue-15.md), [issue-17.md](issue-17.md)):

1. **RED** - named failing test first; confirm it fails for the right reason
   (compile failure for a missing type/fn is an accepted RED form in this
   project when the production API does not exist yet; once the type exists,
   assertion failures are the RED form).
2. **GREEN** - smallest implementation that passes that cycle's tests.
3. **REFACTOR** - behavior-preserving cleanup only on green; no behavior
   change without a new RED.
4. One logical behavior per work item; full per-commit gate:
   `cargo test --offline --lib --bins` +
   `cargo clippy --all-targets -- -D warnings` +
   `cargo fmt --check`.
5. **No network.** This issue is pure unit tests only. Do not touch
   `tests/s3_integration.rs` or env-gated suites.
6. Docs-only changes (decision-log row) have no RED; they land under the
   all-green gate.
7. Work items continue the project W-series at **W178+**. One commit per
   item (or RED+GREEN pair collapsed only when the RED is compile-fail on a
   brand-new symbol and the GREEN is the first body - still prefer separate
   mental cycles; commits may be `test:` then `feat:` if that keeps bisect
   clean).
8. **Do not** edit W25 characterization tests, walk tests, or config tests.
   Characterization tests are never silently edited. This issue must not
   change their behavior.
9. **Do not** change `Cargo.toml` dependencies.

### Mutation-check habit (cheap, required on match rules)

After each GREEN match rule, flip one assertion or one fixture key in a
throwaway edit to confirm the test would fail if the rule regressed, then
revert. Record "mutation-checked" in the commit body for the core pins
(basename, dir prefix false friends, star non-crossing, validation names
pattern).

---

## Design (what lands in the tree)

### New file: `src/ignore.rs`

Internal shape (implementation detail; tests lock behavior not privates):

```rust
#[derive(Debug, Clone)]
pub struct IgnoreSet {
    rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
enum Rule {
    /// Final-segment equality (no `/` in pattern, no `*`).
    Basename(String),
    /// Final-segment glob (no `/` in pattern, has `*`).
    BasenameGlob(SegmentGlob),
    /// Full-key equality (has `/`, no trailing `/`, no `*`).
    Exact(String),
    /// Segment zip (has `/`, no trailing `/`, has `*`).
    ExactSegs(Vec<Segment>),
    /// Dir prefix: literal string including trailing `/` when no globs.
    DirPrefix(String),
    /// Dir prefix with per-segment globs.
    DirPrefixSegs(Vec<Segment>),
}

#[derive(Debug, Clone)]
enum Segment {
    Exact(String),
    Glob(SegmentGlob),
}

/// Pre-split on `*` so match is allocation-light at query time.
#[derive(Debug, Clone)]
struct SegmentGlob {
    /// Literal pieces between `*` wildcards (len = star_count + 1).
    parts: Vec<String>,
}
```

Compile pipeline per raw pattern string:

1. Reject if `pat.is_empty()`.
2. Reject if `pat.starts_with('/')`.
3. Reject if `pat.contains("**")`.
4. Reject if `pat.contains('!')`.
5. Reject if `pat.contains('\\')`.
6. Reject if `pat.contains('[') || pat.contains(']')`.
7. Reject if `pat.contains('?')`.
8. `dir = pat.ends_with('/')`; `body = dir then strip one trailing / else pat`.
9. Split `body` on `/`; reject any empty segment.
10. Classify into one `Rule` variant as in the table.
11. For glob segments, split on `*` into `SegmentGlob { parts }` (preserve
    empty parts for leading/trailing/consecutive stars **inside a segment**
    only when the raw string did not contain `**` - consecutive stars are
    already rejected at step 3, so interior parts are never adjacent-empty
    from `**`; a segment that is exactly `*` becomes `parts = ["", ""]`).

`matches(&self, key: &str) -> bool`:

- `self.rules.iter().any(|r| r.matches(key))`
- Basename: `final_segment(key) == name`
- BasenameGlob: `segment_glob_matches(final_segment(key), &glob)`
- Exact: `key == s`
- ExactSegs: split key into segments (**no** trailing-empty from folder form -
  if `key.ends_with('/')` this rule returns false, because an exact non-dir
  pattern never ends with `/`)
- DirPrefix: `key == prefix || key.starts_with(prefix)` where `prefix` retains
  trailing `/`
- DirPrefixSegs: strip at most one trailing `/` from key for segment split
  **or** keep folder form consistent: split on `/`, drop a single trailing
  empty from a folder key; require `key_segs.len() >= pat_segs.len()` and
  zip-match the prefix; additionally, if `key_segs.len() == pat_segs.len()`,
  the key must have been a folder key (ended with `/`) so a file whose path
  equals the dir path without slash does not match. Equivalent simpler rule
  for the no-glob path is the string-prefix rule - prefer string-prefix for
  `DirPrefix(String)` and implement `DirPrefixSegs` carefully with tests.

Helper `final_segment(key: &str) -> &str` (private):

```text
let k = key.strip_suffix('/').unwrap_or(key);
match k.rfind('/') {
    Some(i) => &k[i+1..],
    None => k,
}
```

### `src/lib.rs` wiring

```rust
pub mod ignore;
pub use ignore::IgnoreSet;
```

No call sites in `build_plan`, `local`, `cli`, or `config` in this issue.

### Error message shape (stable enough for tests)

Tests assert:

- `err.to_string()` (or `Display`) contains the pattern text, and
- contains a reason token (`empty`, `leading '/'` / `leading /`, `**`,
  `negation` / `!`, `character class` / `[`, `escape` / `\`, `?`,
  `empty path segment` as applicable).

Suggested format (implementer may tweak wording once, pinned by tests):

```text
invalid ignore pattern "./**/x": '**' is not supported
invalid ignore pattern "": pattern must not be empty
invalid ignore pattern "/abs": pattern must not start with '/'
```

Use `Error::Other(msg)` so `Display` is the bare message (see
`src/error.rs` - `Other` writes `{msg}` with no prefix).

### Files explicitly out of scope (must stay untouched)

- `src/config.rs`, `src/cli.rs`, `src/local.rs`, `src/plan/**`,
  `src/exec.rs`, `src/store/**`, `tests/**`
- `doc/cli.md`, `doc/sync-model.md` user-facing ignore wording (#34)
- `Cargo.toml` / `Cargo.lock` dependency sections

Allowed docs touch: `doc/roadmap.md` decision-log one-liner + this plan file.

---

## Work items

### W178 - module seam + empty set (compile RED -> GREEN)

**RED:**

1. Add `src/ignore.rs` tests module first via a `lib.rs` `pub mod ignore`
   pointing at a file that contains **only** tests referencing
   `IgnoreSet::from_patterns` / `matches` (or add the test file content and
   empty `todo!` type - preferred project form: tests call the real API and
   fail to compile).
2. Test `ignore_set_empty_patterns_matches_nothing`:
   - `let set = IgnoreSet::from_patterns(&[]).unwrap();`
   - assert `!set.matches("")` (defensive; empty key is not a normal entity
     key but must not panic)
   - assert `!set.matches(".DS_Store")`
   - assert `!set.matches("notes/a.md")`
   - assert `!set.matches(".git/")`

Confirm compile failure (`IgnoreSet` missing) or, if a stub already exists
from scaffolding, assertion failure.

**GREEN:**

- `IgnoreSet { rules: vec[] }` with `from_patterns` accepting only the empty
  slice path for now **or** full compile stub that rejects everything else
  with `todo!` - prefer full `from_patterns` body that loops and compiles,
  with only empty/valid-but-unimplemented paths deferred. Smallest clean
  GREEN: implement `from_patterns` + `matches` for the zero-pattern case
  only; any non-empty pattern returns a temporary `Error::Other("not yet")`
  **only if** no non-empty test is in this commit. Cleaner: this commit
  implements the full compile/match skeleton with basename/exact/dir/star
  still wrong only if untested - **do not ship wrong behavior under a
  green suite**. So W178 implements empty-set only; non-empty patterns may
  `unwrap` paths unimplemented via not being called, or `from_patterns`
  returns error on any non-empty until later W items expand acceptance.
  **Preferred:** `from_patterns` in W178 accepts empty and **also** stores
  raw strings without matching logic beyond `false`, and later items replace
  storage with compiled rules as tests demand - actually that would make
  future RED soft. Better approach:

  - W178: type + empty-set behavior; `from_patterns` on non-empty returns
    `Err(Error::Other(...))` **or** compiles into rules that still don't
    match (dangerous). **Strict:** only empty input is `Ok`; non-empty is
    `Err` with message `unimplemented` - no, that fights W179.

  **Strict fine-grained sequence used here:**

  - W178 adds API + empty OK + `matches` always false (even if patterns were
    stored uncompiled). Storing uncompiled patterns with `matches` => false
    would be a lie once someone calls it. So: W178 does not need to accept
    non-empty yet; `from_patterns` returns `Err` for non-empty with a clear
    internal message **not** pinned by tests, replaced in W179. That is
    awkward.

  **Practical TDD used on this codebase for new types:** one commit that
  adds the failing tests for the first behavior **and** the type with just
  enough body to pass that behavior; next commit adds the next tests
  (failing) then body.

  For W178 specifically: implement `from_patterns` fully parsing into rules
  **as far as empty + structure requires**, and `matches` returns false when
  `rules` is empty. Parsing of non-empty can be complete early if small -
  but then later tests start green without RED. **Avoid implementing
  untested match arms.** Match arms for untested rule kinds should
  `unreachable!` or return false only if unreachable from compile.

  Simplest discipline:

  1. W178 GREEN implements compile + match for **nothing but empty input**.
     `from_patterns` on non-empty: `todo!()` or returns Err - **not** called
     by W178 tests.
  2. W179 RED adds basename tests; GREEN implements basename compile+match.
  3. etc.

Gate.

**Commit:** `feat: [30] add IgnoreSet empty-set seam (W178)`

---

### W179 - basename-anywhere (`.DS_Store`)

**RED:**

Table-driven or explicit test `ignore_set_basename_ds_store`:

| patterns | key | expect |
| -------- | --- | ------ |
| `[".DS_Store"]` | `.DS_Store` | true |
| | `notes/.DS_Store` | true |
| | `a/b/.DS_Store` | true |
| | `.DS_Store/` (folder form) | true |
| | `DS_Store.bak` | false |
| | `notes/DS_Store.bak` | false |
| | `.ds_store` (case) | false |
| | `notes/.DS_Store.bak` | false |
| | `not-.DS_Store` | false |
| | `notes/.DS_Store/extra` | false (final segment `extra`) |

Also pin multi-pattern OR: patterns `[".DS_Store", "Thumbs.db"]` matches
both basenames.

Confirm RED (basename not implemented / always false).

**GREEN:** compile slash-free, metachar-free patterns to `Rule::Basename`;
implement `final_segment` + equality. Mutation-check one row.

**Commit:** `feat: [30] IgnoreSet basename-anywhere match (W179)`

---

### W180 - trailing-slash dir prefix (`.git/`, `.trash/`)

**RED:** test `ignore_set_dir_prefix_git` (and trash false friends):

| patterns | key | expect |
| -------- | --- | ------ |
| `[".git/"]` | `.git/` | true |
| | `.git/objects/aa` | true |
| | `.git/objects/aa/bb` | true |
| | `.gitignore` | false |
| | `git/` | false |
| | `.github/workflows/x` | false |
| | `foo.git/` | false |
| `[".trash/"]` | `.trash/` | true |
| | `.trash/foo.md` | true |
| | `not-trash.md` | false |
| | `foo.trash` | false |
| | `.trashfile` | false |
| | `notes/.trash/` | false (not a prefix of the pattern; pattern is vault-rooted path form) |

Note: dir patterns are **path-prefix**, not basename. `.trash/` does not
ignore `notes/.trash/` under this table (unlike basename `.DS_Store`). This
matches the issue table (`path/to/dir/` = prefix on the key string) and the
default profile (vault-root `.trash/`, `.git/`). Pin it explicitly so #32
does not invent basename-dir behavior later.

Confirm RED.

**GREEN:** `Rule::DirPrefix(String)` with `key == p || key.starts_with(p)`.
Mutation-check `.gitignore` and `.trashfile`.

**Commit:** `feat: [30] IgnoreSet dir-prefix trailing-slash match (W180)`

---

### W181 - exact path (`.obsidian/workspace.json`)

**RED:** test `ignore_set_exact_workspace_json`:

| patterns | key | expect |
| -------- | --- | ------ |
| `[".obsidian/workspace.json"]` | `.obsidian/workspace.json` | true |
| | `.obsidian/workspace` | false |
| | `.obsidian/workspace-mobile.json` | false |
| | `.obsidian/workspace.json/extra` | false |
| | `.obsidian/workspace.json/` | false |
| | `x/.obsidian/workspace.json` | false |
| `[".obsidian/workspace"]` | `.obsidian/workspace` | true |
| | `.obsidian/workspace.json` | false |

**GREEN:** `Rule::Exact`. No partial prefix for non-trailing-slash patterns.

**Commit:** `feat: [30] IgnoreSet exact path match (W181)`

---

### W182 - single-segment `*` glob

**RED:** test `ignore_set_star_segment`:

| patterns | key | expect |
| -------- | --- | ------ |
| `[".obsidian/workspace*"]` | `.obsidian/workspace` | true |
| | `.obsidian/workspace.json` | true |
| | `.obsidian/workspace-mobile.json` | true |
| | `.obsidian/app.json` | false |
| | `.obsidian/workspaces/x` | false (`*` does not cross `/`) |
| | `workspace` | false (segment count) |
| `["*.tmp"]` | `foo.tmp` | true |
| | `notes/foo.tmp` | true (basename glob) |
| | `foo.tmp.x` | false |
| | `fooxtmp` | false |
| `["pre*mid*suf"]` | `preMIDsuf` wait - case exact: `premidXsuf` etc. | pin one happy + one miss |
| `["*"]` | `a` | true |
| | `a/b` | true (final segment any) |
| | `a/b/` | true (final segment `b`) |

Also pin path glob with internal slash: `["foo/*/bar"]` matches `foo/x/bar`,
not `foo/x/y/bar`, not `foo/bar`.

Confirm RED.

**GREEN:**

- Basename glob + exact-segs + (if needed) dir-prefix-segs.
- `SegmentGlob` matcher: ordered literal parts; wildcards fill gaps; no `/`
  in the key segment.
- Reject raw `**` still deferred to W183 if not already - if a user pattern
  `.obsidian/**` could reach glob compile, W183 must land before or with any
  path that would mis-accept `**`. **Order constraint:** W183 validation
  can land before W182 if easier; either order is fine as long as `**` never
  silently becomes "two stars". Prefer **W183 before W182** if implementing
  star compile in the same parser that must reject `**` first.

Revise order: see sequencing below.

**Commit:** `feat: [30] IgnoreSet per-segment '*' glob (W182)`

---

### W183 - loud validation rejects

**RED:** test `ignore_set_rejects_doublestar_negation_empty_abs` (table-driven):

| pattern | reason must mention (substr) |
| ------- | ---------------------------- |
| `""` | `empty` |
| `"/abs"` or `"/.DS_Store"` | `leading` and `/` |
| `"./**/x"` or `"**"` or `"a/**/b"` | `**` |
| `"!foo"` or `"foo!"` or `"a/!b"` | `!` (and/or `negation`) |
| `"foo[bar]"` | `class` or `[` |
| `"foo\\bar"` | `escape` or `\\` |
| `"foo?"` | `?` |
| `"a//b"` | `empty` segment |
| `"/"` | `leading` |

For each: `from_patterns(&[pat.to_string()])` is `Err`, `Display` contains
the pattern (e.g. the raw characters) and the reason token.

Optional same-commit: mixed list where the **second** pattern is invalid
still errors naming the invalid one (first may be valid `.DS_Store`).

Confirm RED.

**GREEN:** validation steps at the top of compile (order listed in Design).
No match-behavior change for already-pinned valid patterns.

**Commit:** `feat: [30] IgnoreSet loud pattern validation (W183)`

---

### W184 - default-profile fixture lock (optional but cheap)

**RED/GREEN in one small commit** if not already covered:

Compile the epic's D3 built-in set:

```text
.git/
.trash/
.DS_Store
.obsidian/workspace
.obsidian/workspace.json
.obsidian/workspace-mobile.json
```

Assert:

- each compiles (`from_patterns` Ok)
- `.obsidian/app.json` is **not** ignored
- `.obsidian/workspace.json` is ignored
- `.git/HEAD` is ignored
- `notes/.DS_Store` is ignored
- `notes/foo.md` is not

This is the bridge fixture #31/#34 will reuse mentally; still pure #30.

**Commit:** `test: [30] lock Obsidian default pattern set against IgnoreSet (W184)`

---

### W185 - hygiene: re-export, rustdoc, roadmap decision log, plan status

No RED.

1. Rustdoc on `IgnoreSet` and methods citing issue #30 and the table
   (brief; point to epic #9 for application).
2. `pub use ignore::IgnoreSet` if not done in W178.
3. `doc/roadmap.md` decision-log row, dated, e.g.:

   > I30-matcher | Issue #30: pure `IgnoreSet` hand-rolled matcher
   > (basename / exact / dir-prefix / segment `*`); no new dep; validation
   > loud on empty / leading `/` / `**` / `!` / classes / escapes / `?`.
   > Application and W25 retire remain #31-#34.

4. Flip this plan's **Status** line to `implemented` when landing.
5. Full gate once more.

**Commit:** `docs: [30] IgnoreSet rustdoc + roadmap decision log (W185)`

---

## Sequencing (commits on the branch)

```text
W178 empty seam
  -> W179 basename
  -> W180 dir prefix
  -> W181 exact path
  -> W183 validation   (before star compile so ** cannot leak)
  -> W182 star glob
  -> W184 default-profile fixture
  -> W185 docs / decision log
```

Rationale for W183 before W182: the star compiler must never see `**`;
landing rejects first keeps the RED/GREEN for globs honest.

Each arrow is a separate commit with the full offline gate green.

---

## Acceptance mapping (issue checkboxes -> work items)

| Acceptance | Work item |
| ---------- | --------- |
| `IgnoreSet` compiles patterns and matches per the table | W178-W183 |
| Basename `.DS_Store` drops nested + root; not `DS_Store.bak` | W179 |
| Trailing-slash `.trash/` prefix; no false friends | W180 |
| Exact `.obsidian/workspace.json` only that key | W181 |
| Star `.obsidian/workspace*` three workspace names; not `app.json` | W182 |
| Validation loud: empty, leading `/`, `**`, `!` naming pattern | W183 (+ classes/`\`/`?` extras) |
| No new direct dependency | entire issue; Cargo.toml untouched |
| `cargo fmt` / `clippy -D warnings` / offline lib tests green | every commit gate |

Issue test sketch names (use these names so the issue can be checked off
literally):

- `ignore_set_basename_ds_store` (W179)
- `ignore_set_dir_prefix_git` (W180)
- `ignore_set_exact_workspace_json` (W181)
- `ignore_set_star_segment` (W182)
- `ignore_set_rejects_doublestar_negation_empty_abs` (W183)
- `ignore_set_empty_patterns_matches_nothing` (W178)

---

## Explicit non-goals (refuse scope creep while implementing)

- `[ignore].profile` / built-in injection / `profile = "none"` (#31)
- Walker prune, `WalkReport.skipped_ignored` (#32)
- Remote listing filter / delete invariant (#33)
- Retire W25, CLI wiring, cli.md ignore section rewrite (#34)
- `matches_dir` convenience
- New `Error` variant
- `globset` / `ignore` / `gitignore` crate
- NFC case fold, Windows path semantics, byte-vs-char glob classes
- Matching against absolute paths or store keys with bucket prefix
- Planner `Skip(ignored)` rows (absence is later issues' job)

---

## Risk notes

| Risk | Mitigation |
| ---- | ---------- |
| Dir-prefix false friends (`.trash/` vs `.trashfile`) | string prefix **including** trailing `/`; W180 pins |
| Basename vs path `.trash` confusion | slash-free only basename; `.trash/` is dir rule; pin both |
| `**` accidentally compiled as two globs | reject substring `**` before split; W183 before W182 |
| Hand-rolled glob complexity blow-up | tripwire: stop + confirm dep; keep segment-local only |
| Accidental walk/CLI coupling | file touch list; review diff for `local.rs`/`cli.rs` zero |
| Empty key / odd keys in `matches` | no panic; empty set + basename of `""` is `""`; not matched by normal patterns |

---

## Post-landing (not this plan's commits)

- Close #30 after PR merge (or when acceptance is green on `main`).
- Unblock #31 / #32 / #33 in parallel per epic graph; #34 last.
- PR description should state: pure matcher only; W25 still in force;
  no user-visible behavior change.

---

## Implementation checklist (copy into PR body)

- [ ] W178 empty `IgnoreSet` seam
- [ ] W179 basename-anywhere
- [ ] W180 dir-prefix trailing `/`
- [ ] W181 exact path
- [ ] W183 validation loud rejects
- [ ] W182 segment `*`
- [ ] W184 default profile fixture
- [ ] W185 rustdoc + roadmap log + plan status
- [ ] `Cargo.toml` deps unchanged
- [ ] No edits to `cli.rs` / `config.rs` / `local.rs` / `build_plan` path
- [ ] Full gate green on tip
