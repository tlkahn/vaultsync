//! TOML config loading + settings resolution (pure, Phase 2 Slice 2).
//!
//! The TOML file never carries secrets: `S3Store` is built from the resolved
//! `StoreSettings` (bucket/region/endpoint/prefix/path_style), while
//! credentials always come from the AWS default chain (env, shared config,
//! profile) - never from this file (cli.md lock).

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::Error;

pub const DEFAULT_MTIME_TOLERANCE_MS: u64 = 1000;
pub const DEFAULT_CONCURRENCY: u32 = 4;
/// I20-r1/F2: the ceiling on `[transfer].concurrency` (config-layer only -
/// library callers of `execute_plan`/`S3Store::new` are uncapped, and the
/// pool still clamps workers to `min(concurrency, items)`). Rationale: S3
/// single-client throughput saturates well below 256 concurrent ops; beyond
/// that it is OS-thread cost for zero gain, so a larger value is a config
/// mistake, not a tuning choice - rejected loudly (W56 ethos) rather than
/// clamped.
pub const MAX_CONCURRENCY: u32 = 256;

/// AWS SDK standard-mode retry defaults (I8-config): the resolved policy when
/// `[transfer.retry]` is absent or a field is unset mirrors the SDK's own
/// `RetryConfig::standard()` numbers (3 attempts / 1s initial / 20s max);
/// absent/unset fields resolve to those values. This does not make a default
/// run a no-op vs pre-I8 flight: ambient AWS retry env/profile is replaced at
/// client build on purpose (I8-retry-config-owned).
pub const DEFAULT_RETRY_MAX_ATTEMPTS: u32 = 3;
pub const DEFAULT_RETRY_BASE_DELAY_MS: u64 = 1000;
pub const DEFAULT_RETRY_MAX_DELAY_MS: u64 = 20000;

/// Built-in Obsidian default ignore profile (issue #31 / roadmap D3).
/// Vault-relative; exact strings; single source of truth for docs + resolve.
/// The `obsidian` profile is the default when `[ignore]` is absent or
/// `profile` is absent; `profile = "none"` disables it. User `[ignore].patterns`
/// **extend** this set (union), never replace it.
pub const OBSIDIAN_DEFAULT_IGNORE_PATTERNS: &[&str] = &[
    ".git/",
    ".trash/",
    ".DS_Store",
    ".obsidian/workspace",
    ".obsidian/workspace.json",
    ".obsidian/workspace-mobile.json",
];

/// On-disk config mirroring [cli.md]. All sections optional; defaults applied
/// at resolution time. Unknown keys anywhere in the file are rejected loudly
/// (W56, B nit): a typo like `mtime_tolerance` (missing `_ms`) or a
/// misspelled section key surfaces as a parse error naming the key instead of
/// silently keeping a default.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default)]
    pub vault_root: Option<PathBuf>,
    #[serde(default)]
    pub store: Option<StoreConfig>,
    #[serde(default)]
    pub ignore: Option<IgnoreConfig>,
    #[serde(default)]
    pub transfer: Option<TransferConfig>,
}

/// `[store]` section. `type` must be `"s3"`; `bucket` is required when the
/// section is present. Credentials are never configured here.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreConfig {
    #[serde(default, rename = "type")]
    pub store_type: Option<String>,
    #[serde(default)]
    pub bucket: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub path_style: Option<bool>,
}

/// `[ignore]` section. `profile` selects the built-in default ignore set
/// (issue #31 D3/D-profile-values: `"obsidian"` is the default when the key
/// is absent, `"none"` disables built-ins; unknown values are loud errors at
/// resolution). `patterns` are user additions that **extend** the active
/// profile (union). Resolution happens in [`resolve_settings`]; application
/// lands in #34 - W25/M3 still keys off the raw user list only.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IgnoreConfig {
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub patterns: Vec<String>,
}

/// `[transfer]` section.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferConfig {
    #[serde(default)]
    pub concurrency: Option<u32>,
    #[serde(default)]
    pub mtime_tolerance_ms: Option<u64>,
    #[serde(default)]
    pub retry: Option<RetryConfig>,
}

/// `[transfer.retry]` section (I8). All fields optional; absent section (or
/// absent field) resolves to the AWS SDK standard-mode defaults at
/// [`resolve_settings`] time (3 / 1000 / 20000). Unknown keys are rejected
/// loudly (W56) via `deny_unknown_fields`, matching the sibling sections.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryConfig {
    #[serde(default)]
    pub max_attempts: Option<u32>,
    #[serde(default)]
    pub base_delay_ms: Option<u64>,
    #[serde(default)]
    pub max_delay_ms: Option<u64>,
}

/// Fully resolved runtime settings (config + CLI + env merged).
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub vault_root: PathBuf,
    pub store: StoreSettings,
    pub mtime_tolerance_ms: u64,
    pub concurrency: u32,
    /// resolved `[transfer.retry]` policy (I8). Milliseconds at this layer;
    /// `Duration` conversion happens at the S3 boundary.
    pub retry: RetrySettings,
    /// Raw user `[ignore].patterns` only (no profile injection). W25/M3
    /// still gates on this field until #34 (issue #31 D-w25-seq): absent
    /// `[ignore]` leaves it empty even though the resolved list is non-empty,
    /// so the default profile never trips the Phase 3 refusal.
    pub ignore_patterns: Vec<String>,
    /// Fully resolved ignore list: active profile built-ins first, then user
    /// patterns, exact-string deduped, validated via `IgnoreSet`. Unused by
    /// the CLI until #34 wire-up; present so the Obsidian default can land
    /// without tripping W25 (issue #31 sequencing note option 2). #34 reads
    /// this field and retires W25.
    pub resolved_ignore_patterns: Vec<String>,
}

/// Resolved retry policy (I8). Milliseconds at this layer; `Duration`
/// conversion is owned by the S3 boundary (`build_retry_config`). `Default`
/// is the AWS SDK standard-mode default (3 / 1000 / 20000).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetrySettings {
    /// Total attempts including the initial one (1 = retries disabled).
    pub max_attempts: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for RetrySettings {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_RETRY_MAX_ATTEMPTS,
            base_delay_ms: DEFAULT_RETRY_BASE_DELAY_MS,
            max_delay_ms: DEFAULT_RETRY_MAX_DELAY_MS,
        }
    }
}

/// Resolved store connection settings (no credentials - those stay in the AWS
/// chain).
#[derive(Debug, Clone, PartialEq)]
pub struct StoreSettings {
    /// Empty only when no `[store]` section was present (mock/offline phase).
    pub bucket: String,
    /// Region override; `None` means "let the AWS default chain decide" (env,
    /// shared config, profile) - never a hardcoded guess (W7/B-M2).
    pub region: Option<String>,
    pub endpoint: Option<String>,
    /// Vault-relative prefix with a normalized trailing `/` (`""` when none).
    pub prefix: String,
    pub path_style: bool,
}

/// The default search order: `./.vaultsync.toml` then
/// `~/.config/vaultsync/config.toml`. Injected here (rather than hard-coded)
/// so the search-order test can point at temp dirs.
pub fn default_search_paths(cwd: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut v = vec![cwd.join(".vaultsync.toml")];
    if let Some(h) = home {
        v.push(h.join(".config/vaultsync/config.toml"));
    }
    v
}

