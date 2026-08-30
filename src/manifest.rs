//! Pure manifest codec + validation + entity mapping (issue 45, P2).
//!
//! No IO: this module parses/serializes the `.vaultsync/manifest/v1.json`
//! body and maps entries to/from [`Entity`]. The inventory facade owns the
//! store calls; this module is byte-level pure.

use crate::entity::ensure_valid_key;
use crate::error::Error;
use serde::{Deserialize, Serialize};

/// The only accepted schema id (issue 45, D-schema). Unknown schema =>
/// reject.
pub const MANIFEST_SCHEMA: &str = "vaultsync.manifest.v1";

/// Soft cap on a manifest body in bytes (issue 45, Q5 / D-config): parse and
/// read refuse a manifest larger than this, loudly, to bound memory on
/// pathological stores.
pub const MANIFEST_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// A parsed manifest body (v1 schema).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManifestV1 {
    pub schema: String,
    pub created_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    pub entry_count: usize,
    pub entries: Vec<ManifestEntry>,
}

/// One file entry. `mtime_ms` is JSON `null` when unknown (maps to `None`);
/// `etag` is optional.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub key: String,
    pub size: u64,
    pub mtime_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

/// Parse + validate a manifest body. Rejects a body above the soft byte cap
/// (Q5) and unknown schemas (D-schema); full entry validation (count, keys,
/// duplicates) lands in W227/W228. Entries are sorted by key after parse so
/// callers get a stable mapping regardless of writer order (D-order).
pub fn parse_manifest_bytes(bytes: &[u8]) -> Result<ManifestV1, Error> {
    parse_manifest_bytes_with_cap(bytes, MANIFEST_MAX_BYTES)
}

