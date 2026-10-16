//! Canonical entity records shared across crates.
//!
//! All paths in these records are repository-relative with forward slashes
//! (TIS §4.1). Timestamps are RFC 3339 strings in UTC.

use crate::enums::*;
use crate::id::*;
use serde::{Deserialize, Serialize};

/// A registered repository (TIS §4.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepositoryRecord {
    pub repository_id: RepositoryId,
    /// Local filesystem path of the repository root. Local-operational only;
    /// it must never be copied into portable exported evidence.
    pub root_path: String,
    pub canonical_uri: Option<String>,
    pub default_branch: Option<String>,
    pub configuration_hash: String,
    /// Detected limitations (e.g. submodules present but not indexed).
    pub limitations: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// An immutable repository state (TIS §4.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotRecord {
    pub snapshot_id: SnapshotId,
    pub repository_id: RepositoryId,
    pub commit_hash: Option<String>,
    pub branch: Option<String>,
    pub kind: SnapshotKind,
    pub is_dirty: bool,
    /// BLAKE3 over the sorted `{path → content_hash}` manifest.
    pub manifest_hash: String,
    pub indexer_version: String,
    pub configuration_hash: String,
    pub status: SnapshotStatus,
    pub created_at: String,
}

/// A file row within a snapshot. Excluded files are recorded with a reason
/// category — nothing is silently dropped (TIS §7.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileRecord {
    pub file_id: FileId,
    pub snapshot_id: SnapshotId,
    pub repository_id: RepositoryId,
    /// Repository-relative, forward-slash path.
    pub path: String,
    /// BLAKE3 of the raw file bytes (CRLF-sensitive by design; see plan R2).
    pub content_hash: String,
    pub size_bytes: u64,
    /// Language when recognized (e.g. `rust`), otherwise `None`.
    pub language: Option<String>,
    pub binary: bool,
    pub inclusion: FileInclusion,
    pub exclusion_reason: Option<ExclusionReason>,
}

/// A symbol extracted for a snapshot (TIS §6.1).
///
/// Line numbers are snapshot-scoped and never part of identity; the logical
/// identity is `symbol_key` (language + qualified name + scope + signature
/// hash).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SymbolRecord {
    pub symbol_id: SymbolId,
    pub repository_id: RepositoryId,
    pub snapshot_id: SnapshotId,
    pub file_id: FileId,
    pub symbol_key: String,
    pub qualified_name: String,
    pub kind: SymbolKind,
    pub language: String,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
    pub signature_hash: String,
    pub resolver: ResolutionMethod,
    pub resolution_status: ResolutionStatus,
}

/// A source-level reference to a (possibly unresolved) target (TIS §6.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferenceRecord {
    pub reference_id: ReferenceId,
    pub snapshot_id: SnapshotId,
    pub file_id: FileId,
    pub source_symbol_id: Option<SymbolId>,
    pub target_name: String,
    pub relationship: RelationshipType,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
    pub resolution_method: ResolutionMethod,
    pub resolution_status: ResolutionStatus,
}

/// A typed relationship with provenance (TIS §6.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdgeRecord {
    pub edge_id: EdgeId,
    pub repository_id: RepositoryId,
    pub snapshot_id: SnapshotId,
    pub source_symbol_id: SymbolId,
    /// Target symbol when resolved to a symbol of the same snapshot.
    pub target_symbol_id: Option<SymbolId>,
    /// Name-level target when the edge is not (yet) resolved.
    pub target_name: Option<String>,
    pub edge_type: RelationshipType,
    pub resolution_method: ResolutionMethod,
    pub resolution_status: ResolutionStatus,
    pub confidence: ConfidenceCategory,
    /// `true` when the edge was inferred (e.g. test association heuristic),
    /// `false` when it was directly extracted from source syntax.
    pub inferred: bool,
    pub provenance_ref: Option<String>,
    pub created_at: String,
}

/// A versioned engineering claim (TIS §6.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimRecord {
    pub claim_id: ClaimId,
    pub repository_id: RepositoryId,
    pub snapshot_id: SnapshotId,
    pub claim_type: String,
    pub text: String,
    pub status: ClaimStatus,
    pub source_kind: String,
    pub confidence_category: ConfidenceCategory,
    pub created_at: String,
    pub validated_at: Option<String>,
    pub updated_at: String,
}

/// An evidence record (TIS §6.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceRecord {
    pub evidence_id: EvidenceId,
    pub repository_id: RepositoryId,
    pub snapshot_id: SnapshotId,
    pub evidence_type: EvidenceType,
    /// Repository-relative source reference, e.g. `path#L12-L20`.
    pub source_ref: String,
    pub source_hash: Option<String>,
    pub extraction_method: ResolutionMethod,
    pub extraction_version: String,
    pub actor: String,
    pub validation_method: Option<String>,
    pub validation_outcome: Option<String>,
    pub created_at: String,
}

/// A dependency link from a claim to a symbol, file, evidence or claim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimDependencyRecord {
    pub claim_id: ClaimId,
    pub depends_on: String,
    /// `symbol` | `file` | `evidence` | `claim`.
    pub depends_on_kind: String,
}

/// A job record (TIS §15.1). Step 1 executes jobs synchronously; the record
/// exists so future async workers reuse the same ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobRecord {
    pub job_id: JobId,
    pub repository_id: RepositoryId,
    pub job_type: String,
    pub state: JobState,
    pub idempotency_key: String,
    pub payload_json: Option<String>,
    pub retry_count: u32,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

