//! Execution policy model (TIS §11.2, SDD §15.3).
//!
//! Policies live only in the user's data directory (`<data>/policies/<name>.toml`).
//! Repository content is untrusted input (TIS §17.2): no repo-provided file,
//! script, or instruction can define, extend, or override a policy. The
//! built-in `analysis` policy executes nothing.

use ccc_domain::{blake3_hex, canonical_json, CccError};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The built-in analysis-only policy. It is always available and can never be
/// shadowed by a data-dir file of the same name.
pub const DEFAULT_POLICY_NAME: &str = "analysis";

/// Default per-check output cap (64 KiB); truncation is flagged, never silent.
pub const DEFAULT_MAX_OUTPUT_BYTES: u64 = 65_536;

/// A verification execution policy (TIS §11.2, SDD §15.3). The serialized
/// form is the record of what was in force: `hash()` covers every field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VerificationPolicy {
    /// Policy file name without extension. Must match the requested name.
    pub name: String,
    /// `false` (the default) means no subprocess is ever spawned for checks:
    /// every executable suggestion is reported `not_run`.
    pub allow_execution: bool,
    /// Exact argv prefixes permitted for execution (e.g. `cargo test`). A
    /// command must match one full prefix at a word boundary to run.
    pub allowlist: Vec<String>,
    /// Prefixes denied even when an allowlist prefix matches. Checked first.
    pub disallowlist: Vec<String>,
    /// Wall-clock timeout per check; expiry kills the process tree and
    /// reports `timed_out` (never `failed`).
    pub timeout_secs: u64,
    /// Cap on captured stdout/stderr per check; overruns are truncated and
    /// the record flags it.
    pub max_output_bytes: u64,
    /// Environment variable names passed through to executed checks; the
    /// executed environment is minimal and this list bounds it.
    pub env_allowlist: Vec<String>,
    /// Whether passing checks may revalidate claims whose dependencies they
    /// cover (SDD §15.6). Disable to keep verification purely observational.
    pub revalidate_claims: bool,
}

impl Default for VerificationPolicy {
    fn default() -> Self {
        Self::analysis()
    }
}

impl VerificationPolicy {
    /// The built-in default policy: analysis-only, zero execution (TIS §11.2).
    pub fn analysis() -> Self {
        Self {
            name: DEFAULT_POLICY_NAME.into(),
            allow_execution: false,
            allowlist: Vec::new(),
            disallowlist: Vec::new(),
            timeout_secs: 600,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            env_allowlist: vec!["PATH".into(), "CARGO_HOME".into(), "RUSTUP_HOME".into()],
            revalidate_claims: true,
        }
    }

    /// BLAKE3 over the canonical (sorted-key, float-free) serialization of the
    /// effective policy. Recorded in every verification record so a report is
    /// always traceable to the exact policy text in force.
    pub fn hash(&self) -> String {
        let canonical = canonical_json(self).unwrap_or_default();
        blake3_hex(canonical.as_bytes())
    }

    /// Exact-prefix allowlist decision for a command (TIS §11.2). The
    /// disallowlist wins; an allowlist prefix must cover the whole first
    /// token boundary (e.g. `cargo test` permits `cargo test -- --exact x`
    /// but `cargo te` permits nothing).
    pub fn classify(&self, command: &str) -> PermitDecision {
        let trimmed = command.trim();
        if trimmed.is_empty() {
            return PermitDecision::Blocked("empty command".into());
        }
        if self.disallowlist.iter().any(|p| prefix_covers(p, trimmed)) {
            return PermitDecision::Blocked(format!(
                "command {trimmed:?} matches the policy disallowlist"
            ));
        }
        if self.allowlist.iter().any(|p| prefix_covers(p, trimmed)) {
            return PermitDecision::Allowed;
        }
        PermitDecision::Blocked(format!(
            "command {trimmed:?} is not in the policy allowlist (exact prefix match required)"
        ))
    }
}

/// The outcome of policy classification for one command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermitDecision {
    Allowed,
    Blocked(String),
}

/// `prefix` covers `command` when it equals the command or is followed by
/// whitespace at a token boundary.
fn prefix_covers(prefix: &str, command: &str) -> bool {
    let prefix = prefix.trim();
    if prefix.is_empty() {
        return false;
    }
    let prefix_norm = normalize_spaces(prefix);
    let command_norm = normalize_spaces(command);
    command_norm == prefix_norm
        || command_norm
            .strip_prefix(&prefix_norm)
            .is_some_and(|rest| rest.starts_with(' '))
}

fn normalize_spaces(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Directory that holds policy files (`<data>/policies/`).
pub fn policies_dir(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("policies")
}

/// Rejects names that could escape the policies directory or shadow paths:
/// only alphanumeric, `-`, `_`, and `.` interior characters are accepted, and
/// `.`/`..` are rejected outright.
fn validate_policy_name(name: &str) -> Result<(), CccError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.starts_with('.')
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(CccError::invalid_request(format!(
            "invalid policy name {name:?}; use letters, digits, '-' or '_'"
        )));
    }
    Ok(())
}

