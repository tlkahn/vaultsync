# Issue 5 plan: CI workflow - pin toolchain, verify MSRV 1.85, fmt/clippy/test

**Status:** implemented (pending green CI on the introducing PR)
**Issue:** https://github.com/tlkahn/vaultsync/issues/5 (OPEN, P1 - dependency hub for #6 and #7)

## Execution record (deviations decided during implementation)

- **MSRV bump 1.85 -> 1.95 (user decision, option "bump").** The step 1
  pre-flight check failed: `cargo +1.85.0 check --locked` errors because the
  current locked tree (aws-sdk-s3 1.144.0 / aws-config 1.11.0 / the
  aws-smithy-* crates, 25 packages total) now declares `rust_version =
  "1.94.1"` (crc-fast 1.10.0 needs 1.89). Since stable rust ships as x.0.0,
  the true floor is 1.95.0 = the pinned toolchain. Kept the final dep tree
  and set `rust-version = "1.95"`; the `msrv` job now verifies 1.95.0
  (forward guard: if the pinned channel is later bumped, the msrv job still
  proves the declared floor builds). Clippy surfaced new 1.95 lints on
  existing code (collapsible_if x7, manual_is_multiple_of x2 in
  config.rs/local.rs/s3.rs/examples-s3_spike.rs/plan tests) - fixed in the
  same commit so `-D warnings` lands green.
