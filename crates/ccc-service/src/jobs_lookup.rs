//! Job-ledger lookups (REST `GET /v1/jobs/{id}`, TIS §13.1/§15.1).

use crate::{output, AppContext, OpOutput};
use ccc_domain::{CccError, JobId};
use ccc_storage::JobStore;
use std::str::FromStr as _;

/// One job record by id. Unknown ids are SNAPSHOT_NOT_FOUND (the ledger's
/// only not-found code, consistent with the CLI's record lookups).
pub async fn job_get(ctx: &AppContext, id: &str) -> Result<OpOutput, CccError> {
    let store: &ccc_storage::SqliteStore = &ctx.store;
    let job_id = JobId::from_str(id)
        .map_err(|_| CccError::invalid_request(format!("invalid job id {id:?}")))?;
    let record = store
        .get_job(job_id)
        .await?
        .ok_or_else(|| CccError::snapshot_not_found(format!("job {id} not found")))?;
    output(&record, ccc_schema::EnvelopeMeta::default(), String::new())
}
