//! Pure manifest codec + validation + entity mapping (issue 45, P2).
//!
//! No IO: this module parses/serializes the `.vaultsync/manifest/v1.json`
//! body and maps entries to/from [`Entity`]. The inventory facade owns the
//! store calls; this module is byte-level pure.

use crate::entity::{Entity, ensure_valid_key};
use crate::error::Error;
use serde::{Deserialize, Serialize};

/// The only accepted schema id (issue 45, D-schema). Unknown schema =>
/// reject.
pub const MANIFEST_SCHEMA: &str = "vaultsync.manifest.v1";

/// Soft cap on a manifest body in bytes (issue 45, Q5 / D-config): parse and
/// read refuse a manifest larger than this, loudly, to bound memory on
/// pathological stores.
pub const MANIFEST_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// A parsed manifest body (v1 schema). v1 is a CLOSED schema: unknown
/// fields fail closed (`deny_unknown_fields`, W256/L1 review 5472028291) so
/// a typo'd hand-edit never silently drops data; the extension valve is the
/// schema id (a future `vaultsync.manifest.v2` rejects here).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
/// `etag` is optional. Closed schema (`deny_unknown_fields`, W256/L1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

/// Serialize a manifest body as compact JSON with entries sorted by key
/// ascending (W229, D-order: writers must emit sorted keys).
pub fn serialize_manifest(m: &ManifestV1) -> Result<Vec<u8>, Error> {
    let mut m = m.clone();
    m.entries.sort_by(|a, b| a.key.cmp(&b.key));
    serde_json::to_vec(&m)
        .map_err(|e| Error::Other(format!("failed to serialize inventory manifest: {e}")))
}

/// Ancestor folder keys (each trailing-`/` prefix) of a key. The single
/// shared implementation (W230, D-folders): the mock, the S3 backend, and
/// the manifest folder synthesis all use this one function so the folder
/// views cannot drift between warm (manifest) and cold (list) planning.
/// `parent_folders("notes/deep/c.md")` => `["notes/", "notes/deep/"]`.
pub(crate) fn parent_folders(key: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, b) in key.bytes().enumerate() {
        if b == b'/' {
            out.push(key[..=i].to_string());
        }
    }
    out
}

/// Synthesize folder-view entities from file entities (W230, D-folders):
/// every ancestor prefix of every file key, sorted, with folder shape
/// (trailing `/`, size 0, no mtime/etag). Same algorithm as the store
/// listing's folder synthesis so warm == cold plan parity holds. Duplicate
/// folder keys collapse (a set), so a file set maps to unique folders.
pub(crate) fn synthesize_folders(files: &[Entity]) -> Vec<Entity> {
    let mut folders: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for f in files {
        for p in parent_folders(&f.key) {
            folders.insert(p);
        }
    }
    folders
        .into_iter()
        .map(|key| Entity {
            key,
            size: 0,
            mtime_ms: None,
            etag: None,
        })
        .collect()
}

/// Map parsed manifest entries to file `Entity` rows (W230, 5.5). Folder
/// views are NOT included - callers combine with [`synthesize_folders`] for
/// the planner input shape. Parsing already validated keys/count/dups; this
/// mapping is infallible for a parsed manifest.
pub fn manifest_to_file_entities(m: &ManifestV1) -> Result<Vec<Entity>, Error> {
    let mut out = Vec::with_capacity(m.entries.len());
    for e in &m.entries {
        out.push(Entity {
            key: e.key.clone(),
            size: e.size,
            mtime_ms: e.mtime_ms,
            etag: e.etag.clone(),
        });
    }
    out.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(out)
}

