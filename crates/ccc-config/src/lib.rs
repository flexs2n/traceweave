//! `ccc-config` — configuration loading, precedence, and the effective
//! `configuration_hash` (TIS §4.2/§4.3).
//!
//! Precedence: built-in defaults < `config.toml` in the data directory <
//! environment overrides (`CCC_*`). The `configuration_hash` is the BLAKE3 of
//! the canonically serialized effective indexing configuration; it feeds
//! snapshot identity and artifact cache keys.

use ccc_domain::{blake3_hex, canonical_json, CccError};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Current configuration schema version; changing it changes every
/// `configuration_hash`.
pub const CONFIG_SCHEMA_VERSION: u32 = 1;

/// Indexing-scoped configuration. Only this subset participates in
/// `configuration_hash` so that relocating the data directory does not churn
/// snapshot identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexingConfig {
    /// Include untracked, non-ignored files in discovery and manifests.
    pub include_untracked: bool,
    /// Glob patterns (repo-relative, forward slashes) excluded from indexing.
    pub exclude_globs: Vec<String>,
    /// Files larger than this are excluded with reason `oversized`.
    pub max_file_size_bytes: u64,
    /// Bound on concurrent parsing tasks (TIS §15 bounded concurrency).
    pub parse_concurrency: usize,
    /// Default dependency-traversal depth for bundles and impact (TIS §16.4).
    pub traversal_depth: u32,
    /// Cap on the number of lexical candidates considered before ranking.
    pub max_candidates: usize,
}

impl Default for IndexingConfig {
    fn default() -> Self {
        Self {
            include_untracked: true,
            exclude_globs: vec![
                "target/**".into(),
                ".git/**".into(),
                "node_modules/**".into(),
                "dist/**".into(),
                "vendor/**".into(),
            ],
            max_file_size_bytes: 1_048_576,
            parse_concurrency: 4,
            traversal_depth: 3,
            max_candidates: 200,
        }
    }
}

impl IndexingConfig {
    /// BLAKE3 of the canonical serialization of this configuration.
    pub fn configuration_hash(&self) -> Result<String, CccError> {
        let mut value = serde_json::to_value(self)
            .map_err(|e| CccError::internal_error(format!("config serialization: {e}")))?;
        if let serde_json::Value::Object(map) = &mut value {
            map.insert(
                "config_schema_version".into(),
                serde_json::Value::from(CONFIG_SCHEMA_VERSION),
            );
        }
        canonical_json(&value)
            .map(|s| blake3_hex(s.as_bytes()))
            .map_err(|e| CccError::internal_error(format!("config hashing: {e}")))
    }
}

/// REST server configuration (Step 4, NFR-020). Not part of the
/// `configuration_hash` — the server's identity has no effect on snapshot
/// identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    /// Socket address the REST server binds (default loopback-only).
    pub bind: String,
    /// Optional bearer token. A non-loopback bind without a token is refused
    /// at startup (`CCC_API_TOKEN` env overrides this value).
    pub token: Option<String>,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:7713".into(),
            token: None,
        }
    }
}

/// Full local configuration, including the data directory location.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CccConfig {
    pub indexing: IndexingConfig,
    pub api: ApiConfig,
}

impl CccConfig {
    pub fn indexing(&self) -> &IndexingConfig {
        &self.indexing
    }

    pub fn api(&self) -> &ApiConfig {
        &self.api
    }
}

/// Resolves the data directory: explicit `--data-dir` > `CCC_HOME` >
/// `$HOME/.ccc` (or `$USERPROFILE\.ccc` on Windows).
pub fn resolve_data_dir(explicit: Option<&Path>) -> PathBuf {
    if let Some(dir) = explicit {
        return dir.to_path_buf();
    }
    if let Ok(home) = std::env::var("CCC_HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home);
        }
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".into());
    Path::new(&home).join(".ccc")
}

