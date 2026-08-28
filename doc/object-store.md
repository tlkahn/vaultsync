# Object store abstraction

## Purpose

Isolate provider SDKs behind a tiny trait so the planner never imports S3 types. Future Azure Blob / GCS implementations should not fork the sync algorithm.

## Trait (normative sketch)

Names are Rust-ish; adjust to final code style. Sketch matches the shipped
surface in `src/store/mod.rs`.

```rust
pub struct Entity {
    pub key: String,       // vault-relative; no leading '/'; folders end with '/'
    pub size: u64,         // bytes; 0 for folders
    pub mtime_ms: Option<u64>,
    pub etag: Option<String>,
}

pub trait ObjectStore {
    fn list(&self, prefix: &str) -> Result<Vec<Entity>>;          // folders synthesized from prefixes
    fn head(&self, key: &str) -> Result<Entity>;
    fn get_to(&self, key: &str, w: &mut dyn Write) -> Result<Entity>;   // streaming
    fn put_from(&self, key: &str, r: &mut dyn Read, size: u64, mtime_ms: Option<u64>) -> Result<Entity>; // streaming
    fn delete(&self, key: &str) -> Result<()>;
}
```

**Locked:** expose **streaming in the trait from day one** (`get_to` /
`put_from`). Buffered `Vec<u8>` helpers may wrap the streaming API for small
files and tests, but the planner/executor talk to streaming methods so large
PDF/PNG objects never force a trait break.

There is no `content_type` field on `Entity` in v1; Content-Type handling (if
any) is backend-side only (post-v1 if it becomes a trait concern).

Async shape (`AsyncRead` / `AsyncWrite`) is allowed if the S3 spike selects an async SDK; keep the same streaming responsibility either way.

## Prefix handling

Config `prefix: "myvault/"` is applied **inside the S3 backend**, not by the planner.

- Planner keys are vault-relative (`notes/a.md`)
- Backend maps to `s3://bucket/myvault/notes/a.md`

This keeps plans readable and backends interchangeable (Azure container + prefix, GCS bucket + prefix).

## S3 implementation notes

### Client

Prefer the official AWS SDK for Rust **or** a thin signed-request stack. Dependency policy for this workspace: **minimize crates**; confirm before adding heavy SDK trees.

Practical options (pick in Phase 2 spike, not earlier):

1. `aws-sdk-s3` + `aws-config` - full featured, large compile
2. `rust-s3` - smaller, less "official"
3. hand-rolled SigV4 for the 5 calls we need - maximum control, more auth code to maintain

**Locked process:** choose after a Phase 2 spike. Spike criteria: list/get/put/delete against AWS and MinIO; credential chain; path-style; custom endpoint. Prefer the lightest option that clears the matrix; if `aws-sdk-s3` wins, use **tokio** and keep the async surface inside the S3 backend (planner stays sync-friendly at the type level where practical).

### Operations mapping

