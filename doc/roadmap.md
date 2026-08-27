# Roadmap

## Phase 0 - Design (done)

- [x] Project root under `~/Projects/vaultsync`
- [x] Overall design docs in `doc/`
- [x] Review pass: lock open `TODO(decision)` items
- [x] `cargo init` structure (single package recommended)

## Phase 1 - Skeleton (current)

- [x] Rust package with `vaultsync` binary + library modules: `entity`, `plan`, `local`, `store` (trait + mock)
- [x] Planner unit tests with fixture trees (no network)
- [x] CLI stubs: `status`, `push`, `pull`, `check`, `version` printing help/plans against mock

Exit criteria: `cargo test` green (67 tests); `vaultsync status` against mock store in a temp vault prints a correct plan.

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
| 2026-08-27 | P1 planner | `plan()` always reports full classification; mode + `opts.delete` select execution semantics |
| 2026-08-27 | P1 mode mapping | Per-delta action matrix locked (Status/Push/Pull table in [phase-1.md](plans/phase-1.md)); forces only affect Conflict rows |
| 2026-08-27 | P1 folders | Folder-only actions are always Skip `folder` (empty folders do not round-trip to S3) |
| 2026-08-27 | P1 mock delete-missing | `delete` on a missing key returns `Error::NotFound` (not idempotent) |
| 2026-08-27 | P1 `push`/`pull` | Dry-run stubs print the plan and mutate nothing; `status` uses empty in-memory mock by default |
| 2026-08-27 | P1r-mtime-conflict | Conflict when mtimes within tolerance and size differs; beyond tolerance, newer side wins even if size differs (PR 1 review fix) |
| 2026-08-27 | P1r-status-delete | `status` rejects `--delete`; the Status action matrix never emits Delete* rows |
| 2026-08-27 | P1r-key-validation | `ensure_valid_key` enforced on local walk emit + mock `put_from`; rejects `.` / `..` / empty path segments |
| 2026-08-27 | P1r-type-collision | **Deferred to Phase 2:** detect key collisions between a `K` file and a `K/` folder (or children under a file key); emit Conflict `type_mismatch`/`path_collision`. Not implemented in the PR 1 fix. |
| 2026-08-27 | P1r-mtime-none | Phase 1 rule locked: missing mtime treated as `0` in classify. Phase 2 revisit: unknown mtime should not silently lose/win; consider Conflict when either side is missing mtime and sizes differ |
| 2026-08-27 | P1r-both-forces | Both `force_local` and `force_remote` set => forces cancel, Conflict `conflict_mtime_size` (no silent precedence) |
| 2026-08-27 | P1r-list-prefix | `ObjectStore::list` prefix is a raw string `starts_with`, not a path-segment boundary; folder callers pass trailing `/` |
| 2026-08-27 | P1r-stub-exit | Phase 1 `push`/`pull` stubs remain exit 0 always; Phase 2 decides whether real push/pull use exit 2 on conflict/dirty before execute |
| 2026-08-27 | P1r-put-size | Mock `put_from` `size as usize` full-buffer read is mock-only; the real backend must stream without this pattern |

## Open decisions

None. Spike-gated work (D1/D2 final crate pick) lives in Phase 2, not as design blockers.

## Phase 2 checklist (deferred PR 1 review items)

Written down so they are not silently dropped. Do not implement in this fix PR.

- [ ] File-vs-folder path collision: reject/Conflict a `K` file vs a `K/` folder (or children under a file key). P1r-type-collision.
- [ ] Unknown-mtime policy: revisit `mtime None -> 0` when a real backend is present; consider Conflict when either side lacks mtime and sizes differ. P1r-mtime-none.
- [ ] Real `push`/`pull` exit codes: decide whether execute (not stub) uses exit 2 on conflict/dirty before acting. P1r-stub-exit.
- [ ] Force-flag combination surface: if/reopen how `--force-local --force-remote` is exposed at the CLI. Currently planner cancels both to Conflict. P1r-both-forces.
- [ ] Real backend `put_from` must stream without the mock's `size as usize` full-buffer read. P1r-put-size.

Read-only decision-log rows: P1r-mtime-conflict, P1r-status-delete, P1r-key-validation, P1r-list-prefix.