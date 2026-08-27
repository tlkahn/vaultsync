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

Exit criteria: `cargo test` green (135 tests); `vaultsync status` against mock store in a temp vault prints a correct plan.

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
| 2026-08-27 | P1r-mtime-none | Phase 1 rule locked: missing mtime treated as `0` in classify. Phase 2 revisit: unknown mtime should not silently lose/win; consider Conflict when either side is missing mtime and sizes differ. Revisit note: pre-epoch mtimes now saturate to `Some(0)` (P1r4-mtime-pre-epoch), so `mtime_ms: None` means only "the FS could not provide an mtime" |
| 2026-08-27 | P1r-both-forces | Both `force_local` and `force_remote` set => forces cancel, Conflict `conflict_mtime_size` (no silent precedence) |
| 2026-08-27 | P1r-list-prefix | `ObjectStore::list` prefix is a raw string `starts_with`, not a path-segment boundary; folder callers pass trailing `/` |
| 2026-08-27 | P1r-stub-exit | Phase 1 `push`/`pull` stubs remain exit 0 always; Phase 2 decides whether real push/pull use exit 2 on conflict/dirty before execute |
| 2026-08-27 | P1r-put-size | Mock `put_from` `size as usize` full-buffer read is mock-only; the real backend must stream without this pattern |
| 2026-08-27 | P1r3-force-mode | On Conflict, forces are mode-aware: `force_local` -> Upload on Push/Status, Skip on Pull (keep local); `force_remote` -> Download on Pull/Status, Skip on Push (keep remote). Both forces still cancel. Amends "forces only affect Conflict rows" with direction semantics. |
| 2026-08-27 | P1r3-put-folder-key | `put_from` rejects keys ending in `/` (`InvalidKey`). Folder marker objects deferred. `ensure_valid_key` still allows trailing `/` for folder *entities*. |
| 2026-08-27 | P1r3-cli-trailing | `version` / `check` / `help` reject unknown trailing tokens (same parse hygiene as flag parsing). |
| 2026-08-27 | P1r3-get-to-lock | Mock `get_to` clones bytes under the lock, drops guard, then writes (no user `Write` while holding the map mutex). |
| 2026-08-27 | P1r4-etag | Mock etag is content-derived (FNV-1a-64 hex): equal across stores for same content, stable on re-put, changes with content. Planner still treats etag as opaque (Phase 1 does not compare etags). |
| 2026-08-27 | P1r4-key-ctl | `ensure_valid_key` also rejects control chars (`char::is_control`) and whitespace-only segments; `...` and space-padded names stay valid. Local emit fails loud on such filenames. |
| 2026-08-27 | P1r4-remote-ingest | `build_plan` validates every remote list key with `ensure_valid_key` (fail closed) before planning; `plan()` stays pure. Extends P1r-key-validation ahead of the Phase 2 executor. |
| 2026-08-27 | P1r4-vault-value | `--vault` rejects empty/flag-like (leading `-`) values; repeated `--vault` is a parse error. Closes the R3/B2 silent `--delete` swallow. |
| 2026-08-27 | P1r4-walk-stat | Walker stats each file once (size + mtime from the same `Metadata`); `NotFound` mid-walk skips the entry, other IO errors stay fatal. |
| 2026-08-27 | P1r4-mtime-pre-epoch | Pre-1970 mtimes saturate to `Some(0)`; `mtime_ms: None` now means only "FS could not provide an mtime". Amends the P1r-mtime-none revisit note. |
| 2026-08-27 | P1r4-folder-contract | `list` synthesizes folder views; folder keys are not objects and must not be passed to `head`/`get_to`/`delete` (locked by `mock_folder_keys_are_not_object_targets` + trait docs). |
| 2026-08-27 | P1r4-folder-mtime | Local folder entities carry real mtimes (`Some`); remote synthesized folders use `None`. Asymmetry intentional; Phase 2 must not compare folder mtimes across sides. |
| 2026-08-27 | P1r4-symlink | All symlinks (files and dirs) are skipped silently in Phase 1; follow/warn policy is a Phase 2 decision. |
| 2026-08-27 | P1r5-backslash-key | `path_to_key` builds keys from `Path` components joined with `/`; no pre-validation `\\` -> `/` rewrite. On Unix, a filename containing `\` fails the walk loud (`InvalidKey`), consistent with `ensure_valid_key` and P1r4-key-ctl. Non-UTF8 components also fail loud (`to_str`, no U+FFFD collapse). |
| 2026-08-27 | P1r5-put-prealloc | Mock `read_exact_n` / `put_from` must not preallocate caller-controlled `size`; use `Read::take` + `read_to_end` + length check (`UnexpectedEof` on short read). Extends/hardens P1r-put-size for the mock itself (real backend still must stream). |
| 2026-08-27 | P1r5-root-dir | `LocalFs::list` requires the vault root to be a directory; file roots error with `Error::Other("vault root is not a directory: ...")` (missing roots stay IO errors). |
| 2026-08-27 | P1r5-mtime-pull | Amends P1r-mtime-none revisit: under the Phase 1 `None -> 0` rule, a remote missing mtime against a present local classifies `local_newer`, so **Pull plans Skip** (local kept). Phase 2 must address pull-direction staleness (and consider `status` visibility for None-mtime pairs), not only overwrite-direction loss. |
| 2026-08-27 | P1r6-argv-utf8 | CLI argv must be valid UTF-8: `run_from_env` reads `args_os()` and rejects non-UTF8 arguments with a clear `error:` message and exit 1 (fail loud, consistent with the walker's non-UTF8 key policy). Phase 2+ may refine: OsString-aware `--vault` value so a non-UTF8 vault *root* becomes reachable. |
| 2026-08-27 | P1r6-mtime-zero | Amends P1r-mtime-none / P1r5-mtime-pull: pre-epoch mtimes saturate to `Some(0)`, indistinguishable from `None -> 0` in the classifier. Equal-size pairs skip on zero evidence; diff-size pairs conflict. Phase 2 etag comparison resolves; classify unchanged in Phase 1. |
| 2026-08-27 | P1r6-windows-keys | `ensure_valid_key` accepts Windows-illegal names (device names `CON`/`NUL`/`aux`/`COM*`, trailing dot/space segments). Accepted as a platform caveat (macOS/Linux/S3 handle them); revisit with platform-aware validation or warnings at Windows-port time. |
| 2026-08-27 | P1r6-root-symlink | A symlinked vault root is followed by design (`fs::metadata` on the root); only entries below the root skip symlinks. Locked by `local_list_follows_symlinked_root`. |

## Open decisions

None. Spike-gated work (D1/D2 final crate pick) lives in Phase 2, not as design blockers.

## Phase 2 checklist (deferred PR 1 review items)

Written down so they are not silently dropped. Do not implement in this fix PR.

- [ ] File-vs-folder path collision: reject/Conflict a `K` file vs a `K/` folder (or children under a file key). P1r-type-collision.
- [ ] Unknown-mtime policy: revisit `mtime None -> 0` when a real backend is present; consider Conflict when either side lacks mtime and sizes differ. P1r-mtime-none. Revisit must cover **pull-direction staleness** (remote `None` + local present classifies `local_newer`; Pull plans Skip and keeps local) and `status` visibility for None-mtime pairs (P1r5-mtime-pull).
- [ ] Real `push`/`pull` exit codes: executor-era `push`/`pull` must return non-zero when the executed plan contained conflicts (sync-model: "non-zero exit if any conflict"); the Phase 1 stub's unconditional 0 is a placeholder locked by `run_push_stub_conflict_exit_0_placeholder`. P1r-stub-exit.
- [ ] Force-flag combination surface: if/reopen how `--force-local --force-remote` is exposed at the CLI. Currently planner cancels both to Conflict. P1r-both-forces.
- [ ] Real backend `put_from` must stream without the mock's `size as usize` full-buffer read. P1r-put-size.
- [ ] **Folder + `--delete` policy (R2.1):** choose (a) post-pass empty-dir cleanup outside the plan, (b) plan `DeleteLocal`/`DeleteRemote` for folders when `opts.delete`, or (c) document permanent orphan empty dirs as a known limitation. Characterization tests lock current Skip behavior until this lands. P1r3-folder-delete.
- [ ] **Remote key ingest validation (R2.2):** validate keys on list/head ingest (or once in `build_plan`) before any local path join. Control chars + ws-only segments are now rejected at `ensure_valid_key` and `build_plan` validates `list` output (P1r4-key-ctl, P1r4-remote-ingest); remaining executor work: validate `head` responses too, and route *all* local path construction through a single `key_to_local_path(vault, key) -> Result<PathBuf>` that validates before joining. Extends P1r-key-validation.
- [ ] **Key identity across filesystems (A2/B4):** decide canonicalization before the real backend lands - NFC-normalize at emit/ingest vs preserve bytes; detect case-only collisions (`Note.md` vs `note.md`) in a plan preflight when the local volume is case-insensitive (Conflict or warn); document v1 key identity as case-sensitive / codepoint-exact.
- [ ] **Symlink policy (P1r4-symlink):** `--follow-symlinks` (off by default) or a warn-side Skip reason for skipped symlinked dirs; Obsidian users symlink attachment folders.
- [ ] **Non-UTF8 local names:** `path_to_key` currently `to_string_lossy()` (U+FFFD collision risk); decide fail-closed vs lossy for exotic volumes.
- [ ] **Folder mtime use:** folder mtimes are asymmetric by design (P1r4-folder-mtime); do not build Phase 2 logic on cross-side folder mtime comparison.
- [ ] **Walker depth (Phase 3 note):** recursion is unbounded; add a depth cap or iterative walk during hardening, before executor-era deep trees (L3; next to the symlink-policy item).
- [ ] **MSRV + CI (Phase 2/3 note):** pin `rust-version` and add a fmt/clippy/test workflow when CI exists.
- [ ] **Executor `put_from` size verification (R3.3):** real backend/executor must **re-stat after read and fail on size/mtime mismatch** - not merely trust the declared size (a file that grew between walk and put would otherwise yield a silently truncated, self-consistent object). Extends P1r-put-size (mock "exactly size bytes" contract stays).
- [ ] **Skip-row output policy (R3 low):** hide `S` rows by default or behind `-v` once vaults are large; Phase 1 fixtures may keep full print.
- [ ] **`--vault` value hygiene (R3 low):** reject empty/flag-like (leading `-`) values (P1r4-vault-value); decide repeated `--vault` policy (error vs last-wins).
- [ ] **`--vault` `-foo` escape hatch (P1r5):** support `--vault=<path>` and/or `--` so a vault literally named `-foo` is reachable. Documented tradeoff of P1r4-vault-value (leading `-` values rejected); clap migration note, not a Phase 1 defect (L4).

Read-only decision-log rows: P1r-mtime-conflict, P1r-status-delete, P1r-key-validation, P1r-list-prefix.