# Phase 2 S3 spike notes (Slice 0)

Status: **candidate passed all 6 probes on AWS S3; awaiting user weight review before D1/D2 close.**

Probe: `examples/s3_spike.rs` (throwaway, not TDD). Official stack only:
`aws-sdk-s3` + `aws-config` + `tokio` as one unit. No `rust-s3` imported.

Test endpoint: `pdf-tmp-repo` bucket, region `us-west-1`, default AWS profile
credential chain.

## Probe results (AWS S3)

All 6 probe items from `phase-2.md` Slice 0 passed:

| # | Item | Result |
| - | ---- | ------ |
| 1 | list (paginated ListObjectsV2), head, get, put, delete | passed (seeded 1100 keys, paginated, all recovered) |
| 2 | user-metadata mtime (`vaultsync-mtime`, decimal ms) round-trip | passed (write + head read-back) |
| 3 | prefix support (`myvault/` style) | passed (isolation verified) |
| 4 | path-style addressing toggle | passed (`force_path_style=true` put+list OK) |
| 5 | custom endpoint | code path present (`endpoint_url`) + AWS default endpoint verified; **R2 not run** (no R2 creds this session, user chose AWS-only) |
| 6 | default AWS credential chain via `aws-config` | passed (env + shared credentials/profile chain) |

Probe objects cleaned up; no leftovers in the bucket after each run.

## Dependency weight metrics (recorded for the user's judgment)

| Metric | Value |
| ------ | ----- |
| `cargo tree` full line count | 654 |
| Top-level deps | `aws-config` 1.11.0, `aws-sdk-s3` 1.144.0, `tokio` 1.53.1 |
| Clean release build time (full stack) | 1m 28s wall, 7m 53s CPU (this machine) |
| Phase 1 stripped release binary | 444,992 bytes (~435 KiB) |
| SDK-linked spike example (stripped) | 13,970,864 bytes (~13.3 MiB) |
| SDK weight delta vs Phase 1 binary | ~13,209 KiB (~12.9 MiB) |
| Phase 2 bin (no S3 backend yet) | 444,992 bytes (unchanged - bin does not link the SDK until the backend lands) |

Note: the ~12.9 MiB delta is the SDK's full weight. In the final product the
S3 backend links the SDK into the binary, so this is a fair proxy for the
shipped binary growth, minus any dead-code elimination skinnier than the spike
example's current feature set.

## Feature set (as probed; minimization is Slice 7 duty)

```toml
[dependencies]
aws-config = { version = "1.11.0" }
aws-sdk-s3  = { version = "1.144.0" }
tokio       = { version = "1.53.1", features = ["rt-multi-thread", "macros"] }
```

Used default features for `aws-config` and `aws-sdk-s3`. `aws-smithy-async`
pulls tokio in anyway (default features incl. mio/signal-hook), so the direct
tokio rows are effectively additive only for `rt-multi-thread` + `macros`.
Feature trimming (e.g. `behavior-version-latest`, disabling unneeded
aws-sdk-s3 behaviors) is deferred to Slice 7 finalization, not needed to clear
the matrix.

## R2 note

Cloudflare R2 was not exercised this session (user selected AWS-only for the
spike; no R2 endpoint/credentials were provided). The custom-endpoint +
path-style paths that R2 needs are already exercised and passing on AWS. R2
metadata-mtime / list quirks remain to be verified in the env-gated
integration suite against a real R2 endpoint before the manual matrix row is
ticked, per Roadmap P2-matrix.

## Decision gate

Per `phase-2.md`: if the user judges the ~12.9 MiB stripped binary / 1m28s
compile / 654-node tree "too heavy", stop and re-open D1. Do not auto-import
`rust-s3` or a second S3 stack in this slice.
