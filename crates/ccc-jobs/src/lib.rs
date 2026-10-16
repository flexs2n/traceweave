//! `ccc-jobs` — job records for indexing/compile/impact runs (TIS §15.1,
//! §13.3).
//!
//! Step 1 executes jobs synchronously; the ledger exists so future async
//! workers reuse the same records (SDD §19). Every mutation endpoint accepts
//! an idempotency key: repeated requests with the same key and equivalent
//! inputs return the same logical job instead of duplicating work.

use ccc_domain::*;
use ccc_storage::{JobStore, StoreResult};

/// Derives a stable idempotency key from the operation inputs (BLAKE3 of the
/// canonical, `\x1f`-joined parts).
pub fn idempotency_key(parts: &[&str]) -> String {
    blake3_hex(parts.join("\x1f").as_bytes())
}

/// Inserts (or returns the existing) job and transitions it to `running`.
/// The returned record is the one logical job for this idempotency key.
pub async fn begin_job(
    store: &dyn JobStore,
    repository_id: RepositoryId,
    job_type: &str,
    idempotency_key: &str,
    payload_json: Option<&str>,
) -> StoreResult<JobRecord> {
    let now = now_rfc3339();
    let record = JobRecord {
        job_id: JobId::generate(),
        repository_id,
        job_type: job_type.to_string(),
        state: JobState::Queued,
        idempotency_key: idempotency_key.to_string(),
        payload_json: payload_json.map(str::to_string),
        retry_count: 0,
        created_at: now.clone(),
        updated_at: now.clone(),
        finished_at: None,
        error_code: None,
        error_message: None,
    };
    let job = store.insert_job_idempotent(&record).await?;
    if job.state == JobState::Queued {
        store
            .update_job_state(job.job_id, JobState::Running, &now, None, None, None)
            .await?;
        Ok(JobRecord {
            state: JobState::Running,
            ..job
        })
    } else {
        // Idempotent replay: the logical job already exists (running or
        // terminal); return it unchanged rather than duplicating work.
        Ok(job)
    }
}

/// Marks the job `succeeded` with the given result payload.
pub async fn finish_success(
    store: &dyn JobStore,
    mut job: JobRecord,
    payload_json: Option<&str>,
) -> StoreResult<JobRecord> {
    validate_transition(job.state, JobState::Succeeded)?;
    let now = now_rfc3339();
    store
        .update_job_state(
            job.job_id,
            JobState::Succeeded,
            &now,
            Some(&now),
            None,
            None,
        )
        .await?;
    if let Some(payload) = payload_json {
        job.payload_json = Some(payload.to_string());
    }
    Ok(JobRecord {
        state: JobState::Succeeded,
        finished_at: Some(now),
        ..job
    })
}

/// Marks the job `failed` with the structured error.
pub async fn finish_failure(
    store: &dyn JobStore,
    job: JobRecord,
    error: &CccError,
) -> StoreResult<JobRecord> {
    validate_transition(job.state, JobState::Failed)?;
    let now = now_rfc3339();
    store
        .update_job_state(
            job.job_id,
            JobState::Failed,
            &now,
            Some(&now),
            Some(error.code.as_str()),
            Some(error.message.as_str()),
        )
        .await?;
    Ok(JobRecord {
        state: JobState::Failed,
        finished_at: Some(now),
        error_code: Some(error.code.as_str().to_string()),
        error_message: Some(error.message.clone()),
        ..job
    })
}

fn validate_transition(from: JobState, to: JobState) -> StoreResult<()> {
    if job_state_transition_is_legal(from, to) {
        Ok(())
    } else {
        Err(CccError::job_conflict(format!(
            "illegal job state transition {from:?} -> {to:?}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccc_storage::test_support::InMemoryJobStore;

    fn job_store() -> InMemoryJobStore {
        InMemoryJobStore::new()
    }

    #[tokio::test]
    async fn same_idempotency_key_yields_one_logical_job() {
        let store = job_store();
        let repo = RepositoryId::generate();
        let key = idempotency_key(&["repo_add", "/tmp/repo"]);
        let first = begin_job(&store, repo, "repo_add", &key, None)
            .await
            .unwrap();
        let second = begin_job(&store, repo, "repo_add", &key, None)
            .await
            .unwrap();
        assert_eq!(first.job_id, second.job_id, "one logical job (TIS §13.3)");
        let jobs = store.list_jobs(repo).await.unwrap();
        assert_eq!(jobs.len(), 1);
    }

    #[tokio::test]
    async fn different_keys_create_distinct_jobs() {
        let store = job_store();
        let repo = RepositoryId::generate();
        let a = begin_job(
            &store,
            repo,
            "index",
            &idempotency_key(&["index", "a"]),
            None,
        )
        .await
        .unwrap();
        let b = begin_job(
            &store,
            repo,
            "index",
            &idempotency_key(&["index", "b"]),
            None,
        )
        .await
        .unwrap();
        assert_ne!(a.job_id, b.job_id);
    }

    #[tokio::test]
    async fn state_machine_follows_tis_lifecycle() {
        let store = job_store();
        let repo = RepositoryId::generate();
        let job = begin_job(
            &store,
            repo,
            "context_compile",
            &idempotency_key(&["c", "1"]),
            None,
        )
        .await
        .unwrap();
        assert_eq!(job.state, JobState::Running);
        let done = finish_success(&store, job, Some(r#"{"ok":true}"#))
            .await
            .unwrap();
        assert_eq!(done.state, JobState::Succeeded);
        assert!(done.finished_at.is_some());
        // Terminal states reject further transitions.
        let err = finish_success(&store, done, None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::JobConflict);
    }

    #[tokio::test]
    async fn failure_records_error_code_and_message() {
        let store = job_store();
        let repo = RepositoryId::generate();
        let job = begin_job(&store, repo, "index", &idempotency_key(&["i", "1"]), None)
            .await
            .unwrap();
        let failed = finish_failure(&store, job, &CccError::index_stale("generation mismatch"))
            .await
            .unwrap();
        assert_eq!(failed.state, JobState::Failed);
        assert_eq!(failed.error_code.as_deref(), Some("INDEX_STALE"));
        let stored = store.get_job(failed.job_id).await.unwrap().unwrap();
        assert_eq!(stored.error_message.as_deref(), Some("generation mismatch"));
    }
}