/// [`parse_manifest_bytes`] with an injectable cap (pub(crate) so the soft
/// cap is testable without a 64 MiB fixture, W228).
pub(crate) fn parse_manifest_bytes_with_cap(bytes: &[u8], cap: u64) -> Result<ManifestV1, Error> {
    if bytes.len() as u64 > cap {
        return Err(Error::Other(format!(
            "manifest body is {} bytes, above the {} byte soft cap; refusing to parse (run vaultsync repair to rebuild it)",
            bytes.len(),
            cap
        )));
    }
    let m: ManifestV1 = serde_json::from_slice(bytes).map_err(|e| {
        Error::Other(format!(
            "inventory manifest is corrupt (JSON parse failed): {e}"
        ))
    })?;
    if m.schema != MANIFEST_SCHEMA {
        return Err(Error::Other(format!(
            "inventory manifest has unknown schema {:?} (expected {MANIFEST_SCHEMA}); run vaultsync repair to rebuild it",
            m.schema
        )));
    }
    // W227: `entry_count` must equal `entries.len()` - a mismatch is a
    // corrupt body (truncated writer or hand-edit), never a silent
    // reinterpretation of the inventory.
    if m.entry_count != m.entries.len() {
        return Err(Error::Other(format!(
            "inventory manifest is corrupt: entry_count {} != {} entries; run vaultsync repair to rebuild it",
            m.entry_count,
            m.entries.len()
        )));
    }
    // W228 (fail-closed): every entry must be a valid FILE key - no folder
    // rows (D-folders: folder views are synthesized, never stored), no
    // invalid keys, no control-plane keys (D-reserved: the writer strips
    // `.vaultsync/**`; a reader seeing one treats the body as corrupt).
    let mut seen = std::collections::BTreeSet::new();
    for e in &m.entries {
        if e.key.ends_with('/') {
            return Err(Error::Other(format!(
                "inventory manifest is corrupt: folder entry {:?} (folder rows are not stored); run vaultsync repair to rebuild it",
                e.key
            )));
        }
        ensure_valid_key(&e.key)?;
        if crate::local::is_vaultsync_control_plane_key(&e.key) {
            return Err(Error::Other(format!(
                "inventory manifest is corrupt: reserved entry {:?} (control-plane keys are not stored); run vaultsync repair to rebuild it",
                e.key
            )));
        }
        if !seen.insert(e.key.clone()) {
            return Err(Error::Other(format!(
                "inventory manifest is corrupt: duplicate entry key {:?}; run vaultsync repair to rebuild it",
                e.key
            )));
        }
    }
    // D-order: readers accept any order and sort after parse (writers must
    // emit sorted keys; readers normalize so the mapping is stable).
    let mut m = m;
    m.entries.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parse_entry_point_exists() {
        // W226 (issue 45): the manifest codec's parse entry point. RED today:
        // `parse_manifest_bytes` does not exist (compile failure).
        let body =
            br#"{"schema":"vaultsync.manifest.v1","created_ms":0,"entry_count":0,"entries":[]}"#;
        let m = parse_manifest_bytes(body).unwrap();
        assert_eq!(m.schema, MANIFEST_SCHEMA);
        assert!(m.entries.is_empty());
    }

    #[test]
    fn manifest_parse_happy_path_with_entries() {
        // W227 (issue 45): a valid compact body with two entries parses, and
        // `entry_count` agrees with `entries.len()`. Entries are re-sorted by
        // key after parse (writers emit sorted keys; readers accept any order
        // and normalize - D-order).
        let body = br#"{"schema":"vaultsync.manifest.v1","created_ms":1735689600000,"generator":"vaultsync 0.1.0","prefix":"vault/","entry_count":2,"entries":[{"key":"notes/b.md","size":2,"mtime_ms":200,"etag":"\"b\""},{"key":"a.md","size":1,"mtime_ms":100,"etag":"\"a\""}]}"#;
        let m = parse_manifest_bytes(body).unwrap();
        assert_eq!(m.schema, MANIFEST_SCHEMA);
        assert_eq!(m.created_ms, 1735689600000);
        assert_eq!(m.generator.as_deref(), Some("vaultsync 0.1.0"));
        assert_eq!(m.prefix.as_deref(), Some("vault/"));
        assert_eq!(m.entry_count, 2);
        assert_eq!(m.entries.len(), 2);
        let keys: Vec<&str> = m.entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["a.md", "notes/b.md"], "entries must be sorted");
        assert_eq!(m.entries[0].size, 1);
        assert_eq!(m.entries[0].mtime_ms, Some(100));
        assert_eq!(m.entries[0].etag.as_deref(), Some("\"a\""));
    }

    #[test]
    fn manifest_parse_rejects_unknown_schema() {
        // W227 (issue 45, D-schema): only `vaultsync.manifest.v1` is
        // accepted; an unknown schema is rejected, never silently treated as
        // empty inventory.
        let body =
            br#"{"schema":"vaultsync.manifest.v2","created_ms":0,"entry_count":0,"entries":[]}"#;
        let err = parse_manifest_bytes(body).unwrap_err();
        assert!(
            format!("{err}").contains("unknown schema"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn manifest_parse_rejects_entry_count_mismatch() {
        // W227 (issue 45): `entry_count` must equal `entries.len()` - a
        // mismatch means the body is corrupt (truncated writer or hand-edit).
        let body = br#"{"schema":"vaultsync.manifest.v1","created_ms":0,"entry_count":3,"entries":[{"key":"a.md","size":1,"mtime_ms":100}]}"#;
        let err = parse_manifest_bytes(body).unwrap_err();
        assert!(
            format!("{err}").contains("entry_count"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn manifest_parse_rejects_json_corruption() {
        // W227 (issue 45): non-JSON bytes are a loud corrupt-manifest error
        // (auto mode falls back cold; strict mode fails the run).
        let err = parse_manifest_bytes(b"not json at all").unwrap_err();
        assert!(format!("{err}").contains("corrupt"), "unexpected: {err}");
    }

    #[test]
    fn manifest_parse_rejects_duplicate_keys() {
        // W228 (issue 45): duplicate entry keys fail closed - a reader must
        // never silently last-win on a hand-corrupted body.
        let body = br#"{"schema":"vaultsync.manifest.v1","created_ms":0,"entry_count":2,"entries":[{"key":"a.md","size":1,"mtime_ms":100},{"key":"a.md","size":2,"mtime_ms":200}]}"#;
        let err = parse_manifest_bytes(body).unwrap_err();
        assert!(format!("{err}").contains("duplicate"), "unexpected: {err}");
    }

    #[test]
    fn manifest_parse_rejects_folder_key_entry() {
        // W228 (issue 45, D-folders): folder rows are never stored in the
        // manifest - folder views are synthesized from file keys. A trailing
        // `/` entry is reject.
        let body = br#"{"schema":"vaultsync.manifest.v1","created_ms":0,"entry_count":1,"entries":[{"key":"notes/","size":0,"mtime_ms":null}]}"#;
        let err = parse_manifest_bytes(body).unwrap_err();
        assert!(format!("{err}").contains("folder"), "unexpected: {err}");
    }

    #[test]
    fn manifest_parse_rejects_invalid_entry_key() {
        // W228 (issue 45): entries must pass `ensure_valid_key` (a manifest
        // that escaped the writer's validation is corrupt).
        for key in ["../x", "/abs", "a//b", "a/\nb"] {
            let body = format!(
                r#"{{"schema":"vaultsync.manifest.v1","created_ms":0,"entry_count":1,"entries":[{{"key":{key:?},"size":1,"mtime_ms":100}}]}}"#
            );
            let err = parse_manifest_bytes(body.as_bytes()).unwrap_err();
            assert!(
                matches!(err, Error::InvalidKey(_)),
                "key {key:?} must be rejected, got {err:?}"
            );
        }
    }

    #[test]
    fn manifest_parse_rejects_control_plane_entry() {
        // W228 (issue 45, D-reserved): the manifest never stores control-
        // plane keys as entries (the writer strips them; a reader seeing one
        // fails closed).
        let body = br#"{"schema":"vaultsync.manifest.v1","created_ms":0,"entry_count":1,"entries":[{"key":".vaultsync/manifest/v1.json","size":1,"mtime_ms":100}]}"#;
        let err = parse_manifest_bytes(body).unwrap_err();
        assert!(format!("{err}").contains("reserved"), "unexpected: {err}");
    }

    #[test]
    fn manifest_parse_rejects_over_soft_cap() {
        // W228 (issue 45, Q5): a body above the soft byte cap is refused
        // loudly, without parsing. Uses the injectable cap (no 64 MiB
        // fixture).
        let body =
            br#"{"schema":"vaultsync.manifest.v1","created_ms":0,"entry_count":0,"entries":[]}"#;
        let err = parse_manifest_bytes_with_cap(body, 8).unwrap_err();
        assert!(format!("{err}").contains("soft cap"), "unexpected: {err}");
    }
}