/// Load config: an explicit `--config` path must exist and parse (`loud`
/// error otherwise); otherwise the first existing search path wins; with none,
/// an empty default (no error) - see [`resolve_settings`].
pub fn load_config(explicit: Option<&Path>, search: &[PathBuf]) -> Result<FileConfig, Error> {
    if let Some(p) = explicit {
        let text = std::fs::read_to_string(p)
            .map_err(|e| Error::Other(format!("cannot read config {}: {e}", p.display())))?;
        return parse_config_str(&text)
            .map_err(|e| Error::Other(format!("invalid config {}: {e}", p.display())));
    }
    for p in search {
        if p.is_file() {
            let text = std::fs::read_to_string(p)
                .map_err(|e| Error::Other(format!("cannot read config {}: {e}", p.display())))?;
            return parse_config_str(&text)
                .map_err(|e| Error::Other(format!("invalid config {}: {e}", p.display())));
        }
    }
    Ok(FileConfig::default())
}

/// Parse TOML text into a [`FileConfig`]. Error messages carry the toml
/// parser's line/column info.
pub fn parse_config_str(text: &str) -> Result<FileConfig, toml::de::Error> {
    toml::from_str(text)
}

/// Snapshot of the relevant env vars, injected so resolution is testable
/// without real env. Phase 2 cares about `AWS_REGION` only (credentials
/// themselves stay in the AWS chain).
#[derive(Debug, Default, Clone)]
pub struct EnvSnapshot {
    pub aws_region: Option<String>,
}

/// Merge config + env into a [`Settings`]. Pure: no IO, no network.
///
/// W83/r9 N1: the `--vault`/config merge no longer lives here (the old
/// `cli: &Cli` parameter was test-only in production - the sole production
/// call passed `Cli::default()`); the single merge site is
/// `resolve_vault_from_config` in the CLI layer, which only applies the
/// config vault root when `--vault` was left at its unset sentinel.
pub fn resolve_settings(cfg: &FileConfig, env: &EnvSnapshot) -> Result<Settings, Error> {
    let vault_root = cfg.vault_root.clone().unwrap_or_else(|| PathBuf::from("."));
    let store = resolve_store(cfg.store.as_ref(), env)?;
    let mtime_tolerance_ms = cfg
        .transfer
        .as_ref()
        .and_then(|t| t.mtime_tolerance_ms)
        .unwrap_or(DEFAULT_MTIME_TOLERANCE_MS);
    let concurrency = cfg
        .transfer
        .as_ref()
        .and_then(|t| t.concurrency)
        .unwrap_or(DEFAULT_CONCURRENCY);
    // I20-config (W56 loud-config ethos, matching the I8 retry validation
    // shape): concurrency >= 1 - 0 is invalid (a run that can never transfer
    // is meaningless); 1 is valid (the dedicated sequential path, I20-one).
    // I20-r1/F2: concurrency > MAX_CONCURRENCY is a config mistake (S3
    // single-client throughput saturates well below 256 concurrent ops;
    // beyond that it is OS-thread cost for zero gain), so it is rejected
    // loudly naming the key and the cap - same shape as the `== 0` arm.
    if concurrency == 0 {
        return Err(Error::Other(format!(
            "transfer.concurrency must be >= 1 (1 = sequential), got {concurrency}"
        )));
    }
    if concurrency > MAX_CONCURRENCY {
        return Err(Error::Other(format!(
            "transfer.concurrency must be <= {MAX_CONCURRENCY} (got {concurrency}); values above the cap are OS-thread cost for zero S3 throughput gain"
        )));
    }
    let (ignore_patterns, resolved_ignore_patterns) = resolve_ignore(cfg.ignore.as_ref())?;
    let retry = resolve_retry(cfg.transfer.as_ref())?;
    Ok(Settings {
        vault_root,
        store,
        mtime_tolerance_ms,
        concurrency,
        retry,
        ignore_patterns,
        resolved_ignore_patterns,
    })
}

/// Resolve `[ignore]` into `(user patterns, resolved patterns)` (issue #31).
///
/// D3/D-profile-values: the built-in Obsidian set is the default when the
/// section or `profile` key is absent; `profile = "none"` disables built-ins;
/// user `patterns` **extend** the active profile (union, never replacement).
/// The resolved list is built-ins first, then user patterns (W190), exact-
/// string deduped (W191), profile-validated (W192), and pattern-validated via
/// `IgnoreSet` (W193). The user list stays raw (W25 gates on it until #34 -
/// D-w25-seq).
fn resolve_ignore(ignore: Option<&IgnoreConfig>) -> Result<(Vec<String>, Vec<String>), Error> {
    let user = ignore.map(|i| i.patterns.clone()).unwrap_or_default();
    // D3/D-profile-values: absent profile (or `None`) -> `obsidian`;
    // `profile = "none"` disables built-ins (escape hatch). Unknown values
    // become loud errors in W192 (not yet wired).
    let profile = ignore.and_then(|i| i.profile.as_deref());
    // W192: unknown profile values (incl. "", "Obsidian", "none ") are
    // loud errors naming the raw value and the allowed set - no soft-default,
    // no clamping, no trim (D-profile-values).
    if let Some(other) = profile
        && other != "obsidian"
        && other != "none"
    {
        return Err(Error::Other(format!(
            "ignore.profile: unknown profile {other:?} (allowed: \"obsidian\" | \"none\")"
        )));
    }
    let mut resolved: Vec<String> = match profile {
        // W189: `none` -> no built-ins; the user list is the whole resolved
        // list (union with the empty profile).
        Some("none") => Vec::new(),
        // W188: `obsidian` / absent key -> the built-in Obsidian set.
        _ => OBSIDIAN_DEFAULT_IGNORE_PATTERNS
            .iter()
            .map(|s| s.to_string())
            .collect(),
    };
    // W190: user patterns extend the active profile (union; stable order:
    // built-ins first, user next). W191: exact-string dedup, first wins - a
    // user entry repeating a built-in (or an earlier user entry) is dropped.
    for p in user.iter() {
        if !resolved.contains(p) {
            resolved.push(p.clone());
        }
    }
    // W193 (D-validate-seam): validate the full resolved list via
    // `IgnoreSet::from_patterns` and discard the matcher - the matcher's own
    // message (naming the bad pattern + reason) is reused verbatim; no
    // reimplementation of pattern rules here, no new Error variant.
    let _ = crate::IgnoreSet::from_patterns(&resolved)?;
    Ok((user, resolved))
}