/// An index generation (TIS §4.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerationRecord {
    pub generation_id: GenerationId,
    pub repository_id: RepositoryId,
    pub snapshot_id: SnapshotId,
    pub status: GenerationStatus,
    /// Counters: parsed/reused/excluded/failed (TIS §18).
    pub counters_json: String,
    pub created_at: String,
    pub published_at: Option<String>,
}

/// A persisted context bundle row (TIS §9.7). Item details and the full
/// canonical JSON live in the artifact store / item table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BundleRecord {
    pub bundle_id: BundleId,
    pub repository_id: RepositoryId,
    pub snapshot_id: SnapshotId,
    pub generation_id: GenerationId,
    pub schema_version: String,
    pub task_text: String,
    pub normalized_json: String,
    pub retrieval_policy_version: String,
    pub budget_tokens: u64,
    pub estimated_tokens: u64,
    pub token_estimate_basis: String,
    pub stopping_reason: String,
    pub bundle_hash: String,
    pub artifact_hash: String,
    pub created_at: String,
}

/// A persisted change-impact report row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImpactRecord {
    pub impact_id: ImpactId,
    pub repository_id: RepositoryId,
    pub snapshot_id: SnapshotId,
    pub base: String,
    pub head: String,
    pub schema_version: String,
    pub report_hash: String,
    pub artifact_hash: String,
    pub created_at: String,
}

/// An immutable verification record (TIS §11.1 step 9, §11.3): identifies the
/// exact snapshot, policy (name + hash), mode, and per-status summary. Check
/// details live in [`VerificationCheckRecord`] rows; command output lives in
/// the artifact store (refs, never inline).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerificationRecord {
    pub verification_id: VerificationId,
    pub repository_id: RepositoryId,
    pub snapshot_id: SnapshotId,
    pub base: String,
    pub head: String,
    /// Policy file name without extension (e.g. `analysis`).
    pub policy_name: String,
    /// BLAKE3 over the canonical serialization of the effective policy.
    pub policy_hash: String,
    pub mode: VerificationMode,
    /// Per-status counts over the record's checks (serialized JSON object:
    /// status string → count). Summary only — check rows are authoritative.
    pub statuses_json: String,
    pub artifact_hash: String,
    pub created_at: String,
}

/// One check row of a verification record (queryable, TIS §11.3). Output is
/// stored as capped blob artifacts referenced by `stdout_ref`/`stderr_ref`;
/// truncation is flagged, never silent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerificationCheckRecord {
    pub check_id: VerificationId,
    pub verification_id: VerificationId,
    pub position: u32,
    /// The command (or analysis suggestion) this check corresponds to.
    pub suggested_command: String,
    /// `directly` | `transitively` | `historically` | `fallback` (SDD §15.2),
    /// or `directly` for static-analysis entries.
    pub relation: String,
    pub status: VerificationStatus,
    pub exit_code: Option<i64>,
    pub duration_ms: Option<u64>,
    pub stdout_ref: Option<String>,
    pub stderr_ref: Option<String>,
    /// `true` when stored output was capped at the policy limit.
    pub truncated: bool,
    /// Minimal environment summary actually applied (allowlisted names only).
    pub env_summary_json: String,
}

/// A stored evaluation suite: immutable once recorded; the hash is the key
/// (SDD Phase G).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvaluationSuiteRecord {
    /// BLAKE3 over the suite file bytes.
    pub suite_hash: String,
    pub name: String,
    pub case_count: u32,
    /// Canonical JSON of the parsed suite document.
    pub suite_json: String,
    pub created_at: String,
}

/// An immutable evaluation run record (SDD §20.2, TIS §19.5). Runs with
/// identical (suite, policy, config) inputs produce identical `metrics_json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvaluationRunRecord {
    pub run_id: EvaluationRunId,
    pub repository_id: RepositoryId,
    pub suite_name: String,
    pub suite_hash: String,
    pub policy_version: String,
    pub policy_hash: String,
    pub config_hash: String,
    pub metrics_json: String,
    pub created_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> RepositoryRecord {
        RepositoryRecord {
            repository_id: RepositoryId::generate(),
            root_path: "C:/tmp/repo".into(),
            canonical_uri: None,
            default_branch: Some("main".into()),
            configuration_hash: "abc".into(),
            limitations: vec![],
            created_at: "2026-10-16T00:00:00Z".into(),
            updated_at: "2026-10-16T00:00:00Z".into(),
        }
    }

    #[test]
    fn repository_record_round_trips() {
        let record = repo();
        let json = serde_json::to_string(&record).unwrap();
        let back: RepositoryRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, record);
    }

    #[test]
    fn edge_record_serializes_enums_snake_case() {
        let edge = EdgeRecord {
            edge_id: EdgeId::generate(),
            repository_id: RepositoryId::generate(),
            snapshot_id: SnapshotId::generate(),
            source_symbol_id: SymbolId::generate(),
            target_symbol_id: None,
            target_name: Some("other::thing".into()),
            edge_type: RelationshipType::Calls,
            resolution_method: ResolutionMethod::TreeSitter,
            resolution_status: ResolutionStatus::Candidate,
            confidence: ConfidenceCategory::Unverified,
            inferred: false,
            provenance_ref: Some("src/lib.rs#L4".into()),
            created_at: "2026-10-16T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&edge).unwrap();
        assert!(json.contains("\"edge_type\":\"calls\""), "{json}");
        assert!(
            json.contains("\"resolution_method\":\"tree_sitter\""),
            "{json}"
        );
        assert!(
            json.contains("\"resolution_status\":\"candidate\""),
            "{json}"
        );
    }
}
