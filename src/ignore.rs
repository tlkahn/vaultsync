//! Pure ignore-set matcher (issue #30).
//!
//! Compiles `[ignore].patterns`-style strings into a reusable matcher over
//! vault-relative entity keys. Pure: no IO, no filesystem, no `Settings`.
//!
//! Pattern shapes (normative table in the issue #30 plan, doc/plans/issue-30):
//! - `name`: any key whose final segment equals `name` (basename-anywhere);
//! - `path/to/file`: exactly that key;
//! - `path/to/dir/`: that folder key and everything under it;
//! - a segment containing `*`: per-segment glob (`*` = any run of non-`/`
//!   chars, including empty; does not cross `/`);
//! - empty, leading `/`, `**`, `!`, `[`/`]`, `\`, `?`: loud rejects.
//!
//! Multiple patterns OR together; zero patterns match nothing.

use crate::error::Error;

/// A compiled set of ignore patterns (pure matcher).
///
/// Construct once with [`IgnoreSet::from_patterns`], then query repeatedly
/// with [`IgnoreSet::matches`]. No re-parsing happens after construction.
#[derive(Debug, Clone)]
pub struct IgnoreSet {
    rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
enum Rule {
    /// Final-segment equality (no `/` in pattern, no `*`).
    Basename(String),
    /// Full-key equality (has `/`, no trailing `/`, no `*`).
    Exact(String),
    /// Dir prefix: literal string including trailing `/` when no globs.
    DirPrefix(String),
}

impl IgnoreSet {
    /// Compile `patterns` into a reusable matcher.
    ///
    /// Empty input is valid (matches nothing). On an invalid pattern,
    /// `Err(Error::Other(..))` naming the offending pattern and the reason.
    pub fn from_patterns(patterns: &[String]) -> Result<Self, Error> {
        let mut rules = Vec::new();
        for pat in patterns {
            rules.push(compile_pattern(pat)?);
        }
        Ok(IgnoreSet { rules })
    }

