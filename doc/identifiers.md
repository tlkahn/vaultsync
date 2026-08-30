# Identifiers and terminology conventions

How this project names decisions, work items, roadmap records, review
findings, and plan-local references, so anyone reading an issue, plan,
review, or commit can find the source of truth for a label and can mint new
labels without colliding.

## Why this exists

Issues and plans reference labels like `D-wire`, `W203`, `I34-wire`, `N3`,
`F1`, and `R41-f2-pattern`. Each label is a **stable handle** to a specific
artifacts (a locked decision, a work item, a roadmap row, a review finding,
a plan-local scope choice). Handles let a review say "honors D-wire" or a
commit say "fixes F1 (W212)" without restating the whole argument, and let a
reader jump from any mention to the definition.

The labels are a project-local scheme, not an industry standard. They serve
the same purpose as RFC/ADR numbering but stay lightweight and live inside
the issue/plan/roadmap files the project already writes.

## Label families

| Label | Meaning | Defined in | Minted by |
| ----- | ------- | ---------- | --------- |
| `D-*` (e.g. `D-wire`, `D-w25-retire`, `D-report`) | Locked design decision; choose once, do not reopen during implementation without an explicit revisit | GitHub issue body ("Locked decisions") then expanded in `doc/plans/issue-N.md` | Issue author, when the issue scope is set |
| `D1` / `D2` / `D3` | Short decision ids used in the roadmap for the earliest locks (e.g. `D3` = default `.obsidian/` profile policy) | `doc/roadmap.md` decision log | Same as `D-*` |
| `W###` (e.g. `W203`, `W212`) | Work item: one logical change, usually one commit, in a running series | `doc/plans/issue-N.md` "Work items" sections | Implementer, continuing the series at the next free number |
| `I##-slug` (e.g. `I34-wire`, `I33-remote-ignore`, `I27-r2`) | Roadmap decision-log row recording an issue's outcome after it lands | `doc/roadmap.md` "Decision log" | Whoever closes the issue |
| `N#` (e.g. `N1`-`N6`) | Non-blocking review finding | Review comment + `doc/plans/pr-###-review-*.md` | Reviewer |
| `F#` (e.g. `F1`-`F9`) | Review finding in later reviews (replaces `N#`; same role, any severity) | Review comment + `doc/plans/pr-###-fix-*.md` | Reviewer |
| `R##-slug` (e.g. `R41-f2-pattern`, `R39-scope`) | Plan-local scope decision inside a fix plan (not a global lock) | `doc/plans/pr-###-fix-*.md` "Scope decisions" table | Fix-plan author |
| `M#` (e.g. `M3`) | Milestone tag; historically used alongside `W#` in comments (e.g. "W25/M3") | Old issue text; still referenced in code comments | Issue author (legacy) |
| `A#` / `B#` / `C#` | Component id in a plan's architecture diagram for cross-reference | The plan file that draws the diagram | Plan author |

## Where a decision is defined (the source-of-truth chain)

For any `D-*` decision, follow this chain. Later layers are records or
citations, not the definition.

1. **GitHub issue body** - owns the decision and its scope ("Locked decisions
   owned here; do not reopen in implementation"). Example: issue #34 defines
   `D-w25-retire`, `D-report`, and the default-profile activation.
2. **`doc/plans/issue-N.md`** - expands each locked decision into a table
   (id, lock aspect, concrete choice) plus the work items that implement it.
   Example: `doc/plans/issue-34.md` section "Locked decisions (owned by
   #34)" lists `D-wire` with three rows (Source of truth, Both halves,
   Check).
3. **`doc/roadmap.md` decision log** - a single `I##-slug` row summarizing the
   decision after it lands, with a plan pointer. This is history, not the
   operative definition.
4. **Code comments / tests** - cross-references only ("issue #34 D-wire").
   The code is expected to implement the lock; the comment does not define it.

Example trace for `D-wire`:

```text
issue #34 body            owns it
doc/plans/issue-34.md     "Locked decisions" table: compile IgnoreSet from
                          resolved_ignore_patterns only; same set into both
                          LocalFs::with_ignore and build_plan; check no-op
doc/roadmap.md I34-wire   landed summary row (2026-08-31)
src/cli.rs / src/config.rs comments  cite "issue #34 D-wire"
```

## Lifecycle

```text
Issue locks decisions (D-*)
  -> plan breaks them into work items (W###)
    -> commits land one W at a time
      -> roadmap logs the outcome (I##-slug)
        -> review findings (N# / F#) may spawn a fix plan (R##-*, more W###)
```

## Work items and the W-series

`W###` is a global running series (not per-issue). The next plan states
"continue the project W-series at W203+" and the implementer uses the next
free numbers. One logical behavior per work item; one commit per work item
(or a documented atomic pair). Commit subjects carry the ids:

```text
feat: [34] thread IgnoreSet through CLI dispatch seam (W203)
test: [34] pull delete e2e positive DL control (W212, PR41 F1)
docs: [34] PR41 r1 fix plan status + I34-r1 log (W217)
```

Rules:

- Pick the next free `W###`, never reuse.
- A single commit may contain a RED-verified test plus its GREEN
  implementation; record the local RED observation in the commit body.
- Mutation-check characterization pins (GREEN on arrival, temporarily break
  the path, observe RED, revert) and record the break in the commit body.

## Review findings: N vs F

- Early reviews used `N#` for "non-blocking nits" (`N1`-`N6` in PR 39).
- Later reviews use `F#` for findings of any severity (`F1`-`F9` in PR 41),
  with severity stated per finding (medium/low/nit).
- The finding number is stable for the review thread: the fix plan references
  the same id (e.g. "W212 - F1: pull delete e2e positive control").

## Plan-local scope decisions (R##-slug)

Inside a fix plan, `R##-slug` rows record choices that were not locked by the
issue (e.g. whether to widen an API, which fixture pattern to use). They are
plan-local; a future plan is free to decide differently. The slug is a short
kebab of the question (`R41-f7-api`, `R39-n4c-fixture`).

## Diagram component ids (A#, B#, C#)

Plans that include a Mermaid architecture diagram label components with
stable ids (`A1` settings boundary, `A2` dispatch, `B1` test pin, `C1`
rustdoc). Work items and scope tables reference those ids so the diagram,
the tests, and the docs can be cross-checked.

## Common mistakes

| Mistake | Correction |
| ------- | ---------- |
| "D-wire is defined in the code" | No: the code implements it; the definition is the issue + plan |
| Reusing a `W###` or skipping ahead | Always take the next free number |
| Inventing `D-*` inside an implementation | Decisions are locked by the issue; implementation proposes, does not lock |
| Treating an `I##-slug` row as the decision | It is the landed record; go to the issue/plan for the choice and rationale |
| Using `N#` and `F#` interchangeably in one thread | Pick one per review round; keep the same id for the same finding |
| Editing `R##-slug` after merge | Plan-local; if the choice matters after merge, promote it to a `D-*` or roadmap row |

## When to mint which label

| You are ... | Mint |
| ----------- | ---- |
| Writing an issue that owns a new design choice | `D-*` in the issue body; expand in `doc/plans/issue-N.md` |
| Planning implementation | Next free `W###` per work item |
| Closing an issue | `I##-slug` row in `doc/roadmap.md` |
| Reviewing a PR | `F#` (or `N#`) per finding in the review comment |
| Writing a fix plan for a review | `R##-slug` for plan-local scope questions; new `W###` for work items |
| Drawing a plan diagram | `A#` / `B#` / `C#` component ids |
