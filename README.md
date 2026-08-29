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
confirmation rail - `--yes`/`--max-delete`/prompt - ignore patterns,
concurrency, CI) is next; the Phase 2 delete-safety surface is already
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
  3's request pool; a bounded transient retry (W117) covers throttles
  mid-enrich.
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
- **`[ignore].patterns` and `[transfer].concurrency` are Phase 3.** They are
  parsed but not yet applied: `push`/`pull`/`check` refuse loudly on a
  non-empty `[ignore].patterns` (`status` warns), and an explicitly-set
  `[transfer].concurrency` that **differs from the default** warns on every
  run until the pool exists (an explicit copy of the default, `4`, is
  silent - it is behaviorally indistinguishable from omitting the key).
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
