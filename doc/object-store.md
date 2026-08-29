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

pub struct Listing {
    pub entities: Vec<Entity>,  // sorted by key
    pub warnings: Vec<String>,  // advisory backend notes (e.g. keys dropped while listing)
}

pub trait ObjectStore: Send + Sync {
    fn list(&self, prefix: &str) -> Result<Listing>;   // folders synthesized from key prefixes
    fn head(&self, key: &str) -> Result<Entity>;
    fn get_to(&self, key: &str, w: &mut dyn Write) -> Result<Entity>;   // streaming
    fn put_from(&self, key: &str, r: &mut dyn Read, size: u64, mtime_ms: Option<u64>) -> Result<Entity>; // streaming
    fn delete(&self, key: &str) -> Result<()>;
}
```

`ObjectStore: Send + Sync` (I20-traits, issue 20 cycle 1): implementations
must be shareable as `&dyn ObjectStore` across the worker threads of the
bounded `std::thread` pool, so interior-mutable state uses `Mutex`/`Atomic*`
(never `Cell`/`RefCell`). This is a compile-time contract pinned by
`assert_ss`-style tests for `S3Store`, `MemoryStore`, `LocalFs`, and
`dyn ObjectStore` itself.

`Listing.warnings` carries advisory notes about the listing (e.g. dropped
non-empty `*/` keys); the CLI prints them (one `warning: ...` line each),
and library consumers may inspect or ignore them - warnings never fail the
listing.

**Locked:** expose **streaming in the trait from day one** (`get_to` /
`put_from`). Buffered `Vec<u8>` helpers may wrap the streaming API for small
files and tests, but the planner/executor talk to streaming methods so large
PDF/PNG objects never force a trait break.

There is no `content_type` field on `Entity` in v1; Content-Type handling (if
any) is backend-side only (post-v1 if it becomes a trait concern).

Async shape (`AsyncRead` / `AsyncWrite`) is allowed if the S3 spike selects an async SDK; keep the same streaming responsibility either way.

**D2 (restated):** async lives only inside `store::s3` - `S3Store` owns a
private `tokio::runtime::Runtime` and `block_on`s per call; planner,
executor, and CLI stay sync and runtime-free. Runtime flavor is the W48
current-thread runtime (`Builder::new_current_thread().enable_all()`),
confirmed under concurrent `block_on` by the issue-20 cycle-2 probe (N
threads `block_on` the same runtime and overlap in wall-clock), so the
pool fan-out lives on the caller side (`crate::pool::run_bounded`), not in
the runtime. No `async` outside `store::s3` without a new roadmap
decision-log entry.

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
| list | `ListObjectsV2` paginated; folder prefixes derived from keys (no delimiter in the request; `CommonPrefixes` is not used). ListObjectsV2 cannot return user metadata, so each listed object entity is then enriched via a per-object `HeadObject` (`enrich_with_head_mtimes`) into one coherent head snapshot: `mtime_ms` comes from `vaultsync-mtime` (falling back to `LastModified`), while `etag` and `size` come from the `HeadObject` (R2-5). The planner ignores etags (`plan_etag_ignored`), so the etag remains an opaque token. Request shape is N+1 (one list cycle + N heads); since issue 20 the N heads fan out through the same bounded pool as the transfer passes, capped by `[transfer].concurrency` (I20-heads; `1` = sequential, byte-for-byte the pre-issue-20 shape). With several non-NotFound head errors in one listing the returned error is the listing-earliest one (deterministic); in-flight heads are not cancelled. Transient head errors (`Unavailable`/`Timeout`) are owned by the SDK standard-mode `RetryConfig` configured from `[transfer.retry]` (I8; the old W117 head stopgap is retired) - heads are single-attempt at this boundary (the SDK's shared retry quota can also stop retries before `max_attempts` under a sustained storm). Reserved-namespace leftovers (`.vaultsync-check-*` / `.*.vaultsync-tmp-*`) are partitioned out before any head is issued (W118). A `NotFound` head drops the row (concurrent-delete race, surfaced as a bounded warning); any other head error fails the listing. |
| head | `HeadObject` |
| get_to | `GetObject` (stream body into the caller's writer). Transient errors are owned by the SDK `RetryConfig` from `[transfer.retry]` for the request itself; a **mid-body** connection loss after the response header starts streaming is an accepted gap (I8-midbody) - the SDK cannot retry an already-consumed body, so the download fails per-key (`Unavailable`) and the next run converges (sync is idempotent). |
| put_from | `PutObject` single-PUT via `ByteStream::from_path` (buffered to a disk temp, never a `size`-sized memory buffer); 5 GiB ceiling. Multipart is a post-v1 item. |
| delete | `DeleteObject` (batch delete later) |

Upload temp buffers live in the OS temp dir as `vaultsync-upload-<pid>-<n>`
(owner-only `0o600`, `create_new`). Their lifecycle includes a 24h reap
(W88/r10-L2), run **at most once per process, on the first `S3Store::new`**
(W97): best-effort removal of `vaultsync-upload-*` files older than 24h (a
crash/SIGKILL between buffering and the post-upload `remove_file` would
otherwise leak a full buffered object in the shared temp dir). Fresh
in-flight buffers and unrelated files are never touched.

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