/// Loads a policy by name from the data directory (never from repository
/// content — TIS §11.2). The built-in `analysis` policy is always available.
/// Unknown names are `INVALID_REQUEST` (no silent fallback, no false
/// availability).
pub fn load_policy(data_dir: &std::path::Path, name: &str) -> Result<VerificationPolicy, CccError> {
    if name == DEFAULT_POLICY_NAME {
        return Ok(VerificationPolicy::analysis());
    }
    validate_policy_name(name)?;
    let path = policies_dir(data_dir).join(format!("{name}.toml"));
    if !path.exists() {
        return Err(CccError::invalid_request(format!(
            "unknown verification policy {name:?}; see `ccc policies list`"
        )));
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        CccError::invalid_request(format!("cannot read policy file {}: {e}", path.display()))
    })?;
    let policy: VerificationPolicy = toml::from_str(&raw).map_err(|e| {
        CccError::invalid_request(format!("invalid policy file {}: {e}", path.display()))
    })?;
    if policy.name != name {
        return Err(CccError::invalid_request(format!(
            "policy file {} declares name {:?}; requested {name:?}",
            path.display(),
            policy.name
        )));
    }
    Ok(policy)
}

/// Lists available policy names: the built-in default plus every data-dir
/// policy file stem.
pub fn list_policy_names(data_dir: &std::path::Path) -> Vec<String> {
    let mut names = vec![DEFAULT_POLICY_NAME.to_string()];
    if let Ok(entries) = std::fs::read_dir(policies_dir(data_dir)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "toml") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    if validate_policy_name(stem).is_ok() {
                        names.push(stem.to_string());
                    }
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analysis_policy_is_analysis_only_by_construction() {
        let policy = VerificationPolicy::analysis();
        assert!(!policy.allow_execution);
        assert!(
            policy.allowlist.is_empty(),
            "default policy has no allowlist"
        );
        assert_eq!(policy.classify("cargo test"), PermitDecision::Blocked("command \"cargo test\" is not in the policy allowlist (exact prefix match required)".to_string()));
    }

    #[test]
    fn hash_is_stable_and_changes_with_content() {
        let mut policy = VerificationPolicy::analysis();
        let h1 = policy.hash();
        assert_eq!(h1, policy.hash(), "hash is deterministic");
        policy.allow_execution = true;
        assert_ne!(policy.hash(), h1, "policy content changes the hash");
    }

    #[test]
    fn allowlist_matches_exact_prefixes_only() {
        let policy = VerificationPolicy {
            name: "local".into(),
            allow_execution: true,
            allowlist: vec!["cargo test".into(), "cargo build".into()],
            disallowlist: vec!["cargo publish".into()],
            ..VerificationPolicy::default()
        };
        assert_eq!(policy.classify("cargo test"), PermitDecision::Allowed);
        assert_eq!(
            policy.classify("cargo test  -- --exact sneaky"),
            PermitDecision::Allowed,
            "extra arguments stay allowed under a covering prefix"
        );
        assert_eq!(
            policy.classify("cargo test --release"),
            PermitDecision::Allowed
        );
        assert!(matches!(
            policy.classify("cargo te"),
            PermitDecision::Blocked(_)
        ));
        assert!(
            matches!(policy.classify("cargo publish"), PermitDecision::Blocked(_)),
            "disallowlist wins"
        );
        assert!(matches!(
            policy.classify("rm -rf /"),
            PermitDecision::Blocked(_)
        ));
        assert!(matches!(policy.classify(""), PermitDecision::Blocked(_)));
    }

    #[test]
    fn unknown_policy_name_is_invalid_request() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_policy(dir.path(), "no-such-policy").unwrap_err();
        assert_eq!(err.code, ccc_domain::ErrorCode::InvalidRequest);
    }

    #[test]
    fn policy_names_cannot_escape_the_policies_dir() {
        let dir = tempfile::tempdir().unwrap();
        for evil in ["../evil", "a/b", "a\\b", "..", ".", ".hidden"] {
            assert!(
                load_policy(dir.path(), evil).is_err(),
                "{evil} must be rejected"
            );
        }
    }

    #[test]
    fn policy_files_load_from_the_data_dir_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(policies_dir(dir.path())).unwrap();
        std::fs::write(
            policies_dir(dir.path()).join("local-tests.toml"),
            "name = \"local-tests\"\nallow_execution = true\nallowlist = [\"cargo test\"]\ntimeout_secs = 30\n",
        )
        .unwrap();
        let policy = load_policy(dir.path(), "local-tests").unwrap();
        assert!(policy.allow_execution);
        assert_eq!(policy.timeout_secs, 30);
        assert_eq!(policy.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
        assert_eq!(
            policy.classify("cargo test one two"),
            PermitDecision::Allowed
        );

        let names = list_policy_names(dir.path());
        assert_eq!(
            names,
            vec!["analysis".to_string(), "local-tests".to_string()]
        );
    }

    #[test]
    fn analysis_policy_cannot_be_shadowed_by_a_data_dir_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(policies_dir(dir.path())).unwrap();
        std::fs::write(
            policies_dir(dir.path()).join("analysis.toml"),
            "name = \"analysis\"\nallow_execution = true\n",
        )
        .unwrap();
        let policy = load_policy(dir.path(), "analysis").unwrap();
        assert!(
            !policy.allow_execution,
            "the built-in default never picks up data-dir overrides"
        );
    }
}