| Trait | S3 API |
| ----- | ------ |
| list | `ListObjectsV2` paginated; synthesize folder prefixes from `CommonPrefixes` if delimiter `/` is used, or derive from keys |
| head | `HeadObject` |
| get_to | `GetObject` (stream body into the caller's writer) |
| put_from | `PutObject` single-PUT via `ByteStream::from_path` (buffered to a disk temp, never a `size`-sized memory buffer); 5 GiB ceiling. Multipart is a post-v1 item. |

Upload temp buffers live in the OS temp dir as `vaultsync-upload-<pid>-<n>`
(owner-only `0o600`, `create_new`). Their lifecycle includes a 24h reap
(W88/r10-L2): each `S3Store` construction best-effort removes
`vaultsync-upload-*` files older than 24h (a crash/SIGKILL between buffering
and the post-upload `remove_file` would otherwise leak a full buffered object
in the shared temp dir). Fresh in-flight buffers and unrelated files are never
touched.
| delete | `DeleteObject` (batch delete later) |

Delete is **idempotent-friendly** (PR2 A-M3/B-L6): deleting an already-absent
key may return `Ok` (S3 is idempotent) or `NotFound` (mock / local). Both are
the achieved goal state; the executor normalizes `NotFound` to success.

Keys under the reserved namespace `.vaultsync-check-*` (used by `check`'s
connectivity probe) and `.*.vaultsync-tmp-*` (the download/upload temp-sibling
pattern, W63) must not be created by user content. Both prefixes are filtered
out of walks and plans (final-segment policy, W63/A-L3), so a crash leftover -
a probe stranded between put and delete, or a temp sibling that reached the
store out-of-band - can never Download to disk or re-upload (R4-L4/W42). A run
that encounters such a leftover counts it and surfaces it on stderr instead of
dropping it silently (W79).

Zero-byte marker objects at the exact store prefix (e.g. an S3-console
"Make Folder" marker that strips to an empty vault-relative key) are ignored;
objects whose keys end in `/` (some tools write content into `dir/` markers)
are invisible to sync in v1 (their trailing `/` is not a vault file key) - a
non-zero-size `*/` key surfaces a warning instead of being silently dropped
(W70).

Partial listings fail closed (W61): a provider that reports a truncated page
without a continuation token is refused loudly - a partial listing would be
indistinguishable from a complete one, and `pull --delete` would classify
genuinely-remote files missing from the partial page as local extras and
delete them locally.

## Design constraints

**In-memory listing and planning (H3).** `ObjectStore::list` collects the
full prefixed listing into one `Vec` (`list_prefix_objects` pages through
`ListObjectsV2` but accumulates every page in memory), and `build_plan`
holds all local and remote entities in `HashMap`s while planning. Total
memory is O(remote objects + local files), which is the deliberate v1
trade: the vault-scale target is tens of thousands of objects, where full
in-memory planning is comfortable. Streaming or paged planning (plan
increments as pages arrive, bounded memory regardless of store size) is a
Phase 3 concern and must start from this explicit assumption.

### Metadata

- Store client mtime in object metadata key `vaultsync-mtime` (or `mtime`) as decimal ms.
- Do not require ACL headers beyond bucket defaults.
- Content-Type: v1 sends no explicit Content-Type; objects are stored under
the SDK default `application/octet-stream`. Extension-based guessing is
dropped (content type is backend-side only, per the trait note above; there
is no `content_type` field on `Entity`).

### Compatibility targets

v1 should document tested matrix rows as they are verified:

| Endpoint class | Notes |
| -------------- | ----- |
| AWS S3 | primary |
| MinIO | path-style often required |
| Cloudflare R2 | S3 API subset; verify metadata and list |
| Backblaze B2 S3 API | verify |
| Garage / SeaweedFS S3 | best-effort |

CORS is irrelevant for a native CLI (unlike remotely-save in-browser).

### What we drop from remotely-save S3 code

- Obsidian `requestUrl` HTTP handler / CORS bypass
- reverse proxy unsigned URL hacks
- browser `FetchHttpHandler` patches
- encryption-aware listing
- remote base-dir dual model beyond a single prefix

Keep:

- prefix support
- path-style option
- parts concurrency idea (later)
- accurate mtime via user metadata
- optional folder object generation as a flag (default off)

## Future providers

| Provider | Crate (future) | Mapping |
| -------- | -------------- | ------- |
| Azure Blob | `vaultsync-azure` | container + prefix; metadata for mtime |
| GCS | `vaultsync-gcs` | bucket + prefix; custom metadata |

Both should implement the same `ObjectStore` trait. No planner changes.

## Mock store

In-memory or temp-dir-backed `ObjectStore` for tests:

- `list`/`head`/`get_to`/`put_from`/`delete` over a `HashMap` or local sandbox folder
- used by planner/executor tests without network

## Errors

Map provider errors to a small core enum:

```text
NotFound
Unauthorized
Timeout
Unavailable
InvalidObject
Other(msg)
```

CLI prints actionable text (wrong region, bad bucket, expired key).
