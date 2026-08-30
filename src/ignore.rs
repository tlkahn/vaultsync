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

/// Compile one raw pattern into a [`Rule`]. Slash-free, metachar-free
/// patterns become basename-anywhere; trailing-slash patterns become a dir
/// prefix; other shapes are rejected until their work item lands (W181 exact
/// path, W182 segment `*`).
fn compile_pattern(pat: &str) -> Result<Rule, Error> {
    if pat.ends_with('/') {
        return Ok(Rule::DirPrefix(pat.to_string()));
    }
    let segments: Vec<&str> = pat.split('/').collect();
    if segments.len() == 1 && !pat.contains('*') {
        return Ok(Rule::Basename(pat.to_string()));
    }
    Err(Error::Other(
        "ignore pattern shape not yet implemented".to_string(),
    ))
}

impl Rule {
    fn matches(&self, key: &str) -> bool {
        match self {
            Rule::Basename(name) => final_segment(key) == name,
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
}
