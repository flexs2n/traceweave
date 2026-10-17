//! `ccc-memory` — engineering memory, contradiction detection and conflict
//! resolution (PRD Phase 3: FR-032/033/034, FR-043 freshness projection).
//!
//! Memory is a projection over claims, never a second source of truth (SDD
//! §5.11, TIS §12). Agent-generated memory is always `candidate` (FR-033,
//! FR-080): promotion happens only through scoped revalidation
//! (`ccc-verification`) or explicit human approval — never automatically.
//!
//! Contradictions are never auto-resolved (FR-034, TIS §8.3, SDD §9.4):
//! pairs of same-scope records with disagreeing content both flip
//! `conflicted` with both sides and provenance inspectable; pairs from
//! different snapshots are labeled scope differences instead. Resolution is
//! an explicit human act (`resolve_conflict`) that records actor + rationale.
//!
//! Detection is deliberately conservative: claims group by *direct, identical*
//! dependency targets (never propagated through name-level candidate edges —
//! plan risk: conflict false positives) and only within the same claim type
//! and snapshot; memories group by identical (scope, type, normalized title).

use ccc_domain::*;
use ccc_storage::{EvidenceStore, MemoryStore};
use std::collections::HashMap;

/// Upper bound on dependency links recorded per agent capture (resource
/// limit, mirroring `ccc-evidence::MAX_CLAIMS_PER_RUN`, TIS §16.4).
pub const MAX_FINDING_DEPENDENCIES: usize = 10_000;

/// Input for a human-authored memory (FR-032).
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryDraft {
    pub title: String,
    pub summary: String,
    pub memory_type: MemoryType,
    /// Repository-relative directory prefix; empty = repository-wide.
    pub scope: String,
    pub created_by: String,
    pub sharing_policy: String,
    pub sensitivity: String,
}

/// Input for an edit-as-supersede (FR-032): the new version replaces the old
/// one; history stays immutable (SDD §5.8).
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryEdit {
    pub draft: MemoryDraft,
    /// Why the edit happened (recorded in the superseded record's trail).
    pub reason: Option<String>,
}

/// A finding captured from an agent workflow (FR-033): usually a failed
/// verification run. Creates one `candidate` claim (source_kind `agent`) and
/// one candidate memory linked to it, with the verification id and the
/// covered file/symbol scope as dependency links.
#[derive(Debug, Clone)]
pub struct AgentFinding<'a> {
    pub title: String,
    pub summary: String,
    pub memory_type: MemoryType,
    /// Producing process/actor, e.g. `agent:ccc-verification`.
    pub created_by: String,
    pub verification_id: VerificationId,
    pub covered_file_ids: &'a [FileId],
    pub covered_symbol_ids: &'a [SymbolId],
}

/// What one agent capture wrote (FR-033) — including any conflicts the
/// finding triggered when it landed.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentCapture {
    pub memory: MemoryRecord,
    pub claim: ClaimRecord,
    pub conflicts: ConflictReport,
}

/// One likely-contradiction pair (FR-034) — or a labeled scope difference.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ConflictPair {
    /// `claim_pair` | `memory_pair`.
    pub kind: String,
    pub left_id: String,
    pub right_id: String,
    pub left_title: String,
    pub right_title: String,
    pub left_text: String,
    pub right_text: String,
    /// `conflicted` (same snapshot/scope, disagreeing content) or
    /// `scope_differs` (different snapshots — never auto-conflicted).
    pub label: String,
    /// The shared scope element: `symbol:<id>` or `memory_scope:<value>`.
    pub shared_scope: String,
    pub snapshot_match: bool,
}

/// Outcome of one detection pass (FR-034).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConflictReport {
    /// Pairs flipped to `conflicted` by this pass.
    pub conflicted: Vec<ConflictPair>,
    /// Pairs that share scope but not snapshot — labeled, never flipped.
    pub scope_differences: Vec<ConflictPair>,
}

/// Outcome of the freshness projection (FR-043).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FreshnessSummary {
    pub stale_memories: Vec<String>,
    pub conflicted_memories: Vec<String>,
    /// Memories returned to `candidate` because every linked claim was
    /// revalidated (only claim-derived staleness recovers automatically).
    pub recovered_memories: Vec<String>,
}

/// Result of a human conflict resolution (SDD §9.4 steps 7–8).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ConflictResolution {
    pub kept: MemoryRecord,
    pub dropped: MemoryRecord,
    /// How the loser was retired (`retracted`; already-superseded versions
    /// are rejected as terminal by the transition guard).
    pub outcome: String,
}