- **Integration suite skipped in CI (issue #17).** `cargo test --locked`
  hangs when it reaches `tests/s3_integration.rs` (tracked separately as
  issue #17; the env-gated suite was expected to self-skip, not hang). Test
  and msrv jobs now run `cargo test --locked --lib --bins` (367 unit tests)
  and skip the tests/ target. Compilation of the suite stays covered by the
  clippy job's `--all-targets`. #6's no-silent-skip/`#[ignore]` hardening
  now also must account for the #17 hang before the suite can run in CI.

**Design refs:** [roadmap.md](../roadmap.md) Phase 3 item 7, [test-matrix.md](../test-matrix.md), [phase-2.md](phase-2.md) CI note
**Verified baseline:** default branch `main`; local toolchain `rustc 1.95.0`;
`rust-version = "1.85"` already in `Cargo.toml`; `Cargo.lock` is format v4
(readable by cargo >= 1.78, so 1.85 can consume it); no `.github/` directory
yet; no dev-dependencies; the S3 integration suite
(`tests/s3_integration.rs`) compiles always but self-skips at runtime unless
`VAULTSYNC_TEST_S3_BUCKET` is set.

---

## Scope (from the issue)

1. Pin the Rust toolchain.
2. Verify MSRV 1.85 - a CI job building/testing with 1.85. This is also the
   "exact verification against the final dep tree" the `Cargo.toml` MSRV
   comment defers to Phase 3.
3. fmt / clippy / test workflow (the Phase 2 checklist's deferred CI note).

Explicitly **out of scope** (dependent issues that build on this one):

- #6 - setting `VAULTSYNC_TEST_S3_BUCKET` in CI + no-silent-skip hardening
  (`#[ignore]`/`--ignored` or a CI sentinel). This plan only makes sure the
  workflow has a clean place to add that.
- #7 - Cloudflare R2 endpoint matrix row. Same: structure must not preclude
  adding an endpoint/prefix matrix dimension to the test job later.
- Windows CI, release/publish workflows, coverage.

## Locked decisions (made with the user before writing this plan)

| ID | Decision | Choice |
| -- | -------- | ------ |
| I5-pin | Toolchain pinning mechanism | **`rust-toolchain.toml` checked in**, channel pinned to the exact current stable `1.95.0` (matches local dev today), `profile = "minimal"`, `components = ["rustfmt", "clippy"]`. CI and all contributors resolve the same toolchain automatically; fmt/clippy results cannot drift when a new stable ships. Bumping is a deliberate one-line edit + decision-log row. |
| I5-os | Test matrix OSes | **`ubuntu-latest` + `macos-latest`.** The project is Unix-style and development happens on macOS; Windows is out of scope for v1 (paths/symlink assumptions). fmt, clippy, and the MSRV job run on ubuntu-latest only. |
| I5-cache | Build caching | **`Swatinem/rust-cache@v2`** in every job. The aws-sdk-s3 dep tree makes cold builds take several minutes; one third-party action is accepted. All other actions are official (`actions/checkout@v4`). |

## Design

### Files added

- `rust-toolchain.toml`:

  ```toml
  [toolchain]
  channel = "1.95.0"
  profile = "minimal"
  components = ["rustfmt", "clippy"]
  ```

  Local note: after this lands, plain `cargo` in the repo resolves to the
  `1.95.0` toolchain (rustup installs it on first use if absent) instead of
  the `stable` default. Same version today, so no behavior change; the
  per-commit gate (`cargo test --offline` + clippy + fmt) is unchanged.

- `.github/workflows/ci.yml` (single workflow, four jobs). As-designed
  snapshot; the shipped workflow differs (MSRV 1.95, `cargo test --locked
  --lib --bins` per the execution record above) - see
  `.github/workflows/ci.yml` for current truth:

  ```yaml
  name: CI

  on:
    push:
      branches: [main]
    pull_request:

  concurrency:
    group: ci-${{ github.workflow }}-${{ github.ref }}
    cancel-in-progress: true

  env:
    CARGO_TERM_COLOR: always

  jobs:
    fmt:
      name: rustfmt
      runs-on: ubuntu-latest
      steps:
        - uses: actions/checkout@v4
        - name: Install pinned toolchain
          run: rustup show
        - uses: Swatinem/rust-cache@v2
        - run: cargo fmt --check

    clippy:
      name: clippy
      runs-on: ubuntu-latest
      steps:
        - uses: actions/checkout@v4
        - name: Install pinned toolchain
          run: rustup show
        - uses: Swatinem/rust-cache@v2
        - run: cargo clippy --all-targets --locked -- -D warnings

    test:
      name: test (${{ matrix.os }})
      runs-on: ${{ matrix.os }}
      strategy:
        fail-fast: false
        matrix:
          os: [ubuntu-latest, macos-latest]
      steps:
        - uses: actions/checkout@v4
        - name: Install pinned toolchain
          run: rustup show
        - uses: Swatinem/rust-cache@v2
        - run: cargo test --locked

    msrv:
      name: msrv (1.85)
      runs-on: ubuntu-latest
      env:
        RUSTUP_TOOLCHAIN: 1.85.0   # env beats rust-toolchain.toml
      steps:
        - uses: actions/checkout@v4
        - name: Install Rust 1.85.0
          run: rustup toolchain install 1.85.0 --profile minimal
        - uses: Swatinem/rust-cache@v2
        - run: cargo check --locked
        - run: cargo test --locked
  ```

### Design notes

- **Commands mirror the local per-commit gate exactly**
  (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test`), plus `--locked` everywhere so CI always builds the committed
  `Cargo.lock` (reproducibility; a lockfile bump is a deliberate commit).
- **`rustup show`** is the zero-config install step for the pinned-toolchain
  jobs: it installs the `rust-toolchain.toml` channel (with the declared
  components) if missing. No third-party toolchain action needed.
- **MSRV override:** `RUSTUP_TOOLCHAIN` has higher precedence than
  `rust-toolchain.toml`, so the msrv job gets 1.85.0 while every other job
  gets the pinned 1.95.0. The toolchain file stays truthful for contributors.
- **Cache isolation:** rust-cache's key incorporates the OS and the compiler
  version, so the 1.95.0 jobs and the 1.85.0 msrv job never share caches.
  In the msrv job the toolchain-install step runs before rust-cache so the
  key is computed against 1.85.0 deterministically.
- **`--locked` + lockfile v4:** cargo 1.85 reads lockfile format v4 (supported
  since 1.78), so the msrv job consumes the committed lockfile as-is.
- **Env-gated integration tests:** `cargo test --locked` runs the default
  suite; `tests/s3_integration.rs` self-skips without
  `VAULTSYNC_TEST_S3_BUCKET` (verified behavior of the current suite). #6
  will add the bucket secret plus a no-silent-skip sentinel - the test job
  needs no structural change for that, just `env:` entries. #7 adds an
  endpoint/prefix matrix dimension to the same job.
- **Triggers:** `push` to `main` + every `pull_request`. The PR that
  introduces this workflow is itself the acceptance test.
- **Concurrency group** cancels superseded runs on the same ref to keep the
  (slow, uncached first-time) aws-sdk builds from queueing.

## Method / work steps

This is CI infrastructure, not a library behavior change, so the strict
RED/GREEN TDD cycle does not apply; verification is the workflow running
green on its own PR.

1. **Pre-flight MSRV check (local, before writing any YAML).**
   - `rustup toolchain install 1.85.0 --profile minimal`
   - `cargo +1.85.0 check --locked` and `cargo +1.85.0 test --locked`
   - Record the outcome in this plan.
   - **Contingency if a transitive dep raised its MSRV past 1.85** (aws-sdk
     crates do this occasionally): decide with the user between
     (a) `cargo update -p <crate> --precise <last-1.85-compatible>` and
     committing the lockfile (keeps `rust-version = "1.85"` truthful), or
     (b) bumping `rust-version` + the issue/roadmap text (weakens the MSRV
     promise). Default preference is (a) unless the pin is impossible.
2. Add `rust-toolchain.toml` (content above). Sanity: `cargo --version` in
   the repo now reports 1.95.0; re-run the local per-commit gate once to
   confirm no drift.
3. Add `.github/workflows/ci.yml` (content above).
4. Local validation: YAML parses clean (`actionlint` if available, else a
   YAML load), and each job's run command passes locally under the toolchain
   that job will use (`cargo fmt --check`; `cargo clippy --all-targets
   --locked -- -D warnings`; `cargo test --locked`; `cargo +1.85.0 check
   --locked`; `cargo +1.85.0 test --locked`).
5. Branch `ci/issue-5`, PR with `Closes #5`. Verify on the PR:
   - all five checks green: rustfmt, clippy, test (ubuntu), test (macos),
     msrv (1.85);
   - the msrv job log shows `rustc 1.85.x` and builds the locked dep tree;
   - the test jobs' logs show the S3 integration suite's skip notes
     (self-skip, not pass) - confirming the suite still compiles on both OSes
     and stays silent-but-green until #6.
6. After merge: add a roadmap decision-log row (toolchain pinned via
   `rust-toolchain.toml` at 1.95.0; MSRV verified against the locked dep
   tree in CI; cache via rust-cache) and tick the Phase 3 item 7 wording to
   reflect what remains for #6/#7.

## Acceptance criteria

- [x] `rust-toolchain.toml` pins `1.95.0` with rustfmt + clippy components.
- [x] `.github/workflows/ci.yml` runs rustfmt, clippy (`--all-targets`,
      warnings denied), and the unit test suite on ubuntu + macOS against the
      pinned toolchain and the committed lockfile.
- [x] A dedicated job checks and tests with the declared MSRV, proving the
      `rust-version = "1.95"` declaration against the final dep tree. (MSRV
      bumped from the original 1.85 because the aws crates now require 1.94.1
      - see Execution record.)
- [ ] All jobs green on the introducing PR; `tests/` integration suite is
      gated out of CI pending issue #17 (lib/bin unit tests run instead).
- [x] Roadmap decision log records the pinning/MSRV/cache choices.

## Risks / notes

- First run per job is a cold aws-sdk-s3 build (several minutes); subsequent
  runs hit rust-cache. Acceptable.
- If the pre-flight MSRV check fails on a transitive dep, step 1's
  contingency is resolved with the user **before** the workflow is written -
  the msrv job must land green, not aspirational.
- A future stable bump (1.96 etc.) is a deliberate `rust-toolchain.toml`
  edit; clippy may surface new lints then - handle in the bump commit, not
  silently in unrelated PRs.
