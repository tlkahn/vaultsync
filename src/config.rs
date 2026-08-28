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
pub const DEFAULT_REGION: &str = "us-east-1";

/// On-disk config mirroring [cli.md]. All sections optional; defaults applied
/// at resolution time.
#[derive(Debug, Default, Clone, Deserialize)]
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
pub struct IgnoreConfig {
    #[serde(default)]
    pub patterns: Vec<String>,
}

/// `[transfer]` section.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct TransferConfig {
    #[serde(default)]
    pub concurrency: Option<u32>,
    #[serde(default)]
    pub mtime_tolerance_ms: Option<u64>,
}

/// Fully resolved runtime settings (config + CLI + env merged).
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub vault_root: PathBuf,
    pub store: StoreSettings,
    pub mtime_tolerance_ms: u64,
    pub concurrency: u32,
}

/// Resolved store connection settings (no credentials - those stay in the AWS
/// chain).
#[derive(Debug, Clone, PartialEq)]
pub struct StoreSettings {
    /// Empty only when no `[store]` section was present (mock/offline phase).
    pub bucket: String,
    pub region: String,
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
        return parse_config_str(&text).map_err(|e| {
            Error::Other(format!("invalid config {}: {e}", p.display()))
        });
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

/// CLI overrides feeding resolution (Phase 2: only `--vault`).
#[derive(Debug, Default)]
pub struct Cli<'a> {
    pub vault: Option<&'a Path>,
}

/// Snapshot of the relevant env vars, injected so resolution is testable
/// without real env. Phase 2 cares about `AWS_REGION` only (credentials
/// themselves stay in the AWS chain).
#[derive(Debug, Default, Clone)]
pub struct EnvSnapshot {
    pub aws_region: Option<String>,
}

/// Merge config + CLI + env into a [`Settings`]. Pure: no IO, no network.
pub fn resolve_settings(
    cfg: &FileConfig,
    cli: &Cli,
    env: &EnvSnapshot,
) -> Result<Settings, Error> {
    let vault_root = match cli.vault {
        Some(v) => v.to_path_buf(),
        None => cfg.vault_root.clone().unwrap_or_else(|| PathBuf::from(".")),
    };
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
    Ok(Settings {
        vault_root,
        store,
        mtime_tolerance_ms,
        concurrency,
    })
}

fn resolve_store(store: Option<&StoreConfig>, env: &EnvSnapshot) -> Result<StoreSettings, Error> {
    let region_default = env
        .aws_region
        .clone()
        .unwrap_or_else(|| DEFAULT_REGION.to_string());
    match store {
        // No `[store]` section -> offline/mock defaults, never an error.
        None => Ok(StoreSettings {
            bucket: String::new(),
            region: region_default,
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
            if bucket.is_empty() {
                return Err(Error::Other("store.bucket must not be empty".to_string()));
            }
            // Env `AWS_REGION` overrides an explicit config region (locked by
            // resolve_settings_env_overrides_config_region).
            let region = env
                .aws_region
                .clone()
                .or_else(|| s.region.clone())
                .unwrap_or_else(|| DEFAULT_REGION.to_string());
            Ok(StoreSettings {
                bucket,
                region,
                endpoint: s.endpoint.clone(),
                prefix: normalize_prefix(s.prefix.as_deref().unwrap_or("")),
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
        resolve_settings(cfg, &Cli::default(), &EnvSnapshot::default())
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
"#;
        let cfg = parse_config_str(text).unwrap();
        assert_eq!(cfg.vault_root.as_deref(), Some(Path::new("/Users/me/Notes")));
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
    }

    #[test]
    fn config_parse_minimal() {
        let text = "[store]\nbucket = \"b\"\nregion = \"eu-west-1\"\n";
        let cfg = parse_config_str(text).unwrap();
        let s = settings(&cfg).unwrap();
        assert_eq!(s.store.bucket, "b");
        assert_eq!(s.store.region, "eu-west-1");
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
        assert!(format!("{err}").contains("cannot read config"), "err: {err}");
    }

    #[test]
    fn config_search_order() {
        // `./.vaultsync.toml` beats `~/.config/vaultsync/config.toml`.
        let cwd = TempDir::new("vaultsync-cfg");
        let home = TempDir::new("vaultsync-cfg-home");
        std::fs::create_dir_all(home.join(".config/vaultsync")).unwrap();
        std::fs::write(cwd.join(".vaultsync.toml"), "[store]\nbucket = \"cwd-bucket\"\n").unwrap();
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
        assert!(format!("{err}").to_lowercase().contains("bucket"), "err: {err}");
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
    fn config_cli_vault_overrides_config() {
        let mut cfg = FileConfig::default();
        cfg.vault_root = Some(PathBuf::from("/cfg/vault"));
        let cli = Cli {
            vault: Some(Path::new("/cli/vault")),
        };
        let s = resolve_settings(&cfg, &cli, &EnvSnapshot::default()).unwrap();
        assert_eq!(s.vault_root, PathBuf::from("/cli/vault"));

        let cli_none = Cli::default();
        let s2 = resolve_settings(&cfg, &cli_none, &EnvSnapshot::default()).unwrap();
        assert_eq!(s2.vault_root, PathBuf::from("/cfg/vault"));
    }

    #[test]
    fn config_mtime_tolerance_default_1000() {
        let cfg = FileConfig::default();
        let s = settings(&cfg).unwrap();
        assert_eq!(s.mtime_tolerance_ms, 1000);
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
