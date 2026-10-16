//! Evidence revalidation scoped by verification outcomes (SDD §15.6, TIS
//! §11.1 step 10).
//!
//! A passing verification supports only the claims whose dependencies
//! intersect the verified files/symbols — never universally — and only when
//! every executable check actually completed. Failed checks mark covered
//! claims `conflicted`; negative results are preserved as evidence rows and
//! verification records, never discarded after a later successful rerun.
//! Ambiguous outcomes (skipped/blocked/unavailable/timed-out checks) leave
//! claims untouched: status stays `stale`.

use ccc_domain::*;
use ccc_storage::{EvidenceStore, StoreResult};

/// What revalidation did for one verification record.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RevalidationSummary {
    /// Claims flipped to `supported` (with `validated_at` + verification link).
    pub supported_claims: Vec<String>,
    /// Claims flipped to `conflicted` by a failing check.
    pub conflicted_claims: Vec<String>,
    /// `true` when the outcome could not be established (pending or failed
    /// infrastructure) — covered claims deliberately stay untouched.
    pub ambiguous: bool,
    /// Why nothing changed (empty when the flips above explain it).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// Inputs for one revalidation pass.
pub struct RevalidationInput<'a> {
    pub evidence: &'a dyn EvidenceStore,
    pub repository_id: RepositoryId,
    pub verification_id: VerificationId,
    pub policy_name: &'a str,
    pub policy_allows_revalidation: bool,
    /// Active-generation file ids the checks covered (the changed set).
    pub scope_file_ids: &'a [FileId],
    /// Active-generation symbol ids the checks covered.
    pub scope_symbol_ids: &'a [SymbolId],
    pub checks: &'a [crate::VerificationCheckDto],
}

/// Applies the verification outcome to covered claims (SDD §15.6).
///
/// Coverage is the intersection of claim dependencies with the verified
/// files/symbols — never universal. Ambiguous runs (pending checks) touch
/// nothing.
pub async fn revalidate(input: RevalidationInput<'_>) -> StoreResult<RevalidationSummary> {
    let RevalidationInput {
        evidence,
        repository_id,
        verification_id,
        policy_name,
        policy_allows_revalidation,
        scope_file_ids,
        scope_symbol_ids,
        checks,
    } = input;
    let mut summary = RevalidationSummary::default();
    if !policy_allows_revalidation {
        summary.ambiguous = true;
        summary.note = "policy disabled claim revalidation; claims untouched".into();
        return Ok(summary);
    }

    let covered = evidence
        .claims_depending_on(
            repository_id,
            &scope_file_ids
                .iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>(),
            &scope_symbol_ids
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
        )
        .await?;
    if covered.is_empty() {
        summary.note = "no claims depend on the verified files/symbols".into();
        return Ok(summary);
    }

    let passed = checks
        .iter()
        .filter(|c| c.status == VerificationStatus::Passed.as_str())
        .count();
    let failed = checks
        .iter()
        .filter(|c| c.status == VerificationStatus::Failed.as_str())
        .count();
    let pending = checks
        .iter()
        .filter(|c| !matches!(c.status.as_str(), "passed" | "failed"))
        .count();

    let now = now_rfc3339();
    if failed > 0 {
        // Negative outcome: covered claims become conflicted. The negative
        // result is preserved as a verification evidence row per claim.
        for claim in &covered {
            let updated = evidence
                .update_claim_validation(claim.claim_id, ClaimStatus::Conflicted, None, &now)
                .await?;
            if updated {
                record_verification_link(
                    evidence,
                    repository_id,
                    claim.claim_id,
                    verification_id,
                    policy_name,
                    "failed",
                    &now,
                )
                .await?;
                summary.conflicted_claims.push(claim.claim_id.to_string());
            }
        }
        summary.conflicted_claims.sort();
        return Ok(summary);
    }

    if passed == 0 || pending > 0 {
        // Ambiguous: skipped/blocked/unavailable/timed-out/cancelled checks
        // can establish nothing (TIS §11.3 — never `passed` when skipped).
        summary.ambiguous = true;
        summary.note = format!(
            "no completed passing run ({passed} passed, {failed} failed, {pending} pending); \
             claims stay untouched"
        );
        return Ok(summary);
    }

    // Every check passed and at least one executed: covered non-terminal
    // claims flip to `supported` with a fresh `validated_at`.
    for claim in &covered {
        if claim.status == ClaimStatus::Superseded || claim.status == ClaimStatus::Retracted {
            continue;
        }
        let updated = evidence
            .update_claim_validation(claim.claim_id, ClaimStatus::Supported, Some(&now), &now)
            .await?;
        if updated {
            record_verification_link(
                evidence,
                repository_id,
                claim.claim_id,
                verification_id,
                policy_name,
                "passed",
                &now,
            )
            .await?;
            summary.supported_claims.push(claim.claim_id.to_string());
        }
    }
    summary.supported_claims.sort();
    Ok(summary)
}

/// Chains the claim to the verification record: a `verification`-type
/// evidence row plus a `claim_dependencies` link, so outcomes stay queryable
/// per claim (TIS §6.3).
async fn record_verification_link(
    evidence: &dyn EvidenceStore,
    repository_id: RepositoryId,
    claim_id: ClaimId,
    verification_id: VerificationId,
    policy_name: &str,
    outcome: &str,
    now: &str,
) -> StoreResult<()> {
    let claim = match evidence.get_claim(claim_id).await? {
        Some(c) => c,
        None => return Ok(()),
    };
    let record = EvidenceRecord {
        evidence_id: EvidenceId::generate(),
        repository_id,
        snapshot_id: claim.snapshot_id,
        evidence_type: EvidenceType::Verification,
        source_ref: format!("verification:{verification_id}"),
        source_hash: None,
        extraction_method: ResolutionMethod::RuntimeTrace,
        extraction_version: "ccc-verification/1".into(),
        actor: "ccc-verification".into(),
        validation_method: Some(policy_name.to_string()),
        validation_outcome: Some(outcome.to_string()),
        created_at: now.to_string(),
    };
    evidence
        .insert_evidence(std::slice::from_ref(&record))
        .await?;
    evidence
        .insert_claim_dependencies(&[ClaimDependencyRecord {
            claim_id,
            depends_on: verification_id.to_string(),
            depends_on_kind: "verification".into(),
        }])
        .await?;
    Ok(())
}
