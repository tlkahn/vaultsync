# Roadmap

## Phase 0 - Design (done)

- [x] Project root under `~/Projects/vaultsync`
- [x] Overall design docs in `doc/`
- [x] Review pass: lock open `TODO(decision)` items
- [x] `cargo init` structure (single package recommended)

## Phase 1 - Skeleton (done)

- [x] Rust package with `vaultsync` binary + library modules: `entity`, `plan`, `local`, `store` (trait + mock)
- [x] Planner unit tests with fixture trees (no network)
- [x] CLI stubs: `status`, `push`, `pull`, `check`, `version` printing help/plans against mock

Exit criteria: `cargo test` green (138 tests); `vaultsync status` against mock store in a temp vault prints a correct plan.

## Phase 2 - Real local FS + S3

1. [x] Local walker/reader/writer (Slices 3)
2. [x] S3 backend spike (Slice 0, D1 closed: `aws-sdk-s3` + `aws-config` + `tokio`; list/get/put/delete, metadata mtime, prefix, path-style, custom endpoint)
3. [x] Config TOML + AWS credential chain (Slice 2, 7, 8)
4. [x] `check` against a real bucket (Slice 8)
5. [x] Manual test matrix: AWS done; **R2 row pending** (no R2 endpoint/creds this session) - see [test-matrix.md](test-matrix.md)

Exit criteria: push/pull a sample vault including nested folders and a binary attachment is verified on **AWS S3** (byte-identical + exact mtimes); **Cloudflare R2** row remains open (P2-matrix).

## Phase 3 - Hardening

1. `--delete` safety (`--yes`, `--max-delete`, confirm prompt)
2. Ignore patterns + Obsidian default profile
3. Concurrency limits, retries with backoff on transient S3 errors
4. Multipart upload above the 5 GiB single-PUT ceiling (get/put already
   stream: `get_to` streams the body; `put_from` buffers to a disk temp and
   streams it - never a size-sized memory buffer; the ceiling is rejected
   client-side before buffering, W80)
5. Lock file to prevent concurrent runs on same vault
6. JSON schema stability for `--json`
7. Integration test optional gate in CI
8. Head-on-list (or HEAD-sample on size-equal candidates) so list-driven plans
   see client mtimes instead of upload `LastModified` (PR2 A-M1 follow-up)
9. CI: pin the toolchain and verify MSRV 1.85 (A-L9); set
   `VAULTSYNC_TEST_S3_BUCKET` in CI so the env-gated suite genuinely runs,
   and consider `#[ignore]` + `--ignored` or a CI sentinel so a silent skip
   cannot look green (B-L7 hardening)
10. Consider anchoring a relative `vault_root` to the config file's directory
    instead of the cwd (PR2 B-L10) - breaking-change review