/// Resolve + validate `[transfer.retry]` (I8): each absent field falls back
/// to the AWS SDK standard-mode default (per-field, not all-or-nothing); an
/// absent section resolves to the full default. Validation is loud (W56
/// ethos) and names the offending config key(s), in order:
/// `max_attempts >= 1` (1 deliberately disables retries, matching
/// `RetryConfig::disabled()`); `base_delay_ms >= 1` and `max_delay_ms >= 1`
/// (SDK requires non-zero backoffs; checked before the base>max rule so a
/// lone zero names the right key); then `base_delay_ms <= max_delay_ms`.
fn resolve_retry(transfer: Option<&TransferConfig>) -> Result<RetrySettings, Error> {
    let r = transfer.and_then(|t| t.retry.as_ref());
    let max_attempts = r
        .and_then(|r| r.max_attempts)
        .unwrap_or(DEFAULT_RETRY_MAX_ATTEMPTS);
    let base_delay_ms = r
        .and_then(|r| r.base_delay_ms)
        .unwrap_or(DEFAULT_RETRY_BASE_DELAY_MS);
    let max_delay_ms = r
        .and_then(|r| r.max_delay_ms)
        .unwrap_or(DEFAULT_RETRY_MAX_DELAY_MS);
    if max_attempts == 0 {
        return Err(Error::Other(format!(
            "transfer.retry.max_attempts must be >= 1 (1 disables retries), got {max_attempts}"
        )));
    }
    if base_delay_ms == 0 {
        return Err(Error::Other(format!(
            "transfer.retry.base_delay_ms must be >= 1 (the SDK requires a non-zero initial backoff), got {base_delay_ms}"
        )));
    }
    if max_delay_ms == 0 {
        return Err(Error::Other(format!(
            "transfer.retry.max_delay_ms must be >= 1 (the SDK requires a non-zero max backoff), got {max_delay_ms}"
        )));
    }
    if base_delay_ms > max_delay_ms {
        return Err(Error::Other(format!(
            "transfer.retry.base_delay_ms ({base_delay_ms}) must not exceed transfer.retry.max_delay_ms ({max_delay_ms})"
        )));
    }
    Ok(RetrySettings {
        max_attempts,
        base_delay_ms,
        max_delay_ms,
    })
}

