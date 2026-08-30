//! Pure manifest codec + validation + entity mapping (issue 45, P2).
//!
//! No IO: this module parses/serializes the `.vaultsync/manifest/v1.json`
//! body and maps entries to/from [`Entity`]. The inventory facade owns the
//! store calls; this module is byte-level pure.

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
}