/// Loads the effective configuration: defaults < file < environment.
pub fn load_config(data_dir: &Path) -> Result<CccConfig, CccError> {
    let mut indexing = IndexingConfig::default();
    let mut api = ApiConfig::default();

    let file_path = data_dir.join("config.toml");
    if file_path.exists() {
        let raw = std::fs::read_to_string(&file_path).map_err(|e| {
            CccError::invalid_request(format!(
                "cannot read config file {}: {e}",
                file_path.display()
            ))
        })?;
        let file_cfg: FileConfig = toml::from_str(&raw)
            .map_err(|e| CccError::invalid_request(format!("invalid config.toml: {e}")))?;
        apply_file(&mut indexing, file_cfg.indexing);
        apply_api_file(&mut api, file_cfg.api);
    }

    apply_env(&mut indexing)?;
    apply_api_env(&mut api)?;

    Ok(CccConfig { indexing, api })
}

/// Serde shape of the on-disk `config.toml` (all fields optional).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FileConfig {
    indexing: FileIndexing,
    api: FileApiConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FileApiConfig {
    bind: Option<String>,
    token: Option<String>,
}

fn apply_api_file(target: &mut ApiConfig, file: FileApiConfig) {
    if let Some(v) = file.bind {
        target.bind = v;
    }
    if let Some(v) = file.token {
        target.token = Some(v);
    }
}

