# Architecture

## Overview

```text
+------------------+
| vaultsync (CLI)  |   thin: parse args, load config, print results
+--------+---------+
         |
         v
+------------------+
| vaultsync-core   |   plan + execute; pure-ish orchestration
+--+------------+--+
   |            |
   v            v
+--------+   +-------------+
| local  |   | objectstore |   trait + S3 impl (later Azure/GCS)
+--------+   +-------------+
```

Dependency rule: **arrows point toward smaller, stabler crates.** CLI depends on core; core depends on local + objectstore traits; backends implement traits. Nothing in `objectstore` or `local` imports planner/CLI code.

## Crate layout (proposed)

```text
vaultsync/                 # workspace root
  crates/
    vaultsync-core/        # Entity, plan, execute, filters
    vaultsync-local/       # local directory walker/reader/writer
    vaultsync-s3/          # S3 ObjectStore impl
    vaultsync-cli/         # binary: vaultsync
  doc/                     # design docs (this tree)
```

Optional later:

- `vaultsync-azure`, `vaultsync-gcs` - extra backends
- `vaultsync-obsidian` - plugin frontend (TypeScript) calling core via CLI or FFI - **not v1**

Names are provisional. Prefer few crates over a micro-crate explosion. If three library crates feel heavy at the start, begin with:

```text
vaultsync/                 # bin + lib in one package
```

and split only when backend boundaries hurt. **Locked (v1): single package** with one library target and one binary target (`vaultsync`). Split into a workspace only when a second backend or a second frontend forces it.

## Layers

### 1. Entity layer

Uniform, flat, copyable description of a file or folder. Borrowed in spirit from remotely-save `Entity`, simplified:

```text
Entity {
  key          # vault-relative path; folders end with '/'
  size         # bytes; 0 for folders
  mtime_ms     # client-visible mtime when known
  etag         # remote opaque version token when known
}
```

(No `side` field: which side an entity belongs to is implied by which list
produced it.)

Rules:

- Folders **must** be represented as keys ending with `/`.
- Keys **must not** start with `/`.
- Keys use `/` separators even on Windows local walks.
- No encryption fields (`keyEnc`, `sizeEnc`) in v1.

### 2. Local filesystem

Responsibilities:

- walk vault root to `Vec<Entity>`
- read file bytes
- write file bytes and apply mtime when the OS allows
- create parent directories
- delete file or directory (v1: permanent `unlink`/`remove_dir` only when `--delete` is set; optional trash backend is post-v1 if a low-cost crate is accepted)

Must not:

- know about S3
- know about sync decisions

### 3. Object store

See [object-store.md](./object-store.md). Trait (streaming from day one,
matching `src/store/mod.rs`):

```text
list(prefix) -> Vec<Entity>
head(key) -> Entity
get_to(key, w) -> Entity        # stream body into w
put_from(key, r, size, mtime_ms) -> Entity   # store exactly size bytes from r
delete(key) -> ()
```

S3 notes that affect the trait:

- list is prefix + pagination
- mtime: prefer user metadata for vault mtime; fall back to `LastModified`
- folders: default **no** folder objects (prefix-only); optional marker mode later

### 4. Planner

Pure function:

```text
plan(local: &[Entity], remote: &[Entity], mode: Mode, opts: PlanOpts) -> Plan
```

- builds a key-keyed map
- classifies each key: equal / local_only / remote_only / local_newer / remote_newer / conflict_same_mtime_diff_size (rare)
- emits actions: `Upload`, `Download`, `DeleteLocal`, `DeleteRemote`, `Skip`, `Conflict`

No IO. Easy to unit test with fixture entity lists.

### 5. Executor

Applies a `Plan` using LocalFs + ObjectStore:

- bounded concurrency
- dry-run short-circuits before mutating calls
- streams progress events to the CLI (issue 27: coarse, completion-driven
  `PassStart`/`KeyDone`/`PassEnd`/`RunEnd` events via the `Progress` trait in
  [src/progress.rs](../src/progress.rs); `exec` emits, `cli` renders - the
  renderers (TTY bar / quiet) and the `ProgressMode` seam live there too)

### 6. CLI

- loads config (file + env + flags; precedence documented in [cli.md](./cli.md))
- invokes plan/execute
- prints human text by default; `--json` for machines

## Config (v1 sketch)

Config is data, not a service registry.

```text
vault_root: /path/to/vault
store:
  type: s3
  endpoint: optional
  region: ...
  bucket: ...
  prefix: optional/vault-prefix/
  path_style: bool
  # credentials: standard AWS chain (env, profile, instance role)
ignore:
  - .obsidian/workspace*
  - .trash/
  - ...
concurrency: 4
```

Credentials **should not** be required inside the config file. Prefer the AWS standard chain so the tool behaves like `aws-cli`.

## Obsidian coupling boundary

| Concern | Where it lives |
| -------- | ---------------- |
| path walk, mtime, bytes | `local` |
| S3 protocol | `s3` |
| diff decisions | `core` planner |
| default ignore patterns for Obsidian | `core` defaults or CLI profile `--profile obsidian` |
| plugin UI, settings tab | future frontend only |

## Concurrency and safety

- Planner is single-threaded and pure.
- Executor may run N concurrent transfers.
- v1 **should** refuse to run if it detects another `vaultsync` lock file in the vault (simple flock/pid file). No distributed lock on S3 in v1.
- Partial failure: finish outstanding workers, return non-zero, print failed keys. No distributed transaction claim.

## Logging

- stderr: logs
- stdout: primary user output (status table, JSON plan)
- levels: error, warn, info, debug (debug includes per-key decisions)

## Testing strategy

| Layer | How |
| ----- | --- |
| planner | pure unit tests, table-driven |
| local | temp directories |
| s3 | optional integration tests behind `VAULTSYNC_S3_TEST=1`; mock trait for unit tests |
| cli | smoke tests on temp vault + mock store if injected; else end-to-end optional |

## What we deliberately do not architect in v1

- encryption wrapper FS (remotely-save `FakeFsEncrypt`)
- local prev-sync database (remotely-save `localdb` / prevSync records)
- remote metadata sidecar files
- PRO/account gating
- background sync scheduler inside the binary (use cron)
