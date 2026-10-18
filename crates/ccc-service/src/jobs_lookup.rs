//! Job-ledger lookups + submission helpers (REST `GET /v1/jobs/{id}` and the
//! async job endpoints, TIS §13.1/§13.3/§15.1).

use crate::{output, AppContext, OpOutput};
use ccc_domain::{CccError, JobId, JobRecord, JobState, RepositoryId};
use ccc_storage::{JobStore, SqliteStore};
use std::str::FromStr as _;

/// One job record by id. Unknown ids are SNAPSHOT_NOT_FOUND (the ledger's
/// only not-found code, consistent with the CLI's record lookups).
pub async fn job_get(ctx: &AppContext, id: &str) -> Result<OpOutput, CccError> {
    let store: &SqliteStore = &ctx.store;
    let job_id = parse_job_id(id)?;
    let record = store
        .get_job(job_id)
        .await?
        .ok_or_else(|| CccError::snapshot_not_found(format!("job {id} not found")))?;
    output(&record, ccc_schema::EnvelopeMeta::default(), String::new())
}

pub fn parse_job_id(id: &str) -> Result<JobId, CccError> {
    JobId::from_str(id).map_err(|_| CccError::invalid_request(format!("invalid job id {id:?}")))
}

/// Opens (or replays) the logical job for an idempotency key — the same
/// ledger entry the synchronous CLI path records (TIS §13.3).
pub async fn begin_job(
    store: &SqliteStore,
    repository_id: RepositoryId,
    job_type: &str,
    key: &str,
) -> Result<JobRecord, CccError> {
    ccc_jobs::begin_job(store, repository_id, job_type, key, None).await
}

/// Best-effort failure recording for jobs whose operation failed before it
/// could reach its own `finish_failure` (e.g. config/policy load errors in
/// the spawned task). Running → Failed only; terminal jobs stay untouched.
pub async fn fail_job(store: &SqliteStore, job_id: JobId, err: &CccError) -> Result<(), CccError> {
    if let Some(job) = store.get_job(job_id).await? {
        if job.state == JobState::Running {
            ccc_jobs::finish_failure(store, job, err).await?;
        }
    }
    Ok(())
}
