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

Phase 0 design locked and Phase 1 skeleton complete (planner + module tree + CLI stubs against an in-memory mock store; `cargo test` green). Phase 2 (real local FS + S3) is next.

```text
cargo build
cargo run -- version
cargo run -- status --vault <vault-dir>   # human plan against mock, exit 2 when dirty
cargo run -- push --vault <vault-dir>     # dry-run stub, no store/disk mutation
cargo run -- pull --vault <vault-dir>
cargo run -- check                         # mock connectivity stub
```
