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

/// `[ignore]` section (patterns are a Phase 3 feature; parsed but unused).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IgnoreConfig {
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
    /// Parsed non-empty `[ignore].patterns` (W25/M3). A Phase 3 feature that
    /// is surfaced loudly - never silently applied - so a user copying the
    /// cli.md example is not let to believe patterns are in effect.
    pub ignore_patterns: Vec<String>,
    /// True when the TOML explicitly set `[transfer].concurrency` (W28/M6):
    /// inert until Phase 3 (the pool does not exist), so dispatch warns rather
    /// than silently accepting it.
    pub concurrency_explicitly_set: bool,
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
    let ignore_patterns = cfg
        .ignore
        .as_ref()
        .map(|i| i.patterns.clone())
        .unwrap_or_default();
    let concurrency_explicitly_set = cfg.transfer.as_ref().and_then(|t| t.concurrency).is_some();
    Ok(Settings {
        vault_root,
        store,
        mtime_tolerance_ms,
        concurrency,
        ignore_patterns,
        concurrency_explicitly_set,
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