/// Build a [`ManifestV1`] from file entities (W230). Rejects folder
/// entities (D-folders: never stored), validates every key, rejects
/// duplicates, and sorts by key ascending (D-order). `created_ms` is the
/// writer's wall clock (diagnostic only); `generator`/`prefix` are optional
/// documentation fields.
pub fn file_entities_to_manifest(
    files: &[Entity],
    created_ms: u64,
    generator: Option<String>,
    prefix: Option<String>,
) -> Result<ManifestV1, Error> {
    let mut entries: Vec<ManifestEntry> = Vec::with_capacity(files.len());
    let mut seen = std::collections::BTreeSet::new();
    for f in files {
        if f.is_folder() {
            return Err(Error::Other(format!(
                "cannot write folder entity {:?} into the manifest (folder rows are not stored)",
                f.key
            )));
        }
        ensure_valid_key(&f.key)?;
        if crate::local::is_vaultsync_control_plane_key(&f.key) {
            return Err(Error::Other(format!(
                "cannot write reserved key {:?} into the manifest (control-plane keys are not stored)",
                f.key
            )));
        }
        if !seen.insert(f.key.clone()) {
            return Err(Error::Other(format!(
                "duplicate key {:?} in manifest writer input",
                f.key
            )));
        }
        entries.push(ManifestEntry {
            key: f.key.clone(),
            size: f.size,
            mtime_ms: f.mtime_ms,
            etag: f.etag.clone(),
        });
    }
    entries.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(ManifestV1 {
        schema: MANIFEST_SCHEMA.to_string(),
        created_ms,
        generator,
        prefix,
        entry_count: entries.len(),
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ObjectStore;

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
    fn parse_rejects_unknown_entry_field() {
        // W256 (L1, review 5472028291): a typo'd / unknown entry field
        // (e.g. `mTime_ms` instead of `mtime_ms`) must fail closed instead
        // of silently dropping the field. RED until `deny_unknown_fields`.
        let body = br#"{"schema":"vaultsync.manifest.v1","created_ms":0,"entry_count":1,"entries":[{"key":"a.md","size":1,"mTime_ms":100}]}"#;
        let err = parse_manifest_bytes(body).unwrap_err();
        assert!(
            format!("{err}").contains("unknown field"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn parse_rejects_unknown_top_level_field() {
        // W256 (L1, review 5472028291): an unknown top-level field fails
        // closed the same way (v1 is a closed schema; the extension valve is
        // the schema id).
        let body = br#"{"schema":"vaultsync.manifest.v1","created_ms":0,"entry_count":0,"entries":[],"version":2}"#;
        let err = parse_manifest_bytes(body).unwrap_err();
        assert!(
            format!("{err}").contains("unknown field"),
            "unexpected: {err}"
        );
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

    #[test]
    fn manifest_null_mtime_maps_to_none_and_etag_optional() {
        // W229 (issue 45, Q4): `mtime_ms: null` maps to `Entity.mtime_ms =
        // None` (classified by existing unknown-mtime rules); `etag` may be
        // omitted entirely (first entry) or present (second entry).
        let body = br#"{"schema":"vaultsync.manifest.v1","created_ms":0,"entry_count":2,"entries":[{"key":"a.md","size":1,"mtime_ms":null},{"key":"b.md","size":2,"mtime_ms":200,"etag":"\"b\""}]}"#;
        let m = parse_manifest_bytes(body).unwrap();
        assert_eq!(m.entries[0].mtime_ms, None);
        assert_eq!(m.entries[0].etag, None);
        assert_eq!(m.entries[1].mtime_ms, Some(200));
        assert_eq!(m.entries[1].etag.as_deref(), Some("\"b\""));
    }

    #[test]
    fn manifest_serialize_round_trip_compact_sorted() {
        // W229 (issue 45): writers serialize compact JSON with entries sorted
        // by key ascending; the round-trip preserves every field, including
        // null mtime and absent etag (which stay absent, not `null`).
        let m = ManifestV1 {
            schema: MANIFEST_SCHEMA.to_string(),
            created_ms: 1234,
            generator: Some("vaultsync 0.1.0".to_string()),
            prefix: None,
            entry_count: 2,
            entries: vec![
                ManifestEntry {
                    key: "z.md".to_string(),
                    size: 3,
                    mtime_ms: Some(300),
                    etag: None,
                },
                ManifestEntry {
                    key: "a.md".to_string(),
                    size: 1,
                    mtime_ms: None,
                    etag: Some("\"a\"".to_string()),
                },
            ],
        };
        let bytes = serialize_manifest(&m).unwrap();
        // compact: no pretty whitespace
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(!text.contains('\n'), "compact JSON, got: {text}");
        let back = parse_manifest_bytes(&bytes).unwrap();
        assert_eq!(back.schema, MANIFEST_SCHEMA);
        assert_eq!(back.created_ms, 1234);
        assert_eq!(back.generator.as_deref(), Some("vaultsync 0.1.0"));
        assert_eq!(back.prefix, None);
        // sorted by key ascending after round-trip
        let keys: Vec<&str> = back.entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["a.md", "z.md"]);
        assert_eq!(back.entries[0].mtime_ms, None);
        assert_eq!(back.entries[0].etag.as_deref(), Some("\"a\""));
        assert_eq!(back.entries[1].size, 3);
        assert_eq!(back.entries[1].mtime_ms, Some(300));
        assert_eq!(back.entries[1].etag, None);
    }

    #[test]
    fn manifest_to_file_entities_maps_fields() {
        // W230 (issue 45, 5.5): entries map 1:1 to file `Entity` rows (no
        // folder synthesis here - that is `synthesize_folders`).
        let body = br#"{"schema":"vaultsync.manifest.v1","created_ms":0,"entry_count":2,"entries":[{"key":"a.md","size":1,"mtime_ms":null},{"key":"notes/b.md","size":2,"mtime_ms":200,"etag":"\"b\""}]}"#;
        let m = parse_manifest_bytes(body).unwrap();
        let ents = manifest_to_file_entities(&m).unwrap();
        assert_eq!(ents.len(), 2);
        assert!(!ents.iter().any(|e| e.is_folder()));
        let a = ents.iter().find(|e| e.key == "a.md").unwrap();
        assert_eq!(a.size, 1);
        assert_eq!(a.mtime_ms, None);
        assert_eq!(a.etag, None);
        let b = ents.iter().find(|e| e.key == "notes/b.md").unwrap();
        assert_eq!(b.size, 2);
        assert_eq!(b.mtime_ms, Some(200));
        assert_eq!(b.etag.as_deref(), Some("\"b\""));
    }

    #[test]
    fn synthesize_folders_matches_list_ethos() {
        // W230 (issue 45, D-folders): folder views are synthesized from file
        // keys with the SAME algorithm the mock/s3 listing uses
        // (`parent_folders`), so planners see identical folder Skip behavior
        // warm vs cold. Parity pin: for the same file set, the synthesized
        // folders equal the folder entities MemoryStore::list would produce.
        let files = vec![
            crate::entity::file("a.md", 1, Some(1)),
            crate::entity::file("notes/b.md", 2, Some(2)),
            crate::entity::file("notes/deep/c.md", 3, Some(3)),
        ];
        let folders = synthesize_folders(&files);
        let mut folder_keys: Vec<String> = folders.iter().map(|f| f.key.clone()).collect();
        folder_keys.sort();
        assert_eq!(
            folder_keys,
            vec!["notes/".to_string(), "notes/deep/".to_string()],
            "folder views: {folder_keys:?}"
        );
        assert!(folders.iter().all(|f| f.is_folder() && f.size == 0));
        // Parity: MemoryStore list of the same keys yields the same folder
        // keys (folders-only projection).
        let store = crate::store::mock::MemoryStore::new();
        for f in &files {
            let mut c = std::io::Cursor::new(vec![0u8; f.size as usize]);
            store.put_from(&f.key, &mut c, f.size, f.mtime_ms).unwrap();
        }
        let listed_folders: Vec<String> = store
            .list("")
            .unwrap()
            .entities
            .into_iter()
            .filter(|e| e.is_folder())
            .map(|e| e.key)
            .collect();
        assert_eq!(
            folder_keys, listed_folders,
            "folder synthesis must match the list ethos"
        );
    }

    #[test]
    fn file_entities_to_manifest_round_trip() {
        // W230 (issue 45): `file_entities_to_manifest` builds a valid body
        // from file entities (sorted, validated, no folders) that parses
        // back to the same file rows.
        let files = vec![
            crate::entity::file("b.md", 2, Some(200)),
            crate::entity::file("a.md", 1, None),
        ];
        let m = file_entities_to_manifest(&files, 42, Some("vaultsync 0.1.0".to_string()), None)
            .unwrap();
        assert_eq!(m.entry_count, 2);
        assert_eq!(m.created_ms, 42);
        assert_eq!(m.generator.as_deref(), Some("vaultsync 0.1.0"));
        let bytes = serialize_manifest(&m).unwrap();
        let back = parse_manifest_bytes(&bytes).unwrap();
        let ents = manifest_to_file_entities(&back).unwrap();
        let keys: Vec<&str> = ents.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["a.md", "b.md"], "sorted by key");
        assert_eq!(ents[0].mtime_ms, None);
        assert_eq!(ents[1].mtime_ms, Some(200));
    }

    #[test]
    fn file_entities_to_manifest_rejects_folder_entities() {
        // W230 (issue 45): folder entities must never be written into the
        // manifest (D-folders) - the builder refuses them.
        let files = vec![crate::entity::folder("notes")];
        let err = file_entities_to_manifest(&files, 0, None, None).unwrap_err();
        assert!(format!("{err}").contains("folder"), "unexpected: {err}");
    }
}
