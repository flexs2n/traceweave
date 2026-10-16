//! Enumeration values fixed by the TIS (§6.2, §6.3, §7.3, §15.1).
//!
//! These names are part of the serialized contract; renaming them is a
//! breaking change. Values are serialized as `snake_case` strings, with
//! `tree_sitter` spelled exactly as the TIS specifies.

use serde::{Deserialize, Serialize};

/// Typed relationship between symbols or artifacts (TIS §6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipType {
    Calls,
    References,
    Imports,
    Implements,
    Extends,
    Reads,
    Writes,
    Configures,
    Tests,
    Generates,
    BuildDependsOn,
    PublishesContract,
    ConsumesContract,
}

/// Method by which a relationship or symbol was established (TIS §7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionMethod {
    TreeSitter,
    Scip,
    Lsp,
    Compiler,
    BuildAdapter,
    TestAdapter,
    RuntimeTrace,
    Heuristic,
}

/// How confidently a relationship or identity is established (TIS §7.2).
///
/// Syntax parsing is never presented as semantic resolution: name-level
/// references default to [`ResolutionStatus::Candidate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionStatus {
    Resolved,
    Candidate,
    Unresolved,
}

/// Status of a versioned engineering claim (TIS §6.3, SDD §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimStatus {
    Candidate,
    Supported,
    Verified,
    Stale,
    Conflicted,
    Superseded,
    Retracted,
}

/// State machine for jobs (TIS §15.1): `queued → running → succeeded | failed | cancelled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl JobState {
    /// Returns `true` when the state is terminal.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobState::Succeeded | JobState::Failed | JobState::Cancelled
        )
    }
}

/// Legal transitions of the job state machine (TIS §15.1).
pub fn job_state_transition_is_legal(from: JobState, to: JobState) -> bool {
    use JobState::*;
    matches!(
        (from, to),
        (Queued, Running)
            | (Queued, Cancelled)
            | (Running, Succeeded)
            | (Running, Failed)
            | (Running, Cancelled)
    )
}

/// Kind of a syntactic symbol extracted from source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Function,
    Struct,
    Enum,
    Trait,
    Impl,
    Module,
    UseDeclaration,
    TypeAlias,
    Const,
    Static,
    Macro,
    Field,
    Variant,
    Other,
}

/// Type of an evidence record (TIS §6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceType {
    SourceFact,
    TestResult,
    Configuration,
    History,
    HumanReview,
    AgentOutput,
    Verification,
}

/// Which repository state a snapshot captures (TIS §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotKind {
    Committed,
    WorkingTree,
    Staged,
}

/// Lifecycle status of a snapshot (TIS §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotStatus {
    Building,
    Published,
    Failed,
    Superseded,
}

/// Lifecycle status of an index generation (TIS §4.4, SDD §7.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationStatus {
    Building,
    Published,
    Failed,
    Superseded,
}

/// Why a file was excluded from indexing (TIS §7.1: exclusions are explained,
/// never silent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExclusionReason {
    ExcludedByPattern,
    Binary,
    Oversized,
    UnsupportedLanguage,
    Submodule,
    Other,
}

/// Whether a file participates in indexing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileInclusion {
    Included,
    Excluded,
}

/// Confidence category attached to edges and claims (TIS §6.2/§6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceCategory {
    Confirmed,
    Corroborated,
    Inferred,
    Unverified,
}

/// Change kind for a single path in a normalized change set (SDD §5.15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relationship_types_serialize_per_tis() {
        assert_eq!(
            serde_json::to_string(&RelationshipType::BuildDependsOn).unwrap(),
            "\"build_depends_on\""
        );
        assert_eq!(
            serde_json::to_string(&RelationshipType::PublishesContract).unwrap(),
            "\"publishes_contract\""
        );
        assert_eq!(
            serde_json::to_string(&RelationshipType::ConsumesContract).unwrap(),
            "\"consumes_contract\""
        );
    }

    #[test]
    fn tree_sitter_spells_with_underscore() {
        assert_eq!(
            serde_json::to_string(&ResolutionMethod::TreeSitter).unwrap(),
            "\"tree_sitter\""
        );
        let back: ResolutionMethod = serde_json::from_str("\"tree_sitter\"").unwrap();
        assert_eq!(back, ResolutionMethod::TreeSitter);
    }

    #[test]
    fn resolution_status_enums_round_trip() {
        for status in [
            ResolutionStatus::Resolved,
            ResolutionStatus::Candidate,
            ResolutionStatus::Unresolved,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let back: ResolutionStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, status);
        }
    }

    #[test]
    fn claim_statuses_round_trip() {
        for status in [
            ClaimStatus::Candidate,
            ClaimStatus::Supported,
            ClaimStatus::Verified,
            ClaimStatus::Stale,
            ClaimStatus::Conflicted,
            ClaimStatus::Superseded,
            ClaimStatus::Retracted,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, format!("\"{}\"", json.trim_matches('"')));
            let back: ClaimStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, status);
        }
    }

    #[test]
    fn job_state_transitions_follow_tis_state_machine() {
        assert!(job_state_transition_is_legal(
            JobState::Queued,
            JobState::Running
        ));
        assert!(job_state_transition_is_legal(
            JobState::Running,
            JobState::Succeeded
        ));
        assert!(job_state_transition_is_legal(
            JobState::Running,
            JobState::Failed
        ));
        assert!(job_state_transition_is_legal(
            JobState::Running,
            JobState::Cancelled
        ));
        assert!(job_state_transition_is_legal(
            JobState::Queued,
            JobState::Cancelled
        ));
        assert!(!job_state_transition_is_legal(
            JobState::Succeeded,
            JobState::Running
        ));
        assert!(!job_state_transition_is_legal(
            JobState::Failed,
            JobState::Queued
        ));
        assert!(JobState::Succeeded.is_terminal());
        assert!(!JobState::Running.is_terminal());
    }
}
