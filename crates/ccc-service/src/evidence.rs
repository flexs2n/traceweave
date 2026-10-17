//! Evidence ledger queries (FR-038–040, ui.md UI-016).

use crate::{output, parse_claim_status, AppContext, OpOutput};
use ccc_domain::CccError;
use ccc_schema::EnvelopeMeta;
use ccc_storage::*;
use std::str::FromStr as _;

pub async fn evidence_list(
    ctx: &AppContext,
    status: Option<&str>,
    limit: usize,
) -> Result<OpOutput, CccError> {
    let store: &ccc_storage::SqliteStore = &ctx.store;
    let status_filter = status.map(parse_claim_status).transpose()?;
    let repos = store.list_repositories().await?;
    let mut claims = Vec::new();
    for repo in &repos {
        for claim in store.list_claims(repo.repository_id).await? {
            if let Some(want) = status_filter {
                if claim.status != want {
                    continue;
                }
            }
            claims.push(claim);
        }
    }
    claims.truncate(limit);
    let mut human = String::new();
    if claims.is_empty() {
        human.push_str("No claims recorded.\n");
    }
    for c in &claims {
        human.push_str(&format!(
            "{} [{}] ({}) {} — {}\n",
            c.claim_id,
            serde_json::to_string(&c.status)
                .unwrap_or_default()
                .trim_matches('"'),
            c.claim_type,
            c.text,
            c.snapshot_id
        ));
    }
    output(&claims, EnvelopeMeta::default(), human)
}

pub async fn evidence_show(ctx: &AppContext, evidence_id: &str) -> Result<OpOutput, CccError> {
    let store: &ccc_storage::SqliteStore = &ctx.store;
    let id = ccc_domain::EvidenceId::from_str(evidence_id)
        .map_err(|_| CccError::invalid_request(format!("invalid evidence id {evidence_id:?}")))?;
    let record = store
        .get_evidence(id)
        .await?
        .ok_or_else(|| CccError::snapshot_not_found(format!("evidence {evidence_id} not found")))?;
    let human = format!(
        "Evidence {}\n  type: {:?}\n  source: {}\n  extraction: {} ({}), actor: {}\n  snapshot: {}\n",
        record.evidence_id,
        record.evidence_type,
        record.source_ref,
        serde_json::to_string(&record.extraction_method).unwrap_or_default(),
        record.extraction_version,
        record.actor,
        record.snapshot_id
    );
    output(&record, EnvelopeMeta::default(), human)
}
