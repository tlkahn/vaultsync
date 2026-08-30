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
//! - empty, leading `/`, `**`, `!`, `[`/`]`, `\`, `?`, `.` / `..` segments,
//!   whitespace-only segments, and control characters: loud rejects (dotfile
//!   *names* like `.DS_Store` / `.git/` stay valid).
//!
//! Multiple patterns OR together; zero patterns match nothing.

use crate::error::Error;

/// A compiled set of ignore patterns (pure matcher).
///
/// Construct once with [`IgnoreSet::from_patterns`], then query repeatedly
/// with [`IgnoreSet::matches`]. No re-parsing happens after construction.
///
/// This issue ships only the pure matcher (issue #30); application - walk
/// prune (#32), remote filter (#33), W25 retirement (#34) - is later epic #9
/// work that filters entity lists through this type before planning.
#[derive(Debug, Clone)]
pub struct IgnoreSet {
    rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
enum Rule {
    /// Final-segment equality (no `/` in pattern, no `*`).
    Basename(String),
    /// Final-segment glob (no `/` in pattern, has `*`).
    BasenameGlob(SegmentGlob),
    /// Full-key equality (has `/`, no trailing `/`, no `*`).
    Exact(String),
    /// Segment zip (has `/`, no trailing `/`, has `*`).
    ExactSegs(Vec<Segment>),
    /// Dir prefix: literal string including trailing `/` when no globs.
    DirPrefix(String),
    /// Dir prefix with per-segment globs.
    DirPrefixSegs(Vec<Segment>),
}

#[derive(Debug, Clone)]
enum Segment {
    Exact(String),
    Glob(SegmentGlob),
}

/// Pre-split on `*` so match is allocation-light at query time.
#[derive(Debug, Clone)]
struct SegmentGlob {
    /// Literal pieces between `*` wildcards (len = star_count + 1).
    parts: Vec<String>,
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
    ///
    /// `key` should already be a vault-relative entity key accepted by
    /// [`crate::entity::ensure_valid_key`]. This matcher is pure
    /// string/segment matching and does **not** re-validate keys (e.g. empty
    /// segments or `..` components are not rejected here).
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
    // Control characters anywhere in the raw pattern (parity with
    // ensure_valid_key), checked pre-split so e.g. "a/\nb" fails on
    // `control` rather than a confusing segment split.
    if pat.chars().any(char::is_control) {
        return Err(invalid(pat, "control characters are not allowed"));
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
        if *seg == "." || *seg == ".." {
            return Err(invalid(pat, "'.' or '..' path segments are not allowed"));
        }
        if seg.chars().all(char::is_whitespace) {
            return Err(invalid(pat, "whitespace-only path segment is not allowed"));
        }
    }
    let has_star = segments.iter().any(|s| s.contains('*'));
    let segments: Vec<Segment> = segments
        .iter()
        .map(|s| {
            if s.contains('*') {
                Segment::Glob(SegmentGlob::from(s))
            } else {
                Segment::Exact(s.to_string())
            }
        })
        .collect();
    if dir {
        return Ok(if has_star {
            Rule::DirPrefixSegs(segments)
        } else {
            Rule::DirPrefix(pat.to_string())
        });
    }
    if !has_star {
        return Ok(if segments.len() == 1 {
            Rule::Basename(pat.to_string())
        } else {
            Rule::Exact(pat.to_string())
        });
    }
    if segments.len() == 1 {
        // Reuse the already-built `Segment` instead of re-splitting `pat`.
        match segments.into_iter().next().unwrap() {
            Segment::Glob(g) => Ok(Rule::BasenameGlob(g)),
            Segment::Exact(s) => Ok(Rule::Basename(s)), // defensive; has_star makes this unreachable
        }
    } else {
        Ok(Rule::ExactSegs(segments))
    }
}

fn invalid(pat: &str, reason: &str) -> Error {
    Error::Other(format!("invalid ignore pattern {pat:?}: {reason}"))
}

impl Rule {
    fn matches(&self, key: &str) -> bool {
        match self {
            Rule::Basename(name) => final_segment(key) == name,
            Rule::BasenameGlob(glob) => segment_glob_matches(final_segment(key), glob),
            Rule::Exact(s) => key == s,
            Rule::ExactSegs(segs) => exact_segs_match(key, segs),
            Rule::DirPrefix(prefix) => key == prefix || key.starts_with(prefix),
            Rule::DirPrefixSegs(segs) => dir_prefix_segs_match(key, segs),
        }
    }
}

// Match path is collect-free so #32/#33 can call matches per entity without
// per-query Vec churn.

/// Segment-count equality + per-segment match. An exact non-dir pattern never
/// matches a folder key (folder form has one more, trailing-empty segment).
fn exact_segs_match(key: &str, segs: &[Segment]) -> bool {
    if key.ends_with('/') {
        return false;
    }
    let mut parts = key.split('/');
    for seg in segs {
        let Some(ks) = parts.next() else {
            return false;
        };
        match seg {
            Segment::Exact(s) if s.as_str() != ks => return false,
            Segment::Glob(g) if !segment_glob_matches(ks, g) => return false,
            _ => {}
        }
    }
    parts.next().is_none()
}

/// Dir form with per-segment globs: at least as many key segments as pattern
/// segments, matching in order; a key with exactly the pattern's segment count
/// must be a folder key (a file equal to the dir path without slash does not
/// match).
fn dir_prefix_segs_match(key: &str, segs: &[Segment]) -> bool {
    let folder = key.ends_with('/');
    let body = key.strip_suffix('/').unwrap_or(key);
    let mut parts = body.split('/');
    for seg in segs {
        let Some(ks) = parts.next() else {
            return false;
        };
        match seg {
            Segment::Exact(s) if s.as_str() != ks => return false,
            Segment::Glob(g) if !segment_glob_matches(ks, g) => return false,
            _ => {}
        }
    }
    // Equal segment count requires a folder key so a file equal to the dir
    // path without slash does not match (same rule as before).
    let has_more = parts.next().is_some();
    if !has_more && !folder {
        return false;
    }
    // has_more already peeked one; further key segments are fine (prefix
    // match). No need to consume the rest.
    true
}

impl SegmentGlob {
    fn from(seg: &str) -> Self {
        SegmentGlob {
            parts: seg.split('*').map(str::to_string).collect(),
        }
    }
}

/// Match one key segment against a pre-split glob: `parts[0]` is anchored as
/// a prefix (empty = leading `*`); later literals may appear anywhere, in
/// order, with backtracking; the final literal after the last `*` must be a
/// suffix (empty final part = trailing `*`). `*` never crosses `/` (a key
/// segment never contains `/`).
fn segment_glob_matches(seg: &str, glob: &SegmentGlob) -> bool {
    let parts = &glob.parts;
    debug_assert!(!parts.is_empty(), "a glob always has at least one part");
    // Defensive: if parts were ever empty, never abort a query path.
    let Some((p0, rest)) = parts.split_first() else {
        return seg.is_empty();
    };
    if !seg.starts_with(p0.as_str()) {
        return false;
    }
    match_after_star(&seg[p0.len()..], rest)
}

/// Match `seg` (the still-unconsumed tail) against the literal parts that
/// follow a star. `lit` is the next literal after a star, so it may be
/// anywhere in `seg`; interior literals backtrack over every occurrence; the
/// final part is a suffix check (empty = trailing `*`).
fn match_after_star(seg: &str, parts: &[String]) -> bool {
    let Some((lit, rest)) = parts.split_first() else {
        // Past the last star with no final literal: trailing `*`.
        return true;
    };
    if rest.is_empty() {
        // Final literal after the last star.
        return lit.is_empty() || seg.ends_with(lit.as_str());
    }
    if lit.is_empty() {
        // Interior empty literal (only reachable if `**` slipped through);
        // treat as another star.
        return match_after_star(seg, rest);
    }
    // Try every occurrence of `lit`, backtracking on failure. Advance one
    // char at a time (UTF-8 safe: a match start is always a char boundary).
    let mut from = 0usize;
    while from <= seg.len() {
        let Some(rel) = seg[from..].find(lit.as_str()) else {
            return false;
        };
        let pos = from + rel;
        if match_after_star(&seg[pos + lit.len()..], rest) {
            return true;
        }
        let next = seg[pos..].chars().next().map_or(1, char::len_utf8);
        from = pos + next;
    }
    false
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
    fn ignore_set_rejects_dot_whitespace_control() {
        // Loud rejects for patterns that could never name a real entity
        // segment, mirroring `ensure_valid_key` segment rules: `.` / `..`
        // segments, whitespace-only segments, and control characters.
        // Dotfile *names* (.DS_Store, .git/, .obsidian/...) stay valid.
        let cases: &[(&str, &[&str])] = &[
            (".", &["'.'"]),
            ("..", &["'..'"]),
            ("foo/.", &["'.'"]),
            ("foo/../bar", &["'..'"]),
            (" ", &["whitespace"]),
            ("  ", &["whitespace"]),
            ("a/ /b", &["whitespace"]),
            ("a/\tb", &["control"]),
            ("a/\u{0}b", &["control"]),
            ("a/\nb", &["control"]),
        ];
        for (pat, tokens) in cases {
            let err = IgnoreSet::from_patterns(&[pat.to_string()]).unwrap_err();
            let msg = err.to_string();
            // `invalid` names the pattern via `{pat:?}` (control chars appear
            // Debug-escaped, e.g. `\t`), so match against that form.
            let named = format!("{pat:?}");
            assert!(msg.contains(&named), "pattern {pat:?} not named in {msg:?}");
            for t in *tokens {
                assert!(msg.contains(t), "token {t:?} missing in {msg:?}");
            }
        }
        // Keep-green: dotfile names and star patterns must still compile.
        let ok: &[&str] = &[
            ".DS_Store",
            ".git/",
            ".obsidian/workspace.json",
            "foo.bar",
            "*",
            "*.tmp",
            ".obsidian/workspace*",
        ];
        for pat in ok {
            assert!(
                IgnoreSet::from_patterns(&[pat.to_string()]).is_ok(),
                "pattern {pat:?} should compile"
            );
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

    #[test]
    fn ignore_set_star_segment() {
        // `*` is the only v1 metacharacter: any run of non-`/` chars inside
        // one segment, including empty; never crossing `/`.
        let cases: &[(&[&str], &str, bool)] = &[
            (&[".obsidian/workspace*"], ".obsidian/workspace", true),
            (&[".obsidian/workspace*"], ".obsidian/workspace.json", true),
            (
                &[".obsidian/workspace*"],
                ".obsidian/workspace-mobile.json",
                true,
            ),
            (&[".obsidian/workspace*"], ".obsidian/app.json", false),
            (&[".obsidian/workspace*"], ".obsidian/workspaces/x", false),
            (&[".obsidian/workspace*"], "workspace", false),
            (&["*.tmp"], "foo.tmp", true),
            (&["*.tmp"], "notes/foo.tmp", true),
            (&["*.tmp"], "foo.tmp.x", false),
            (&["*.tmp"], "fooxtmp", false),
            (&["pre*mid*suf"], "premidXsuf", true),
            (&["pre*mid*suf"], "preXsuf", false),
            (&["*"], "a", true),
            (&["*"], "a/b", true),
            (&["*"], "a/b/", true),
            (&["foo/*/bar"], "foo/x/bar", true),
            (&["foo/*/bar"], "foo/x/y/bar", false),
            (&["foo/*/bar"], "foo/bar", false),
            // Dir form with a glob segment: `cache/<any>/` and everything
            // under it, never a file equal to the dir path without slash.
            (&["cache/*/"], "cache/x/", true),
            (&["cache/*/"], "cache/x/y/", true),
            (&["cache/*/"], "cache/x", false),
            (&["cache/*/"], "cache/", false),
            // --- anchor: parts[0] must be a prefix (false positives under
            // the old unanchored first-literal `find`) ---
            (&["b*"], "ab", false),
            (&["b*"], "xb", false),
            (&["workspace*"], "xworkspace", false),
            (&["workspace*"], "my_workspace", false),
            (&["a*a"], "baa", false),
            (&[".obsidian/workspace*"], ".obsidian/xworkspace", false),
            (&[".obsidian/workspace*"], ".obsidian/my_workspace", false),
            // --- backtracking / final literal ends_with (false negatives
            // under the old greedy first-occurrence `find`) ---
            (&["a*a"], "aaa", true),
            (&["a*a"], "aa", true),
            (&["a*b"], "abxb", true),
            (&["a*bc"], "abcbc", true),
            (&["*a"], "baa", true),
            (&["*a"], "aaa", true),
            (&["*b"], "bb", true),
            (&["a*a*a"], "aaaaa", true),
            // --- UTF-8 multi-byte literals / backtrack advance (r2 F2):
            // the byte-wise `chars().next().len_utf8()` step keeps multi-byte
            // match starts aligned; pins so a "simplify to bytes" refactor
            // cannot regress these under the ASCII-only star table ---
            (&["café*"], "caféx", true),
            (&["café*"], "cafex", false),
            (&["*é*"], "aéb", true),
            (&["*é*"], "aeb", false),
            (&["ä*ä"], "äää", true),
            (&["ä*"], "xä", false),
            // --- correct positive that a naive "starts_with only" fix
            // would break: trailing `*` consumes extra chars ---
            (&["workspace*"], "workspaces", true),
            (&[".obsidian/workspace*"], ".obsidian/workspaces", true),
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
    fn ignore_set_default_profile_fixture() {
        // The epic #9 D3 built-in set (the bridge fixture #31/#34 reuse):
        // everything compiles, and the workspace trio / .git / .DS_Store
        // behave while unrelated files do not. Built from
        // `OBSIDIAN_DEFAULT_IGNORE_PATTERNS` (issue #31 D-constant) so the
        // six strings have exactly one source of truth.
        let set = set(crate::config::OBSIDIAN_DEFAULT_IGNORE_PATTERNS);
        let cases: &[(&str, bool)] = &[
            (".obsidian/app.json", false),
            (".obsidian/workspace.json", true),
            (".obsidian/workspace-mobile.json", true),
            (".git/HEAD", true),
            (".trash/foo.md", true),
            ("notes/.DS_Store", true),
            ("notes/foo.md", false),
            // The workspace trio are exact *file* patterns: the folder form
            // of the same path must NOT match (Exact is string equality).
            (".obsidian/workspace", true),
            (".obsidian/workspace/", false),
            (".obsidian/workspace.json/", false),
        ];
        for (key, expect) in cases {
            assert_eq!(set.matches(key), *expect, "key {key:?}");
        }
    }
}
