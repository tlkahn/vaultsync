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
green offline (no network in the default suite). Phase 3 (delete safety, ignore
patterns, concurrency, CI) is next.

```text
cargo build
cargo run -- version
cargo run -- status --config <cfg>      # human plan against the real store; 0/2/1
cargo run -- push --config <cfg>        # upload local-newer; real mutation
cargo run -- pull --config <cfg>        # download remote-newer
cargo run -- check --config <cfg>       # connectivity probe (put/get/delete)
```

Without `--config`, `status` runs against an in-memory mock store (useful for
status/dry-run against a local dir); `push`/`pull`/`check` refuse loudly until
a `[store]` section with a `bucket` is configured (they must never silently
action a plan - or delete files - against a non-existent store).

## Known behaviors

- **List-driven plans compare upload `LastModified`.** A plan is built from
  `list`, which exposes S3's second-granular `LastModified`, not the client
  `vaultsync-mtime` carried in object metadata. After a `push`, a later `pull`
  can therefore re-download unchanged files once (they look "remote newer" by
  seconds). Bytes and the applied `vaultsync-mtime` are correct; only the
  *plan* is pessimistic. An opt-in head-on-list to surface client mtimes in
  plans is Phase 3.
- **Mock store is `status` only.** Without a `[store]` section, `push`/
  `pull`/`check` refuse (exit 1); only `status` runs against the in-memory
  mock.
- **The integration suite skips without `VAULTSYNC_TEST_S3_BUCKET`.** The S3
  tests compile always but skip at runtime with a `[skip]` note when the env
  var is unset, keeping `cargo test` green offline. Set it (plus optional
  region/endpoint/prefix vars) in CI to make them run for real.
