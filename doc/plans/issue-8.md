# Issue 8 plan: retries with backoff on transient S3 errors (SDK RetryConfig + `[transfer.retry]`)

**Status:** planned
**Issue:** https://github.com/tlkahn/vaultsync/issues/8 (OPEN, P3-6a; retries
split out ahead of concurrency, which is follow-up #20)
**Branch:** worktree-retries-with-backoff-on-transient-s3-errors
**Design refs:** [object-store.md](../object-store.md), [cli.md](../cli.md),
[roadmap.md](../roadmap.md) (Phase 3 item 3 + decision log)
**Verified baseline:** `cargo test --offline -- --skip s3_integ_list_paginates`
green at `48902ef` (367 lib + 10 env-gated integration, paginate test skipped
per #17); `cargo clippy --all-targets -- -D warnings` clean;
`cargo fmt --check` clean.

---

## Problem recap (from the issue, verified against the tree)

Transient-error classification already exists at the S3 boundary
(`map_sdk_err`, src/store/s3.rs): 408 -> `Error::Timeout` (W65/A-L1),
429/5xx -> `Error::Unavailable`, mid-body `get_to` connection loss ->
`Error::Unavailable`. But **nothing retries most operations**: list pages,
get, put, and delete are single-attempt. Only list-enrichment heads have the
W117 stopgap (`head_with_retry` in src/store/mod.rs: `HEAD_MAX_ATTEMPTS = 3`,
fixed `[100ms, 300ms]` backoff, no jitter), and it covers heads only.

Post-I15 every list-driven plan issues N sequential per-object heads
(`enrich_with_head_mtimes`), and any transient head error that outlasts the
stopgap fails the **whole** listing fail-closed (I15-errors, W61 ethos,
`pull --delete` safety). One 429/5xx blip on a ~1k-object vault fails the
entire sync run. The stopgap also double-sleeps inside an already-sequential
loop (worst +400ms per struggling key).

Locked fix (issue #8 spec): the aws-sdk-s3 client's `RetryConfig` (standard
mode: exponential backoff with jitter, SDK-classified throttling/5xx/
connection-reset retryables) owns retry for **all** S3 ops; the W117 stopgap
is retired; `[transfer.retry]` exposes the knobs config-only with SDK
standard defaults.

## Locked decisions (made with the user before writing this plan)

| ID | Decision | Choice |
| -- | -------- | ------ |
| I8-layer | Retry layer | **aws-sdk-s3 client `RetryConfig`, standard mode**, configured at client build in `S3Store::new`. No hand-rolled retry wrapper at the `ObjectStore` boundary. Verified against the locked tree (aws-smithy-types 1.6.2): `RetryConfig::standard()` = 3 attempts / 1s initial / 20s max backoff; `with_max_attempts(u32)` / `with_initial_backoff(Duration)` / `with_max_backoff(Duration)` builders; `max_attempts()` / `initial_backoff()` / `max_backoff()` getters; `aws_sdk_s3::config::Builder::retry_config(..)` accepts it; re-exported at `aws_sdk_s3::config::retry::RetryConfig`. |
| I8-stopgap | W117 fate | **Retire it.** `head_with_retry` / `HEAD_MAX_ATTEMPTS` / `HEAD_BACKOFF_MS` removed; `enrich_with_head_mtimes` calls `head()` exactly once per object. Post-exhaustion `Unavailable`/`Timeout` still fails the listing fail-closed (I15-errors unchanged). Rationale: avoid double-retry multiplication (3 outer x N SDK attempts) and two interacting backoff schedules. Roadmap decision-log entry supersedes W117. |
| I8-midbody | Mid-body `get_to` loss | **Documented accepted gap.** The SDK cannot retry a response body stream that is already being consumed, so "connection lost mid-body" (`Error::Unavailable`, src/store/s3.rs) fails the download per-key; the next run converges (sync is idempotent). No executor-level retry in this issue. |
| I8-config | Config surface | **New `[transfer.retry]` TOML section, config-only** (no CLI flag). Fields `max_attempts` / `base_delay_ms` / `max_delay_ms`, all optional; absent section resolves to SDK standard defaults (3 / 1000 / 20000). `max_attempts = 1` is valid and effectively disables retries (matches `RetryConfig::disabled()` semantics). |
| I8-validation | Validation | At `resolve_settings` time, loud `Error::Other` naming the config key (W56 loud-config ethos): `max_attempts >= 1`; `base_delay_ms <= max_delay_ms`. |
| I8-plumbing | Settings flow | New resolved struct `RetrySettings { max_attempts: u32, base_delay_ms: u64, max_delay_ms: u64 }` in src/config.rs (millis at this layer; `Duration` conversion at the s3 boundary). `Settings` gains `pub retry: RetrySettings`. **`S3Store::new(settings: &StoreSettings, retry: &RetrySettings)`** takes it as a second parameter - keeps `[transfer]`-derived policy out of `[store]`-derived `StoreSettings`, so existing `StoreSettings` literals in tests are untouched and only `S3Store::new` call sites change. TOML-side struct is named `RetryConfig` (project convention: `StoreConfig`/`TransferConfig`/`IgnoreConfig`); src/store/s3.rs refers to the SDK type by full path `aws_sdk_s3::config::retry::RetryConfig` to keep the names unambiguous. |
| I8-observability | Per-retry logging | **None.** SDK retries are silent; non-goal for this issue (revisit if users report confusion). |

## Method: strict fine-grained TDD

Same rules of engagement as Phase 1/2 and the issue-15 fix
([phase-1.md](phase-1.md), [phase-2.md](phase-2.md),
[issue-15.md](issue-15.md)):

1. **RED** - named failing test first, exercising a production API; confirm it
   fails for the right reason (missing section/field/behavior, not a typo).
2. **GREEN** - smallest implementation that passes.
3. **REFACTOR** - behavior-preserving cleanup on green.
4. One logical behavior per cycle; per-cycle gate (note the #17 skip flag):
   `cargo test --offline -- --skip s3_integ_list_paginates`
   `cargo clippy --all-targets -- -D warnings`
   `cargo fmt --check`
5. **No network in the default suite.** Everything here is pinnable offline:
   TOML parsing, resolution/validation (pure), the RetrySettings -> SDK
   `RetryConfig` mapping (pure builder + getters), and the retired-stopgap
   fail-closed semantics (mock store). The one network-visible behavior -
   the SDK actually retrying a real 429/5xx - is owned by the SDK's own
   test-suite; we do not add a mock HTTP server (no new dev-dependencies per
   the dependency policy).

## Cycles

### Cycle 0 - pre-flight

- Baseline gate green (recorded above).
- SDK API confirmed against the locked tree (see I8-layer) - no spike needed.

### Cycle 1 - parse `[transfer.retry]` (src/config.rs)

- **RED**:
  - `config_parse_retry_section` - TOML with
    `[transfer.retry] max_attempts = 5, base_delay_ms = 250, max_delay_ms = 4000`
    parses; assert the three `Option` fields land on
    `FileConfig.transfer.retry`.
  - `config_unknown_retry_key_rejected` - a typo key inside `[transfer.retry]`
    (e.g. `max_attemps = 3`) is a parse error (`deny_unknown_fields`,
    matching `config_unknown_transfer_key_rejected`).
  - Extend `config_parse_full_example` (or add a sibling) so the full-example
    TOML carries the new section - keeps the "example stays parseable"
    invariant when cli.md is updated in cycle 6.
- **GREEN**: `#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)] pub struct RetryConfig { max_attempts:
Option<u32>, base_delay_ms: Option<u64>, max_delay_ms: Option<u64> }`;
`TransferConfig` gains `#[serde(default)] pub retry: Option<RetryConfig>`.
- **REFACTOR**: none expected.

### Cycle 2 - resolution + defaults (src/config.rs)

- **RED**:
  - `resolve_settings_retry_defaults_sdk_standard` - no `[transfer.retry]`
    section => `Settings.retry == RetrySettings { max_attempts: 3,
    base_delay_ms: 1000, max_delay_ms: 20000 }` (pinned against
    `DEFAULT_RETRY_*` constants).
  - `resolve_settings_retry_partial_fills_defaults` - only `max_attempts`
    set => delays fall back to defaults (per-field, not all-or-nothing).
  - `resolve_settings_retry_full_override` - all three set => all three
    resolved verbatim.
- **GREEN**: `pub struct RetrySettings { max_attempts: u32, base_delay_ms:
u64, max_delay_ms: u64 }` (`Debug, Clone, Copy, PartialEq, Eq`) with
`Default` = SDK standard; `DEFAULT_RETRY_MAX_ATTEMPTS: u32 = 3`,
`DEFAULT_RETRY_BASE_DELAY_MS: u64 = 1000`, `DEFAULT_RETRY_MAX_DELAY_MS: u64 =
20000`; resolution in `resolve_settings`; `Settings` gains `pub retry:
RetrySettings`. Fix the `Settings` literals in src/cli.rs tests
(compile-driven: add `retry: RetrySettings::default()`).
- **REFACTOR**: none expected.

### Cycle 3 - validation (src/config.rs)

- **RED**:
  - `resolve_settings_retry_rejects_zero_max_attempts` - `max_attempts = 0`
    => `Err`, message names `transfer.retry.max_attempts`.
  - `resolve_settings_retry_allows_max_attempts_1` - `1` resolves fine
    (retries disabled; document in the test name/comment).
  - `resolve_settings_retry_rejects_base_above_max` - `base_delay_ms >
    max_delay_ms` => `Err`, message names both keys.
- **GREEN**: validation inside `resolve_settings` (before constructing
  `Settings`), `Error::Other` with loud key-naming messages.
- **REFACTOR**: none expected.

### Cycle 4 - SDK wiring (src/store/s3.rs + call sites)

- **RED**:
  - `retry_config_from_settings_maps_all_fields` - pure
    `build_retry_config(&RetrySettings) -> aws_sdk_s3::config::retry::RetryConfig`;
    assert `max_attempts()` / `initial_backoff()` / `max_backoff()` reflect
    the input (millis -> `Duration`).
  - `retry_config_from_settings_default_is_sdk_standard` -
    `build_retry_config(&RetrySettings::default())` equals
    `RetryConfig::standard()` on all three getters (pins I8-config's
    "defaults are the SDK's own").
- **GREEN**: implement `build_retry_config`
  (`RetryConfig::standard().with_max_attempts(..).with_initial_backoff(..).with_max_backoff(..)`);
  `S3Store::new` gains the `retry: &RetrySettings` parameter and calls
  `b = b.retry_config(build_retry_config(retry))` in the client builder;
  update call sites: src/cli.rs dispatch passes `&settings.retry`; the
  src/store/s3.rs unit-test constructors and tests/s3_integration.rs call
  sites pass `&RetrySettings::default()`.
- **REFACTOR**: none expected.
- Note: no behavioral offline test can observe the client using the config
  (it is internal to the SDK); the pure-builder pins plus the SDK doc
  contract are the seam. This is deliberate (I8-layer).

### Cycle 5 - retire the W117 stopgap (src/store/mod.rs)

- **RED** (pin the new behavior first, through `enrich_with_head_mtimes`):
  - `enrich_fails_closed_on_first_transient_head_error` (replaces
    `enrich_retries_transient_head_errors`): a store whose first head is
    `Unavailable` and whose second would succeed now fails the listing with
    `Unavailable`, and the attempt counter is exactly 1.
  - `enrich_transient_head_failure_is_single_attempt` (replaces
    `enrich_retry_is_bounded_and_fail_closed`): an always-`Unavailable`
    store fails closed after exactly 1 head call (no sleeps, no loop).
  - `enrich_does_not_retry_nontransient_errors` stays (still true; adjust
    only if its attempt-count assertion assumed the stopgap).
  - NotFound-drop / warning-surface tests stay untouched (behavior
    unchanged).
- **GREEN**: delete `head_with_retry`, `HEAD_MAX_ATTEMPTS`,
  `HEAD_BACKOFF_MS`; `enrich_with_head_mtimes` matches on `store.head(&e.key)`
  directly; update the module-level and fn-level docs (W117 references become
  "superseded by I8 / SDK `RetryConfig`", pointing at the roadmap decision
  log).
- **REFACTOR**: shrink `FlakyHeadStore` to what the new pins still need
  (attempt counting); drop a now-unused `FlakyKind` variant if one becomes
  dead - decide in-cycle, keep the harness minimal.

### Cycle 6 - docs + decision log (not TDD)

- `doc/cli.md`: add `[transfer.retry]` to the config example with the three
  keys commented at their SDK-standard defaults, one line stating these map
  to the AWS SDK standard-mode retry policy (exponential backoff + jitter)
  and that `max_attempts = 1` disables retries.
- `doc/object-store.md`: list-row wording - heads no longer self-retry;
  transient handling for all ops is owned by the SDK standard-mode
  `RetryConfig` configured from `[transfer.retry]`; mid-body `get_to`
  connection loss called out as the accepted gap (I8-midbody).
- `doc/roadmap.md`: decision-log entry **I8-retry-sdk** (supersedes W117:
  SDK `RetryConfig` owns retry/backoff/jitter for all ops; stopgap retired;
  mid-body gap documented; config-only knobs, SDK-standard defaults) and
  update Phase 3 item 3 text: retries landed under #8, concurrency remainder
  is #20.
- `README.md`: the Phase 3 note at line ~81 mentions `[transfer].concurrency`
  only; add `[transfer.retry]` is **not** inert (it is live post-#8) - i.e.
  leave the inert list as-is and add nothing unless the example drifts.
  Re-check during the cycle.

### Cycle 7 - final gate + acceptance sweep

- Full gate: `cargo test --offline -- --skip s3_integ_list_paginates`,
  `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`.
- Env-gated integration suite (10 tests with the paginate skip) must pass
  unchanged - behavior against real S3 is compatible (the SDK's default
  retry config equals our defaults, so a default-config run is a no-op
  change in flight).
- Walk the issue #8 acceptance checklist and tick each box against a cycle.

## Acceptance mapping (issue #8 checklist -> cycles)

| Issue criterion | Cycle |
| --------------- | ----- |
| `[transfer.retry]` parses, validates, wires into the client `RetryConfig`; defaults = SDK standard; `max_attempts = 1` disables retries | 1, 2, 3, 4 |
| W117 stopgap removed; exactly one `head()` per object; post-exhaustion transient errors fail the listing fail-closed | 5 |
| Unit tests green: config parse/validation, fail-closed enrichment, untouched 408/429/5xx mapping tests | 1-5 (gate) |
| Decision log, cli.md, object-store.md updated | 6 |
| fmt/clippy/test + integration gate pass | per-cycle + 7 |

## Non-goals (restated from the issue)

- No new dependencies (SDK retry machinery is already in the locked tree).
- No per-retry logging/observability.
- No retry of mid-body stream failures (accepted gap, I8-midbody).
- No concurrency: request rate is unchanged (still sequential); bounded
  concurrency and its throttling interaction is #20.
- No mock HTTP server to force real 429s (no new dev-deps; SDK retry
  classification is covered by the SDK's own tests).

## Commit strategy

One commit per cycle (`Issue 8: <cycle subject>`), each leaving the gate
green; cycles 1-4 could reasonably squash into one "config + wiring" commit
and cycle 5 stands alone (behavioral retirement) if a shorter history is
preferred - decide at PR time. Branch: `issue-8-retry-config` (or per repo
convention).
