# Issue 6 plan: Integration test gate in CI (VAULTSYNC_TEST_S3_BUCKET + no-silent-skip)

**Status:** implemented on branch `ci/issue-6` (pending AWS one-time setup + green CI on the introducing PR)
**Issue:** https://github.com/tlkahn/vaultsync/issues/6 (OPEN, P1; body is the
full spec - this plan operationalizes it)

**Design refs:** [issue #6 spec](https://github.com/tlkahn/vaultsync/issues/6),
[roadmap.md](../roadmap.md) Phase 3 item 6 + item 7 tail, PR2 B-L7,
[issue-5.md](issue-5.md) (CI workflow this builds on),
[ci.yml](../../.github/workflows/ci.yml), issue #17 (paginate-test slowness),
issue #8 (request pool; the expected #17 fix)

**Verified baseline:** `main` at ccb815b.`tests/s3_integration.rs` (10 tests)
is env-gated: `with_store` and `path_style_env` print `[skip] ...` and pass
when `VAULTSYNC_TEST_S3_BUCKET` is unset. CI runs `cargo test --locked
--lib --bins` in the `test` and `msrv` jobs (suite gated out, per the I5
execution record and #17); clippy `--all-targets` still compiles it. The
harness already gives per-test unique prefixes (`vaultsync-itest-<ts>-<name>/`
under an optional `VAULTSYNC_TEST_S3_PREFIX` base) and a sweep-with-stderr
cleanup in `with_store`. Crate is edition 2024 (`std::env::set_var` is
`unsafe` - shapes the sentinel test design below).

---

## Execution record (deviations decided during implementation)

- **Local env already exports the suite's S3 vars.** The dev shell has
  `VAULTSYNC_TEST_S3_BUCKET`/`REGION`/`PATH_STYLE` set, so the local gate
  runs were live against real S3 (not self-skips): the non-paginate suite
  passed in 12s / 40s wall clock, and the `with_store` cleanup list did not
  stall (small N per test, per the Risks note). The skip-path verification
  used `env -u VAULTSYNC_TEST_S3_BUCKET` explicitly.
- **Sentinel smoke (plan step 4):** `env -u VAULTSYNC_TEST_S3_BUCKET
  VAULTSYNC_TEST_S3_REQUIRE=1` fails all 9 env-gated tests with the sentinel
  message while the pure `bucket_or_skip_sentinel_unit` test still passes;
  a bogus bucket name fails in 2.3s at the store/network level, not the
  sentinel. Both as designed.
- **Full-suite local run hangs as known (#17).** Plain `cargo test --test
  s3_integration` (including the paginate test) hangs even locally; all
  local verification used `--skip s3_integ_list_paginates`, matching the
  PR-gate job.
- **actionlint clean** on the updated workflow.
- **Still pending (not repo-side):** AWS one-time setup (Design section 3),
  repo variables, the PR verification (step 7), the sentinel break-test on
  the PR (step 8), and the first nightly observation (step 9).

Update (PR #19 execution):

- **AWS setup done** (root creds): bucket `vaultsync-itest-695018654605`
  (us-west-1, public access blocked), lifecycle rule `expire-ci-itest-leaks`
  (7 days on `ci/vaultsync-itest-`), OIDC provider, role
  `vaultsync-itest-ci`; repo variables set.
- **OIDC trust-policy fix:** first run failed `AssumeRoleWithWebIdentity`.
  A temporary debug step decoded the actual JWT: GitHub now emits
  `sub = repo:tlkahn@335719/vaultsync@1348364460:pull_request` (new
  owner@id/repo@id format), which `repo:tlkahn/vaultsync:*` never matches.
  Trust policy now StringLike-matches BOTH the legacy and the ID-pinned new
  format (the new one is rename-proof). Debug commit reverted.
- **PR gate green:** all 9 S3 tests + the sentinel unit test ran for real
  via OIDC (log shows `[ok]` lines; 0.74s of tests - the runner is close to
  us-west-1; 1m55s end-to-end cold, 26s warm).
- **Sentinel break-test (step 8):** a commit blanking
  `VAULTSYNC_TEST_S3_BUCKET` turned the job red in 35s as required - but via
  an opaque "failed to construct request", not the sentinel: an empty repo
  variable expands to `""`, and `std::env::var` returns `Ok("")`, so the
  sentinel never fired. Hardened: `bucket_or_skip` treats
  empty/whitespace-only as missing (unit test gains the arms); break-test
  commit reverted; follow-up run green.
- **Remaining:** merge, then the first nightly observation (step 9).

---

## Scope (from the issue spec)

1. A no-silent-skip sentinel (`VAULTSYNC_TEST_S3_REQUIRE=1`) in the harness.
2. An `integration` job in CI that runs the suite against real S3 (PR gate
   minus the paginate test) plus a nightly full-suite run.
3. AWS plumbing: dedicated test bucket, OIDC -> least-privilege IAM role,
   lifecycle expiry backstop.
4. Docs/roadmap updates in the same commit(s).

Explicitly **out of scope**:

- Fixing the #17 slowness itself (that is #8's request pool). This plan only
  routes around it with `--skip` on the PR gate.
- MinIO/R2 endpoint matrix rows (PR2-defer-r2-row, issue #7's neighbor).
- Multipart-upload integration coverage (post-v1).
- Changing what the tests assert; the only harness change is skip behavior.

## Locked decisions (made with the user before writing this plan)

| ID | Decision | Choice |
| -- | -------- | ------ |
| I6-sentinel | No-skip mechanism | **`VAULTSYNC_TEST_S3_REQUIRE=1` require-env sentinel.** Skip sites fail loudly when set; local `cargo test` without S3 stays green. `#[ignore]` + `--ignored` rejected: worse local ergonomics, and the suite would silently never run anywhere by default. |
| I6-creds | CI auth to S3 | **GitHub OIDC -> IAM role** via `aws-actions/configure-aws-credentials@v4`; `id-token: write` on the integration job only. No long-lived keys in secrets. Fallback (documented, not planned): dedicated IAM user keys in secrets, same scoping. |
| I6-trigger | When the suite runs | **PRs + main: full suite minus `s3_integ_list_paginates`; nightly cron: everything.** The `--skip` is dropped and the two jobs merge once #17 lands. |
| I6-bucket | Bucket layout | **Dedicated test bucket**, all CI objects under `VAULTSYNC_TEST_S3_PREFIX=ci/`; lifecycle rule expires `ci/vaultsync-itest-*` after 7 days as the leak backstop. Bucket name/region live in repo **variables** (not secrets) so logs show them. |
| I6-timeout | Hang guard | **`timeout-minutes: 20`** on both integration jobs. A stuck run fails the job instead of idling to the 6h default. Revisit when #17 lands and the real paginate runtime is known. |

## Design

### 1. Harness: require-mode sentinel (`tests/s3_integration.rs`)

Extract the skip decision into a pure, unit-testable helper so the sentinel
itself gets test coverage without env mutation (edition 2024 makes
`std::env::set_var` unsafe; parallel tests would race on process env anyway):

```rust
/// Skip-or-require decision for the bucket env gate (I6-sentinel).
/// `bucket`: the resolved VAULTSYNC_TEST_S3_BUCKET value.
/// Returns Ok(Some(bucket)) to run, Ok(None) to skip (caller prints the
/// note), Err(msg) when require mode is on and the bucket is missing.
fn bucket_or_skip(bucket: Option<String>, require: bool, name: &str)
    -> Result<Option<String>, String>
{
    match (bucket, require) {
        (Some(b), _) => Ok(Some(b)),
        (None, false) => Ok(None),
        (None, true) => Err(format!(
            "{name}: VAULTSYNC_TEST_S3_BUCKET is unset but \
             VAULTSYNC_TEST_S3_REQUIRE=1 - refusing to silently skip"
        )),
    }
}

fn require_mode() -> bool {
    std::env::var("VAULTSYNC_TEST_S3_REQUIRE").map(|v| v == "1").unwrap_or(false)
}
```

- `with_store` calls `bucket_or_skip(std::env::var("VAULTSYNC_TEST_S3_BUCKET").ok(),
  require_mode(), name)`: `Ok(Some)` proceeds, `Ok(None)` keeps today's
  `[skip]` eprintln + return, `Err` **panics** (fail the test, fail the job).
- `path_style_env` gets the same treatment. Its `Option` return type already
  encodes skip; require mode turns the `None` case into a panic at the same
  call site.
- The vhost path-style test's endpoint-conditional early return **stays a
  skip** even in require mode (legitimate AWS-vs-custom-endpoint branch, not
  missing config), but its note gains "(`VAULTSYNC_TEST_S3_REQUIRE=1` set;
  this branch is intentional)" so the log is unambiguous.
- New unit test in the same file (runs everywhere, no S3 needed):
  `bucket_or_skip(None, true, "t")` is `Err`, `bucket_or_skip(None, false, ..)`
  is `Ok(None)`, `bucket_or_skip(Some(b), _, ..)` passes through. Pure
  function, no unsafe, no env races.
- The module doc comment gains a line documenting
  `VAULTSYNC_TEST_S3_REQUIRE=1`.

### 2. Workflow: `integration` job + nightly (`.github/workflows/ci.yml`)

Two jobs, same shape, different command and trigger:

```yaml
on:
  push:
    branches: [main]
  pull_request:
  schedule:
    - cron: "17 5 * * *"   # nightly full suite (off-hour UTC)

jobs:
  # ... existing fmt / clippy / test / msrv unchanged ...

  integration:
    name: integration (S3, PR gate)
    # PRs + main only; nightly is the separate job below.
    if: github.event_name != 'schedule'
    runs-on: ubuntu-latest
    timeout-minutes: 20
    permissions:
      contents: read
      id-token: write        # OIDC, this job only (I6-creds)
    env:
      VAULTSYNC_TEST_S3_BUCKET: ${{ vars.VAULTSYNC_TEST_S3_BUCKET }}
      VAULTSYNC_TEST_S3_REGION: ${{ vars.VAULTSYNC_TEST_S3_REGION }}
      VAULTSYNC_TEST_S3_PREFIX: ci/
      VAULTSYNC_TEST_S3_REQUIRE: "1"
    steps:
      - uses: actions/checkout@v4
      - name: Install pinned toolchain
        run: rustup show
      - uses: Swatinem/rust-cache@v2
      - uses: aws-actions/configure-aws-credentials@v4
        with:
          role-to-assume: ${{ vars.VAULTSYNC_CI_ROLE_ARN }}
          aws-region: ${{ vars.VAULTSYNC_TEST_S3_REGION }}
      # s3_integ_list_paginates is excluded until #17 lands (I6-trigger);
      # --nocapture keeps [ok]/[skip]/cleanup notes in the job log.
      - run: >-
          cargo test --locked --test s3_integration --
          --skip s3_integ_list_paginates --nocapture

  integration-nightly:
    name: integration (S3, nightly full)
    if: github.event_name == 'schedule'
    # identical to `integration` except:
    #   - run: cargo test --locked --test s3_integration -- --nocapture
```

Notes:

- **Job-permissions override:** the workflow's top-level `permissions:
  contents: read` stays; only these jobs add `id-token: write`.
- **Repo variables, not secrets**, for bucket/region/role ARN (none are
  sensitive; visibility in logs aids debugging). The OIDC flow stores no AWS
  keys at all.
- **Nightly may be red while #17 is open** (paginate test can hit the 20-min
  timeout). It does not gate PRs; it is the regression net and the bucket-leak
  tripwire (the paginate seed/sweep under `ci/` makes leaked accumulation
  visible). If the red nightly becomes noise before #17 lands, temporarily
  give nightly the same `--skip` and note it in the execution record.
- **Concurrency group:** the existing workflow-level group covers these jobs;
  no change needed (integration runs on the same ref cancel as before).
- Existing `test`/`msrv` jobs keep `--lib --bins`; their comments are updated
  to point at the new integration job instead of "#6 will wire this up".

### 3. AWS one-time setup (manual, outside the repo; documented in the PR)

1. Create the dedicated test bucket (name e.g. `vaultsync-itest-<acct>`,
   region `us-west-1` to match the harness default). Block all public access.
2. Lifecycle rule: prefix `ci/vaultsync-itest-`, expire current versions after
   7 days (backstop for crashed-runner leaks; the in-harness sweeper stays
   primary).
3. IAM policy `vaultsync-itest-ci`: `s3:ListBucket` on the bucket with
   `s3:prefix = ci/*`; `s3:GetObject`/`PutObject`/`DeleteObject` on
   `arn:aws:s3:::<bucket>/ci/*`. Nothing else.
4. IAM role `vaultsync-itest-ci` with the policy attached; trust policy for
   `token.actions.githubusercontent.com`, `aud = sts.amazonaws.com`, `sub`
   conditioned to `repo:tlkahn/vaultsync:*`.
5. GitHub repo variables: `VAULTSYNC_TEST_S3_BUCKET`, `VAULTSYNC_TEST_S3_REGION`,
   `VAULTSYNC_CI_ROLE_ARN`.
6. Record the bucket name, region, role ARN, and lifecycle rule in the PR
   description (and roadmap decision-log row) so the infra is reproducible.

### 4. Docs in the same commit

- `doc/roadmap.md`: decision-log row (sentinel design, OIDC choice, trigger
  split, bucket/lifecycle); Phase 3 item 6 marked done with the `#17` `--skip`
  caveat; item 7's "what remains" tail trimmed accordingly.
- `tests/s3_integration.rs` module doc: the new env var (section 1).

## Method / work steps

TDD applies to the harness change (step 2); the rest is CI/AWS infrastructure
verified by the workflow running on its own PR.

1. **Branch** `ci/issue-6` off main.
2. **RED:** add the `bucket_or_skip` unit tests (three arms) with the helper
   unimplemented/stubbed; confirm they fail to compile/run.
3. **GREEN:** implement `bucket_or_skip` + `require_mode`; rewire
   `with_store` and `path_style_env`; update the vhost skip note and module
   doc. Local gate: `cargo test --lib --bins` unaffected, `cargo test --test
   s3_integration` still skips green locally (require mode off), clippy
   `--all-targets` clean, fmt clean.
4. **Local sentinel smoke:** `VAULTSYNC_TEST_S3_REQUIRE=1 cargo test --test
   s3_integration` (bucket unset) must fail every test with the sentinel
   message; with the bucket var also set to a bogus name it must fail at
   `S3Store::new`/first call, not at the sentinel.
5. **Workflow:** add the two jobs per section 2; adjust the `test`/`msrv`
   comments. Validate locally (`actionlint` if available, else YAML load).
6. **AWS setup** (section 3, with the user's AWS access). Record the values.
7. **PR** with `Closes #6`. Verify on the PR:
   - `integration (S3, PR gate)` green, and its log shows `[ok]` for each of
     the 9 non-paginate tests (proof of real execution, not skips);
   - `test`/`msrv`/`fmt`/`clippy` still green and unchanged in shape.
8. **Sentinel break-test:** push a throwaway commit on the PR (or a scratch
   branch) blanking `VAULTSYNC_TEST_S3_BUCKET`; the integration job must go
   red at the first test. Revert the commit (or close the scratch PR). Record
   the run link in the PR.
9. **Post-merge:** trigger the nightly once via a temporary
   `workflow_dispatch` addition (or wait for the cron); note the outcome in
   the execution record. If red on paginate as expected, decide with the user
   whether to add the temporary nightly `--skip` until #17.

## Acceptance criteria

- [ ] `VAULTSYNC_TEST_S3_REQUIRE=1` turns every bucket-missing skip into a
      test failure; the pure helper is unit-tested (three arms); local
      non-S3 `cargo test` behavior unchanged.
- [ ] `integration` job runs on PRs and main against real S3 (minus
      paginate), with `timeout-minutes: 20` and OIDC auth; log shows `[ok]`
      lines for every run test.
- [ ] `integration-nightly` runs the full suite on cron.
- [ ] Sentinel break-test on the PR demonstrated a red job when the bucket
      variable is blanked.
- [ ] Dedicated bucket + `ci/` prefix + 7-day lifecycle rule exist and are
      recorded in the PR/roadmap; no AWS keys stored as secrets.
- [ ] Roadmap decision-log row + Phase 3 item 6 wording updated in the same
      commit; `test`/`msrv` workflow comments repointed.

## Risks / notes

- **#17 may make the PR gate flaky-slow, not just nightly:** if even the
  non-paginate tests stall on this endpoint (each runs `with_store`'s
  cleanup `list`, which pays the I15 per-object-head cost on its own seeded
  objects - small N, so expected seconds, but unproven on this network), the
  20-min timeout still bounds it; treat any timeout as data for #8, not as a
  reason to weaken the gate.
- **OIDC setup is account-side and manual** (step 6): it needs the user's AWS
  admin access and cannot be PR-gated. The repo-side change is reviewable
  without it; the job goes green only after setup.
- **Fork PRs:** `vars` and OIDC `id-token` behave differently for forked PRs
  (no access to repo variables/role in the base repo's context); fork PRs
  will fail or skip the integration job depending on config. Acceptable for
  now (single-maintainer repo); note in the workflow comment so a future
  fork contributor is not confused.
- **Cost:** each PR run is tens of S3 requests plus one 8 MiB PUT; nightly
  adds the 1050-object paginate churn. Negligible, but the lifecycle rule
  also caps storage cost from leaks.
