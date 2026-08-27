# Roadmap

## Phase 0 - Design (done)

- [x] Project root under `~/Projects/vaultsync`
- [x] Overall design docs in `doc/`
- [x] Review pass: lock open `TODO(decision)` items
- [x] `cargo init` structure (single package recommended)

## Phase 1 - Skeleton (current)

1. Rust package with `vaultsync` binary + library modules: `entity`, `plan`, `local`, `store` (trait + mock)
2. Planner unit tests with fixture trees (no network)
3. CLI stubs: `status`, `push`, `pull`, `check`, `version` printing help/plans against mock

Exit criteria: `cargo test` green; `vaultsync status` against mock store in a temp vault.

## Phase 2 - Real local FS + S3

1. Local walker/reader/writer
2. S3 backend spike (list/get/put/delete, metadata mtime, prefix, path-style, custom endpoint)
3. Config TOML + AWS credential chain
4. `check` against a real bucket
5. Manual test matrix: AWS + one S3-compatible endpoint

Exit criteria: push/pull a sample vault including nested folders and a binary attachment.

## Phase 3 - Hardening

1. `--delete` safety (`--yes`, `--max-delete`, confirm prompt)
2. Ignore patterns + Obsidian default profile
3. Concurrency limits, retries with backoff on transient S3 errors
4. Streaming for large objects if not already done
5. Lock file to prevent concurrent runs on same vault
6. JSON schema stability for `--json`
7. Integration test optional gate in CI

Exit criteria: daily-driver usable for single-user backup (`push --delete` from trusted machine).

## Phase 4 - Polish (still v1.0)

1. Man page or `doc/user-guide.md`
2. Install path (cargo install / homebrew formula later)
3. Crash/partial-run report quality
4. Performance notes on vaults with 10k+ files (list cost, parallel put)

## Explicitly later (post-v1)

| Item | Notes |
| ---- | ----- |
| Bidirectional `sync` command | Only as documented composition + policy |
| Local prev-sync history (v3-like) | Enables deletion propagation without full mirror |
| Encryption wrapper | AGE or rclone-compatible; separate layer |
| Azure Blob / GCS backends | Same `ObjectStore` trait |
| Obsidian community plugin | Thin: shell out or embed; no logic fork |
| Watch mode / launchd helper | Wrapper around CLI |
| Multipart upload tuning | When large video/PDF users appear |
| Checksum mode (`--checksum`) | Content equality beyond mtime/size |
| Conflict copies | `file.conflict.<ts>.md` optional policy |
| Trash-based local deletes | Optional safer local delete behind `--delete` |

## Decision log

Record choices here as they are made.

| Date | Decision | Choice |
| ---- | -------- | ------ |
| 2026-08-27 | Project name | `vaultsync` |
| 2026-08-27 | Product form | CLI first (library + CLI); plugin later |
| 2026-08-27 | Language | Rust |
| 2026-08-27 | v1 scope | Minimal list/compare/push/pull/delete; no crypt; no smart merge |
| 2026-08-27 | Package layout | Single crate (`lib` + `bin`) until second backend/frontend forces split |
| 2026-08-27 | Sync verbs | `push` / `pull` / `status`; no bidirectional `sync` in v1 |
| 2026-08-27 | Deletion model | Explicit `--delete` mirror only; no prev-sync DB in v1 |
| 2026-08-27 | D1 S3 client crate | Deferred to Phase 2 spike (AWS + MinIO; creds; path-style; custom endpoint). Prefer lightest option that clears the matrix. |
| 2026-08-27 | D2 Async runtime | Follow D1: tokio if `aws-sdk-s3` wins; keep async inside the S3 backend |
| 2026-08-27 | D3 Default `.obsidian/` policy | sync-with-excludes (settings yes; workspace session files no); default Obsidian profile |
| 2026-08-27 | D4 Local delete safety | v1 permanent unlink/remove behind `--delete`; trash optional post-v1 |
| 2026-08-27 | D5 Streaming in trait day one | yes (streaming `get_to` / `put_from`; buffered helpers allowed) |
| 2026-08-27 | D6 License | Dual MIT OR Apache-2.0 |

## Open decisions

None. Spike-gated work (D1/D2 final crate pick) lives in Phase 2, not as design blockers.
