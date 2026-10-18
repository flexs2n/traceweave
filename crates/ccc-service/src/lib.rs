//! `ccc-service` — the shared operation layer (SDD §16, Step 4 T1).
//!
//! Every core operation — repositories, indexing, search, context
//! compilation, evidence, memory, impact, verification, evaluation, export,
//! snapshot diff — is exposed exactly once here and consumed by the CLI
//! (`ccc-cli`), the REST API (`ccc-api`), and the MCP adapter (`ccc-mcp`)
//! so evidence and freshness semantics are never duplicated (SDD §16.1).
//!
//! Operations never print. Each returns an [`OpOutput`]: the machine
//! payload (`data`) plus envelope metadata (`meta`) that every integration
//! embeds in its own response shape, and a pre-rendered human rendering
//! (`human`) the CLI prints verbatim.

mod context;
mod eval;
mod evidence;
mod export;
mod impact;
mod index;
mod jobs_lookup;
mod memory;
mod repo;
mod search;
mod snapshot;
mod verify;

pub use context::{context_compile, context_compile_record, context_diff, context_show};
pub use eval::{eval_run, eval_run_record, eval_show};
pub use evidence::{evidence_list, evidence_show};
pub use export::export_data;
pub use impact::{impact_analyze, impact_analyze_record};
pub use index::{index_current, index_once, index_repo, index_repo_record, status};
pub use jobs_lookup::job_get;
pub use memory::{
    conflicts, memory_add, memory_list, memory_resolve, memory_retract, memory_search, memory_show,
    memory_supersede,
};
pub use repo::{repo_add, repo_metadata, repo_snapshots, repo_status};
pub use search::{search, search_record};
pub use snapshot::snapshot_diff;
pub use verify::{policies_list, policies_show, verify, verify_record};

use ccc_domain::{CccError, RepositoryId, RepositoryRecord, SnapshotId};
use ccc_schema::{Envelope, EnvelopeMeta, SCHEMA_VERSION};
use ccc_storage::*;
use serde::Serialize;
use serde_json::value::RawValue;
use std::path::PathBuf;
use std::str::FromStr as _;
use std::sync::Arc;

/// The shared wiring every operation needs: one resolved data directory,
/// one SQLite store, one artifact store. The CLI, API, and MCP server each
/// construct this once and pass it to every operation.
pub struct AppContext {
    /// The resolved data directory (`--data-dir` > `CCC_HOME` > `~/.ccc`).
    pub data_dir: PathBuf,
    pub store: Arc<ccc_storage::SqliteStore>,
    pub artifacts: Arc<ccc_storage::FsArtifactStore>,
}

impl AppContext {
    /// Opens the store for an initialized data directory (the CLI's
    /// `open_store` + artifact-store wiring, moved verbatim).
    pub async fn open(data_dir: &std::path::Path) -> Result<AppContext, CccError> {
        if !data_dir.exists() {
            return Err(CccError::invalid_request(format!(
                "ccc is not initialized in {}; run `ccc init` first",
                data_dir.display()
            )));
        }
        let store = Arc::new(ccc_storage::SqliteStore::connect(&data_dir.join("ccc.db")).await?);
        let artifacts = Arc::new(ccc_storage::FsArtifactStore::new(
            data_dir.join("blobs"),
            store.clone(),
        ));
        Ok(AppContext {
            data_dir: data_dir.to_path_buf(),
            store,
            artifacts,
        })
    }
}

/// One operation's output: the machine payload, envelope metadata, and the
/// pre-rendered human text. `data` is a `RawValue` so typed records keep
/// their exact serialized byte order — CLI `--json` output is byte-identical
/// to the pre-refactor envelopes.
pub struct OpOutput {
    pub data: Box<RawValue>,
    pub meta: EnvelopeMeta,
    /// Pre-rendered human output. Either plain text (one line per `println!`
    /// of the pre-refactor CLI) or, when the operation's `--format` selects
    /// the full envelope in human mode, the pre-serialized envelope JSON.
    pub human: String,
}

impl OpOutput {
    /// The full machine envelope every integration embeds (TIS §13.4).
    pub fn envelope(&self) -> Envelope<Box<RawValue>> {
        Envelope {
            schema_version: SCHEMA_VERSION.into(),
            data: self.data.clone(),
            meta: Some(self.meta.clone()),
        }
    }

    /// The envelope serialized exactly as the CLI prints it.
    pub fn envelope_json(&self) -> Result<String, CccError> {
        serde_json::to_string(&self.envelope())
            .map_err(|e| CccError::internal_error(format!("json serialization: {e}")))
    }
}