fn normalize_text(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn invalid(message: impl Into<String>) -> CccError {
    CccError::invalid_request(message)
}

fn validate_draft(draft: &MemoryDraft) -> Result<(), CccError> {
    if draft.title.trim().is_empty() {
        return Err(invalid("memory title must not be empty"));
    }
    if draft.summary.trim().is_empty() {
        return Err(invalid("memory summary must not be empty"));
    }
    if draft.created_by.trim().is_empty() {
        return Err(invalid("memory created_by must not be empty"));
    }
    Ok(())
}

async fn validate_claims_exist(
    evidence: &dyn EvidenceStore,
    claim_ids: &[ClaimId],
) -> Result<(), CccError> {
    for claim_id in claim_ids {
        if evidence.get_claim(*claim_id).await?.is_none() {
            return Err(CccError::snapshot_not_found(format!(
                "claim {claim_id} not found"
            )));
        }
    }
    Ok(())
}

fn draft_record(
    repository_id: RepositoryId,
    snapshot_id: SnapshotId,
    draft: &MemoryDraft,
) -> MemoryRecord {
    let now = now_rfc3339();
    MemoryRecord {
        memory_id: MemoryId::generate(),
        repository_id,
        snapshot_id,
        memory_type: draft.memory_type,
        title: draft.title.trim().to_string(),
        summary: draft.summary.trim().to_string(),
        scope: draft.scope.trim().to_string(),
        created_by: draft.created_by.trim().to_string(),
        created_at: now.clone(),
        last_accessed_at: now.clone(),
        // Memory without supporting claims stays candidate/unverified — it is
        // never silently trusted (SDD §5.11, key decision 2).
        status: ClaimStatus::Candidate,
        sharing_policy: draft.sharing_policy.trim().to_string(),
        sensitivity: draft.sensitivity.trim().to_string(),
        supersedes_memory_id: None,
        stale_cause: None,
        updated_at: now,
    }
}

/// What a memory mutation wrote, including any conflicts the new content
/// triggered (FR-034 detection runs after every mutation so disagreeing
/// records surface immediately, never silently).
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryMutation {
    pub record: MemoryRecord,
    pub conflicts: ConflictReport,
}

async fn create_memory(
    evidence: &dyn EvidenceStore,
    memories: &dyn MemoryStore,
    repository_id: RepositoryId,
    snapshot_id: SnapshotId,
    claim_ids: &[ClaimId],
    draft: MemoryDraft,
) -> Result<MemoryRecord, CccError> {
    validate_draft(&draft)?;
    validate_claims_exist(evidence, claim_ids).await?;
    let record = draft_record(repository_id, snapshot_id, &draft);
    memories
        .insert_memory(&record, claim_ids)
        .await
        .map_err(|e| CccError::internal_error(format!("memory insert: {e}")))?;
    Ok(record)
}

/// Creates a human-authored memory with provenance and optional supporting
/// claim links (FR-032), then runs contradiction detection (FR-034) over the
/// repository. The record starts `candidate`.
pub async fn create_memory_with_detection(
    evidence: &dyn EvidenceStore,
    memories: &dyn MemoryStore,
    repository_id: RepositoryId,
    snapshot_id: SnapshotId,
    claim_ids: &[ClaimId],
    draft: MemoryDraft,
) -> Result<MemoryMutation, CccError> {
    let record = create_memory(
        evidence,
        memories,
        repository_id,
        snapshot_id,
        claim_ids,
        draft,
    )
    .await?;
    let conflicts = detect_conflicts(evidence, memories, repository_id).await?;
    Ok(MemoryMutation { record, conflicts })
}

/// Edit-as-supersede (FR-032, SDD §5.8): inserts a new version linked to the
/// old one and flips the old record `superseded`, then runs contradiction
/// detection (FR-034). History is immutable.
pub async fn supersede_memory(
    evidence: &dyn EvidenceStore,
    memories: &dyn MemoryStore,
    repository_id: RepositoryId,
    snapshot_id: SnapshotId,
    old_memory_id: MemoryId,
    claim_ids: &[ClaimId],
    edit: MemoryEdit,
) -> Result<MemoryMutation, CccError> {
    validate_draft(&edit.draft)?;
    validate_claims_exist(evidence, claim_ids).await?;
    let old = memories
        .get_memory(old_memory_id)
        .await?
        .ok_or_else(|| CccError::snapshot_not_found(format!("memory {old_memory_id} not found")))?;
    if old.repository_id != repository_id {
        return Err(invalid("memory belongs to a different repository"));
    }
    if matches!(old.status, ClaimStatus::Superseded | ClaimStatus::Retracted) {
        return Err(invalid(format!(
            "memory {old_memory_id} is {} and can no longer be edited",
            serde_json::to_string(&old.status)
                .unwrap_or_default()
                .trim_matches('"')
        )));
    }

    let mut record = draft_record(repository_id, snapshot_id, &edit.draft);
    record.supersedes_memory_id = Some(old_memory_id);
    memories
        .insert_memory(&record, claim_ids)
        .await
        .map_err(|e| CccError::internal_error(format!("memory insert: {e}")))?;
    let flipped = memories
        .update_memory_status(
            old_memory_id,
            ClaimStatus::Superseded,
            &edit.draft.created_by,
            edit.reason.as_deref(),
            &now_rfc3339(),
        )
        .await?;
    if !flipped {
        return Err(invalid(
            "the superseded memory became terminal during the edit",
        ));
    }
    let conflicts = detect_conflicts(evidence, memories, repository_id).await?;
    Ok(MemoryMutation { record, conflicts })
}

/// Retracts a memory (logical removal from active retrieval, FR-036); the
/// record and its history stay queryable. Idempotent for retracted records.
pub async fn retract_memory(
    memories: &dyn MemoryStore,
    memory_id: MemoryId,
    actor: &str,
    reason: Option<&str>,
) -> Result<MemoryRecord, CccError> {
    let record = memories
        .get_memory(memory_id)
        .await?
        .ok_or_else(|| CccError::snapshot_not_found(format!("memory {memory_id} not found")))?;
    if record.status == ClaimStatus::Retracted {
        return Ok(record);
    }
    let flipped = memories
        .update_memory_status(
            memory_id,
            ClaimStatus::Retracted,
            actor,
            reason,
            &now_rfc3339(),
        )
        .await?;
    if !flipped {
        return Err(invalid(format!(
            "memory {memory_id} is superseded and cannot be retracted"
        )));
    }
    Ok(memories.get_memory(memory_id).await?.unwrap_or(record))
}

/// Captures an agent finding (FR-033): one `candidate` claim (source_kind
/// `agent`) with the verification id and covered scope as dependencies, one
/// `candidate` memory linked to it, and an `agent_output` evidence row for
/// provenance. Never auto-promoted (FR-080).
pub async fn capture_agent_finding(
    evidence: &dyn EvidenceStore,
    memories: &dyn MemoryStore,
    repository_id: RepositoryId,
    snapshot_id: SnapshotId,
    finding: AgentFinding<'_>,
) -> Result<AgentCapture, CccError> {
    if finding.title.trim().is_empty() || finding.summary.trim().is_empty() {
        return Err(invalid("agent finding needs a title and a summary"));
    }
    let now = now_rfc3339();
    let evidence_row = EvidenceRecord {
        evidence_id: EvidenceId::generate(),
        repository_id,
        snapshot_id,
        evidence_type: EvidenceType::AgentOutput,
        source_ref: format!("verification:{}", finding.verification_id),
        source_hash: None,
        extraction_method: ResolutionMethod::Heuristic,
        extraction_version: "ccc-memory/1".into(),
        actor: finding.created_by.trim().to_string(),
        validation_method: None,
        validation_outcome: None,
        created_at: now.clone(),
    };
    let claim = ClaimRecord {
        claim_id: ClaimId::generate(),
        repository_id,
        snapshot_id,
        claim_type: "agent_finding".into(),
        text: finding.summary.trim().to_string(),
        // Agent conclusions stay candidates until validated under the
        // applicable policy (TIS §12; repeated assertions do not corroborate).
        status: ClaimStatus::Candidate,
        source_kind: "agent".into(),
        confidence_category: ConfidenceCategory::Unverified,
        created_at: now.clone(),
        validated_at: None,
        updated_at: now.clone(),
        supersedes_claim_id: None,
    };
    let mut links = vec![ClaimDependencyRecord {
        claim_id: claim.claim_id,
        depends_on: finding.verification_id.to_string(),
        depends_on_kind: "verification".into(),
    }];
    for file_id in finding
        .covered_file_ids
        .iter()
        .take(MAX_FINDING_DEPENDENCIES)
    {
        links.push(ClaimDependencyRecord {
            claim_id: claim.claim_id,
            depends_on: file_id.to_string(),
            depends_on_kind: "file".into(),
        });
    }
    for symbol_id in finding
        .covered_symbol_ids
        .iter()
        .take(MAX_FINDING_DEPENDENCIES.saturating_sub(links.len() - 1))
    {
        links.push(ClaimDependencyRecord {
            claim_id: claim.claim_id,
            depends_on: symbol_id.to_string(),
            depends_on_kind: "symbol".into(),
        });
    }

    evidence
        .insert_evidence(std::slice::from_ref(&evidence_row))
        .await?;
    evidence.insert_claims(std::slice::from_ref(&claim)).await?;
    evidence.insert_claim_dependencies(&links).await?;

    let draft = MemoryDraft {
        title: finding.title.trim().to_string(),
        summary: finding.summary.trim().to_string(),
        memory_type: finding.memory_type,
        scope: String::new(),
        created_by: finding.created_by.trim().to_string(),
        sharing_policy: "private".into(),
        sensitivity: "none".into(),
    };
    let memory = draft_record(repository_id, snapshot_id, &draft);
    memories
        .insert_memory(&memory, &[claim.claim_id])
        .await
        .map_err(|e| CccError::internal_error(format!("memory insert: {e}")))?;
    let conflicts = detect_conflicts(evidence, memories, repository_id).await?;
    Ok(AgentCapture {
        memory,
        claim,
        conflicts,
    })
}

/// Contradiction detection (FR-034). Deterministic and conservative:
///
/// - **Claim pairs** share an identical *symbol* dependency, the same claim
///   type and the same snapshot, and their normalized texts disagree. Shared
///   file-only scope is too broad and is never used (plan risk: false
///   positives from name-level edges).
/// - **Memory pairs** share the same scope, type and normalized title, with
///   disagreeing summaries.
///
/// Same-snapshot pairs both flip `conflicted` (both sides stay inspectable);
/// cross-snapshot pairs are labeled scope differences and never flipped
/// (SDD §9.4 step 3). Nothing is auto-resolved here — resolution is human.
pub async fn detect_conflicts(
    evidence: &dyn EvidenceStore,
    memories: &dyn MemoryStore,
    repository_id: RepositoryId,
) -> Result<ConflictReport, CccError> {
    let claims = evidence.list_claims(repository_id).await?;
    let claim_ids: Vec<ClaimId> = claims.iter().map(|c| c.claim_id).collect();
    let deps = evidence.list_claim_dependencies(&claim_ids).await?;
    let now = now_rfc3339();

    // Group non-terminal, same-type claims by shared symbol dependency.
    let by_id: HashMap<ClaimId, &ClaimRecord> = claims.iter().map(|c| (c.claim_id, c)).collect();
    let mut symbol_scope: HashMap<String, Vec<ClaimId>> = HashMap::new();
    for dep in &deps {
        if dep.depends_on_kind != "symbol" {
            continue;
        }
        if let Some(claim) = by_id.get(&dep.claim_id) {
            if !matches!(
                claim.status,
                ClaimStatus::Superseded | ClaimStatus::Retracted
            ) {
                symbol_scope
                    .entry(dep.depends_on.clone())
                    .or_default()
                    .push(claim.claim_id);
            }
        }
    }

    let mut report = ConflictReport::default();
    let mut flipped_claims: Vec<ClaimId> = Vec::new();
    for (symbol_id, group) in symbol_scope {
        // Distinct claim ids sharing this symbol.
        let mut distinct: Vec<ClaimId> = group.clone();
        distinct.sort();
        distinct.dedup();
        if distinct.len() < 2 {
            continue;
        }
        for i in 0..distinct.len() {
            for j in (i + 1)..distinct.len() {
                let (a, b) = (by_id[&distinct[i]], by_id[&distinct[j]]);
                if a.claim_type != b.claim_type
                    || normalize_text(&a.text) == normalize_text(&b.text)
                {
                    continue;
                }
                let pair = ConflictPair {
                    kind: "claim_pair".into(),
                    left_id: a.claim_id.to_string(),
                    right_id: b.claim_id.to_string(),
                    left_title: a.claim_type.clone(),
                    right_title: b.claim_type.clone(),
                    left_text: a.text.clone(),
                    right_text: b.text.clone(),
                    label: "conflicted".into(),
                    shared_scope: format!("symbol:{symbol_id}"),
                    snapshot_match: a.snapshot_id == b.snapshot_id,
                };
                if a.snapshot_id == b.snapshot_id {
                    for claim in [a, b] {
                        if flipped_claims.contains(&claim.claim_id) {
                            continue;
                        }
                        // Terminal states are never touched by the store.
                        if evidence
                            .update_claim_validation(
                                claim.claim_id,
                                ClaimStatus::Conflicted,
                                None,
                                &now,
                            )
                            .await?
                        {
                            flipped_claims.push(claim.claim_id);
                        }
                    }
                    report.conflicted.push(pair);
                } else {
                    report.scope_differences.push(scope_differs(pair));
                }
            }
        }
    }

    // Memory pairs: same scope + type + normalized title, different summary.
    let memory_rows = memories.list_memories(repository_id).await?;
    let mut title_scope: HashMap<String, Vec<MemoryRecord>> = HashMap::new();
    for memory in &memory_rows {
        if matches!(
            memory.status,
            ClaimStatus::Superseded | ClaimStatus::Retracted
        ) {
            continue;
        }
        let key = format!(
            "{}|{}|{}",
            memory.scope,
            memory.memory_type.as_str(),
            normalize_text(&memory.title)
        );
        title_scope.entry(key).or_default().push(memory.clone());
    }
    let mut flipped_memories: Vec<MemoryId> = Vec::new();
    for group in title_scope.into_values() {
        if group.len() < 2 {
            continue;
        }
        for i in 0..group.len() {
            for j in (i + 1)..group.len() {
                let (a, b) = (&group[i], &group[j]);
                if normalize_text(&a.summary) == normalize_text(&b.summary) {
                    continue;
                }
                let pair = ConflictPair {
                    kind: "memory_pair".into(),
                    left_id: a.memory_id.to_string(),
                    right_id: b.memory_id.to_string(),
                    left_title: a.title.clone(),
                    right_title: b.title.clone(),
                    left_text: a.summary.clone(),
                    right_text: b.summary.clone(),
                    label: "conflicted".into(),
                    shared_scope: format!("memory_scope:{}", a.scope),
                    snapshot_match: a.snapshot_id == b.snapshot_id,
                };
                if a.snapshot_id == b.snapshot_id {
                    for memory in [a, b] {
                        if flipped_memories.contains(&memory.memory_id) {
                            continue;
                        }
                        if memories
                            .update_memory_status(
                                memory.memory_id,
                                ClaimStatus::Conflicted,
                                "ccc-memory",
                                Some(format!(
                                    "contradicts memory {}",
                                    if memory.memory_id == a.memory_id {
                                        b.memory_id
                                    } else {
                                        a.memory_id
                                    }
                                ))
                                .as_deref(),
                                &now,
                            )
                            .await?
                        {
                            flipped_memories.push(memory.memory_id);
                        }
                    }
                    report.conflicted.push(pair);
                } else {
                    report.scope_differences.push(scope_differs(pair));
                }
            }
        }
    }
    Ok(report)
}

fn scope_differs(mut pair: ConflictPair) -> ConflictPair {
    pair.label = "scope_differs".into();
    pair
}

/// Current conflicted pairs (both sides + shared scope), for `ccc conflicts`
/// (ui.md UI-018 analog): regroups the records already carrying `conflicted`.
pub async fn list_conflicts(
    evidence: &dyn EvidenceStore,
    memories: &dyn MemoryStore,
    repository_id: RepositoryId,
) -> Result<Vec<ConflictPair>, CccError> {
    let claims = evidence.list_claims(repository_id).await?;
    let conflicted_claims: Vec<&ClaimRecord> = claims
        .iter()
        .filter(|c| c.status == ClaimStatus::Conflicted)
        .collect();
    let claim_ids: Vec<ClaimId> = conflicted_claims.iter().map(|c| c.claim_id).collect();
    let deps = evidence.list_claim_dependencies(&claim_ids).await?;
    let by_id: HashMap<ClaimId, &ClaimRecord> = claims.iter().map(|c| (c.claim_id, c)).collect();

    let mut symbol_scope: HashMap<String, Vec<ClaimId>> = HashMap::new();
    for dep in &deps {
        if dep.depends_on_kind == "symbol" {
            symbol_scope
                .entry(dep.depends_on.clone())
                .or_default()
                .push(dep.claim_id);
        }
    }

    let mut pairs: Vec<ConflictPair> = Vec::new();
    let mut seen: std::collections::BTreeSet<(String, String)> = Default::default();
    for (symbol_id, group) in symbol_scope {
        let mut distinct: Vec<ClaimId> = group;
        distinct.sort();
        distinct.dedup();
        for i in 0..distinct.len() {
            for j in (i + 1)..distinct.len() {
                let (Some(a), Some(b)) = (by_id.get(&distinct[i]), by_id.get(&distinct[j])) else {
                    continue;
                };
                if a.status != ClaimStatus::Conflicted || b.status != ClaimStatus::Conflicted {
                    continue;
                }
                let key = (a.claim_id.to_string(), b.claim_id.to_string());
                if !seen.insert(key) {
                    continue;
                }
                pairs.push(ConflictPair {
                    kind: "claim_pair".into(),
                    left_id: a.claim_id.to_string(),
                    right_id: b.claim_id.to_string(),
                    left_title: a.claim_type.clone(),
                    right_title: b.claim_type.clone(),
                    left_text: a.text.clone(),
                    right_text: b.text.clone(),
                    label: "conflicted".into(),
                    shared_scope: format!("symbol:{symbol_id}"),
                    snapshot_match: a.snapshot_id == b.snapshot_id,
                });
            }
        }
    }

    let memory_rows = memories.list_memories(repository_id).await?;
    let mut title_scope: HashMap<String, Vec<&MemoryRecord>> = HashMap::new();
    for memory in memory_rows
        .iter()
        .filter(|m| m.status == ClaimStatus::Conflicted)
    {
        let key = format!(
            "{}|{}|{}",
            memory.scope,
            memory.memory_type.as_str(),
            normalize_text(&memory.title)
        );
        title_scope.entry(key).or_default().push(memory);
    }
    for group in title_scope.into_values() {
        if group.len() < 2 {
            continue;
        }
        for i in 0..group.len() {
            for j in (i + 1)..group.len() {
                let (a, b) = (group[i], group[j]);
                if normalize_text(&a.summary) == normalize_text(&b.summary) {
                    continue;
                }
                let key = (a.memory_id.to_string(), b.memory_id.to_string());
                if !seen.insert(key) {
                    continue;
                }
                pairs.push(ConflictPair {
                    kind: "memory_pair".into(),
                    left_id: a.memory_id.to_string(),
                    right_id: b.memory_id.to_string(),
                    left_title: a.title.clone(),
                    right_title: b.title.clone(),
                    left_text: a.summary.clone(),
                    right_text: b.summary.clone(),
                    label: "conflicted".into(),
                    shared_scope: format!("memory_scope:{}", a.scope),
                    snapshot_match: a.snapshot_id == b.snapshot_id,
                });
            }
        }
    }
    pairs.sort_by(|x, y| {
        x.kind
            .cmp(&y.kind)
            .then(x.left_id.cmp(&y.left_id))
            .then(x.right_id.cmp(&y.right_id))
    });
    Ok(pairs)
}

/// Freshness projection (FR-043): a memory follows its linked claims. Any
/// linked claim `stale`/`conflicted` flips the non-terminal memory to the
/// same status with a recorded cause; a memory whose claim-derived staleness
/// cleared (all linked claims revalidated) returns to `candidate`.
/// Terminal memories are never touched; human-resolved conflicts (label
/// `contradicts …`) never auto-recover.
pub async fn project_claim_freshness(
    evidence: &dyn EvidenceStore,
    memories: &dyn MemoryStore,
    repository_id: RepositoryId,
) -> Result<FreshnessSummary, CccError> {
    let mut summary = FreshnessSummary::default();
    let memory_rows = memories.list_memories(repository_id).await?;
    for memory in memory_rows {
        if matches!(
            memory.status,
            ClaimStatus::Superseded | ClaimStatus::Retracted
        ) {
            continue;
        }
        let claim_ids = memories.claim_ids_for_memory(memory.memory_id).await?;
        if claim_ids.is_empty() {
            continue;
        }
        let mut any_stale = false;
        let mut any_conflicted = false;
        for claim_id in &claim_ids {
            if let Some(claim) = evidence.get_claim(*claim_id).await? {
                match claim.status {
                    ClaimStatus::Conflicted => any_conflicted = true,
                    ClaimStatus::Stale => any_stale = true,
                    _ => {}
                }
            }
        }
        let now = now_rfc3339();
        let cause = format!("claim {} reported the invalidation", claim_ids[0]);
        if any_conflicted && memory.status != ClaimStatus::Conflicted {
            if memories
                .update_memory_status(
                    memory.memory_id,
                    ClaimStatus::Conflicted,
                    "ccc-memory",
                    Some(cause.as_str()),
                    &now,
                )
                .await?
            {
                summary
                    .conflicted_memories
                    .push(memory.memory_id.to_string());
            }
        } else if any_stale && memory.status != ClaimStatus::Stale {
            if memories
                .update_memory_status(
                    memory.memory_id,
                    ClaimStatus::Stale,
                    "ccc-memory",
                    Some(cause.as_str()),
                    &now,
                )
                .await?
            {
                summary.stale_memories.push(memory.memory_id.to_string());
            }
        } else if !any_stale && !any_conflicted {
            // Recovery only for claim-derived staleness; conflicted records
            // need a human resolution (SDD §9.4).
            let claim_derived = memory
                .stale_cause
                .as_deref()
                .is_some_and(|c| c.starts_with("claim "));
            if memory.status == ClaimStatus::Stale && claim_derived {
                let recovered = memories
                    .update_memory_status(
                        memory.memory_id,
                        ClaimStatus::Candidate,
                        "ccc-memory",
                        Some("all linked claims revalidated"),
                        &now,
                    )
                    .await?;
                if recovered {
                    summary
                        .recovered_memories
                        .push(memory.memory_id.to_string());
                }
            }
        }
    }
    summary.stale_memories.sort();
    summary.conflicted_memories.sort();
    summary.recovered_memories.sort();
    Ok(summary)
}

/// Human conflict resolution (SDD §9.4 steps 7–8): the losing memory is
/// retracted with the resolution recorded (actor + rationale in its status
/// trail); the kept side returns from `conflicted` to `candidate` — support
/// still requires scoped revalidation (FR-080: no automatic promotion).
pub async fn resolve_conflict(
    memories: &dyn MemoryStore,
    repository_id: RepositoryId,
    keep_id: MemoryId,
    drop_id: MemoryId,
    rationale: &str,
    actor: &str,
) -> Result<ConflictResolution, CccError> {
    if keep_id == drop_id {
        return Err(invalid("keep and drop must be different memories"));
    }
    if rationale.trim().is_empty() {
        return Err(invalid("a resolution requires a recorded rationale"));
    }
    if actor.trim().is_empty() {
        return Err(invalid("a resolution requires a recorded actor"));
    }
    let keep = memories
        .get_memory(keep_id)
        .await?
        .ok_or_else(|| CccError::snapshot_not_found(format!("memory {keep_id} not found")))?;
    let drop = memories
        .get_memory(drop_id)
        .await?
        .ok_or_else(|| CccError::snapshot_not_found(format!("memory {drop_id} not found")))?;
    if keep.repository_id != repository_id || drop.repository_id != repository_id {
        return Err(invalid("keep and drop must belong to the same repository"));
    }
    if matches!(
        drop.status,
        ClaimStatus::Superseded | ClaimStatus::Retracted
    ) {
        return Err(invalid(format!(
            "memory {drop_id} is already terminal; nothing to resolve"
        )));
    }

    let now = now_rfc3339();
    let flipped = memories
        .update_memory_status(
            drop_id,
            ClaimStatus::Retracted,
            actor,
            Some(format!("conflict resolved in favor of {keep_id}: {rationale}").as_str()),
            &now,
        )
        .await?;
    if !flipped {
        return Err(invalid(
            "the losing memory became terminal during resolution",
        ));
    }
    let mut kept = keep.clone();
    if kept.status == ClaimStatus::Conflicted {
        let recovered = memories
            .update_memory_status(
                keep_id,
                ClaimStatus::Candidate,
                actor,
                Some(format!("conflict resolved against {drop_id}: {rationale}").as_str()),
                &now,
            )
            .await?;
        if recovered {
            kept.status = ClaimStatus::Candidate;
        }
    }
    let dropped = memories
        .get_memory(drop_id)
        .await?
        .unwrap_or_else(|| drop.clone());
    Ok(ConflictResolution {
        kept,
        dropped,
        outcome: "retracted".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccc_storage::test_support::{InMemoryEvidenceStore, InMemoryMemoryStore};

    async fn claim(
        store: &InMemoryEvidenceStore,
        repo: RepositoryId,
        snap: SnapshotId,
        text: &str,
    ) -> ClaimId {
        let record = ClaimRecord {
            claim_id: ClaimId::generate(),
            repository_id: repo,
            snapshot_id: snap,
            claim_type: "finding".into(),
            text: text.into(),
            status: ClaimStatus::Candidate,
            source_kind: "human".into(),
            confidence_category: ConfidenceCategory::Unverified,
            created_at: now_rfc3339(),
            validated_at: None,
            updated_at: now_rfc3339(),
            supersedes_claim_id: None,
        };
        store
            .insert_claims(std::slice::from_ref(&record))
            .await
            .unwrap();
        record.claim_id
    }

    fn draft(title: &str, summary: &str, scope: &str) -> MemoryDraft {
        MemoryDraft {
            title: title.into(),
            summary: summary.into(),
            memory_type: MemoryType::FailedApproach,
            scope: scope.into(),
            created_by: "human:test".into(),
            sharing_policy: "private".into(),
            sensitivity: "none".into(),
        }
    }

    #[tokio::test]
    async fn memory_create_starts_candidate_and_links_claims() {
        let evidence = InMemoryEvidenceStore::new();
        let memories = InMemoryMemoryStore::new();
        let repo = RepositoryId::generate();
        let snap = SnapshotId::generate();
        let claim_id = claim(&evidence, repo, snap, "Symbol x::y exists").await;

        let record = create_memory_with_detection(
            &evidence,
            &memories,
            repo,
            snap,
            &[claim_id],
            draft("Title", "A summary", "src"),
        )
        .await
        .unwrap()
        .record;
        assert_eq!(record.status, ClaimStatus::Candidate);
        assert_eq!(
            memories
                .claim_ids_for_memory(record.memory_id)
                .await
                .unwrap(),
            vec![claim_id]
        );
        // Missing claims are rejected (no false availability).
        let err = create_memory_with_detection(
            &evidence,
            &memories,
            repo,
            snap,
            &[ClaimId::generate()],
            draft("T", "S", ""),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::SnapshotNotFound);
    }

    #[tokio::test]
    async fn supersede_creates_a_new_version_and_retires_the_old() {
        let evidence = InMemoryEvidenceStore::new();
        let memories = InMemoryMemoryStore::new();
        let repo = RepositoryId::generate();
        let snap = SnapshotId::generate();
        let first = create_memory_with_detection(
            &evidence,
            &memories,
            repo,
            snap,
            &[],
            draft("Approach", "v1 text", ""),
        )
        .await
        .unwrap()
        .record;

        let second = supersede_memory(
            &evidence,
            &memories,
            repo,
            snap,
            first.memory_id,
            &[],
            MemoryEdit {
                draft: draft("Approach", "v2 text", ""),
                reason: Some("update after review".into()),
            },
        )
        .await
        .unwrap()
        .record;
        assert_eq!(second.supersedes_memory_id, Some(first.memory_id));
        let old = memories.get_memory(first.memory_id).await.unwrap().unwrap();
        assert_eq!(old.status, ClaimStatus::Superseded);
        let events = memories
            .list_memory_status_events(first.memory_id)
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].status, ClaimStatus::Superseded);
        assert_eq!(events[0].reason.as_deref(), Some("update after review"));

        // Editing a terminal memory is rejected.
        let err = supersede_memory(
            &evidence,
            &memories,
            repo,
            snap,
            first.memory_id,
            &[],
            MemoryEdit {
                draft: draft("Approach", "v3", ""),
                reason: None,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
    }

    #[tokio::test]
    async fn agent_capture_stays_candidate_with_provenance() {
        let evidence = InMemoryEvidenceStore::new();
        let memories = InMemoryMemoryStore::new();
        let repo = RepositoryId::generate();
        let snap = SnapshotId::generate();
        let verification = VerificationId::generate();
        let capture = capture_agent_finding(
            &evidence,
            &memories,
            repo,
            snap,
            AgentFinding {
                title: "Failed approach: cache the manifest".into(),
                summary: "Caching the manifest hash broke snapshot identity.".into(),
                memory_type: MemoryType::FailedApproach,
                created_by: "agent:verification".into(),
                verification_id: verification,
                covered_file_ids: &[],
                covered_symbol_ids: &[],
            },
        )
        .await
        .unwrap();

        // FR-080 pin: agent findings are candidates, never auto-promoted.
        assert_eq!(capture.claim.status, ClaimStatus::Candidate);
        assert_eq!(capture.claim.source_kind, "agent");
        assert_eq!(capture.memory.status, ClaimStatus::Candidate);
        let evidence_rows = evidence.list_evidence(repo).await.unwrap();
        assert!(evidence_rows
            .iter()
            .any(|e| e.evidence_type == EvidenceType::AgentOutput
                && e.source_ref == format!("verification:{verification}")));
        let deps = evidence
            .list_claim_dependencies(&[capture.claim.claim_id])
            .await
            .unwrap();
        assert!(deps.iter().any(
            |d| d.depends_on == verification.to_string() && d.depends_on_kind == "verification"
        ));
    }

    #[tokio::test]
    async fn same_scope_disagreement_conflicts_both_sides() {
        let evidence = InMemoryEvidenceStore::new();
        let memories = InMemoryMemoryStore::new();
        let repo = RepositoryId::generate();
        let snap = SnapshotId::generate();
        let symbol = SymbolId::generate();
        let a = claim(&evidence, repo, snap, "Root cause is the config loader").await;
        let b = claim(&evidence, repo, snap, "Root cause is the cache layer").await;
        for claim_id in [&a, &b] {
            evidence
                .insert_claim_dependencies(&[ClaimDependencyRecord {
                    claim_id: *claim_id,
                    depends_on: symbol.to_string(),
                    depends_on_kind: "symbol".into(),
                }])
                .await
                .unwrap();
        }

        let report = detect_conflicts(&evidence, &memories, repo).await.unwrap();
        assert_eq!(report.conflicted.len(), 1);
        assert!(report.scope_differences.is_empty());
        for claim_id in [&a, &b] {
            let c = evidence.get_claim(*claim_id).await.unwrap().unwrap();
            assert_eq!(c.status, ClaimStatus::Conflicted);
        }
        // Both sides stay inspectable via the conflicts view.
        let pairs = list_conflicts(&evidence, &memories, repo).await.unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].shared_scope, format!("symbol:{symbol}"));

        // Identical texts on a separate scope are not a conflict.
        let symbol2 = SymbolId::generate();
        let x = claim(&evidence, repo, snap, "Same assertion, second copy").await;
        let y = claim(&evidence, repo, snap, "Same assertion, second copy").await;
        for claim_id in [&x, &y] {
            evidence
                .insert_claim_dependencies(&[ClaimDependencyRecord {
                    claim_id: *claim_id,
                    depends_on: symbol2.to_string(),
                    depends_on_kind: "symbol".into(),
                }])
                .await
                .unwrap();
        }
        let report = detect_conflicts(&evidence, &memories, repo).await.unwrap();
        assert!(
            !report
                .conflicted
                .iter()
                .any(|p| p.shared_scope == format!("symbol:{symbol2}")),
            "identical text is no conflict"
        );
    }

    #[tokio::test]
    async fn cross_snapshot_disagreement_is_labeled_scope_difference() {
        let evidence = InMemoryEvidenceStore::new();
        let memories = InMemoryMemoryStore::new();
        let repo = RepositoryId::generate();
        let snap_a = SnapshotId::generate();
        let snap_b = SnapshotId::generate();
        let symbol = SymbolId::generate();
        let a = claim(&evidence, repo, snap_a, "Behavior X holds for symbol").await;
        let b = claim(&evidence, repo, snap_b, "Behavior X changed for symbol").await;
        for claim_id in [&a, &b] {
            evidence
                .insert_claim_dependencies(&[ClaimDependencyRecord {
                    claim_id: *claim_id,
                    depends_on: symbol.to_string(),
                    depends_on_kind: "symbol".into(),
                }])
                .await
                .unwrap();
        }
        let report = detect_conflicts(&evidence, &memories, repo).await.unwrap();
        assert!(report.conflicted.is_empty());
        assert_eq!(report.scope_differences.len(), 1);
        assert_eq!(report.scope_differences[0].label, "scope_differs");
        assert!(!report.scope_differences[0].snapshot_match);
        // Neither side flipped.
        for claim_id in [&a, &b] {
            let c = evidence.get_claim(*claim_id).await.unwrap().unwrap();
            assert_eq!(c.status, ClaimStatus::Candidate);
        }
    }

    #[tokio::test]
    async fn memory_pairs_conflict_on_same_scope_type_title_with_different_summary() {
        let evidence = InMemoryEvidenceStore::new();
        let memories = InMemoryMemoryStore::new();
        let repo = RepositoryId::generate();
        let snap = SnapshotId::generate();
        create_memory(
            &evidence,
            &memories,
            repo,
            snap,
            &[],
            draft("Deploy steps", "Run make first", "ops"),
        )
        .await
        .unwrap();
        create_memory(
            &evidence,
            &memories,
            repo,
            snap,
            &[],
            draft("Deploy steps", "Run cargo first", "ops"),
        )
        .await
        .unwrap();
        // Different title → different topic, no conflict.
        create_memory(
            &evidence,
            &memories,
            repo,
            snap,
            &[],
            draft("Rollback", "Run make first", "ops"),
        )
        .await
        .unwrap();

        let report = detect_conflicts(&evidence, &memories, repo).await.unwrap();
        assert_eq!(report.conflicted.len(), 1);
        assert_eq!(report.conflicted[0].kind, "memory_pair");
        let pairs = list_conflicts(&evidence, &memories, repo).await.unwrap();
        assert_eq!(pairs.len(), 1);
    }

    #[tokio::test]
    async fn freshness_projection_flips_only_linked_memories_and_recovers() {
        let evidence = InMemoryEvidenceStore::new();
        let memories = InMemoryMemoryStore::new();
        let repo = RepositoryId::generate();
        let snap = SnapshotId::generate();
        let stale_claim = claim(&evidence, repo, snap, "Claim that will go stale").await;
        let healthy_claim = claim(&evidence, repo, snap, "Healthy claim").await;
        let linked = create_memory(
            &evidence,
            &memories,
            repo,
            snap,
            &[stale_claim],
            draft("Linked", "depends on the stale claim", ""),
        )
        .await
        .unwrap();
        let unrelated = create_memory(
            &evidence,
            &memories,
            repo,
            snap,
            &[healthy_claim],
            draft("Unlinked", "depends on the healthy claim", ""),
        )
        .await
        .unwrap();

        evidence
            .mark_claims_stale(&[stale_claim], &now_rfc3339())
            .await
            .unwrap();
        let summary = project_claim_freshness(&evidence, &memories, repo)
            .await
            .unwrap();
        assert_eq!(summary.stale_memories, vec![linked.memory_id.to_string()]);
        let stale_memory = memories
            .get_memory(linked.memory_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stale_memory.status, ClaimStatus::Stale);
        assert!(stale_memory
            .stale_cause
            .as_deref()
            .unwrap_or_default()
            .starts_with("claim "));
        // Selective: the unrelated memory is untouched (FR-040 posture).
        let untouched = memories
            .get_memory(unrelated.memory_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(untouched.status, ClaimStatus::Candidate);

        // Recovery: the claim is revalidated → the memory returns to candidate.
        evidence
            .update_claim_validation(
                stale_claim,
                ClaimStatus::Supported,
                Some(&now_rfc3339()),
                &now_rfc3339(),
            )
            .await
            .unwrap();
        let summary = project_claim_freshness(&evidence, &memories, repo)
            .await
            .unwrap();
        assert_eq!(
            summary.recovered_memories,
            vec![linked.memory_id.to_string()]
        );
        let recovered = memories
            .get_memory(linked.memory_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovered.status, ClaimStatus::Candidate);
    }

    #[tokio::test]
    async fn conflict_resolution_retracts_the_loser_and_records_the_rationale() {
        let evidence = InMemoryEvidenceStore::new();
        let memories = InMemoryMemoryStore::new();
        let repo = RepositoryId::generate();
        let snap = SnapshotId::generate();
        let keep = create_memory(
            &evidence,
            &memories,
            repo,
            snap,
            &[],
            draft("Decision", "Keep side", ""),
        )
        .await
        .unwrap();
        let drop = create_memory(
            &evidence,
            &memories,
            repo,
            snap,
            &[],
            draft("Decision", "Drop side", ""),
        )
        .await
        .unwrap();
        detect_conflicts(&evidence, &memories, repo).await.unwrap();
        assert_eq!(
            memories
                .get_memory(keep.memory_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            ClaimStatus::Conflicted
        );

        let resolution = resolve_conflict(
            &memories,
            repo,
            keep.memory_id,
            drop.memory_id,
            "verified by cargo test",
            "human:alice",
        )
        .await
        .unwrap();
        assert_eq!(resolution.outcome, "retracted");
        let dropped = memories.get_memory(drop.memory_id).await.unwrap().unwrap();
        assert_eq!(dropped.status, ClaimStatus::Retracted);
        let kept = memories.get_memory(keep.memory_id).await.unwrap().unwrap();
        assert_eq!(kept.status, ClaimStatus::Candidate);
        let events = memories
            .list_memory_status_events(drop.memory_id)
            .await
            .unwrap();
        assert!(events.iter().any(|e| e.actor == "human:alice"
            && e.reason
                .as_deref()
                .unwrap_or_default()
                .contains("verified by cargo test")));

        // Retracting an already-terminal record is rejected (no transitions
        // out of terminal states).
        let err = resolve_conflict(
            &memories,
            repo,
            keep.memory_id,
            drop.memory_id,
            "again",
            "human:alice",
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
    }
}
