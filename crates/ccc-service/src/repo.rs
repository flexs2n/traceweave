//! Repository registration + status (TIS §13.4).

use crate::{meta_for, output, resolve_repo, AppContext, OpOutput};
use ccc_domain::{CccError, RepositoryRecord};
use ccc_jobs as jobs;
use ccc_repository::{GitCli, RepositoryRegistry};
use ccc_storage::*;
use std::path::Path;

pub async fn repo_add(
    ctx: &AppContext,
    path: &Path,
    idempotency_key: Option<&str>,
) -> Result<OpOutput, CccError> {
    let store = &ctx.store;
    let artifacts: &dyn ArtifactStore = &*ctx.artifacts;
    let _ = artifacts;
    let cfg = ccc_config::load_config(&ccc_config::resolve_data_dir(None))?;
    let config_hash = cfg.indexing().configuration_hash()?;

    // Registration itself is idempotent per root path; the job ledger then
    // guarantees one logical job per idempotency key (TIS §13.3).
    let registry = RepositoryRegistry::new(&**store, &GitCli);
    let record = registry.register(path, &config_hash).await?;
    let key = idempotency_key
        .map(str::to_string)
        .unwrap_or_else(|| jobs::idempotency_key(&["repo_add", &record.root_path]));
    let job = jobs::begin_job(&**store, record.repository_id, "repo_add", &key, None).await?;
    let job = if job.state.is_terminal() {
        // Idempotent replay: the logical job already completed.
        job
    } else {
        jobs::finish_success(&**store, job, Some(&record.repository_id.to_string())).await?
    };

    let data = serde_json::json!({
        "repository": record,
        "job_id": job.job_id.to_string(),
        "idempotent_replay": job.created_at != job.updated_at,
    });
    let human = format!(
        "Registered repository {} at {} (job {})\n",
        record.repository_id, record.root_path, job.job_id
    );
    output(&data, meta_for(&record, None, None), human)
}

pub async fn repo_status(ctx: &AppContext, path: Option<&Path>) -> Result<OpOutput, CccError> {
    let store: &ccc_storage::SqliteStore = &ctx.store;
    let repos: Vec<RepositoryRecord> = match path {
        Some(p) => vec![resolve_repo(store, Some(p)).await?],
        None => store.list_repositories().await?,
    };
    let mut human = String::new();
    if repos.is_empty() {
        human.push_str("No repositories registered. Use `ccc repo add <path>`.\n");
    } else {
        for r in &repos {
            let active = store.get_active_generation(r.repository_id).await?;
            human.push_str(&format!(
                "{} — {} [{}]\n",
                r.repository_id,
                r.root_path,
                active
                    .map(|g| format!("generation {}", g.generation_id))
                    .unwrap_or_else(|| "not indexed".into())
            ));
        }
    }
    output(&repos, ccc_schema::EnvelopeMeta::default(), human)
}
