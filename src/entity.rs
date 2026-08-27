//! Vault-relative path descriptor used by the planner and the store trait.
//!
//! An [`Entity`] names a file or folder relative to the vault root. Folders
//! end with `/`; separators are `/`; there is never a leading `/`.

use crate::error::Error;

/// A vault-relative file or folder descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entity {
    /// Vault-relative key. No leading `/`. Folders end with `/`.
    pub key: String,
    /// Byte size; 0 for folders.
    pub size: u64,
    /// Client-visible mtime in ms since epoch, when known.
    pub mtime_ms: Option<u64>,
    /// Remote opaque token (etag) when known. Content-derived in the mock
    /// (FNV-1a); the planner treats it as opaque (Phase 1 never compares
    /// etags).
    pub etag: Option<String>,
}

impl Entity {
    /// True when this key names a folder (trailing `/`).
    pub fn is_folder(&self) -> bool {
        self.key.ends_with('/')
    }
}

/// Validate a vault-relative key.
///
/// Rejects the empty string, a leading `/`, or any backslash. Path segments
/// must not be `.`, `..`, or empty (double slash). A single trailing empty
/// segment from a folder key's final `/` is allowed (e.g. `notes/`).
pub fn ensure_valid_key(key: &str) -> Result<(), Error> {
    if key.is_empty() {
        return Err(Error::InvalidKey("key must not be empty".to_string()));
    }
    if key.starts_with('/') {
        return Err(Error::InvalidKey(format!(
            "key must not start with '/': {key:?}"
        )));
    }
    if key.contains('\\') {
        return Err(Error::InvalidKey(format!(
            "key must not contain backslash: {key:?}"
        )));
    }
    if key.chars().any(char::is_control) {
        return Err(Error::InvalidKey(format!(
            "key must not contain control characters: {key:?}"
        )));
    }

    let segments: Vec<&str> = key.split('/').collect();
    for (i, seg) in segments.iter().enumerate() {
        // A single trailing empty segment (folder key `foo/`) is allowed.
        if seg.is_empty() && i + 1 == segments.len() {
            continue;
        }
        let seg = *seg;
        if seg.is_empty() {
            return Err(Error::InvalidKey(format!(
                "key contains an empty path segment: {key:?}"
            )));
        }
        if seg.chars().all(char::is_whitespace) {
            return Err(Error::InvalidKey(format!(
                "key contains a whitespace-only path segment: {key:?}"
            )));
        }
        if seg == "." || seg == ".." {
            return Err(Error::InvalidKey(format!(
                "key contains a '.' or '..' path segment: {key:?}"
            )));
        }
    }
    Ok(())
}

/// Fixture constructor for a file entity.
pub fn file(key: &str, size: u64, mtime_ms: Option<u64>) -> Entity {
    Entity {
        key: key.to_string(),
        size,
        mtime_ms,
        etag: None,
    }
}

/// Fixture constructor for a folder entity. The trailing `/` is normalized.
pub fn folder(key: &str) -> Entity {
    let normalized = if key.ends_with('/') {
        key.to_string()
    } else {
        format!("{key}/")
    };
    Entity {
        key: normalized,
        size: 0,
        mtime_ms: None,
        etag: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_file_helpers() {
        let e = file("a.md", 10, Some(1000));
        assert_eq!(e.key, "a.md");
        assert_eq!(e.size, 10);
        assert_eq!(e.mtime_ms, Some(1000));
        assert_eq!(e.etag, None);
    }

    #[test]
    fn entity_folder_helper_trailing_slash() {
        assert_eq!(folder("notes").key, "notes/");
        assert_eq!(folder("notes/").key, "notes/");
    }

    #[test]
    fn entity_is_folder() {
        assert!(folder("notes").is_folder());
        assert!(!file("notes/a.md", 1, None).is_folder());
    }

    #[test]
    fn entity_reject_dot_segment() {
        let err = ensure_valid_key("foo/./bar").unwrap_err();
        assert!(matches!(err, Error::InvalidKey(_)));
    }

    #[test]
    fn entity_allows_dotfile_name() {
        assert!(ensure_valid_key(".obsidian/app.json").is_ok());
        assert!(ensure_valid_key(".gitignore").is_ok());
    }

    #[test]
    fn entity_reject_empty_segment() {
        let err = ensure_valid_key("foo//bar").unwrap_err();
        assert!(matches!(err, Error::InvalidKey(_)));
    }

    #[test]
    fn entity_reject_dot_match() {
        assert!(ensure_valid_key("foo/.").is_err());
        assert!(ensure_valid_key("foo/..").is_err());
    }

    #[test]
    fn entity_folder_trailing_slash_ok() {
        assert!(ensure_valid_key("notes/").is_ok());
    }

    #[test]
    fn entity_reject_dotdot_segment() {
        let err = ensure_valid_key("foo/../bar.md").unwrap_err();
        assert!(matches!(err, Error::InvalidKey(_)));
    }

    #[test]
    fn entity_reject_dotdot_only() {
        let err = ensure_valid_key("..").unwrap_err();
        assert!(matches!(err, Error::InvalidKey(_)));
    }

    #[test]
    fn entity_reject_dotdot_prefix() {
        let err = ensure_valid_key("../x").unwrap_err();
        assert!(matches!(err, Error::InvalidKey(_)));
    }

    #[test]
    fn entity_reject_leading_slash() {
        let err = ensure_valid_key("/a").unwrap_err();
        assert!(matches!(err, Error::InvalidKey(_)));
    }

    #[test]
    fn entity_reject_backslash() {
        let err = ensure_valid_key("a\\b").unwrap_err();
        assert!(matches!(err, Error::InvalidKey(_)));
    }

    #[test]
    fn entity_reject_control_chars() {
        // Control characters (NUL, LF, CR, TAB, DEL, ...) anywhere in the key
        // are rejected: they break human output rows and are never needed in
        // vault keys.
        for key in ["a/\nb", "a/\tb.md", "a/\rb", "a\u{0}b", "a\u{7f}b"] {
            let err = ensure_valid_key(key).unwrap_err();
            assert!(matches!(err, Error::InvalidKey(_)), "key {key:?}");
        }
    }

    #[test]
    fn entity_reject_whitespace_only_segment() {
        // A segment consisting entirely of whitespace is rejected (invisible
        // key component). Segments with leading/trailing spaces but real
        // content stay valid (real vaults contain them; S3 stores them fine).
        for key in ["a/ /b", " /a.md", "a/ ", "\t"] {
            let err = ensure_valid_key(key).unwrap_err();
            assert!(matches!(err, Error::InvalidKey(_)), "key {key:?}");
        }
        assert!(ensure_valid_key("a/ b.md").is_ok());
        assert!(ensure_valid_key("a/b ").is_ok());
    }

    #[test]
    fn entity_reject_empty() {
        let err = ensure_valid_key("").unwrap_err();
        assert!(matches!(err, Error::InvalidKey(_)));
    }
}
