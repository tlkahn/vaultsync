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
pub struct IgnoreSet;

impl IgnoreSet {
    /// Compile `patterns` into a reusable matcher.
    ///
    /// Empty input is valid (matches nothing). On an invalid pattern,
    /// `Err(Error::Other(..))` naming the offending pattern and the reason.
    pub fn from_patterns(patterns: &[String]) -> Result<Self, Error> {
        if patterns.is_empty() {
            return Ok(IgnoreSet);
        }
        // W179+ compiles the individual pattern shapes; not reachable from
        // this commit's tests (empty-set seam only).
        Err(Error::Other(
            "ignore pattern compilation not yet implemented".to_string(),
        ))
    }

    /// True when `key` (vault-relative entity key) is ignored by any pattern.
    pub fn matches(&self, _key: &str) -> bool {
        false
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
}
