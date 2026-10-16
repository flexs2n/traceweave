//! `ccc-evidence` — evidence ledger and the claim staleness seam
//! (TIS §6.3, ADR-005, FR-040).
//!
//! During indexing every extracted symbol gets a `source_fact` evidence
//! record and a `candidate` claim ("this symbol exists here"), linked to the
//! symbol and file via `claim_dependencies`. When a later index changes a
//! file, claims depending on that file — or on any symbol it defined — flip
//! to `stale`. This is the conservative invalidation seam only; the full
//! freshness engine is a later step.

use ccc_domain::*;
use ccc_storage::{EvidenceStore, StoreResult};

/// Upper bound on per-run evidence/claim creation (resource limit, TIS §16.4).
pub const MAX_CLAIMS_PER_RUN: usize = 10_000;

/// What got written for one symbol.
#[derive(Debug, Clone)]
pub struct SymbolFact<'a> {
    pub symbol_id: &'a SymbolId,
    pub file_id: &'a FileId,
    pub qualified_name: &'a str,
    pub kind: &'a str,
    pub path: &'a str,
    pub start_line: u32,
    pub end_line: u32,
    pub content_hash: &'a str,
}

/// Creates `source_fact` evidence plus a `candidate` symbol-existence claim
/// for each symbol, with dependency links. Returns the created claim ids.
pub async fn record_symbol_facts(
    store: &dyn EvidenceStore,
    repository_id: RepositoryId,
    snapshot_id: SnapshotId,
    facts: &[SymbolFact<'_>],
    extraction_version: &str,
) -> StoreResult<Vec<ClaimId>> {
    let now = now_rfc3339();
    let mut evidence_rows = Vec::with_capacity(facts.len());
    let mut claim_rows = Vec::with_capacity(facts.len());
    let mut links: Vec<ClaimDependencyRecord> = Vec::with_capacity(facts.len() * 2);

    for fact in facts.iter().take(MAX_CLAIMS_PER_RUN) {
        let evidence = EvidenceRecord {
            evidence_id: EvidenceId::generate(),
            repository_id,
            snapshot_id,
            evidence_type: EvidenceType::SourceFact,
            source_ref: format!("{}#L{}-L{}", fact.path, fact.start_line, fact.end_line),
            source_hash: Some(fact.content_hash.to_string()),
            extraction_method: ResolutionMethod::TreeSitter,
            extraction_version: extraction_version.to_string(),
            actor: "ccc-indexer".to_string(),
            validation_method: None,
            validation_outcome: None,
            created_at: now.clone(),
        };
        let claim = ClaimRecord {
            claim_id: ClaimId::generate(),
            repository_id,
            snapshot_id,
            claim_type: "symbol_existence".to_string(),
            text: format!(
                "Symbol `{}` exists as {} in {}#L{}-L{}",
                fact.qualified_name, fact.kind, fact.path, fact.start_line, fact.end_line
            ),
            // No validation beyond syntax extraction has happened: the claim
            // stays a candidate (TIS §6.3).
            status: ClaimStatus::Candidate,
            source_kind: "extracted".to_string(),
            confidence_category: ConfidenceCategory::Unverified,
            created_at: now.clone(),
            validated_at: None,
            updated_at: now.clone(),
        };
        links.push(ClaimDependencyRecord {
            claim_id: claim.claim_id,
            depends_on: fact.symbol_id.to_string(),
            depends_on_kind: "symbol".into(),
        });
        links.push(ClaimDependencyRecord {
            claim_id: claim.claim_id,
            depends_on: fact.file_id.to_string(),
            depends_on_kind: "file".into(),
        });
        evidence_rows.push(evidence);
        claim_rows.push(claim);
    }

    store.insert_evidence(&evidence_rows).await?;
    store.insert_claims(&claim_rows).await?;
    store.insert_claim_dependencies(&links).await?;
    Ok(claim_rows.into_iter().map(|c| c.claim_id).collect())
}

/// Selective invalidation seam (FR-040, TIS §8.1): flips dependent claims of
/// changed files/symbols to `stale`, leaving unrelated claims untouched.
/// Returns the ids of claims that flipped.
pub async fn invalidate_for_changes(
    store: &dyn EvidenceStore,
    repository_id: RepositoryId,
    changed_file_ids: &[String],
    changed_symbol_ids: &[String],
) -> StoreResult<Vec<ClaimId>> {
    let affected = store
        .claims_depending_on(repository_id, changed_file_ids, changed_symbol_ids)
        .await?;
    let ids: Vec<ClaimId> = affected.into_iter().map(|c| c.claim_id).collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    store.mark_claims_stale(&ids, &now_rfc3339()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccc_storage::test_support::InMemoryEvidenceStore;

    fn fixture_facts() -> Vec<SymbolFact<'static>> {
        vec![
            SymbolFact {
                symbol_id: Box::leak(Box::new(SymbolId::generate())),
                file_id: Box::leak(Box::new(FileId::generate())),
                qualified_name: "crate::alpha::run",
                kind: "function",
                path: "src/alpha.rs",
                start_line: 1,
                end_line: 4,
                content_hash: "hash-alpha",
            },
            SymbolFact {
                symbol_id: Box::leak(Box::new(SymbolId::generate())),
                file_id: Box::leak(Box::new(FileId::generate())),
                qualified_name: "crate::beta::stop",
                kind: "function",
                path: "src/beta.rs",
                start_line: 1,
                end_line: 2,
                content_hash: "hash-beta",
            },
        ]
    }

    #[tokio::test]
    async fn claims_are_candidates_with_provenance() {
        let store = InMemoryEvidenceStore::new();
        let repo = RepositoryId::generate();
        let snap = SnapshotId::generate();
        let facts = fixture_facts();
        let claims = record_symbol_facts(&store, repo, snap, &facts, "test/1")
            .await
            .unwrap();
        assert_eq!(claims.len(), 2);
        for id in &claims {
            let claim = store.get_claim(*id).await.unwrap().unwrap();
            assert_eq!(claim.status, ClaimStatus::Candidate);
            assert!(claim.text.contains("exists as function"));
        }
        let evidence = store.list_evidence(repo).await.unwrap();
        assert_eq!(evidence.len(), 2);
        assert!(evidence
            .iter()
            .all(|e| e.evidence_type == EvidenceType::SourceFact
                && e.extraction_method == ResolutionMethod::TreeSitter
                && e.source_ref.contains("#L")));
    }

    #[tokio::test]
    async fn only_dependent_claims_flip_stale() {
        let store = InMemoryEvidenceStore::new();
        let repo = RepositoryId::generate();
        let snap = SnapshotId::generate();
        let facts = fixture_facts();
        let claims = record_symbol_facts(&store, repo, snap, &facts, "test/1")
            .await
            .unwrap();

        // Change the file of the FIRST symbol only (selective invalidation).
        let flipped = invalidate_for_changes(
            &store,
            repo,
            &[facts[0].file_id.to_string()],
            &[facts[0].symbol_id.to_string()],
        )
        .await
        .unwrap();
        assert_eq!(flipped.len(), 1);

        let first = store.get_claim(claims[0]).await.unwrap().unwrap();
        let second = store.get_claim(claims[1]).await.unwrap().unwrap();
        assert_eq!(
            first.status,
            ClaimStatus::Stale,
            "dependent claim must flip"
        );
        assert_eq!(
            second.status,
            ClaimStatus::Candidate,
            "unrelated claim must stay untouched"
        );
    }

    #[tokio::test]
    async fn invalidation_with_no_changes_is_empty() {
        let store = InMemoryEvidenceStore::new();
        let repo = RepositoryId::generate();
        let snap = SnapshotId::generate();
        let facts = fixture_facts();
        record_symbol_facts(&store, repo, snap, &facts, "test/1")
            .await
            .unwrap();
        let flipped = invalidate_for_changes(&store, repo, &[], &[])
            .await
            .unwrap();
        assert!(flipped.is_empty());
    }
}
