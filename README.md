# vaultsync

Minimal, Unix-style sync of an Obsidian vault (or any plain directory) to object storage.

**v1 target:** Amazon S3 only. CLI first. Rust.

License: [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE)

This repository is past Phase 0 design. Start here:

- [doc/README.md](doc/README.md) - documentation index
- [doc/vision.md](doc/vision.md) - goals, non-goals, principles
- [doc/architecture.md](doc/architecture.md) - crate layout and layers
- [doc/sync-model.md](doc/sync-model.md) - push / pull / status semantics
- [doc/object-store.md](doc/object-store.md) - storage trait and S3 notes
- [doc/cli.md](doc/cli.md) - command surface
- [doc/borrowed-from-remotely-save.md](doc/borrowed-from-remotely-save.md) - what we take and reject
- [doc/roadmap.md](doc/roadmap.md) - phased plan

## One-line pitch

`vaultsync` is to vault-on-S3 what `rsync` is to two directories: small tools, explicit direction, no magic.

## Status

Phase 2 complete: real local FS + real S3 backend (`aws-sdk-s3` + `aws-config` +
`tokio`, D1 closed), TOML config, real `push`/`pull`/`check`, planner
collision/mtime/case policies, `--follow-symlinks`, and an env-gated S3
integration suite. Verified on AWS S3 (byte-identical + exact mtimes); the
Cloudflare R2 matrix row is pending (no R2 endpoint this session). `cargo test`
green offline (no network in the default suite). Phase 3 (the delete
confirmation rail - `--yes`/`--max-delete`/prompt - and CI) is next;
ignore patterns are **live** (issue #34, epic #9 closed: default Obsidian
profile, `profile = "none"` escape hatch, user patterns extend, applied on
both sides). Transfer concurrency already landed under issue 20
(`[transfer].concurrency`, config-only, default 4). The inventory manifest
(issue #45) landed: warm plan build from a rebuildable remote manifest with
an optional local cache, conditional commit after push, and `vaultsync
repair` for bootstrap/repair. The Phase 2 delete-
safety surface is already
landed (freshness guards, parent locality, head-before-delete,
NotFound-as-success).

```text
cargo build
cargo run -- version
cargo run -- status --config <cfg>      # human plan against the real store; 0/2/1
cargo run -- push --config <cfg>        # upload local-newer; real mutation
cargo run -- pull --config <cfg>        # download remote-newer
cargo run -- check --config <cfg>       # connectivity probe (put/get/delete)
```

With no `[store].bucket` after config resolution (including an auto-discovered
`./.vaultsync.toml`), `status` runs against an in-memory mock store (useful for
status/dry-run against a local dir); `push`/`pull`/`check` refuse loudly until
a `[store]` section with a `bucket` is configured (they must never silently
action a plan - or delete files - against a non-existent store).

## Known behaviors

- **List-driven plans compare client `vaultsync-mtime` (issue #15 fixed).**
  A plan is built from `list`, which enriches each object via a per-object
  `HeadObject` (reading the client `vaultsync-mtime` metadata), so plans
  compare the client mtime, not the upload `LastModified`. After a `push`, a
  later `pull`/`status` sees the uploaded file as in-sync (no pessimistic
  one-time re-download). Accepted cost: N+1 requests per list-driven plan
  (1+ ListObjectsV2 pages + N sequential heads, roughly N x RTT) until Phase
  3's request pool; transient throttles / 5xx on any op are retried with
  exponential backoff + jitter by the SDK standard-mode `[transfer.retry]`
  config (I8).
- **Fast warm plans via a remote inventory manifest (issue #45).** After a
  `push` (or `vaultsync repair`), plan build reads the rebuildable JSON
  manifest `.vaultsync/manifest/v1.json` instead of listing + per-object
  heads - steady-state `status`/`push`/`pull` cost one GET instead of N+1
  requests. `[inventory].mode` selects the behavior: `auto` (default: valid
  manifest wins, else cold list+head with a warning), `manifest` (fail
  closed if missing/corrupt, suggesting `repair`), or `list_head` (never
  read the manifest). The manifest is a snapshot, not a deletion journal: it
  is written after successful remote mutations (bodies first, manifest last,
  conditional If-Match/If-None-Match put), by `repair`, and - since issue
  #48 - by a **push-time inventory bootstrap**: a cold `push` under
  `[inventory].bootstrap = "push-ensure"` (default) publishes a baseline
  manifest BEFORE transfers, or adopts a concurrent-valid one, so later plans
  are warm even when this run's transfers fail or nothing is planned
  (validate-before-overwrite H1-V - never a silent clobber). `pull` and
  default `status` never write it; `status --write-manifest` is an explicit
  opt-in; the cold path stays I15-correct and fail-closed. A lost
  conditional race warns (`manifest not committed...` / bootstrap abort per
  F2) and exits 0 when the transfers succeeded. The local cache mirror lives
  at `<vault_root>/.vaultsync/cache/` (never walked or uploaded; 304
  conditional GET on repeat loads; never authority).
- **Reserved-namespace leftovers are filtered before any head** (W118):
  `.vaultsync-check-*` / `.*.vaultsync-tmp-*` keys are partitioned out of a
  listing before a `HeadObject` is issued - no wasted requests and no
  fail-closed scope creep over junk keys.
- **A listed key deleted before its head is dropped with a warning** (W116);
  any other head error fails the listing (fail-closed, W61 ethos), so a plan
  is never built against a knowingly-degraded remote view.
- **Mock store is `status` only.** Without a `[store]` section, `push`/
  `pull`/`check` refuse (exit 1); only `status` runs against the in-memory
  mock.
- **`.*.vaultsync-tmp-*` names are reserved.** Download/upload temp buffers are
  written as `.name.vaultsync-tmp-<pid>-<n>` siblings and cleaned up on every
  error path; the walker additionally skips any file matching this reserved
  pattern so a crash leftover can never be pushed as a real key.
- **`pull --delete` removes only the dirs its deletes emptied.** The empty-dir
  post-pass is scoped to the ancestor chains of the files deleted this run
  (deepest-first, never the vault root): a pre-existing, plan-unrelated empty
  dir (e.g. an intentional `attachments/`) is kept.
- **`[ignore]` is live end-to-end (issue #34, epic #9 closed).** The default
  Obsidian profile applies with no config: the Obsidian six
  (`.git/`, `.trash/`, `.DS_Store`, `.obsidian/workspace`,
  `.obsidian/workspace.json`, `.obsidian/workspace-mobile.json`) are pruned
  from local walks **and** dropped from remote listings, so a `--delete` run
  never removes an ignored path/key. `profile = "none"` disables the
  built-ins; user `patterns` **extend** the active profile (union). Reserved
  vaultsync names (`.*.vaultsync-tmp-*`, `.vaultsync-check-*`) are always
  skipped regardless of profile. Skipped paths/keys are reported on stderr as
  count-only warnings (`ignored N local path(s) / remote key(s) by ignore
  patterns`). Upgrade note: keys already uploaded under these patterns before
  the defaults activated stay on the remote - they are ignored on both sides,
  so `push --delete` will not remove them (delete invariant); remove them
  manually in the store, or temporarily set `profile = "none"` for a run
  (built-ins are disabled for that run). Config-only: there is no
  `--profile` / `--exclude` / `--include` CLI flag.
- **`[transfer].concurrency` is live (issue 20).** It bounds how many
  transfers and list-enrichment heads run in flight (default 4, valid range
  `1..=256`; `1` = sequential; the `256` cap is I20-r1 config-layer - library
  callers are uncapped, the pool clamps to `min(concurrency, items)`).
  Config-only - there is no `--concurrency` flag.
- **The `[transfer.retry]` table under `[transfer]` is live (not Phase 3).** The three knobs
  (`max_attempts` / `base_delay_ms` / `max_delay_ms`) map to the AWS SDK
  standard-mode retry policy on the S3 client; `max_attempts = 1` disables
  retries. All optional; absent = SDK standard defaults (3 / 1000 / 20000),
  config-owned: AWS env/profile retry knobs do not apply.
- **`--follow-symlinks` is inventory-only in v1.** The walker follows
  symlinks and lists them, but push/pull plan any followed *file* symlink as
  `Skip(followed_symlink)` (transfers refuse to open a symlink); only
  `status` shows them as live inventory rows. Dir-symlink children transfer
  normally; a `pull` write through a symlink destination stays refused (fail
  closed).
- **Dir-symlink alias footgun.** A dir symlink whose target is inside the
  vault double-lists the target's content under both keys (push uploads the
  same bytes twice; pull writes both keys). The walk warns on every such
  alias; both copies are still listed and synced (dedup would make the
  surviving key set depend on directory enumeration order).
- **Downloads are capped at their planned size.** A remote object that grew
  (or was replaced) after the plan is refused mid-stream: a download that
  exceeds its planned size errors before the extra bytes are written to disk.
- **Pull overwrite resets permission bits.** A download writes a fresh temp
  sibling (owner-only `0600` while buffering, W108) and renames it over the
  destination, so the destination's permission bits are replaced by the
  temp's: a previously-`0644` note returns as `0600`. v1 has no permission
  model; only content and mtime are preserved.
- **Planner identity is codepoint-exact (no NFC fold).** APFS folds NFD/NFC
  (a note named in decomposed form appears under its composed name), while S3
  does not, so a vault round-tripped through another machine can show a false
  `local_only` + `remote_only` pair for the "same" note name. v1 does not
  normalize (A-L4).
- **The integration suite skips without `VAULTSYNC_TEST_S3_BUCKET`.** The S3
  tests compile always but skip at runtime with a `[skip]` note when the env
  var is unset, keeping `cargo test` green offline. Set it (plus optional
  region/endpoint/prefix vars) in CI to make them run for real.