11. Cloudflare R2 endpoint matrix row (still pending; the path-style toggle
    test exercises both flavors there, PR2 A-M8/W12)

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
| 2026-08-27 | P1r7-special-node-key | Walker validates keys only for emittable nodes (`is_dir`/`is_file`); special files (FIFO/socket/device) are skipped unconditionally, name never inspected. Files/dirs with invalid names still fail loud (P1r4-key-ctl). Locked by `local_list_skips_special_file_with_backslash_name`. |
| 2026-08-27 | P1r7-delete-repeat | Repeated `--delete` is a parse error (fail loud), matching repeated `--vault` (P1r4-vault-value). Locked by `parse_repeated_delete_flag_errors`. |
| 2026-08-27 | P1r7-parse-usage | Every `parse_args` error message ends with the USAGE block (uniform rule). Locked by `parse_errors_always_include_usage`. |
| 2026-08-28 | D1 S3 client stack | Official `aws-sdk-s3` + `aws-config` + `tokio` accepted after the Slice 0 spike cleared the AWS matrix: all 6 probes passed (list/head/get/put/delete, metadata mtime, prefix, path-style, custom-endpoint path, default cred chain). Weight: ~12.9 MiB stripped over the 435 KiB Phase 1 binary, 1m28s clean release build, 654-tree nodes. R2 row still unverified (no endpoint this session; lands in the integration suite). Notes: `doc/spikes/phase-2-s3.md`. `rust-s3` remains docs-only. |
| 2026-08-28 | D2 async runtime | tokio; async lives only inside `store::s3` (`S3Store` owns a private runtime and `block_on`s per call). Planner/executor/CLI stay sync and runtime-free. No `async` outside `store::s3` without a new log entry. |
| 2026-08-28 | P2-scope | Phase 2 = Roadmap 5 items plus the full deferred Phase 2 checklist; each item lands or is explicitly re-deferred with a log entry. |
| 2026-08-28 | P2-matrix | Manual test matrix = AWS S3 + Cloudflare R2 (S3-compatible row). |
| 2026-08-28 | P2-integ | Env-gated automated integration tests (`tests/`, off by default) plus a manual matrix checklist doc. |
| 2026-08-28 | P2-toml | Config parsing via `toml` + `serde` (derive). |
| 2026-08-28 | P2-cli | Migrate CLI parse to `clap` (global flags, `--flag=value`, `--` terminator). |
| 2026-08-28 | 4a path collision | A file key `K` coexisting with a `K/` folder key or `K/...` child is a Conflict `path_collision` in every mode; never force-resolvable; the executor never touches these rows. |
| 2026-08-28 | 4b unknown mtime | When both sides exist and either mtime is `None`: sizes equal -> Skip `equal_unknown_mtime` (visible row, zero overwrite risk); sizes differ -> Conflict `conflict_mtime_unknown` (forces apply per the mode-aware table). Retires the Phase 1 `None -> 0` rule. Pre-epoch `Some(0)` stays comparable, not aliased to `None`. Retired characterization tests flipped in the same commit. |
| 2026-08-28 | 4c key identity | v1 key identity is case-sensitive, codepoint-exact, no NFC normalization (bytes preserved). `build_plan` preflights case-only collisions (same-side and cross-side) to Conflict `case_collision`; Collisions are never auto-paired as Equal, and (Slice 5) never executed. |
| 2026-08-28 | 4d etag policy | Phase 2 does not compare etags and never hashes local files. Real-S3 etags are MD5 only for single-part uploads and provider-dependent (R2), so no cross-store etag equality is portable. `Entity::etag` stays an opaque remote token; `plan()` ignores etag fields. `--checksum` content comparison remains post-v1. The 4b policy is the sole guard for zero-evidence pairs. |
| 2026-08-28 | R2.1 folder + `--delete` | Option (a): the executor runs transfers first, destination deletes last, then a bottom-up `remove_empty_dirs_bottom_up` post-pass cleans now-empty local dirs outside the plan (remote has no folder objects, so nothing to do there). Folder actions stay Skip; no folder delete rows. Characterization Skip tests remain. |
| 2026-08-28 | exit-code lock (P1r-stub-exit) | `status` 0 clean / 2 dirty / 1 error. `push`/`pull` execute the plan: 0 if all selected actions succeeded and no Conflict rows; 2 if the plan contained any Conflict rows (non-conflict actions still execute); 1 on any transfer failure or fatal error. `--dry-run` prints the plan, mutates nothing, exits like status. Retires `run_push_stub_conflict_exit_0_placeholder` in the same commit. Help carries a permanent-`--delete`-no-confirmation warning until Phase 3. |
| 2026-08-28 | check probe lock | `check` writes a tiny probe object (`<prefix>.vaultsync-check-<pid>`), reads it back, deletes it; success only on put+get+delete round-trip. No head-bucket-only fallback - probe failure is a failure. 404 -> not found, 401/403 -> actionable credentials hint, exit 1. Credentials come from the ambient AWS chain (Slice 8). |
| 2026-08-28 | symlink policy (P1r4-symlink) | Default remains skip all symlinks below the root (symlinked root still followed, P1r6-root-symlink). `--follow-symlinks` (off by default, global flag): the walker follows symlinks, guards dir cycles with a canonical-path visited set, and still skips (with a warning) any target escaping the canonicalized vault root - never syncs out-of-vault content silently. Off-by-default skipped symlinks surface as a walk warning count on stderr (`skipped N symlink(s); use --follow-symlinks`), not plan Skip rows. |
| 2026-08-28 | PR2-defer-head-on-list | Deferred (PR2 A-M1): a head-on-list (or HEAD-sample on size-equal candidates) so list-driven plans see client mtimes is a Phase 3 optimization; today the plan compares upload `LastModified` (documented in README). |
| 2026-08-28 | PR2-defer-path-collision-perf | Landed by W52 (PR2 A-M6/B-L8): `path_collision_keys` is the sorted-window + `partition_point` rewrite - O(n log n) on the sorted, dedup'd key list, with naive-equivalence and scale tests. A benchmark before advertising large-vault use remains a perf gate (Phase 4 item 4). |
| 2026-08-28 | PR2-defer-ci | Deferred (PR2 A-L9/B-L7): CI is Phase 3 - pin the toolchain + verify MSRV 1.85, set `VAULTSYNC_TEST_S3_BUCKET` so the env-gated suite runs for real, and use `#[ignore]`/`--ignored` or a CI sentinel so a silent skip cannot look green. |
| 2026-08-28 | PR2-defer-vault-root-baseline | Deferred (PR2 B-L10): anchoring a relative `vault_root` to the config file's directory is a breaking-change Phase 3 review; today it resolves against the cwd (documented in cli.md). |
| 2026-08-28 | PR2-defer-r2-row | Deferred (PR2 W12/A-M8 matrix): the Cloudflare R2 endpoint row is still pending; the path-style toggle test now exercises both addressing flavors there. |
| 2026-08-28 | PR2-W76 guarded-delete-seam | `delete_file_guarded` routes its unlink through the shared `unlink_local_file` seam (pre-unlink locality re-check + `NotFound` mapping); a `#[cfg(test)]` pre-unlink hook in the seam lets tests inject the stat-to-unlink race through production APIs (r8b M1 / r8a-5, the merge-bar item). |
| 2026-08-28 | PR2-W77 scoped-empty-dir-pass | The `pull --delete` empty-dir post-pass is scoped to the ancestor chains of the files deleted this run (both the Ok and the W32 NotFound goal-state arms), deepest-first, never the root, dedup'd across chains; pre-existing plan-unrelated empty dirs are kept. Replaces the vault-wide pass (r9 M1, the merge-bar item). |
| 2026-08-28 | PR2-W78 tmp-path-self-cleanup | `tmp_path_for`'s post-creation tail (create_dir_all, second ensure_locality, alloc_temp_sibling) runs under a cleanup-on-error helper, so no failure can leak created dirs (r9 L2). `remove_created_dirs` is shared with exec's W66 download cleanup. |
| 2026-08-28 | PR2-W79 reserved-keys-warn | Reserved-namespace remote keys dropped by `build_plan` are counted and surfaced on stderr (first 5 names + "and N more"), via a pure `partition_reserved_remote_keys` helper (r9 L1). |
| 2026-08-28 | PR2-W80 single-put-ceiling | `S3Store::put_from` rejects a size above the 5 GiB single-PUT ceiling client-side, before buffering or upload (r8b L3). |
| 2026-08-28 | PR2-W81 root-canon-cache | The vault root is canonicalized once per `LocalFs` (OnceLock) and threaded through `ensure_locality`, the scoped dir cleanup, and the walk; a mid-run root-symlink swap yields one consistent boundary decision per instance (r8a-1 / r9-N2). |
| 2026-08-28 | PR2-W82 report-mutex | `RefCell<WalkReport>` -> `Mutex<WalkReport>` so `LocalFs` is Send/Sync ahead of Phase 3 concurrency (r8a-2). |
| 2026-08-28 | PR2-W83 single-vault-merge-site | The `Cli.vault` merge arm is removed from `resolve_settings` (test-only in production); `resolve_vault_from_config` is the single `--vault`/config merge site; precedence tests retargeted (r9 N1). |