    /// True when `key` (vault-relative entity key) is ignored by any pattern.
    pub fn matches(&self, key: &str) -> bool {
        self.rules.iter().any(|r| r.matches(key))
    }
}

/// Compile one raw pattern into a [`Rule`]. Validation is loud (every reject
/// names the offending pattern); the accepted shapes are basename-anywhere,
/// dir prefix, exact key, and (W182) per-segment `*`.
fn compile_pattern(pat: &str) -> Result<Rule, Error> {
    if pat.is_empty() {
        return Err(invalid(pat, "pattern must not be empty"));
    }
    if pat.starts_with('/') {
        return Err(invalid(pat, "leading '/' is not allowed"));
    }
    if pat.contains("**") {
        return Err(invalid(pat, "'**' is not supported"));
    }
    if pat.contains('!') {
        return Err(invalid(pat, "'!' (negation) is not supported"));
    }
    if pat.contains('\\') {
        return Err(invalid(pat, "'\\' (escape) is not supported"));
    }
    if pat.contains('[') || pat.contains(']') {
        return Err(invalid(
            pat,
            "character classes ('[' / ']') are not supported",
        ));
    }
    if pat.contains('?') {
        return Err(invalid(pat, "'?' is not supported"));
    }
    // A single trailing empty segment (the pattern ended with `/`) sets the
    // dir form and is not a match segment; any other empty segment rejects.
    let dir = pat.ends_with('/');
    let body = if dir { &pat[..pat.len() - 1] } else { pat };
    let segments: Vec<&str> = body.split('/').collect();
    for seg in &segments {
        if seg.is_empty() {
            return Err(invalid(pat, "pattern contains an empty path segment"));
        }
    }
    let has_star = segments.iter().any(|s| s.contains('*'));
    if dir {
        return Ok(Rule::DirPrefix(pat.to_string()));
    }
    if segments.len() == 1 && !has_star {
        return Ok(Rule::Basename(pat.to_string()));
    }
    if segments.len() > 1 && !has_star {
        return Ok(Rule::Exact(pat.to_string()));
    }
    Err(Error::Other(
        "ignore pattern shape not yet implemented".to_string(),
    ))
}

fn invalid(pat: &str, reason: &str) -> Error {
    Error::Other(format!("invalid ignore pattern {pat:?}: {reason}"))
}

impl Rule {
    fn matches(&self, key: &str) -> bool {
        match self {
            Rule::Basename(name) => final_segment(key) == name,
            Rule::Exact(s) => key == s,
            Rule::DirPrefix(prefix) => key == prefix || key.starts_with(prefix),
        }
    }
}

/// The final segment of a vault-relative key: strip **one** trailing `/`
/// (folder form), then the substring after the last `/` (or the whole key if
/// there is none). `.DS_Store/` -> `.DS_Store`; `notes/.DS_Store` ->
/// `.DS_Store`.
fn final_segment(key: &str) -> &str {
    let k = key.strip_suffix('/').unwrap_or(key);
    match k.rfind('/') {
        Some(i) => &k[i + 1..],
        None => k,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignore_set_empty_patterns_matches_nothing() {
        let set = IgnoreSet::from_patterns(&[]).unwrap();
        // Defensive: the empty key is not a normal entity key but must not
        // panic and must not be matched.
        assert!(!set.matches(""));
        assert!(!set.matches(".DS_Store"));
        assert!(!set.matches("notes/a.md"));
        assert!(!set.matches(".git/"));
    }

    #[test]
    fn ignore_set_basename_ds_store() {
        // Basename-anywhere: slash-free, metachar-free patterns match any key
        // whose final segment equals the name (folder form strips one
        // trailing `/` first).
        let cases: &[(&[&str], &str, bool)] = &[
            (&[".DS_Store"], ".DS_Store", true),
            (&[".DS_Store"], "notes/.DS_Store", true),
            (&[".DS_Store"], "a/b/.DS_Store", true),
            (&[".DS_Store"], ".DS_Store/", true),
            (&[".DS_Store"], "DS_Store.bak", false),
            (&[".DS_Store"], "notes/DS_Store.bak", false),
            (&[".DS_Store"], ".ds_store", false),
            (&[".DS_Store"], "notes/.DS_Store.bak", false),
            (&[".DS_Store"], "not-.DS_Store", false),
            (&[".DS_Store"], "notes/.DS_Store/extra", false),
            // Multi-pattern OR: either basename matches.
            (&[".DS_Store", "Thumbs.db"], "Thumbs.db", true),
            (&[".DS_Store", "Thumbs.db"], "notes/Thumbs.db", true),
            (&[".DS_Store", "Thumbs.db"], ".DS_Store", true),
            (&[".DS_Store", "Thumbs.db"], "notes/foo.md", false),
        ];
        for (patterns, key, expect) in cases {
            let set = set(patterns);
            assert_eq!(
                set.matches(key),
                *expect,
                "patterns {patterns:?} key {key:?}"
            );
        }
    }

    fn set(patterns: &[&str]) -> IgnoreSet {
        IgnoreSet::from_patterns(&patterns.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            .unwrap()
    }

    #[test]
    fn ignore_set_dir_prefix_git() {
        // Trailing-slash patterns are vault-rooted **path prefixes**, not
        // basenames: `.git/` ignores that folder and everything under it,
        // and never a sibling false friend (`.gitignore`, `.github/`, ...).
        let cases: &[(&[&str], &str, bool)] = &[
            (&[".git/"], ".git/", true),
            (&[".git/"], ".git/objects/aa", true),
            (&[".git/"], ".git/objects/aa/bb", true),
            (&[".git/"], ".gitignore", false),
            (&[".git/"], "git/", false),
            (&[".git/"], ".github/workflows/x", false),
            (&[".git/"], "foo.git/", false),
            (&[".trash/"], ".trash/", true),
            (&[".trash/"], ".trash/foo.md", true),
            (&[".trash/"], "not-trash.md", false),
            (&[".trash/"], "foo.trash", false),
            (&[".trash/"], ".trashfile", false),
            // Path-prefix, not basename: a nested `.trash/` is NOT ignored by
            // the vault-root pattern (unlike basename `.DS_Store`). Pin so
            // #32 cannot invent basename-dir behavior later.
            (&[".trash/"], "notes/.trash/", false),
        ];
        for (patterns, key, expect) in cases {
            let set = set(patterns);
            assert_eq!(
                set.matches(key),
                *expect,
                "patterns {patterns:?} key {key:?}"
            );
        }
    }

    #[test]
    fn ignore_set_exact_workspace_json() {
        // Slash-bearing, no trailing `/`, no `*`: exact full-key equality
        // only. No prefix behavior, no basename behavior.
        let cases: &[(&[&str], &str, bool)] = &[
            (
                &[".obsidian/workspace.json"],
                ".obsidian/workspace.json",
                true,
            ),
            (&[".obsidian/workspace.json"], ".obsidian/workspace", false),
            (
                &[".obsidian/workspace.json"],
                ".obsidian/workspace-mobile.json",
                false,
            ),
            (
                &[".obsidian/workspace.json"],
                ".obsidian/workspace.json/extra",
                false,
            ),
            (
                &[".obsidian/workspace.json"],
                ".obsidian/workspace.json/",
                false,
            ),
            (
                &[".obsidian/workspace.json"],
                "x/.obsidian/workspace.json",
                false,
            ),
            (&[".obsidian/workspace"], ".obsidian/workspace", true),
            (&[".obsidian/workspace"], ".obsidian/workspace.json", false),
        ];
        for (patterns, key, expect) in cases {
            let set = set(patterns);
            assert_eq!(
                set.matches(key),
                *expect,
                "patterns {patterns:?} key {key:?}"
            );
        }
    }

    #[test]
    fn ignore_set_rejects_doublestar_negation_empty_abs() {
        // Every reject is loud: Err naming the offending pattern (raw chars;
        // backslash appears Debug-escaped) plus a reason token.
        let cases: &[(&str, &[&str])] = &[
            ("", &["empty"]),
            ("/abs", &["leading", "/"]),
            ("/.DS_Store", &["leading", "/"]),
            ("**", &["**"]),
            ("./**/x", &["**"]),
            ("a/**/b", &["**"]),
            ("!foo", &["!", "negation"]),
            ("foo!", &["!"]),
            ("a/!b", &["!"]),
            ("foo[bar]", &["class", "["]),
            ("foo\\bar", &["escape", "\\"]),
            ("foo?", &["?"]),
            ("a//b", &["empty"]),
            // Dir-form variant of the empty-segment reject: `a//b/` strips
            // one trailing `/` then splits `a//b` -> interior empty segment.
            ("a//b/", &["empty"]),
            ("/", &["leading"]),
        ];
        for (pat, tokens) in cases {
            let err = IgnoreSet::from_patterns(&[pat.to_string()]).unwrap_err();
            let msg = err.to_string();
            let named = msg.contains(pat) || msg.contains(&pat.replace('\\', "\\\\"));
            assert!(named, "pattern {pat:?} not named in {msg:?}");
            for t in *tokens {
                assert!(msg.contains(t), "token {t:?} missing in {msg:?}");
            }
        }
    }

    #[test]
    fn ignore_set_invalid_second_pattern_names_it() {
        // A later invalid pattern still errors, naming the offending one
        // (the first, valid pattern compiles fine first).
        let err =
            IgnoreSet::from_patterns(&[".DS_Store".to_string(), "a//b".to_string()]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("a//b"), "second pattern not named: {msg:?}");
    }
}
