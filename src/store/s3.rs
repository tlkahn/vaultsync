//! S3 `ObjectStore` backend (Phase 2 Slice 7).
//!
//! Async containment lock (D2): all `tokio`/aws async lives inside this
//! module. `S3Store` owns a private `tokio::runtime::Runtime` and `block_on`s
//! per call. The planner, executor, CLI, and every other module stay sync and
//! runtime-free.
//!
//! Prefix mapping (object-store.md): vault-relative keys map inside the
//! backend to `s3://bucket/<prefix><key>`. `list` strips the prefix back off.
//!
//! mtime: stored as user metadata `vaultsync-mtime` (decimal ms) on put.
//! `list` uses `LastModified` (ListObjectsV2 does not return metadata); `head`
//! and `get_to` prefer the metadata, falling back to `LastModified`.
//!
//! Consequence (documented limitation): a `list`-driven plan compares against
//! each object's upload `LastModified`, so after a push many unmodified files
//! can look "remote newer" by seconds-of-granularity and a later `pull` may
//! re-download them. Bytes are correct and downloads apply the true client
//! mtime from `get_to` metadata; a per-object `head` in `list` (to surface
//! client mtimes in plans) is a post-v1 optimization.
//!
//! Streaming: `get_to` streams the object body to the caller's writer;
//! `put_from` buffers the reader to a temp file on disk and streams that file
//! to S3 via `ByteStream::from_path` - never a `size`-sized in-memory buffer
//! (P1r-put-size).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;

use aws_sdk_s3::Client;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::ByteStream;

use crate::config::StoreSettings;
use crate::entity::Entity;
use crate::error::Error;
use crate::store::ObjectStore;

/// User-metadata key for the client-visible mtime (decimal ms).
const MTIME_KEY: &str = "vaultsync-mtime";

/// The S3 backend. Async fully contained behind the `Runtime`.
pub struct S3Store {
    client: Client,
    bucket: String,
    /// Vault-relative prefix with a trailing `/` (may be empty).
    prefix: String,
    rt: tokio::runtime::Runtime,
}

impl S3Store {
    /// Build from resolved store settings. Credentials come from the ambient
    /// AWS default chain (env, shared config, profile) - never from the TOML.
    pub fn new(settings: &StoreSettings) -> Result<S3Store, Error> {
        // W48: a current-thread runtime matches the one-`block_on`-at-a-time
        // sync architecture (each call `block_on`s once); a multi-thread
        // runtime would add worker threads with no parallelization benefit.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::Other(format!("failed to start async runtime: {e}")))?;
        let client = rt.block_on(async {
            let sdk = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            let mut b =
                aws_sdk_s3::config::Builder::from(&sdk).force_path_style(settings.path_style);
            // W7/B-M2: only override the region when explicitly configured;
            // `None` leaves the AWS default chain (env, shared config,
            // profile) already loaded into `sdk` to decide - never a
            // hardcoded guess.
            if let Some(r) = &settings.region {
                b = b.region(aws_sdk_s3::config::Region::new(r.clone()));
            }
            if let Some(ep) = &settings.endpoint {
                b = b.endpoint_url(ep);
            }
            Client::from_conf(b.build())
        });
        Ok(S3Store {
            client,
            bucket: settings.bucket.clone(),
            // R5-M1: the trailing-`/` invariant is enforced at the
            // constructor, not trusted from callers - `full_key` raw-concats
            // the prefix, so an unnormalized `"notes"` would map `a.md` to
            // `notesa.md`. Config resolution already normalizes, but the
            // public constructor contract must not depend on that.
            prefix: crate::config::normalize_prefix(&settings.prefix),
            rt,
        })
    }