## Open decisions

None. Spike-gated work (D1/D2 final crate pick) lives in Phase 2, not as design blockers.

## Phase 2 checklist (deferred PR 1 review items)

Written down so they are not silently dropped. Do not implement in this fix PR.

- [[x]]  File-vs-folder path collision: reject/Conflict a `K` file vs a `K/` folder (or children under a file key). P1r-type-collision. LANDED (Slice 4a): Conflict `path_collision`, never force-resolvable, never executed.
- [[x]]  Unknown-mtime policy: revisit `mtime None -> 0` when a real backend is present; consider Conflict when either side lacks mtime and sizes differ. P1r-mtime-none. Revisit must cover **pull-direction staleness** (remote `None` + local present classifies `local_newer`; Pull plans Skip and keeps local) and `status` visibility for None-mtime pairs (P1r5-mtime-pull). LANDED (Slice 4b): either-mtime-None -> size-equal Skip `equal_unknown_mtime` / size-diff Conflict `conflict_mtime_unknown`; `None -> 0` retired; pull-direction hole closed.
- [[x]]  **Etag-aware equality (P1r7):** within-tolerance same-size pairs currently classify `Equal` with zero content evidence; Phase 2 must decide the local-hash policy (`--checksum` / size-gated hashing / never) before any etag short-circuit, since local entities never carry etags today. Mock etags are content-derived (P1r4-etag), so the comparison is testable the moment a policy exists. Complements P1r6-mtime-zero. LANDED (Slice 4d): policy = never hash local files / never compare etags (MD5-only + provider-dependent); plan() ignores etag; `--checksum` stays post-v1.
- [[x]]  Real `push`/`pull` exit codes: executor-era `push`/`pull` must return non-zero when the executed plan contained conflicts (sync-model: "non-zero exit if any conflict"); the Phase 1 stub's unconditional 0 is a placeholder locked by `run_push_stub_conflict_exit_0_placeholder`. P1r-stub-exit. LANDED (Slice 6): 0/2/1; placeholder test retired.
- [[x]]  Force-flag combination surface: if/reopen how `--force-local --force-remote` is exposed at the CLI. Currently planner cancels both to Conflict. P1r-both-forces. LANDED (Slice 1): `--force-local --force-remote` parse; planner cancels to Conflict.
- [[x]]  Real backend `put_from` must stream without the mock's `size as usize` full-buffer read. P1r-put-size. LANDED (Slice 7): temp-file + `ByteStream::from_path` streaming; no size-sized in-memory buffer (s3_integ_streaming_put_large).
- [[x]]  **Folder + `--delete` policy (R2.1):** choose (a) post-pass empty-dir cleanup outside the plan, (b) plan `DeleteLocal`/`DeleteRemote` for folders when `opts.delete`, or (c) document permanent orphan empty dirs as a known limitation. Characterization tests lock current Skip behavior until this lands. P1r3-folder-delete. LANDED (Slice 5, option a): transfers first, deletes last, bottom-up empty-dir post-pass.
- [[x]]  **Remote key ingest validation (R2.2):** validate keys on list/head ingest (or once in `build_plan`) before any local path join. Control chars + ws-only segments are now rejected at `ensure_valid_key` and `build_plan` validates `list` output (P1r4-key-ctl, P1r4-remote-ingest); remaining executor work: validate `head` responses too, and route *all* local path construction through a single `key_to_local_path(vault, key) -> Result<PathBuf>` that validates before joining. Extends P1r-key-validation. LANDED (Slice 3+7): `key_to_local_path` single join site; head/list ingest validates keys; S3 validates before any outbound call.
- [[x]]  **Key identity across filesystems (A2/B4):** decide canonicalization before the real backend lands - NFC-normalize at emit/ingest vs preserve bytes; detect case-only collisions (`Note.md` vs `note.md`) in a plan preflight when the local volume is case-insensitive (Conflict or warn); document v1 key identity as case-sensitive / codepoint-exact. LANDED (Slice 4c): v1 key identity case-sensitive, codepoint-exact, no NFC normalization; case-only collisions -> Conflict `case_collision`.
- [[x]]  **Symlink policy (P1r4-symlink):** `--follow-symlinks` (off by default) or a warn-side Skip reason for skipped symlinked dirs; Obsidian users symlink attachment folders. LANDED (Slice 9): `--follow-symlinks` off by default + skipped-symlink count warning; follow guards loops and skips escaping targets with a warning.
- [[x]]  **Symlink-swap TOCTOU (P1r7):** walker and executor must defend against entry swaps between `file_type` (no-follow) and open - re-verify type at open time (or open with no-follow semantics); on download, resolve the real path and confirm it stays under the canonicalized vault root before writing. Pairs with R2.2 (`key_to_local_path` single join site). LANDED (Slice 3): open_verified no-follow type recheck + opened-fd size/mtime recheck; download locality (canonical root) guard.
- [[x]]  **Folder mtime use:** folder mtimes are asymmetric by design (P1r4-folder-mtime); do not build Phase 2 logic on cross-side folder mtime comparison. Constraint honored: folder mtimes not compared cross-side (4b tests use file entities only).
- [[ ]]  **Walker depth (Phase 3 note):** recursion is unbounded; add a depth cap or iterative walk during hardening, before executor-era deep trees (L3; next to the symlink-policy item). RE-DEFERRED to Phase 3: recursion still unbounded (roadmap Phase 3 hardening item).
- [[x]]  **MSRV + CI (Phase 2/3 note):** pin `rust-version` and add a fmt/clippy/test workflow when CI exists. PARTIAL: `rust-version = "1.85"` pinned (Slice 10); CI fmt/clippy/test workflow remains Phase 3.
- [[x]]  **Executor `put_from` size verification (R3.3):** real backend/executor must **re-stat after read and fail on size/mtime mismatch** - not merely trust the declared size (a file that grew between walk and put would otherwise yield a silently truncated, self-consistent object). Extends P1r-put-size (mock "exactly size bytes" contract stays). LANDED (Slice 3+5): open_verified re-stats opened fd (size + mtime) before put.
- [[x]]  **Skip-row output policy (R3 low):** hide `S` rows by default or behind `-v` once vaults are large; Phase 1 fixtures may keep full print. LANDED (Slice 10): formatter hides S rows by default; -v shows them.
- [x] **`--vault` value hygiene (R3 low):** landed - empty/flag-like (leading `-`) values rejected and repeated `--vault` is a parse error (P1r4-vault-value).
- [[x]]  **`--vault` `-foo` escape hatch (P1r5):** support `--vault=<path>` and/or `--` so a vault literally named `-foo` is reachable. Documented tradeoff of P1r4-vault-value (leading `-` values rejected); clap migration note, not a Phase 1 defect (L4). LANDED (Slice 1): `--vault=-foo` via clap equals form.
