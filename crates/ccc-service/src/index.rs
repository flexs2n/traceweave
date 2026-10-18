//! Indexing + repository status (TIS §7, §13.4).

use crate::{meta_for, output, resolve_repo, AppContext, OpOutput};
use ccc_domain::*;
use ccc_jobs as jobs;
use ccc_repository::{GitCli, GitProvider};
use ccc_snapshot::{find_or_create_snapshot, SnapshotBuilder};
use ccc_storage::*;
use std::path::PathBuf;
use std::str::FromStr as _;
use std::sync::Arc;

pub async fn index_repo(
    ctx: &AppContext,
    repo_path: Option<&std::path::Path>,
    full: bool,
    snapshot_id: Option<&str>,
) -> Result<OpOutput, CccError> {
    let store = &ctx.store;
    let repo = resolve_repo(store, repo_path).await?;
    index_repo_record(ctx, &repo, full, snapshot_id).await
}

/// The index operation against an already-resolved repository record (the
/// REST/MCP adapters resolve `{id}` path params themselves, T4 job runner
/// included).
pub async fn index_repo_record(
    ctx: &AppContext,
    repo: &RepositoryRecord,
    full: bool,
    snapshot_id: Option<&str>,
) -> Result<OpOutput, CccError> {
    let store = &ctx.store;
    let artifacts = &*ctx.artifacts;
    let cfg = ccc_config::load_config(&ccc_config::resolve_data_dir(None))?;
    // The idempotency key includes the current repository state (HEAD + dirty
    // flag): re-running against the same tree replays one logical job, while
    // a changed tree is a genuinely new request.
    let root = PathBuf::from(&repo.root_path);
    let head = GitCli.rev_parse_head(&root)?.unwrap_or_default();
    let dirty = !GitCli.status_porcelain(&root)?.is_empty();
    let key = jobs::idempotency_key(&[
        "index",
        &repo.repository_id.to_string(),
        &format!("{full}"),
        snapshot_id.unwrap_or("current"),
        &cfg.indexing().configuration_hash()?,
        &head,
        &format!("{dirty}"),
    ]);
    let job = jobs::begin_job(&**store, repo.repository_id, "index", &key, None).await?;

    let result = index_once(store, artifacts, repo, &cfg, full, snapshot_id).await;
    match result {
        Ok(outcome) => {
            // Freshness projection (FR-043): memories linked to claims that
            // the reindex invalidated flip `stale` with a recorded cause;
            // unrelated memories stay untouched.
            let memory_freshness =
                ccc_memory::project_claim_freshness(&**store, &**store, repo.repository_id)
                    .await
                    .ok();
            let payload = serde_json::to_string(&outcome).unwrap_or_else(|_| "{}".into());
            let _job = if job.state.is_terminal() {
                // Idempotent replay: the logical job already completed.
                job
            } else {
                jobs::finish_success(&**store, job, Some(&payload)).await?
            };
            let mut human = String::new();
            human.push_str(&format!("Snapshot {}\n", outcome.snapshot_id));
            human.push_str(&format!("Generation {} published\n", outcome.generation_id));
            human.push_str(&format!(
                "Parsed: {}, reused: {}, excluded: {} (unsupported: {}), failed: {}\n",
                outcome.counters.parsed,
                outcome.counters.reused,
                outcome.counters.excluded,
                outcome.counters.unsupported,
                outcome.counters.failed
            ));
            if let Some(freshness) = &memory_freshness {
                if !freshness.stale_memories.is_empty() || !freshness.conflicted_memories.is_empty()
                {
                    human.push_str(&format!(
                        "Memory freshness: {} stale, {} conflicted (linked claims invalidated; \
                         see `ccc memory list --status stale`)\n",
                        freshness.stale_memories.len(),
                        freshness.conflicted_memories.len()
                    ));
                }
            }
            let data = serde_json::json!({
                "snapshot_id": outcome.snapshot_id.to_string(),
                "generation_id": outcome.generation_id.to_string(),
                "counters": outcome.counters,
                "memories_flipped_stale": memory_freshness
                    .as_ref()
                    .map(|s| s.stale_memories.len())
                    .unwrap_or(0),
            });
            output(
                &data,
                meta_for(repo, Some(outcome.snapshot_id), Some(outcome.generation_id)),
                human,
            )
        }
        Err(e) => {
            jobs::finish_failure(&**store, job, &e).await?;
            Err(e)
        }
    }
}