    fn list_prefix_objects(
        &self,
        caller_prefix: &str,
    ) -> Result<Vec<(String, u64, Option<u64>)>, Error> {
        let s3_prefix = format!("{}{}", self.prefix, caller_prefix);
        let client = self.client.clone();
        let bucket = self.bucket.clone();
        let store_prefix = self.prefix.clone();
        self.rt.block_on(async move {
            let mut out = Vec::new();
            let mut continuation: Option<String> = None;
            loop {
                let mut req = client
                    .list_objects_v2()
                    .bucket(&bucket)
                    .prefix(&s3_prefix)
                    .max_keys(1000);
                if let Some(tok) = &continuation {
                    req = req.continuation_token(tok.clone());
                }
                let resp = req.send().await.map_err(|e| map_sdk_err(&e, "list"))?;
                for obj in resp.contents().iter() {
                    let Some(full) = obj.key() else { continue };
                    let Some(rel) = strip_prefix(&store_prefix, full) else {
                        continue;
                    };
                    // LastModified is the only mtime source in a listing.
                    let last = obj.last_modified().and_then(dt_millis);
                    out.push((
                        rel.to_string(),
                        obj.size().map(nonneg_size).unwrap_or(0),
                        last,
                    ));
                }
                match next_continuation(resp.is_truncated(), resp.next_continuation_token())? {
                    Some(t) => continuation = Some(t),
                    None => break,
                }
            }
            Ok(out)
        })
    }
}

/// Fail-closed pagination decision (W61/A-M1): a provider that reports a
/// truncated page must hand back a continuation token. A truncated page with
/// an absent or empty token is a mis-paging S3-compatible provider (R2/MinIO
/// are named matrix targets) - a partial listing would be indistinguishable
/// from a complete one, and `pull --delete` would classify genuinely-remote
/// files missing from the partial page as local extras and delete them
/// locally. Refuse loudly rather than plan against a false remote view. A
/// non-truncated page (or a provider that stays silent about truncation, the
/// AWS-absent case) ends the loop unchanged.
fn next_continuation(
    is_truncated: Option<bool>,
    token: Option<&str>,
) -> Result<Option<String>, Error> {
    if is_truncated == Some(true) {
        match token {
            Some(t) if !t.is_empty() => Ok(Some(t.to_string())),
            _ => Err(Error::Other(
                "provider reports a truncated page with no continuation token; refusing a partial listing"
                    .to_string(),
            )),
        }
    } else {
        Ok(None)
    }
}

/// Map an object's full (prefix-adjusted) key to the vault-relative key.
fn strip_prefix<'a>(prefix: &str, full: &'a str) -> Option<&'a str> {
    if prefix.is_empty() {
        Some(full)
    } else {
        full.strip_prefix(prefix)
    }
}

/// The entity `size` to report for a streamed body (W30/N6/L2): when
/// `Content-Length` is present it is authoritative and equals `written` after
/// the truncation check; when absent, the true streamed byte count is used so
/// the reported size is never a bogus 0.
fn effective_get_size(content_len: Option<i64>, written: u64) -> u64 {
    content_len.map(nonneg_size).unwrap_or(written)
}

/// Clamp a backend size to a non-negative `u64` (W64/A-L6): a pathological
/// negative `Content-Length`/`Size` must never wrap to a huge u64. One shared
/// helper for `head`, listing rows, and `get_to` so the policy cannot drift.
fn nonneg_size(s: i64) -> u64 {
    s.max(0) as u64
}

/// Full S3 key for a vault-relative key.
fn full_key(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}{key}")
    }
}

/// Encode an mtime into the user-metadata header value (None -> no header).
fn encode_mtime(mtime_ms: Option<u64>) -> Option<String> {
    mtime_ms.map(|ms| ms.to_string())
}

/// Decode an mtime from the metadata header, falling back to LastModified.
fn decode_mtime(meta_val: Option<&str>, last_modified_ms: Option<u64>) -> Option<u64> {
    if let Some(v) = meta_val {
        if let Ok(ms) = v.parse::<u64>() {
            return Some(ms);
        }
        // garbage metadata -> fall through to LastModified (sync-model policy 2)
    }
    last_modified_ms
}

/// Convert `aws-smithy` DateTime to ms since epoch. Pre-epoch times saturate
/// to `Some(0)`, mirroring the local side's `system_time_to_ms` policy
/// (W8/A-M4/B-L2): a negative secs count must not wrap to a huge `u64`.
fn dt_millis(dt: &aws_sdk_s3::primitives::DateTime) -> Option<u64> {
    let secs = dt.secs();
    if secs < 0 {
        Some(0)
    } else {
        // R5-L7/W45: keep subsecond millis (discarding up to 999 ms made every
        // S3 LastModified truncate to the second). `subsec_nanos` is within
        // [0, 1e9), so `/1_000_000` is a lossless ms carry for non-negative
        // secs.
        Some(
            (secs as u64)
                .saturating_mul(1000)
                .saturating_add(dt.subsec_nanos() as u64 / 1_000_000),
        )
    }
}

