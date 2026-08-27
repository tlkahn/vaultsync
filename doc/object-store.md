# Object store abstraction

## Purpose

Isolate provider SDKs behind a tiny trait so the planner never imports S3 types. Future Azure Blob / GCS implementations should not fork the sync algorithm.

## Trait (normative sketch)

Names are Rust-ish; adjust to final code style.

```rust
/// Object key relative to the configured prefix. No leading slash.
/// Folders end with '/'.
pub type Key = String;

pub struct ObjectMeta {
    pub key: Key,
    pub size: u64,
    pub mtime_ms: Option<u64>,
    pub etag: Option<String>,
    pub content_type: Option<String>,
}

pub trait ObjectStore {
    fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>>;
    fn head(&self, key: &str) -> Result<ObjectMeta>;
    fn get(&self, key: &str) -> Result<Vec<u8>>; // streaming API later if needed
    fn put(&self, key: &str, body: &[u8], mtime_ms: Option<u64>) -> Result<ObjectMeta>;
    fn delete(&self, key: &str) -> Result<()>;
}
```

**Locked:** expose **streaming in the trait from day one**. Buffered `Vec<u8>` helpers may wrap the streaming API for small files and tests, but the planner/executor talk to streaming methods so large PDF/PNG objects never force a trait break:

```rust
fn get_to(&self, key: &str, w: &mut dyn Write) -> Result<ObjectMeta>;
fn put_from(&self, key: &str, r: &mut dyn Read, size: u64, mtime_ms: Option<u64>) -> Result<ObjectMeta>;
```

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
| get | `GetObject` |
| put | `PutObject` (multipart only if size > threshold; can defer multipart to v1.1) |
| delete | `DeleteObject` (batch delete later) |

### Metadata

- Store client mtime in object metadata key `vaultsync-mtime` (or `mtime`) as decimal ms.
- Do not require ACL headers beyond bucket defaults.
- Content-Type: guess from extension with a tiny map, else `application/octet-stream`.

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

- `list`/`get`/`put`/`delete` over a `HashMap` or local sandbox folder
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
