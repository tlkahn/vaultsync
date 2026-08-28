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
        let rt = tokio::runtime::Builder::new_multi_thread()
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
            prefix: settings.prefix.clone(),
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
                        obj.size().map(|s| s as u64).unwrap_or(0),
                        last,
                    ));
                }
                match resp.next_continuation_token() {
                    Some(t) if !t.is_empty() => continuation = Some(t.to_string()),
                    _ => break,
                }
            }
            Ok(out)
        })
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
        Some((secs as u64).saturating_mul(1000))
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
/// with synthesized folder views (same shape as the mock).
fn convert_listed(items: Vec<(String, u64, Option<u64>)>) -> Vec<Entity> {
    let mut map: BTreeMap<String, Entity> = BTreeMap::new();
    for (key, size, mtime) in items {
        if key.ends_with('/') {
            continue; // object keys never end with '/'
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
    map.into_values().collect()
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
        // W9/A-M5/B-M4: 5xx and 429 (throttle) are transient provider
        // unavailability, not generic Other.
        Some(s) if s >= 500 || s == 429 => Error::Unavailable(msg.to_string()),
        _ => Error::Other(msg.to_string()),
    }
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

/// Create the upload temp buffer file. Owner-only `0o600` perms (W14/A-L1/
/// B-L1) and `create_new` (no reuse of an existing file, killing the
/// predictable-name race). On non-unix it is plain `create_new`.
fn create_temp_upload_file() -> Result<(PathBuf, std::fs::File), Error> {
    let tmp = temp_upload_path();
    #[cfg(unix)]
    let f = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(Error::Io)?
    };
    #[cfg(not(unix))]
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(Error::Io)?;
    Ok((tmp, f))
}

impl ObjectStore for S3Store {
    fn list(&self, prefix: &str) -> Result<Vec<Entity>, Error> {
        let raw = self.list_prefix_objects(prefix)?;
        Ok(convert_listed(raw))
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
            let size = resp.content_length().unwrap_or(0) as u64;
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
            let size = content_len.unwrap_or(0) as u64;
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
                .map_err(|e| Error::Other(format!("get: {e}")))?
            {
                w.write_all(&chunk).map_err(Error::Io)?;
                written += chunk.len() as u64;
            }
            if content_len.is_some() && written != size {
                return Err(Error::Other(format!(
                    "get: truncated body for {rel} (expected {size}, got {written})"
                )));
            }
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

        let upload = (|| -> Result<(), Error> {
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
                req.send().await.map_err(|e| map_sdk_err(&e, "put"))?;
                Ok::<(), Error>(())
            })
        })();
        let _ = std::fs::remove_file(&tmp);
        upload?;

        Ok(Entity {
            key: key.to_string(),
            size,
            mtime_ms,
            etag: None,
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
        let ents = convert_listed(vec![
            ("notes/b.md".to_string(), 3, Some(100)),
            ("a.md".to_string(), 1, Some(200)),
        ]);
        let keys: Vec<&str> = ents.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["a.md", "notes/", "notes/b.md"]);
        let folder = ents.iter().find(|e| e.key == "notes/").unwrap();
        assert!(folder.is_folder());
        assert_eq!(folder.mtime_ms, None);
    }

    #[test]
    fn s3_list_prefix_scoping_keeps_rel_keys() {
        // convert_listed keeps vault-relative keys; folder synthesis is within
        // the returned set.
        let ents = convert_listed(vec![("notes/a.md".to_string(), 1, None)]);
        assert_eq!(ents.len(), 2);
        assert!(ents.iter().any(|e| e.key == "notes/" && e.is_folder()));
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