/// Builds an [`OpOutput`] from serializable data + meta + human text.
pub fn output<T: Serialize>(
    data: &T,
    meta: EnvelopeMeta,
    human: String,
) -> Result<OpOutput, CccError> {
    let data = serde_json::value::to_raw_value(data)
        .map_err(|e| CccError::internal_error(format!("json serialization: {e}")))?;
    Ok(OpOutput { data, meta, human })
}

/// Builds an [`OpOutput`] whose human rendering is the full envelope JSON
/// (the CLI's behavior for `--format json` without `--json`).
pub fn output_envelope_as_human<T: Serialize>(
    data: &T,
    meta: EnvelopeMeta,
) -> Result<OpOutput, CccError> {
    let mut out = output(data, meta, String::new())?;
    out.human = out.envelope_json()?;
    Ok(out)
}

/// Envelope metadata for a repository-scoped operation.
pub fn meta_for(
    repo: &RepositoryRecord,
    snapshot: Option<SnapshotId>,
    generation: Option<ccc_domain::GenerationId>,
) -> EnvelopeMeta {
    EnvelopeMeta {
        repository_id: Some(repo.repository_id.to_string()),
        snapshot_id: snapshot.map(|s| s.to_string()),
        generation_id: generation.map(|g| g.to_string()),
        request_id: None,
        index_current: None,
    }
}

/// Repository resolution (TIS §13.4): explicit path (canonicalized,
/// `\\`→`/`-normalized — load-bearing on Windows) or raw repository id,
/// then the current directory, then the single registered repository.
pub async fn resolve_repo(
    store: &ccc_storage::SqliteStore,
    repo: Option<&std::path::Path>,
) -> Result<RepositoryRecord, CccError> {
    let repos = store.list_repositories().await?;
    if let Some(path) = repo {
        let canonical = std::fs::canonicalize(path)
            .map_err(|e| CccError::invalid_request(format!("cannot resolve path: {e}")))?;
        // Strip the Windows extended-length prefix (matching how register
        // stores root paths) and normalize separators.
        let canonical_str = canonical.to_string_lossy().replace('\\', "/");
        let normalized = canonical_str
            .strip_prefix("//?/")
            .unwrap_or(&canonical_str)
            .to_string();
        if let Some(found) = store.find_repository_by_path(&normalized).await? {
            return Ok(found);
        }
        // Also accept a raw id.
        if let Ok(id) = RepositoryId::from_str(&normalized) {
            if let Some(found) = store.get_repository(id).await? {
                return Ok(found);
            }
        }
        return Err(CccError::repository_not_found(format!(
            "no registered repository at {normalized}"
        )));
    }
    // Default: current directory match, else the single registered repo.
    let cwd = std::env::current_dir()
        .map_err(|e| CccError::internal_error(format!("cannot read cwd: {e}")))?;
    let cwd_norm = cwd.to_string_lossy().replace('\\', "/");
    if let Some(found) = store.find_repository_by_path(&cwd_norm).await? {
        return Ok(found);
    }
    if repos.len() == 1 {
        return Ok(repos[0].clone());
    }
    Err(CccError::repository_not_found(
        "no repository specified and none matches the current directory; use --repo <path>",
    ))
}

/// Resolves a repository from a raw identifier string first (the REST/MCP
/// path-parameter form), falling back to path resolution. Unknown ids are
/// REPOSITORY_NOT_FOUND — never a silent fallback.
pub async fn resolve_repo_ref(
    store: &ccc_storage::SqliteStore,
    id_or_path: &str,
) -> Result<RepositoryRecord, CccError> {
    if let Ok(id) = RepositoryId::from_str(id_or_path) {
        if let Some(found) = store.get_repository(id).await? {
            return Ok(found);
        }
        return Err(CccError::repository_not_found(format!(
            "no registered repository with id {id_or_path}"
        )));
    }
    resolve_repo(store, Some(std::path::Path::new(id_or_path))).await
}

/// Snapshot a memory is authored against: the active generation's snapshot
/// (memories without an indexed repository cannot be scoped — no false
/// availability).
pub async fn active_snapshot_id(
    store: &ccc_storage::SqliteStore,
    repo: &RepositoryRecord,
) -> Result<SnapshotId, CccError> {
    let generation = store
        .get_active_generation(repo.repository_id)
        .await?
        .ok_or_else(|| {
            CccError::snapshot_not_found("no published index generation; run `ccc index` first")
        })?;
    Ok(generation.snapshot_id)
}

/// Parses a claim-status filter string strictly (no false availability).
pub fn parse_claim_status(s: &str) -> Result<ccc_domain::ClaimStatus, CccError> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).map_err(|_| {
        CccError::invalid_request(format!(
            "unknown status {s:?}; known: candidate, supported, verified, stale, conflicted, \
             superseded, retracted"
        ))
    })
}