/// Ancestor folder keys (each trailing-`/` prefix) of a key, like the mock.
fn parent_folders(key: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, b) in key.bytes().enumerate() {
        if b == b'/' {
            out.push(key[..=i].to_string());
        }
    }
    out
}

/// Pure conversion of a raw listing (key, size, mtime) into sorted entities
/// with synthesized folder views (same shape as the mock). Also returns the
/// dropped `*/` keys that carried bytes (size > 0) - S3-console folder
/// markers are zero-byte, so a non-empty trailing-`/` key is a real object
/// that must be surfaced, never silently dropped (W70/A-N2).
fn convert_listed(items: Vec<(String, u64, Option<u64>)>) -> (Vec<Entity>, Vec<(String, u64)>) {
    let mut map: BTreeMap<String, Entity> = BTreeMap::new();
    let mut dropped_nonempty: Vec<(String, u64)> = Vec::new();
    for (key, size, mtime) in items {
        if key.is_empty() {
            // R4-M2: the exact-prefix folder marker. S3-console "Make Folder"
            // writes a zero-byte object at the folder path (and some tools
            // write a marker at the store prefix itself); it strips to an
            // empty relative key, which is not a valid vault key and must be
            // dropped rather than planned.
            continue;
        }
        if key.ends_with('/') {
            // W70/A-N2: a zero-byte trailing-`/` key is the intended console
            // folder marker and is dropped silently (object keys never end
            // with '/'); one carrying actual bytes is a real object that
            // would become invisible - never planned, never warned. Surface
            // it instead of hiding it.
            if size > 0 {
                dropped_nonempty.push((key, size));
            }
            continue;
        }
        map.insert(
            key.clone(),
            Entity {
                key,
                size,
                mtime_ms: mtime,
                etag: None,
            },
        );
    }
    let file_keys: Vec<String> = map.keys().cloned().collect();
    for f in file_keys.iter().flat_map(|k| parent_folders(k)) {
        map.entry(f.clone()).or_insert(Entity {
            key: f,
            size: 0,
            mtime_ms: None,
            etag: None,
        });
    }
    (map.into_values().collect(), dropped_nonempty)
}

/// Validate a key for object operations (head/get/delete): folders are not
/// objects -> NotFound, matching the trait contract.
fn validate_object_key(key: &str) -> Result<(), Error> {
    crate::entity::ensure_valid_key(key)?;
    if key.ends_with('/') {
        return Err(Error::NotFound(key.to_string()));
    }
    Ok(())
}

/// Validate a key for `put_from`: file keys only, folder key rejected loudly.
fn validate_put_key(key: &str) -> Result<(), Error> {
    crate::entity::ensure_valid_key(key)?;
    if key.ends_with('/') {
        return Err(Error::InvalidKey(format!(
            "put_from does not accept folder keys: {key:?}"
        )));
    }
    Ok(())
}

/// Pure error classification. `status` is the HTTP status (None if none);
/// `timeout` maps to `Timeout` regardless of status.
fn classify_error(timeout: bool, status: Option<u16>, msg: &str) -> Error {
    if timeout {
        return Error::Timeout(msg.to_string());
    }
    match status {
        Some(404) => Error::NotFound(msg.to_string()),
        Some(401) | Some(403) => Error::Unauthorized(msg.to_string()),
        // W65/A-L1: 408 (request timeout) is a transient retryable provider
        // signal, classified with the timeout family.
        Some(408) => Error::Timeout(msg.to_string()),
        // W9/A-M5/B-M4: 5xx and 429 (throttle) are transient provider
        // unavailability, not generic Other.
        Some(s) if s >= 500 || s == 429 => Error::Unavailable(msg.to_string()),
        _ => Error::Other(msg.to_string()),
    }
}

/// Classify a mid-body stream failure (W65/A-L1): a `try_next` error after a
/// successful response header is transport-level by construction - the bytes
/// stopped flowing, not a request rejection - so it is transient in the Phase
/// 3 retry sense and must never surface as a generic `Other`.
fn classify_body_err(msg: &str) -> Error {
    Error::Unavailable(format!("get: connection lost mid-body: {msg}"))
}

