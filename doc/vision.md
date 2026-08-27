# Vision

## Problem

Obsidian vaults are plain directories. People want them on S3 (and later other object stores) for backup, multi-device access, and ownership of data. Existing tools either:

- bundle many cloud providers, OAuth flows, encryption modes, and UI into one large plugin (example: remotely-save), or
- are generic sync engines that ignore vault conventions and browser/desktop constraints.

We want the opposite: a small, inspectable tool that does one job well.

## Goals

1. **S3-first vault sync** that a human can reason about end-to-end.
2. **CLI as the primary interface.** Scripts, cron, launchd, CI, and headless machines come first.
3. **Library core** so a future Obsidian plugin (or other UI) is a thin wrapper, not a rewrite.
4. **Unix philosophy:** do one thing; compose; text-friendly output; explicit flags over implicit magic.
5. **KISS:** v1 is list, compare, push, pull, delete. No encryption layer. No smart merge. No multi-provider soup.
6. **Decoupled design:** local FS, object store, planner, and executor do not know about Obsidian or each other beyond small interfaces.
7. **Minimal intrusion on the remote:** the bucket remains ordinary objects. No required remote control plane files for v1.

## Non-goals (v1)

- Bidirectional "set and forget" sync with true cross-device deletion tracking via local history DB.
- Client-side encryption (rclone crypt, AGE, etc.).
- Conflict smart-merge of markdown.
- WebDAV, Dropbox, OneDrive, Google Drive, and similar consumer drives.
- OAuth / PKCE browser flows.
- Running inside Obsidian's renderer as the main product.
- Background daemons or always-on watchers (may appear later as thin wrappers over the CLI).
- Re-implementing the full remotely-save feature surface.

## Principles

### Explicit direction beats clever bidirection

v1 mirrors `rsync` mental model:

- `pull` makes local match remote (with optional `--delete`)
- `push` makes remote match local (with optional `--delete`)
- `status` shows the plan without writing

Bidirectional convenience, if added later, must be defined as composition of these primitives plus an explicit policy - not a third hidden algorithm.

### Plain objects on the remote

Remote keys are vault-relative paths. Folders are either implied by key prefixes (S3 default) or optional zero-byte marker objects. No mandatory `_vaultsync-metadata.json` on the bucket for v1.

Trade-off: without a shared deletion journal on the remote or a local prev-sync DB, **true multi-device deletion propagation is out of scope for v1**. Document this loudly. Users who need mirror semantics use `--delete` on the side that is source of truth.

### Small interfaces, replaceable backends

```text
LocalFs  <-->  Planner  <-->  ObjectStore
                  |
              Executor
```

S3 is the first `ObjectStore`. Azure Blob and GCS should plug in later without touching the planner.

### Borrow ruthlessly, copy little

remotely-save is a reference implementation, not a dependency. We take proven ideas (uniform entity shape, folder trailing slash, mtime-in-metadata for S3, decision tables) and drop everything that fights minimalism (multi-service settings monolith, PRO gates, encryption wrappers, localforage schema zoo).

### Inspectability

Every run should be able to emit:

- a human summary
- a machine-readable plan (JSON)
- optional dry-run that is identical to a real run except IO commits

If a user cannot answer "what will this do to my vault and bucket?", the design failed.

## Success criteria for v1

1. Point `vaultsync` at a vault directory and an S3 bucket/prefix; `status` shows a correct diff.
2. `push` / `pull` transfer only needed objects; `--delete` removes extras on the destination side.
3. Works with AWS S3 and at least one S3-compatible endpoint (e.g. MinIO or R2) via endpoint/region/path-style config.
4. Core logic is unit-tested without network (mock store + temp dirs).
5. Total conceptual surface fits in one focused afternoon of reading `doc/` plus `vaultsync --help`.

## Relationship to Obsidian

Obsidian is the **motivating workload**, not a hard dependency of the core:

- default ignore rules may know about `.obsidian/` quirks
- a future plugin may call the same library or shell out to the CLI
- core APIs take filesystem paths, not `obsidian.Vault`

Treating the vault as a directory keeps the tool useful for plain markdown folders and non-Obsidian clients.