fn apply_api_env(target: &mut ApiConfig) -> Result<(), CccError> {
    if let Ok(v) = std::env::var("CCC_API_BIND") {
        if !v.trim().is_empty() {
            target.bind = v.trim().to_string();
        }
    }
    if let Ok(v) = std::env::var("CCC_API_TOKEN") {
        // Empty string means "no token" (loopback-only semantics).
        target.token = Some(v.trim().to_string()).filter(|s| !s.is_empty());
    }
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FileIndexing {
    include_untracked: Option<bool>,
    exclude_globs: Option<Vec<String>>,
    max_file_size_bytes: Option<u64>,
    parse_concurrency: Option<usize>,
    traversal_depth: Option<u32>,
    max_candidates: Option<usize>,
}

fn apply_file(target: &mut IndexingConfig, f: FileIndexing) {
    if let Some(v) = f.include_untracked {
        target.include_untracked = v;
    }
    if let Some(v) = f.exclude_globs {
        target.exclude_globs = v;
    }
    if let Some(v) = f.max_file_size_bytes {
        target.max_file_size_bytes = v;
    }
    if let Some(v) = f.parse_concurrency {
        target.parse_concurrency = v;
    }
    if let Some(v) = f.traversal_depth {
        target.traversal_depth = v;
    }
    if let Some(v) = f.max_candidates {
        target.max_candidates = v;
    }
}

fn apply_env(target: &mut IndexingConfig) -> Result<(), CccError> {
    if let Some(v) = env_bool("CCC_INCLUDE_UNTRACKED")? {
        target.include_untracked = v;
    }
    if let Ok(v) = std::env::var("CCC_EXCLUDE_GLOBS") {
        target.exclude_globs = v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
    }
    if let Some(v) = env_parse::<u64>("CCC_MAX_FILE_SIZE_BYTES")? {
        target.max_file_size_bytes = v;
    }
    if let Some(v) = env_parse::<usize>("CCC_PARSE_CONCURRENCY")? {
        target.parse_concurrency = v;
    }
    if let Some(v) = env_parse::<u32>("CCC_TRAVERSAL_DEPTH")? {
        target.traversal_depth = v;
    }
    if let Some(v) = env_parse::<usize>("CCC_MAX_CANDIDATES")? {
        target.max_candidates = v;
    }
    Ok(())
}

fn env_bool(key: &str) -> Result<Option<bool>, CccError> {
    match std::env::var(key) {
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(Some(true)),
            "0" | "false" | "no" | "off" => Ok(Some(false)),
            other => Err(CccError::invalid_request(format!(
                "environment variable {key} has invalid boolean value {other:?}"
            ))),
        },
        Err(_) => Ok(None),
    }
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Result<Option<T>, CccError> {
    match std::env::var(key) {
        Ok(v) => v.trim().parse::<T>().map(Some).map_err(|_| {
            CccError::invalid_request(format!("environment variable {key} is not a valid number"))
        }),
        Err(_) => Ok(None),
    }
}

/// Writes a default `config.toml` into a data directory (used by `ccc init`).
pub fn write_default_config(data_dir: &Path) -> Result<PathBuf, CccError> {
    std::fs::create_dir_all(data_dir)
        .map_err(|e| CccError::internal_error(format!("cannot create data dir: {e}")))?;
    let path = data_dir.join("config.toml");
    if path.exists() {
        return Ok(path);
    }
    let defaults = IndexingConfig::default();
    let body = format!(
        "# ccc local configuration (defaults; see docs)\n\n[indexing]\ninclude_untracked = {}\nmax_file_size_bytes = {}\nparse_concurrency = {}\ntraversal_depth = {}\nmax_candidates = {}\nexclude_globs = {:?}\n\n# REST server (ccc serve); token enables bearer auth (NFR-020)\n#[api]\n#bind = \"127.0.0.1:7713\"\n#token = \"\"\n",
        defaults.include_untracked,
        defaults.max_file_size_bytes,
        defaults.parse_concurrency,
        defaults.traversal_depth,
        defaults.max_candidates,
        defaults.exclude_globs
    );
    std::fs::write(&path, body)
        .map_err(|e| CccError::internal_error(format!("cannot write config.toml: {e}")))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clear_env() {
        for key in [
            "CCC_INCLUDE_UNTRACKED",
            "CCC_EXCLUDE_GLOBS",
            "CCC_MAX_FILE_SIZE_BYTES",
            "CCC_PARSE_CONCURRENCY",
            "CCC_TRAVERSAL_DEPTH",
            "CCC_MAX_CANDIDATES",
        ] {
            std::env::remove_var(key);
        }
    }

    /// Environment variables are process-global; tests that touch them must
    /// not run concurrently with each other.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn defaults_apply_without_file_or_env() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_config(dir.path()).unwrap();
        assert_eq!(cfg.indexing, IndexingConfig::default());
        assert_eq!(cfg.indexing.traversal_depth, 3);
    }

    #[test]
    fn file_overrides_defaults() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[indexing]\ntraversal_depth = 5\nexclude_globs = [\"gen/**\"]\n",
        )
        .unwrap();
        let cfg = load_config(dir.path()).unwrap();
        assert_eq!(cfg.indexing.traversal_depth, 5);
        assert_eq!(cfg.indexing.exclude_globs, vec!["gen/**".to_string()]);
        // untouched default survives
        assert_eq!(cfg.indexing.parse_concurrency, 4);
    }

    #[test]
    fn env_overrides_file() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[indexing]\ntraversal_depth = 5\n",
        )
        .unwrap();
        std::env::set_var("CCC_TRAVERSAL_DEPTH", "7");
        std::env::set_var("CCC_INCLUDE_UNTRACKED", "false");
        let cfg = load_config(dir.path()).unwrap();
        clear_env();
        assert_eq!(cfg.indexing.traversal_depth, 7);
        assert!(!cfg.indexing.include_untracked);
    }

    #[test]
    fn invalid_env_value_is_invalid_request() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        std::env::set_var("CCC_MAX_FILE_SIZE_BYTES", "not-a-number");
        let dir = tempfile::tempdir().unwrap();
        let err = load_config(dir.path()).unwrap_err();
        clear_env();
        assert_eq!(err.code, ccc_domain::ErrorCode::InvalidRequest);
    }

    #[test]
    fn configuration_hash_is_stable_across_reloads() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_config(dir.path()).unwrap();
        let h1 = cfg.indexing().configuration_hash().unwrap();
        let cfg2 = load_config(dir.path()).unwrap();
        let h2 = cfg2.indexing().configuration_hash().unwrap();
        assert_eq!(h1, h2, "hash must be stable across reloads");
    }

    #[test]
    fn configuration_hash_changes_with_setting() {
        let a = IndexingConfig {
            parse_concurrency: 8,
            ..Default::default()
        };
        let b = IndexingConfig {
            traversal_depth: 4,
            ..Default::default()
        };
        assert_ne!(
            a.configuration_hash().unwrap(),
            b.configuration_hash().unwrap()
        );
    }
}