/// True when the SDK error is a dispatch (connect/timeout) failure.
fn is_timeout_err<E, R>(e: &SdkError<E, R>) -> bool {
    matches!(e, SdkError::DispatchFailure(df) if df.is_timeout())
}

/// Thin shell over [`classify_error`] for a real SDK error.
fn map_sdk_err<E: std::fmt::Debug>(e: &SdkError<E>, what: &str) -> Error {
    let status = e.raw_response().map(|r| r.status().as_u16());
    // Display (not Debug) keeps the message concise and actionable.
    let msg = format!("{what}: {e}");
    classify_error(is_timeout_err(e), status, &msg)
}

/// A unique temp path for buffering an upload (disk, not RAM).
fn temp_upload_path() -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("vaultsync-upload-{}-{n}", std::process::id()))
}

/// Create a single upload temp buffer file (**not** retried). Owner-only
/// `0o600` perms (W14/A-L1/B-L1) and `create_new` (no reuse). On non-unix it
/// is plain `create_new`.
fn create_temp_upload_file_at(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
    }
}

/// Allocate the first free candidate path exclusively (W29/N4). A stale
/// leftover at an earlier candidate - a crashed run's `vaultsync-upload-
/// <pid>-<n>` + pid reuse - is skipped, leaving it untouched; only if every
/// candidate is taken is it a loud error (never an infinite loop).
fn alloc_first(candidates: &[PathBuf]) -> Result<(PathBuf, std::fs::File), Error> {
    for p in candidates {
        match create_temp_upload_file_at(p) {
            Ok(f) => return Ok((p.clone(), f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(Error::Io(e)),
        }
    }
    Err(Error::Io(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique upload temp file",
    )))
}

/// Create the upload temp buffer file. Owner-only `0o600` perms (W14/A-L1/
/// B-L1) and `create_new`, retrying past a stale leftover from a crashed run
/// (W29/N4): a predictable name + pid reuse would otherwise hard-fail the
/// first upload(s) of a later process. Bounded (100 candidate names) so a
/// pathological collision space is still a loud error.
fn create_temp_upload_file() -> Result<(PathBuf, std::fs::File), Error> {
    let candidates: Vec<PathBuf> = (0..100).map(|_| temp_upload_path()).collect();
    alloc_first(&candidates)
}

impl ObjectStore for S3Store {
    fn list(&self, prefix: &str) -> Result<Vec<Entity>, Error> {
        let raw = self.list_prefix_objects(prefix)?;
        let (entities, dropped_nonempty) = convert_listed(raw);
        // W70/A-N2: `ObjectStore::list` has no warning channel, so the v1
        // surface is a best-effort one-line stderr warning per dropped key
        // ("surface, don't hide") - a non-empty `*/` key would otherwise be
        // invisible: never planned, never warned.
        for (key, size) in dropped_nonempty {
            eprintln!(
                "warning: ignoring remote object {key} ({size} bytes): keys ending in '/' are folder markers; rename it to sync"
            );
        }
        Ok(entities)
    }

    fn head(&self, key: &str) -> Result<Entity, Error> {
        validate_object_key(key)?;
        let fk = full_key(&self.prefix, key);
        let client = self.client.clone();
        let bucket = self.bucket.clone();
        let rel = key.to_string();
        let out = self.rt.block_on(async {
            let resp = client
                .head_object()
                .bucket(&bucket)
                .key(&fk)
                .send()
                .await
                .map_err(|e| map_sdk_err(&e, "head"))?;
            let size = resp.content_length().map(nonneg_size).unwrap_or(0);
            let meta = resp.metadata();
            let meta_val = meta.and_then(|m| m.get(MTIME_KEY).map(String::as_str));
            let last = resp.last_modified().and_then(dt_millis);
            let mtime = decode_mtime(meta_val, last);
            let etag = resp.e_tag().map(|s| s.to_string());
            Ok::<Entity, Error>(Entity {
                key: rel,
                size,
                mtime_ms: mtime,
                etag,
            })
        })?;
        Ok(out)
    }

    fn get_to(&self, key: &str, w: &mut dyn Write) -> Result<Entity, Error> {
        validate_object_key(key)?;
        let fk = full_key(&self.prefix, key);
        let client = self.client.clone();
        let bucket = self.bucket.clone();
        let rel = key.to_string();
        let entity = self.rt.block_on(async {
            let resp = client
                .get_object()
                .bucket(&bucket)
                .key(&fk)
                .send()
                .await
                .map_err(|e| map_sdk_err(&e, "get"))?;
            let meta = resp.metadata();
            let meta_val = meta.and_then(|m| m.get(MTIME_KEY).map(String::as_str));
            let last = resp.last_modified().and_then(dt_millis);
            let content_len = resp.content_length();
            let mtime = decode_mtime(meta_val, last);
            let etag = resp.e_tag().map(|s| s.to_string());
            // A-H1/B-L3: count bytes actually written while streaming. A
            // clean-EOF truncated body with a correct Content-Length must be
            // rejected, not silently finalized (fail closed). Only enforced
            // when Content-Length is reported.
            let mut written: u64 = 0;
            let mut body = resp.body;
            while let Some(chunk) = body
                .try_next()
                .await
                .map_err(|e| classify_body_err(&format!("{e}")))?
            {
                w.write_all(&chunk).map_err(Error::Io)?;
                written += chunk.len() as u64;
            }
            if let Some(cl) = content_len {
                let cl = nonneg_size(cl);
                if written != cl {
                    return Err(Error::Other(format!(
                        "get: truncated body for {rel} (expected {cl}, got {written})"
                    )));
                }
            }
            // W30/N6/L2: report the true byte count. When Content-Length is
            // present it is authoritative (and equals written post-check); when
            // absent, the streamed count is used - never a bogus 0.
            let size = effective_get_size(content_len, written);
            Ok::<Entity, Error>(Entity {
                key: rel,
                size,
                mtime_ms: mtime,
                etag,
            })
        })?;
        Ok(entity)
    }

    fn put_from(
        &self,
        key: &str,
        r: &mut dyn Read,
        size: u64,
        mtime_ms: Option<u64>,
    ) -> Result<Entity, Error> {
        // Validate before any client call (trait N1).
        validate_put_key(key)?;
        let fk = full_key(&self.prefix, key);

        // Buffer the reader to a temp file on disk (never a size-sized Vec),
        // then stream that file to S3. Owner-only perms + create_new (W14).
        let (tmp, mut f) = create_temp_upload_file()?;
        let write_res = (|| -> Result<(), Error> {
            let copied = std::io::copy(&mut r.take(size), &mut f)?;
            if copied != size {
                return Err(Error::Other(format!(
                    "put_from: short read for {key} (expected {size}, got {copied})"
                )));
            }
            f.sync_all()?;
            Ok(())
        })();
        if let Err(e) = write_res {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        // L1-hygiene: drop the write handle before `from_path` re-opens the
        // temp for the upload (std-only handles already allow a concurrent
        // open on Windows, so this is intent documentation, not a fix - see
        // Refutation R-b).
        drop(f);

        let upload = (|| -> Result<Option<String>, Error> {
            let body_res = self.rt.block_on(async {
                ByteStream::from_path(&tmp)
                    .await
                    .map_err(|e| Error::Other(format!("put_from: read temp: {e:?}")))
            });
            let body = body_res?;
            let client = self.client.clone();
            let bucket = self.bucket.clone();
            self.rt.block_on(async move {
                let mut req = client.put_object().bucket(&bucket).key(&fk).body(body);
                if let Some(v) = encode_mtime(mtime_ms) {
                    req = req.metadata(MTIME_KEY, v);
                }
                let resp = req.send().await.map_err(|e| map_sdk_err(&e, "put"))?;
                // R5-L2/W44: return the S3 ETag from the put response so the
                // entity returned by `put_from` matches what `head`/`get_to`
                // report (they already populate etag; put_from did not).
                Ok::<Option<String>, Error>(resp.e_tag().map(String::from))
            })
        })();
        let _ = std::fs::remove_file(&tmp);
        let etag = upload?;

        Ok(Entity {
            key: key.to_string(),
            size,
            mtime_ms,
            etag,
        })
    }

    fn delete(&self, key: &str) -> Result<(), Error> {
        validate_object_key(key)?;
        let fk = full_key(&self.prefix, key);
        let client = self.client.clone();
        let bucket = self.bucket.clone();
        self.rt.block_on(async move {
            client
                .delete_object()
                .bucket(&bucket)
                .key(&fk)
                .send()
                .await
                .map_err(|e| map_sdk_err(&e, "delete"))?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_get_size_reports_streamed_bytes() {
        // W30/N6/L2: an absent Content-Length must yield the true streamed
        // count (never a bogus 0); a present CL is authoritative and, after
        // the caller's truncation check, equals `written`.
        assert_eq!(effective_get_size(Some(5), 5), 5);
        assert_eq!(effective_get_size(Some(0), 0), 0);
        assert_eq!(effective_get_size(None, 123), 123);
        // A present CL that disagrees is handled by the caller's truncation
        // error before this is used; an absent CL with a short body is not
        // measurable and the streamed count is the truth.
        assert_eq!(effective_get_size(Some(5), 4), 5);
        assert_eq!(effective_get_size(None, 0), 0);
    }

    #[test]
    fn s3_key_mapping_applies_prefix() {
        assert_eq!(full_key("myvault/", "notes/a.md"), "myvault/notes/a.md");
        assert_eq!(
            strip_prefix("myvault/", "myvault/notes/a.md"),
            Some("notes/a.md")
        );
        assert_eq!(strip_prefix("myvault/", "other/notes/a.md"), None);
        assert_eq!(full_key("", "notes/a.md"), "notes/a.md");
    }

    #[test]
    fn s3_prefix_empty_ok() {
        assert_eq!(full_key("", "a.md"), "a.md");
        assert_eq!(strip_prefix("", "a.md"), Some("a.md"));
    }

    #[test]
    fn s3_mtime_metadata_roundtrip() {
        assert_eq!(encode_mtime(Some(123)), Some("123".to_string()));
        assert_eq!(encode_mtime(None), None);
        // decode: metadata wins
        assert_eq!(
            decode_mtime(Some("1700000000123"), Some(999)),
            Some(1700000000123)
        );
        // no metadata -> LastModified
        assert_eq!(decode_mtime(None, Some(999)), Some(999));
        assert_eq!(decode_mtime(None, None), None);
        // garbage metadata -> fall back to LastModified
        assert_eq!(decode_mtime(Some("garbage"), Some(999)), Some(999));
    }

    #[test]
    fn s3_list_synthesizes_folders() {
        let (ents, dropped) = convert_listed(vec![
            ("notes/b.md".to_string(), 3, Some(100)),
            ("a.md".to_string(), 1, Some(200)),
        ]);
        assert_eq!(dropped, Vec::<(String, u64)>::new());
        let keys: Vec<&str> = ents.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["a.md", "notes/", "notes/b.md"]);
        let folder = ents.iter().find(|e| e.key == "notes/").unwrap();
        assert!(folder.is_folder());
        assert_eq!(folder.mtime_ms, None);
    }

    #[test]
    fn s3_list_drops_exact_prefix_folder_marker() {
        // R4-M2: a zero-byte "folder marker" object placed at the exact store
        // prefix (S3 console creates such keys when you Make Folder) strips to
        // an empty relative key and must be dropped - never planned as a
        // `""` entity that would fail `ensure_valid_key`.
        let (ents, dropped) = convert_listed(vec![("".to_string(), 0, Some(123))]);
        assert_eq!(ents, Vec::<Entity>::new(), "exact-prefix marker listed");
        assert_eq!(dropped, Vec::<(String, u64)>::new());
    }

    #[test]
    fn s3_list_prefix_scoping_keeps_rel_keys() {
        // convert_listed keeps vault-relative keys; folder synthesis is within
        // the returned set.
        let (ents, _) = convert_listed(vec![("notes/a.md".to_string(), 1, None)]);
        assert_eq!(ents.len(), 2);
        assert!(ents.iter().any(|e| e.key == "notes/" && e.is_folder()));
    }

    #[test]
    fn convert_listed_reports_dropped_nonempty_folder_keys() {
        // W70/A-N2: a trailing-`/` key carrying actual bytes is dropped from
        // the entities (object keys never end with '/') BUT reported, so the
        // S3 list surface can warn instead of hiding it; a zero-byte `*/` key
        // is the intended console folder marker and stays silent.
        let (ents, dropped) = convert_listed(vec![
            ("odd/".to_string(), 10, None),
            ("marker/".to_string(), 0, None),
            ("real.md".to_string(), 3, Some(1)),
        ]);
        assert!(
            !ents.iter().any(|e| e.key == "odd/" || e.key == "marker/"),
            "trailing-slash keys listed: {:?}",
            ents
        );
        assert_eq!(dropped, vec![("odd/".to_string(), 10)]);
        assert!(ents.iter().any(|e| e.key == "real.md"));
    }

    #[cfg(unix)]
    #[test]
    fn put_from_temp_file_is_owner_only() {
        // W14/A-L1/B-L1: the upload temp buffer is created 0600 (owner-only),
        // not the default 0666 & umask.
        use std::os::unix::fs::PermissionsExt;
        let (tmp, _f) = create_temp_upload_file().unwrap();
        let mode = std::fs::metadata(&tmp).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "upload temp perms must be 0600, got {mode:o}");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn s3store_new_normalizes_prefix() {
        // R5-M1: the constructor enforces the trailing-`/` invariant itself,
        // not trusting callers. `full_key` raw-concats the prefix, so an
        // unnormalized `"notes"` would silently map `a.md` -> `notesa.md`.
        for (input, expected) in [
            ("notes", "notes/"),
            ("notes/", "notes/"),
            ("a/b", "a/b/"),
            ("", ""),
        ] {
            let settings = StoreSettings {
                bucket: "bucket".to_string(),
                region: None,
                endpoint: None,
                prefix: input.to_string(),
                path_style: false,
            };
            let store = S3Store::new(&settings).unwrap();
            assert_eq!(store.prefix, expected, "input {input:?}");
        }
    }

    #[test]
    fn create_temp_upload_file_skips_stale_leftover() {
        // W29/N4: a stale leftover at an earlier candidate (a crashed run's
        // predictable `vaultsync-upload-<pid>-<n>` + pid reuse) must be
        // skipped untouched, not a hard failure; the fresh file is allocated at
        // the next free candidate.
        let c0 = std::env::temp_dir().join(format!(
            "vaultsync-upload-{}-W29-stale-0",
            std::process::id()
        ));
        let c1 = std::env::temp_dir().join(format!(
            "vaultsync-upload-{}-W29-stale-1",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&c0);
        let _ = std::fs::remove_file(&c1);
        std::fs::write(&c0, "stale").unwrap();
        let (tmp, _f) = alloc_first(&[c0.clone(), c1.clone()]).unwrap();
        assert_ne!(tmp, c0, "must skip the stale candidate");
        assert_eq!(tmp, c1, "fresh file allocated at the next candidate");
        assert_eq!(
            std::fs::read(&c0).unwrap(),
            b"stale",
            "stale leftover must be left untouched"
        );
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(&c0);
        let _ = std::fs::remove_file(&c1);
    }

    #[test]
    fn next_continuation_fails_closed_on_truncated_without_token() {
        // W61/A-M1: a truncated page must carry a continuation token. A
        // provider that reports truncation with an absent or empty token
        // (mis-paging R2/MinIO matrix target) would yield a partial listing
        // indistinguishable from a complete one - fail loudly, never break
        // silently and plan against a false remote view.
        assert!(matches!(
            next_continuation(Some(true), None),
            Err(Error::Other(_))
        ));
        assert!(matches!(
            next_continuation(Some(true), Some("")),
            Err(Error::Other(_))
        ));
    }

    #[test]
    fn next_continuation_continues_with_token() {
        // W61: a truncated page with a real token continues the loop.
        assert_eq!(
            next_continuation(Some(true), Some("tok")).unwrap(),
            Some("tok".to_string())
        );
    }

    #[test]
    fn next_continuation_ends_on_complete_page() {
        // W61: non-truncated pages (and providers that stay silent about
        // truncation, the AWS-absent case) end the loop unchanged.
        assert_eq!(next_continuation(Some(false), None).unwrap(), None);
        assert_eq!(next_continuation(Some(false), Some("tok")).unwrap(), None);
        assert_eq!(next_continuation(None, None).unwrap(), None);
    }

    #[test]
    fn nonneg_size_clamps_negative() {
        // W64/A-L6: head and listing sizes must never wrap a pathological
        // negative to a huge u64. The shared clamp mirrors effective_get_size's
        // `.max(0)` policy.
        assert_eq!(nonneg_size(-1), 0);
        assert_eq!(nonneg_size(-10_000), 0);
        assert_eq!(nonneg_size(0), 0);
        assert_eq!(nonneg_size(5), 5);
        assert_eq!(nonneg_size(i64::MAX), i64::MAX as u64);
    }

    #[test]
    fn dt_millis_saturates_pre_epoch() {
        // W8/A-M4/B-L2: pre-epoch LastModified (negative secs) saturates to
        // Some(0) like the local side, never wrapping to a huge u64.
        use aws_sdk_s3::primitives::DateTime;
        assert_eq!(dt_millis(&DateTime::from_secs(-1)), Some(0));
        assert_eq!(dt_millis(&DateTime::from_secs(-10_000)), Some(0));
        assert_eq!(dt_millis(&DateTime::from_secs(0)), Some(0));
        assert_eq!(
            dt_millis(&DateTime::from_secs(1_700_000_000)),
            Some(1_700_000_000_000)
        );
    }

    #[test]
    fn dt_millis_preserves_subsecond() {
        // R5-L7/W45: `dt_millis` discards subsecond `LastModified` (up to 999
        // ms). It must keep the millis so a plan at ms granularity does not
        // truncate every S3 LastModified to the second.
        use aws_sdk_s3::primitives::DateTime;
        assert_eq!(
            dt_millis(&DateTime::from_millis(1_700_000_000_123)),
            Some(1_700_000_000_123)
        );
        assert_eq!(
            dt_millis(&DateTime::from_secs_and_nanos(1_700_000_000, 999_000_000)),
            Some(1_700_000_000_999)
        );
    }

    #[test]
    fn s3_error_mapping() {
        // Amended for W9/A-M5/B-M4 (PR2): 5xx and 429 map to Unavailable
        // (previously Other); 404/401/403/timeout mappings unchanged.
        assert!(matches!(
            classify_error(false, Some(404), "x"),
            Error::NotFound(_)
        ));
        assert!(matches!(
            classify_error(false, Some(403), "x"),
            Error::Unauthorized(_)
        ));
        assert!(matches!(
            classify_error(false, Some(401), "x"),
            Error::Unauthorized(_)
        ));
        assert!(matches!(
            classify_error(true, Some(500), "x"),
            Error::Timeout(_)
        ));
        assert!(matches!(
            classify_error(false, Some(500), "x"),
            Error::Unavailable(_)
        ));
        assert!(matches!(
            classify_error(false, Some(503), "x"),
            Error::Unavailable(_)
        ));
        assert!(matches!(
            classify_error(false, Some(429), "x"),
            Error::Unavailable(_)
        ));
        // W65/A-L1: 408 (request timeout) is a transient retryable provider
        // signal, classified with the timeout family.
        assert!(matches!(
            classify_error(false, Some(408), "x"),
            Error::Timeout(_)
        ));
        assert!(matches!(
            classify_error(false, Some(599), "x"),
            Error::Unavailable(_)
        ));
        assert!(matches!(
            classify_error(false, Some(499), "x"),
            Error::Other(_)
        ));
        assert!(matches!(classify_error(false, None, "x"), Error::Other(_)));
    }

    #[test]
    fn get_body_error_is_unavailable() {
        // W65/A-L1: a mid-body stream failure happens after a successful
        // response header, so it is transport-level by construction and
        // transient in the Phase 3 retry sense - never a generic Other (the
        // old mapping hid the transient nature from a retrying caller).
        let e = classify_body_err("connection reset by peer");
        assert!(matches!(e, Error::Unavailable(_)), "got {e:?}");
        assert!(format!("{e}").contains("mid-body"));
    }

    #[test]
    fn s3_rejects_invalid_key_before_request() {
        for bad in ["../x", "/abs", "a/\nb.md"] {
            assert!(
                matches!(validate_put_key(bad), Err(Error::InvalidKey(_))),
                "{bad}"
            );
            assert!(validate_object_key(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn s3_put_rejects_folder_key() {
        assert!(matches!(
            validate_put_key("notes/"),
            Err(Error::InvalidKey(_))
        ));
        // object ops treat folder keys as NotFound (not objects)
        assert!(matches!(
            validate_object_key("notes/"),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn error_new_variants_display() {
        use crate::error::Error;
        assert!(format!("{}", Error::Unauthorized("u".into())).contains("credentials"));
        assert!(format!("{}", Error::Timeout("t".into())).contains("timed out"));
        assert!(format!("{}", Error::Unavailable("x".into())).contains("unavailable"));
    }
}
