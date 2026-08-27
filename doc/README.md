# vaultsync documentation

## Reading order

1. [vision.md](./vision.md) - why this exists, what it refuses to be
2. [architecture.md](./architecture.md) - layers, crates, dependency direction
3. [sync-model.md](./sync-model.md) - how files are compared and transferred
4. [object-store.md](./object-store.md) - storage abstraction; S3 first
5. [cli.md](./cli.md) - user-facing commands
6. [borrowed-from-remotely-save.md](./borrowed-from-remotely-save.md) - inheritance map
7. [roadmap.md](./roadmap.md) - build order

## Doc conventions

- Prefer short files over one giant design dump.
- Normative language: **must**, **should**, **may**.
- Open questions were marked `TODO(decision):` during drafting; Phase 0 locked them into [roadmap.md](./roadmap.md) decision log.
- Implementation details that remain spike-gated (S3 client crate) stay out of normative "must" language until the spike lands.