/// W69/W86 policy: an empty or whitespace-only value is treated as unset
/// (mirroring the SDK's own env provider) - it must never flow through as
/// `Some("")`, which would build `Region::new("")` / `endpoint_url("")`
/// and fail late with an opaque SDK error. Shared by the env region and the
/// config region/endpoint paths so the two can never drift.
fn nonblank(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

fn resolve_store(store: Option<&StoreConfig>, env: &EnvSnapshot) -> Result<StoreSettings, Error> {
    match store {
        // No `[store]` section -> offline/mock defaults, never an error.
        None => Ok(StoreSettings {
            bucket: String::new(),
            region: None,
            endpoint: None,
            prefix: String::new(),
            path_style: false,
        }),
        Some(s) => {
            let t = s.store_type.clone().unwrap_or_else(|| "s3".to_string());
            if t != "s3" {
                return Err(Error::Other(format!(
                    "unsupported store type: {t:?} (only \"s3\" is supported)"
                )));
            }
            let bucket = s
                .bucket
                .clone()
                .ok_or_else(|| Error::Other("store.bucket is required".to_string()))?;
            // r11-L2/W98: bucket is the one *required* store field, so the
            // nonblank policy is a hard error here (unlike optional
            // region/endpoint where blank means unset). A whitespace-only
            // value would otherwise resolve and fail late at the first SDK
            // call. Real values with stray padding pass through verbatim - no
            // silent trimming.
            if bucket.trim().is_empty() {
                return Err(Error::Other(
                    "store.bucket must not be empty or whitespace-only".to_string(),
                ));
            }
            // Env `AWS_REGION` overrides an explicit config region; with
            // neither set the result is `None` so the AWS default chain
            // (env/shared config/profile) decides (W7/B-M2; env-over-config
            // precedence locked by resolve_settings_env_overrides_config_region).
            // W69/A-N1: an empty/whitespace-only env value is treated as
            // unset (mirroring the SDK's own env provider) - it must never
            // override a config region with `Some("")`, which would build
            // `Region::new("")` and break the whole default chain.
            let env_region = nonblank(env.aws_region.clone());
            // r10-M2/W86: same policy for the config value itself - an
            // empty/whitespace `[store].region` is unset, not a hard error
            // (existing hand-written configs must not newly fail), and must
            // never reach `Region::new("")`.
            let region = env_region.or_else(|| nonblank(s.region.clone()));
            // W58/A nit + r11-L3 (W105): the configured prefix must itself
            // be a valid vault key prefix - one policy, `ensure_valid_key`
            // after normalization (an empty, whitespace-only, control-char,
            // or `..` segment fails fast at resolution, naming the config
            // key, instead of silently writing odd keys to the remote; this
            // subsumes the old empty-segment-only check). The normalized
            // form's trailing `/` is a folder marker, not a segment, and
            // `ensure_valid_key` allows it. An empty prefix (no prefix
            // configured) skips validation.
            let prefix = s.prefix.as_deref().unwrap_or("");
            let normalized = normalize_prefix(prefix);
            if !normalized.is_empty()
                && let Err(e) = crate::entity::ensure_valid_key(&normalized)
            {
                return Err(Error::Other(format!(
                    "store.prefix is not a valid key prefix: {prefix:?} ({e})"
                )));
            }
            Ok(StoreSettings {
                bucket,
                region,
                // r10-M2/W86: empty/whitespace endpoint is unset - an
                // `endpoint_url("")` fails late with an opaque SDK error.
                endpoint: nonblank(s.endpoint.clone()),
                prefix: normalized,
                path_style: s.path_style.unwrap_or(false),
            })
        }
    }
}

/// Normalize a configured prefix to a trailing `/` (`"notes"` -> `"notes/"`),
/// leaving an empty prefix as `""`.
pub fn normalize_prefix(prefix: &str) -> String {
    if prefix.is_empty() || prefix.ends_with('/') {
        prefix.to_string()
    } else {
        format!("{prefix}/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn settings(cfg: &FileConfig) -> Result<Settings, Error> {
        resolve_settings(cfg, &EnvSnapshot::default())
    }

    #[test]
    fn config_parse_full_example() {
        // Mirrors the cli.md example. Store buckets/prefix and transfer fields
        // all deserialize; unknown sections like `[ignore]` parse fine.
        let text = r#"
vault_root = "/Users/me/Notes"

[store]
type = "s3"
bucket = "my-vaults"
region = "us-west-2"
endpoint = "https://minio.example"
prefix = "notes/"
path_style = true

[ignore]
patterns = [".git/", ".trash/", ".DS_Store"]

[transfer]
concurrency = 4
mtime_tolerance_ms = 1000

[transfer.retry]
max_attempts = 5
base_delay_ms = 250
max_delay_ms = 4000
"#;
        let cfg = parse_config_str(text).unwrap();
        assert_eq!(
            cfg.vault_root.as_deref(),
            Some(Path::new("/Users/me/Notes"))
        );
        let s = cfg.store.unwrap();
        assert_eq!(s.store_type.as_deref(), Some("s3"));
        assert_eq!(s.bucket.as_deref(), Some("my-vaults"));
        assert_eq!(s.region.as_deref(), Some("us-west-2"));
        assert_eq!(s.endpoint.as_deref(), Some("https://minio.example"));
        assert_eq!(s.prefix.as_deref(), Some("notes/"));
        assert_eq!(s.path_style, Some(true));
        assert_eq!(cfg.ignore.unwrap().patterns.len(), 3);
        let t = cfg.transfer.unwrap();
        assert_eq!(t.concurrency, Some(4));
        assert_eq!(t.mtime_tolerance_ms, Some(1000));
        let retry = t.retry.unwrap();
        assert_eq!(retry.max_attempts, Some(5));
        assert_eq!(retry.base_delay_ms, Some(250));
        assert_eq!(retry.max_delay_ms, Some(4000));
    }

    #[test]
    fn config_parse_ignore_profile_field() {
        // Issue #31 (D-config-surface): `[ignore].profile` parses as an
        // optional string; absent key -> `None` (resolves to `"obsidian"`
        // at resolution time). `deny_unknown_fields` stays: a typo like
        // `profil` is a loud parse error naming the key. RED: `IgnoreConfig`
        // has no `profile` field, so serde rejects it as an unknown field.
        let text = r#"
[ignore]
profile = "obsidian"
patterns = [".git/"]
"#;
        let cfg = parse_config_str(text).unwrap();
        let ig = cfg.ignore.as_ref().unwrap();
        assert_eq!(ig.profile.as_deref(), Some("obsidian"));
        assert_eq!(ig.patterns.len(), 1);

        // No profile key -> None (default resolved later).
        let cfg = parse_config_str("[ignore]\npatterns = []\n").unwrap();
        let ig = cfg.ignore.as_ref().unwrap();
        assert_eq!(ig.profile, None);
        assert!(ig.patterns.is_empty());

        // Unknown key under `[ignore]` is rejected loudly, naming the key
        // (W56 / deny_unknown_fields).
        for bad in ["profil = \"obsidian\"", "foo = 1"] {
            let text = format!("[ignore]\n{bad}\n");
            let err = parse_config_str(&text).unwrap_err();
            let msg = format!("{err}");
            assert!(
                msg.contains("profil") || msg.contains("foo"),
                "unknown ignore key not named in: {msg}"
            );
        }
    }

    #[test]
    fn resolve_absent_ignore_does_not_populate_user_field() {
        // Issue #31 (D-w25-seq, W194): the sequencing invariant - absent
        // `[ignore]` leaves the user-only field empty (so W25/M3 never trips
        // on the built-in default profile) while the resolved field carries
        // the built-ins. A future mistaken merge of the two fields fails
        // loudly here (mutation-checked: reverting `resolve_ignore` to the
        // W186 `ignore_patterns.clone()` baseline flips this RED).
        let cfg = FileConfig::default();
        let s = settings(&cfg).unwrap();
        assert!(s.ignore_patterns.is_empty(), "user field must stay empty");
        assert!(
            !s.resolved_ignore_patterns.is_empty(),
            "resolved field must carry the built-in default"
        );
        assert_eq!(
            s.resolved_ignore_patterns.len(),
            OBSIDIAN_DEFAULT_IGNORE_PATTERNS.len()
        );
    }

    #[test]
    fn resolve_bad_pattern_errors() {
        // Issue #31 (D-validate-seam): the full resolved list is validated
        // through `IgnoreSet::from_patterns` (single seam, matcher messages
        // reused verbatim - no parallel vocabulary, no new Error variant).
        // `profile = "none"` isolates the user patterns from built-ins. RED:
        // no validation at resolution today - bad patterns resolve silently.
        let cases: &[(&[&str], &str)] = &[
            (&[""], "empty"),
            (&["/abs"], "leading"),
            (&["a/**/b"], "**"),
            (&["!foo"], "!"),
            (&["foo?"], "?"),
            (&["a//b"], "empty"),
        ];
        for (patterns, reason) in cases {
            let list = patterns
                .iter()
                .map(|p| format!("\"{p}\""))
                .collect::<Vec<_>>()
                .join(", ");
            let text = format!("[ignore]\nprofile = \"none\"\npatterns = [{list}]\n");
            let cfg = parse_config_str(&text).unwrap();
            let err = settings(&cfg).unwrap_err();
            let msg = format!("{err}");
            let bad = patterns[0];
            assert!(
                msg.contains(&format!("{bad:?}")),
                "{bad:?} must be named in: {msg}"
            );
            assert!(
                msg.contains(reason),
                "{bad:?} must carry reason {reason:?}: {msg}"
            );
        }

        // Under the default profile, a bad *user* pattern still fails even
        // though the six built-ins are valid.
        let text = "[ignore]\npatterns = [\"private/\", \"\"]\n";
        let cfg = parse_config_str(text).unwrap();
        let err = settings(&cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("\"\"") && msg.contains("empty"),
            "bad user pattern under default profile must fail naming it: {msg}"
        );

        // Built-ins alone are valid (constant is load-bearing): no error.
        let cfg = FileConfig::default();
        assert!(settings(&cfg).is_ok());
    }

    #[test]
    fn resolve_unknown_profile_errors() {
        // Issue #31 (D-profile-values): exact codepoint match, case-sensitive,
        // no trim. Anything other than `"obsidian"` | `"none"` (including
        // `""`, `"Obsidian"`, `"none "`) is a loud error naming the raw
        // value and the allowed set (prefer also naming `ignore.profile`).
        // No soft-default, no clamping. RED: unknown values currently fall
        // through to the obsidian default silently.
        // (toml_value, raw_value): the newline case must be written as the
        // TOML escape `\n` (a literal newline is not representable in a TOML
        // basic string); TOML parses it back into the newline character.
        let cases: &[(&str, &str)] = &[
            ("git", "git"),
            ("", ""),
            ("Obsidian", "Obsidian"),
            ("none ", "none "),
            ("obsidian\\n", "obsidian\n"),
        ];
        for (toml_value, raw_value) in cases {
            let text = format!("[ignore]\nprofile = \"{toml_value}\"\n");
            let cfg = parse_config_str(&text).unwrap();
            let err = settings(&cfg).unwrap_err();
            let msg = format!("{err}");
            assert!(
                msg.contains("ignore.profile"),
                "{raw_value:?} error must name ignore.profile: {msg}"
            );
            assert!(
                msg.contains("obsidian") && msg.contains("none"),
                "{raw_value:?} error must mention the allowed set obsidian|none: {msg}"
            );
            assert!(
                msg.contains(&format!("{raw_value:?}")),
                "{raw_value:?} error must name the raw value: {msg}"
            );
        }
    }

    #[test]
    fn resolve_ignore_dedup_exact_string() {
        // Issue #31 (D-dedup): exact `String` equality, first occurrence
        // wins. Built-ins first means a user entry repeating a built-in is
        // dropped; a repeated user entry is dropped too. Order of first
        // occurrence preserved. Not path-semantic (`.git` vs `.git/` stay
        // distinct if both present). RED: no dedup yet - user `.git/` and the
        // second user `.git/` duplicate built-ins.
        let text =
            "[ignore]\nprofile = \"obsidian\"\npatterns = [\".git/\", \"private/\", \".git/\"]\n";
        let cfg = parse_config_str(text).unwrap();
        let s = settings(&cfg).unwrap();
        let expected: Vec<String> = OBSIDIAN_DEFAULT_IGNORE_PATTERNS
            .iter()
            .map(|s| s.to_string())
            .chain(std::iter::once("private/".to_string()))
            .collect();
        assert_eq!(s.resolved_ignore_patterns, expected);

        // User-only dup under `profile = "none"`: exact-string first wins.
        let text = "[ignore]\nprofile = \"none\"\npatterns = [\"a/\", \"b/\", \"a/\"]\n";
        let cfg = parse_config_str(text).unwrap();
        let s = settings(&cfg).unwrap();
        assert_eq!(
            s.resolved_ignore_patterns,
            vec!["a/".to_string(), "b/".to_string()]
        );
    }

    #[test]
    fn resolve_user_extends_obsidian() {
        // Issue #31 (D3): user `patterns` **extend** the active profile
        // (union, never replacement). With no `profile` key (default
        // obsidian) or an explicit `obsidian`, the resolved list is the six
        // built-ins followed by the user patterns. RED: the obsidian arm does
        // not append user patterns yet.
        let expected: Vec<String> = OBSIDIAN_DEFAULT_IGNORE_PATTERNS
            .iter()
            .map(|s| s.to_string())
            .chain(std::iter::once("private/".to_string()))
            .collect();

        // No profile key -> default obsidian.
        let text = "[ignore]\npatterns = [\"private/\"]\n";
        let cfg = parse_config_str(text).unwrap();
        let s = settings(&cfg).unwrap();
        assert_eq!(s.ignore_patterns, vec!["private/".to_string()]);
        assert_eq!(s.resolved_ignore_patterns, expected);

        // Explicit `profile = "obsidian"` behaves identically.
        let text = "[ignore]\nprofile = \"obsidian\"\npatterns = [\"private/\"]\n";
        let cfg = parse_config_str(text).unwrap();
        let s = settings(&cfg).unwrap();
        assert_eq!(s.ignore_patterns, vec!["private/".to_string()]);
        assert_eq!(s.resolved_ignore_patterns, expected);
    }

    #[test]
    fn resolve_profile_none_plus_user() {
        // Issue #31 (D3 escape hatch): `profile = "none"` disables the
        // built-in Obsidian set; the resolved list is exactly the user
        // patterns. RED: today `profile` is ignored, so the six built-ins
        // leak into the resolved list.
        let text = "[ignore]\nprofile = \"none\"\npatterns = [\"private/\"]\n";
        let cfg = parse_config_str(text).unwrap();
        let s = settings(&cfg).unwrap();
        assert_eq!(s.ignore_patterns, vec!["private/".to_string()]);
        assert_eq!(s.resolved_ignore_patterns, vec!["private/".to_string()]);

        // `profile = "none"` with no patterns -> both empty.
        let text = "[ignore]\nprofile = \"none\"\n";
        let cfg = parse_config_str(text).unwrap();
        let s = settings(&cfg).unwrap();
        assert!(s.ignore_patterns.is_empty());
        assert!(s.resolved_ignore_patterns.is_empty());
    }

    #[test]
    fn resolve_default_profile_is_obsidian() {
        // Issue #31 (D3/D-w25-seq): absent `[ignore]` (or an empty section)
        // resolves the built-in Obsidian default set in constant order on the
        // *resolved* field, while the user-only field stays empty so W25 never
        // trips (the critical sequencing hazard). RED: `resolved_ignore_patterns`
        // mirrors the user list today (W186 baseline), so the six built-ins are
        // missing.
        let expected: Vec<String> = OBSIDIAN_DEFAULT_IGNORE_PATTERNS
            .iter()
            .map(|s| s.to_string())
            .collect();

        let cfg = FileConfig::default();
        let s = settings(&cfg).unwrap();
        assert!(
            s.ignore_patterns.is_empty(),
            "user field stays empty (W25-safe)"
        );
        assert_eq!(s.resolved_ignore_patterns, expected);

        // Empty `[ignore]` section (present, no keys) resolves identically.
        let cfg = parse_config_str("[ignore]\n").unwrap();
        let s = settings(&cfg).unwrap();
        assert!(s.ignore_patterns.is_empty());
        assert_eq!(s.resolved_ignore_patterns, expected);
    }

    #[test]
    fn config_parse_retry_section() {
        // I8-config: `[transfer.retry]` parses into the three optional fields
        // on `FileConfig.transfer.retry` (defaults applied later at
        // resolution). RED: `TransferConfig` has no `retry` field yet
        // (compile failure).
        let text = r#"
[transfer.retry]
max_attempts = 5
base_delay_ms = 250
max_delay_ms = 4000
"#;
        let cfg = parse_config_str(text).unwrap();
        let t = cfg.transfer.unwrap();
        assert_eq!(t.concurrency, None, "concurrency stays unset");
        let retry = t.retry.unwrap();
        assert_eq!(retry.max_attempts, Some(5));
        assert_eq!(retry.base_delay_ms, Some(250));
        assert_eq!(retry.max_delay_ms, Some(4000));
    }

    #[test]
    fn resolve_settings_retry_defaults_sdk_standard() {
        // I8-config: no `[transfer.retry]` section => the resolved retry
        // policy is the AWS SDK standard-mode default (3 / 1000 / 20000),
        // pinned against the DEFAULT_RETRY_* constants. RED:
        // `Settings.retry` does not exist yet (compile failure).
        let cfg = FileConfig::default();
        let s = settings(&cfg).unwrap();
        assert_eq!(
            s.retry,
            RetrySettings {
                max_attempts: DEFAULT_RETRY_MAX_ATTEMPTS,
                base_delay_ms: DEFAULT_RETRY_BASE_DELAY_MS,
                max_delay_ms: DEFAULT_RETRY_MAX_DELAY_MS,
            },
            "absent [transfer.retry] resolves to SDK standard defaults"
        );
    }

    #[test]
    fn resolve_settings_retry_partial_fills_defaults() {
        // I8-config: per-field resolution - setting only `max_attempts`
        // leaves the delays at their SDK defaults (not all-or-nothing).
        let text = "[transfer.retry]\nmax_attempts = 5\n";
        let cfg = parse_config_str(text).unwrap();
        let s = settings(&cfg).unwrap();
        assert_eq!(s.retry.max_attempts, 5);
        assert_eq!(s.retry.base_delay_ms, DEFAULT_RETRY_BASE_DELAY_MS);
        assert_eq!(s.retry.max_delay_ms, DEFAULT_RETRY_MAX_DELAY_MS);
    }

    #[test]
    fn resolve_settings_retry_full_override() {
        // I8-config: all three set => all three resolved verbatim.
        let text = "[transfer.retry]\nmax_attempts = 7\nbase_delay_ms = 50\nmax_delay_ms = 5000\n";
        let cfg = parse_config_str(text).unwrap();
        let s = settings(&cfg).unwrap();
        assert_eq!(s.retry.max_attempts, 7);
        assert_eq!(s.retry.base_delay_ms, 50);
        assert_eq!(s.retry.max_delay_ms, 5000);
    }

    #[test]
    fn resolve_settings_retry_rejects_zero_max_attempts() {
        // I8-validation (W56 loud-config ethos): max_attempts = 0 is invalid
        // (the SDK requires >= 1; 1 disables retries, 0 is meaningless). Reject
        // with an error naming the config key. RED: resolves with no error.
        let text = "[transfer.retry]\nmax_attempts = 0\n";
        let cfg = parse_config_str(text).unwrap();
        let err = settings(&cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("transfer.retry.max_attempts"),
            "must name transfer.retry.max_attempts: {msg}"
        );
    }

    #[test]
    fn resolve_settings_retry_allows_max_attempts_1() {
        // I8-config: max_attempts = 1 is valid and deliberately disables
        // retries (matches `RetryConfig::disabled()` semantics) - must not be
        // rejected with the zero rule.
        let text = "[transfer.retry]\nmax_attempts = 1\n";
        let cfg = parse_config_str(text).unwrap();
        let s = settings(&cfg).unwrap();
        assert_eq!(s.retry.max_attempts, 1);
    }

    #[test]
    fn resolve_settings_rejects_zero_concurrency() {
        // I20-config (W56 loud-config ethos, matching the I8 retry validation
        // shape): concurrency = 0 is invalid - a run that can never transfer
        // is meaningless. Reject with an error naming the config key. RED:
        // resolves with no error today.
        let text = "[transfer]\nconcurrency = 0\n";
        let cfg = parse_config_str(text).unwrap();
        let err = settings(&cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("transfer.concurrency"),
            "must name transfer.concurrency: {msg}"
        );
    }

    #[test]
    fn resolve_settings_rejects_concurrency_above_max() {
        // I20-r1/F2 (W56 loud-config ethos): concurrency above the
        // `MAX_CONCURRENCY` ceiling is a config mistake, not a tuning choice
        // (S3 single-client throughput saturates well below 256 concurrent
        // ops; beyond that it is OS-thread cost for zero gain). Reject with
        // an error naming the config key AND the cap value. RED today:
        // resolves with no error (only the `== 0` check exists).
        let text = "[transfer]\nconcurrency = 257\n";
        let cfg = parse_config_str(text).unwrap();
        let err = settings(&cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("transfer.concurrency"),
            "must name transfer.concurrency: {msg}"
        );
        assert!(msg.contains("256"), "must name the cap value 256: {msg}");
    }

    #[test]
    fn resolve_settings_allows_concurrency_at_max() {
        // I20-r1/F2 boundary pin: concurrency = 256 (the `MAX_CONCURRENCY`
        // ceiling itself) is valid - guards an off-by-one in the cap check.
        let text = "[transfer]\nconcurrency = 256\n";
        let cfg = parse_config_str(text).unwrap();
        let s = settings(&cfg).unwrap();
        assert_eq!(s.concurrency, 256);
    }

    #[test]
    fn resolve_settings_allows_concurrency_1() {
        // I20-config: concurrency = 1 is valid (the dedicated sequential
        // path, I20-one) - must not be rejected with the zero rule.
        let text = "[transfer]\nconcurrency = 1\n";
        let cfg = parse_config_str(text).unwrap();
        let s = settings(&cfg).unwrap();
        assert_eq!(s.concurrency, 1);
    }

    #[test]
    fn resolve_settings_retry_rejects_base_above_max() {
        // I8-validation (W56): base_delay_ms > max_delay_ms is self-
        // contradictory; reject with an error naming both keys.
        let text = "[transfer.retry]\nbase_delay_ms = 5000\nmax_delay_ms = 1000\n";
        let cfg = parse_config_str(text).unwrap();
        let err = settings(&cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("transfer.retry.base_delay_ms")
                && msg.contains("transfer.retry.max_delay_ms"),
            "must name both keys: {msg}"
        );
    }

    #[test]
    fn resolve_settings_retry_rejects_lone_delay_against_default_counterpart() {
        // I8-validation (PR21-r2 L5 / W144): per-field defaults mean validation
        // runs against the filled mix (cli.md W136). A lone max_delay_ms = 500
        // fails against default base 1000; a lone base_delay_ms = 30000 fails
        // against default max 20000. Both must name both keys (same surface as
        // the both-set base>max error).
        // Mutation-checked: removing the base>max branch flips this RED.
        let text = "[transfer.retry]\nmax_delay_ms = 500\n";
        let cfg = parse_config_str(text).unwrap();
        let err = settings(&cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("transfer.retry.base_delay_ms")
                && msg.contains("transfer.retry.max_delay_ms"),
            "lone max_delay_ms = 500 must fail naming both keys against default base: {msg}"
        );

        let text = "[transfer.retry]\nbase_delay_ms = 30000\n";
        let cfg = parse_config_str(text).unwrap();
        let err = settings(&cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("transfer.retry.base_delay_ms")
                && msg.contains("transfer.retry.max_delay_ms"),
            "lone base_delay_ms = 30000 must fail naming both keys against default max: {msg}"
        );
    }

    #[test]
    fn resolve_settings_retry_rejects_zero_base_delay() {
        // I8-validation (W130, M2): the SDK requires a non-zero initial
        // backoff, so `base_delay_ms = 0` must be rejected naming
        // transfer.retry.base_delay_ms.
        // RED: resolves fine today (0 <= 20000 default max).
        let text = "[transfer.retry]\nmax_attempts = 5\nbase_delay_ms = 0\nmax_delay_ms = 20000\n";
        let cfg = parse_config_str(text).unwrap();
        let err = settings(&cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("transfer.retry.base_delay_ms") && msg.contains(">= 1"),
            "must name base_delay_ms with a non-zero reason: {msg}"
        );
    }

    #[test]
    fn resolve_settings_retry_rejects_zero_max_delay() {
        // I8-validation (W130, M2): the SDK requires a non-zero max backoff.
        // Two sub-cases per the reviewer's example.
        // RED (lone max_delay_ms = 0): today errors via the base>max rule
        // naming both keys with the wrong reason; a max_delay_ms = 0 alone
        // with the default base (1000) must instead name max_delay_ms with a
        // non-zero reason.
        let text = "[transfer.retry]\nmax_attempts = 5\nmax_delay_ms = 0\n";
        let cfg = parse_config_str(text).unwrap();
        let err = settings(&cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("transfer.retry.max_delay_ms") && msg.contains(">= 1"),
            "must name max_delay_ms with a non-zero reason: {msg}"
        );

        // RED (base and max both 0): resolves today (0 <= 0); must error.
        let text = "[transfer.retry]\nmax_attempts = 5\nbase_delay_ms = 0\nmax_delay_ms = 0\n";
        let cfg = parse_config_str(text).unwrap();
        let err = settings(&cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("transfer.retry.base_delay_ms") && msg.contains(">= 1"),
            "must name base_delay_ms first for the both-zero case: {msg}"
        );
    }

    #[test]
    fn resolve_settings_retry_allows_equal_base_and_max() {
        // I8-validation (W130, M2 boundary pin): base == max with both > 0
        // is valid (reviewer: "equal base==max with both > 0 is fine").
        let text = "[transfer.retry]\nmax_attempts = 5\nbase_delay_ms = 500\nmax_delay_ms = 500\n";
        let cfg = parse_config_str(text).unwrap();
        let s = settings(&cfg).unwrap();
        assert_eq!(s.retry.base_delay_ms, 500);
        assert_eq!(s.retry.max_delay_ms, 500);
    }

    #[test]
    fn config_unknown_retry_key_rejected() {
        // W56 (B nit): an unknown key inside `[transfer.retry]` (here a
        // `max_attemps` typo, missing the second `t`) is a loud parse error
        // naming the key, matching `config_unknown_transfer_key_rejected`.
        // RED: `RetryConfig` does not exist yet (compile failure).
        let text = "[transfer.retry]\nmax_attemps = 3\n";
        let err = parse_config_str(text).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("max_attemps"),
            "unknown retry key not named in: {msg}"
        );
    }

    #[test]
    fn config_parse_minimal() {
        let text = "[store]\nbucket = \"b\"\nregion = \"eu-west-1\"\n";
        let cfg = parse_config_str(text).unwrap();
        let s = settings(&cfg).unwrap();
        assert_eq!(s.store.bucket, "b");
        assert_eq!(s.store.region.as_deref(), Some("eu-west-1"));
        assert_eq!(s.store.prefix, "");
        assert!(!s.store.path_style);
        assert_eq!(s.mtime_tolerance_ms, 1000, "tolerance default");
        assert_eq!(s.concurrency, 4, "concurrency default");
        assert_eq!(s.vault_root, PathBuf::from("."), "no vault_root -> .");
    }

    #[test]
    fn config_missing_file_default_search_ok() {
        // No config anywhere -> defaults, no error.
        let loads = load_config(None, &[]).unwrap();
        assert_eq!(loads.vault_root, None);
        let s = settings(&loads).unwrap();
        assert_eq!(s.store.bucket, "");
    }

    #[test]
    fn config_explicit_missing_file_errors() {
        let missing = Path::new("/nonexistent/vaultsync-config-zzz.toml");
        let err = load_config(Some(missing), &[]).unwrap_err();
        assert!(
            format!("{err}").contains("cannot read config"),
            "err: {err}"
        );
    }

    #[test]
    fn config_search_order() {
        // `./.vaultsync.toml` beats `~/.config/vaultsync/config.toml`.
        let cwd = TempDir::new("vaultsync-cfg");
        let home = TempDir::new("vaultsync-cfg-home");
        std::fs::create_dir_all(home.join(".config/vaultsync")).unwrap();
        std::fs::write(
            cwd.join(".vaultsync.toml"),
            "[store]\nbucket = \"cwd-bucket\"\n",
        )
        .unwrap();
        std::fs::write(
            home.join(".config/vaultsync/config.toml"),
            "[store]\nbucket = \"home-bucket\"\n",
        )
        .unwrap();
        let search = default_search_paths(cwd.path(), Some(home.path()));
        let cfg = load_config(None, &search).unwrap();
        let s = settings(&cfg).unwrap();
        assert_eq!(s.store.bucket, "cwd-bucket");
    }

    #[test]
    fn config_rejects_unknown_store_type() {
        let text = "[store]\ntype = \"azure\"\nbucket = \"b\"\n";
        let cfg = parse_config_str(text).unwrap();
        let err = settings(&cfg).unwrap_err();
        assert!(format!("{err}").contains("azure"), "err: {err}");
    }

    #[test]
    fn config_requires_bucket() {
        let text = "[store]\nregion = \"us-east-1\"\n";
        let cfg = parse_config_str(text).unwrap();
        let err = settings(&cfg).unwrap_err();
        assert!(
            format!("{err}").to_lowercase().contains("bucket"),
            "err: {err}"
        );
    }

    #[test]
    fn resolve_store_rejects_whitespace_only_bucket() {
        // r11-L2 (W98): bucket is the one *required* store field; for a
        // required field the nonblank policy is a hard error (unlike optional
        // region/endpoint where blank means unset). Today only
        // `bucket.is_empty()` is rejected, so `bucket = "   "` resolves and
        // fails late at the first SDK call. RED: resolves with no error.
        for bad in ["", "   "] {
            let text = format!("[store]\nbucket = \"{bad}\"\n");
            let cfg = parse_config_str(&text).unwrap();
            let err = settings(&cfg).unwrap_err();
            assert!(
                format!("{err}").contains("store.bucket") && format!("{err}").contains("empty"),
                "{bad:?} must error naming store.bucket as empty: {err}"
            );
        }
        // Boundary: a nonblank value with stray padding still resolves
        // verbatim - no silent trimming of real values.
        let cfg = parse_config_str("[store]\nbucket = \"b \"\n").unwrap();
        let s = settings(&cfg).unwrap();
        assert_eq!(s.store.bucket, "b ", "no silent trimming of real values");
    }

    #[test]
    fn config_prefix_normalized_trailing_slash() {
        assert_eq!(normalize_prefix("notes"), "notes/");
        assert_eq!(normalize_prefix("notes/"), "notes/");
        assert_eq!(normalize_prefix(""), "");
        assert_eq!(normalize_prefix("a/b/"), "a/b/");
        assert_eq!(normalize_prefix("a/b"), "a/b/");
    }

    #[test]
    fn config_mtime_tolerance_default_1000() {
        let cfg = FileConfig::default();
        let s = settings(&cfg).unwrap();
        assert_eq!(s.mtime_tolerance_ms, 1000);
    }

    #[test]
    fn resolve_settings_empty_config_region_is_unset() {
        // r10-M2 (W86): `[store].region = ""` / whitespace-only must be
        // treated as unset, mirroring the W69 env policy - `Region::new("")`
        // fails late with an opaque SDK error and breaks the default chain.
        // Fails today: the config value flows through unfiltered as
        // `Some("")`.
        for bad in ["", "   "] {
            let text = format!("[store]\nbucket = \"b\"\nregion = \"{bad}\"\n");
            let cfg = parse_config_str(&text).unwrap();
            let s = settings(&cfg).unwrap();
            assert_eq!(
                s.store.region, None,
                "region {bad:?} must resolve to None (unset)"
            );
        }
        // a real value still passes through
        let cfg = parse_config_str("[store]\nbucket = \"b\"\nregion = \"us-east-1\"\n").unwrap();
        assert_eq!(
            settings(&cfg).unwrap().store.region.as_deref(),
            Some("us-east-1")
        );
        // env-region precedence over a whitespace config region is unchanged:
        // the env value wins (whitespace config is unset, not a hard error)
        let cfg = parse_config_str("[store]\nbucket = \"b\"\nregion = \"   \"\n").unwrap();
        let env = EnvSnapshot {
            aws_region: Some("eu-west-3".to_string()),
        };
        let s = resolve_settings(&cfg, &env).unwrap();
        assert_eq!(s.store.region.as_deref(), Some("eu-west-3"));
    }

    #[test]
    fn resolve_settings_whitespace_config_endpoint_is_unset() {
        // r10-M2 (W86): `[store].endpoint = ""` / whitespace-only must
        // resolve to `None` - `endpoint_url("")` fails late with an opaque
        // SDK error. Fails today: the config value flows through unfiltered
        // as `Some("")`.
        for bad in ["", "  "] {
            let text = format!("[store]\nbucket = \"b\"\nendpoint = \"{bad}\"\n");
            let cfg = parse_config_str(&text).unwrap();
            let s = settings(&cfg).unwrap();
            assert_eq!(
                s.store.endpoint, None,
                "endpoint {bad:?} must resolve to None (unset)"
            );
        }
        // a real value still passes through
        let cfg = parse_config_str("[store]\nbucket = \"b\"\nendpoint = \"https://minio.local\"\n")
            .unwrap();
        assert_eq!(
            settings(&cfg).unwrap().store.endpoint.as_deref(),
            Some("https://minio.local")
        );
    }

    #[test]
    fn resolve_settings_env_overrides_config_region() {
        // Slice 8 lock: env `AWS_REGION` overrides an explicit config region.
        let text = "[store]\nbucket = \"b\"\nregion = \"us-east-1\"\n";
        let cfg = parse_config_str(text).unwrap();
        let no_env = resolve_settings(&cfg, &EnvSnapshot::default()).unwrap();
        assert_eq!(no_env.store.region.as_deref(), Some("us-east-1"));
        let env = EnvSnapshot {
            aws_region: Some("eu-west-3".to_string()),
        };
        let with_env = resolve_settings(&cfg, &env).unwrap();
        assert_eq!(
            with_env.store.region.as_deref(),
            Some("eu-west-3"),
            "env overrides config region"
        );
    }

    #[test]
    fn resolve_settings_empty_env_region_falls_back() {
        // W69/A-N1: an empty `AWS_REGION` env value is treated as unset (the
        // SDK's own env provider does the same). It must never override a
        // config region with `Some("")` - which would build
        // `Region::new("")` and break the whole default chain.
        let text = "[store]\nbucket = \"b\"\nregion = \"eu-west-3\"\n";
        let cfg = parse_config_str(text).unwrap();
        let empty_env = EnvSnapshot {
            aws_region: Some(String::new()),
        };
        let with_config = resolve_settings(&cfg, &empty_env).unwrap();
        assert_eq!(
            with_config.store.region.as_deref(),
            Some("eu-west-3"),
            "empty env region must fall back to the config region"
        );
        // whitespace-only env value is also unset
        let ws_env = EnvSnapshot {
            aws_region: Some("   ".to_string()),
        };
        let with_config_ws = resolve_settings(&cfg, &ws_env).unwrap();
        assert_eq!(with_config_ws.store.region.as_deref(), Some("eu-west-3"));
        // and with no config region the result stays None (default chain)
        let no_cfg = parse_config_str("[store]\nbucket = \"b\"\n").unwrap();
        let no_region = resolve_settings(&no_cfg, &empty_env).unwrap();
        assert_eq!(no_region.store.region, None);
    }

    #[test]
    fn region_unconfigured_is_none() {
        // W7/B-M2: no [store].region and no AWS_REGION env -> region is None
        // so the AWS default chain (env/shared config/profile) decides.
        let text = "[store]\nbucket = \"b\"\n";
        let cfg = parse_config_str(text).unwrap();
        let s = settings(&cfg).unwrap();
        assert_eq!(s.store.region, None);
    }

    #[test]
    fn config_unknown_transfer_key_rejected() {
        // W56 (B nit): unknown TOML keys must fail loudly instead of being
        // silently ignored - a `mtime_tolerance` typo (missing `_ms`) must
        // surface as a parse error naming the key, not keep the 1000 default
        // (consistent with the W25/W28 loud-on-inert-config posture).
        let text = "[transfer]\nmtime_tolerance = 5000\n";
        let err = parse_config_str(text).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("mtime_tolerance"),
            "unknown key not named in: {msg}"
        );
    }

    #[test]
    fn config_unknown_top_level_key_rejected() {
        // W56 (B nit): a misspelled top-level key (e.g. `vault_rooot`) is a
        // loud parse error, not a silently ignored unknown.
        let text = "vault_rooot = \"/x\"\n";
        let err = parse_config_str(text).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("vault_rooot"),
            "unknown key not named in: {msg}"
        );
    }

    #[test]
    fn config_prefix_with_empty_segment_rejected() {
        // W58 (A nit): a `[store].prefix` containing an empty path segment
        // ("a//b", "/a") is silently normalized today ("a//b/" - a prefix
        // that would produce keys `ensure_valid_key` rejects). Reject it
        // loudly at resolution, matching the key validator's taste. A
        // trailing slash ("a/") is the normalized form and stays fine.
        for bad in ["a//b", "/a"] {
            let text = format!("[store]\nbucket = \"b\"\nprefix = \"{bad}\"\n");
            let cfg = parse_config_str(&text).unwrap();
            let err = settings(&cfg).unwrap_err();
            let msg = format!("{err}");
            assert!(msg.contains("prefix"), "prefix {bad:?}: {msg}");
        }
        // trailing slash is the normalized form - still accepted
        let text = "[store]\nbucket = \"b\"\nprefix = \"a/\"\n";
        let cfg = parse_config_str(text).unwrap();
        assert_eq!(settings(&cfg).unwrap().store.prefix, "a/");
    }

    #[test]
    fn config_prefix_validated_against_ensure_valid_key() {
        // r11-L3 (W105, rescoped): the configured prefix must itself be a
        // valid vault key prefix (`ensure_valid_key` semantics) so a
        // whitespace-only or control-char segment fails fast at config
        // resolution with an error naming `store.prefix`, instead of
        // silently writing oddly-segmented keys to the remote. This subsumes
        // the W58 empty-segment check with one policy. RED: resolves with no
        // error today.
        for bad in ["a/ /b", "a/\\u0007b"] {
            let text = format!("[store]\nbucket = \"b\"\nprefix = \"{bad}\"\n");
            let cfg = parse_config_str(&text).unwrap();
            let err = settings(&cfg).unwrap_err();
            assert!(
                format!("{err}").contains("store.prefix"),
                "prefix {bad:?} must error naming store.prefix: {err}"
            );
        }
        // W58 empty-segment rejections keep their existing message class
        // (still an error naming the config key).
        for bad in ["a//b", "/a"] {
            let text = format!("[store]\nbucket = \"b\"\nprefix = \"{bad}\"\n");
            let cfg = parse_config_str(&text).unwrap();
            let err = settings(&cfg).unwrap_err();
            assert!(format!("{err}").contains("prefix"), "prefix {bad:?}: {err}");
        }
        // valid prefixes still resolve: plain, trailing-slash, unicode, empty
        for good in ["notes", "a/b/", "tagebuch/", ""] {
            let text = format!("[store]\nbucket = \"b\"\nprefix = \"{good}\"\n");
            let cfg = parse_config_str(&text).unwrap();
            let s = settings(&cfg).unwrap();
            assert!(
                s.store.prefix.is_empty() || s.store.prefix.ends_with('/'),
                "prefix {good:?} resolved to {:?}",
                s.store.prefix
            );
        }
    }

    #[test]
    fn config_invalid_toml_reports_line() {
        // toml parse errors carry line info; they surface through the error.
        let text = "[store\nbucket = \"b\"\n";
        let err = parse_config_str(text).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("line") || msg.contains("Line") || msg.matches('1').next().is_some(),
            "expected line info in: {msg}"
        );
    }
}