pub async fn index_once(
    store: &Arc<SqliteStore>,
    artifacts: &FsArtifactStore,
    repo: &RepositoryRecord,
    cfg: &ccc_config::CccConfig,
    full: bool,
    snapshot_id: Option<&str>,
) -> Result<ccc_indexer::IndexOutcome, CccError> {
    let root = PathBuf::from(&repo.root_path);
    let discovery_started = std::time::Instant::now();
    let (snapshot, manifest) = match snapshot_id {
        Some(id) => {
            let sid = SnapshotId::from_str(id)
                .map_err(|_| CccError::invalid_request(format!("invalid snapshot id {id:?}")))?;
            let snapshot = store
                .get_snapshot(sid)
                .await?
                .ok_or_else(|| CccError::snapshot_not_found(format!("snapshot {id} not found")))?;
            // Rebuild the manifest from stored file hashes.
            let files = store.list_files(sid).await?;
            let mut manifest = std::collections::BTreeMap::new();
            for f in files {
                manifest.insert(f.path.clone(), f.content_hash.clone());
            }
            (snapshot, manifest)
        }
        None => {
            let builder = SnapshotBuilder::new(&GitCli);
            let draft = builder.build_working_tree(
                repo.repository_id,
                &root,
                &cfg.indexing().configuration_hash()?,
                cfg.indexing().include_untracked,
            )?;
            let manifest = draft.manifest.clone();
            let snapshot = find_or_create_snapshot(&**store, draft).await?;
            (snapshot, manifest)
        }
    };
    let discovery_ms = discovery_started.elapsed().as_millis() as u64;
    // Production evidence goes to the SQLite store.
    let outcome = ccc_indexer::run_index(
        &**store,
        &**store,
        &**store,
        artifacts,
        &**store,
        repo,
        &snapshot,
        &root,
        &manifest,
        cfg.indexing(),
        full,
    )
    .await?;
    // Discovery happens in the snapshot build above; record it alongside the
    // in-pipeline stage timings (TIS §18).
    let mut outcome = outcome;
    outcome.counters.timings_ms.discovery_ms = discovery_ms;
    Ok(outcome)
}

/// FR-010 drift guard: cheap identity check of the active index generation
/// against the repository's current state — the snapshot's HEAD commit and
/// dirty flag must both match. A projection computed on demand; it is never
/// persisted and never enters bundle bytes (bundle DTOs stay byte-identical).
/// Returns `false` when no active generation exists.
pub async fn index_current(store: &SqliteStore, repo: &RepositoryRecord) -> Result<bool, CccError> {
    let active = match store.get_active_generation(repo.repository_id).await? {
        Some(gen) => gen,
        None => return Ok(false),
    };
    let snapshot = store
        .get_snapshot(active.snapshot_id)
        .await?
        .ok_or_else(|| {
            CccError::snapshot_not_found("active generation's snapshot record missing")
        })?;
    let root = PathBuf::from(&repo.root_path);
    let head = GitCli.rev_parse_head(&root)?.unwrap_or_default();
    let dirty = !GitCli.status_porcelain(&root)?.is_empty();
    Ok(snapshot.commit_hash.unwrap_or_default() == head && snapshot.is_dirty == dirty)
}

pub async fn status(
    ctx: &AppContext,
    repo_path: Option<&std::path::Path>,
) -> Result<OpOutput, CccError> {
    let store: &SqliteStore = &ctx.store;
    let repo = resolve_repo(store, repo_path).await?;
    let active = store.get_active_generation(repo.repository_id).await?;
    let current = index_current(store, &repo).await?;
    let mut status = serde_json::json!({
        "repository_id": repo.repository_id.to_string(),
        "root_path": repo.root_path,
        "limitations": repo.limitations,
        "indexed": active.is_some(),
    });
    let mut human = String::new();
    if let Some(gen) = &active {
        let counters: serde_json::Value =
            serde_json::from_str(&gen.counters_json).unwrap_or(serde_json::json!({}));
        let files = store.list_files(gen.snapshot_id).await?;
        let excluded: Vec<&FileRecord> = files
            .iter()
            .filter(|f| f.inclusion == FileInclusion::Excluded)
            .collect();
        let unsupported: Vec<&FileRecord> = excluded
            .iter()
            .filter(|f| f.exclusion_reason == Some(ExclusionReason::UnsupportedLanguage))
            .copied()
            .collect();
        status["snapshot_id"] = serde_json::json!(gen.snapshot_id.to_string());
        status["generation_id"] = serde_json::json!(gen.generation_id.to_string());
        status["counters"] = counters.clone();
        status["excluded_files"] = serde_json::json!(excluded.iter().map(|f| serde_json::json!({
            "path": f.path,
            "reason": f.exclusion_reason.map(|r| serde_json::to_string(&r).unwrap_or_default().trim_matches('"').to_string()),
        })).collect::<Vec<_>>());
        status["unsupported_files"] = serde_json::json!(unsupported
            .iter()
            .map(|f| f.path.clone())
            .collect::<Vec<_>>());
        human.push_str(&format!(
            "Repository {} at {}\nActive generation {} (snapshot {})\nCounters: {counters}\n",
            repo.repository_id, repo.root_path, gen.generation_id, gen.snapshot_id
        ));
        human.push_str(&format!(
            "Excluded: {} file(s); unsupported: {}\n",
            excluded.len(),
            unsupported
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        human.push_str(&format!(
            "Index current: {}\n",
            if current {
                "yes"
            } else {
                "no (repository changed since indexing; run `ccc index`)"
            }
        ));
    } else {
        human.push_str(&format!(
            "Repository {} at {} — not indexed yet\n",
            repo.repository_id, repo.root_path
        ));
    }
    let mut meta = meta_for(&repo, None, None);
    meta.index_current = Some(current);
    output(&status, meta, human)
}
