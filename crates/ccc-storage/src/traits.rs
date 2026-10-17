//! Persistence interfaces for `ccc`.
//!
//! Store traits are defined here and implemented by the SQLite adapter in
//! this crate. Feature crates (indexer, search, compiler, impact, jobs)
//! program against `dyn` traits — no code outside this crate touches SQL.
//! Derived indexes (FTS5) are rebuildable and never the sole source of
//! provenance (TIS §5.2).

use ccc_domain::*;
use std::path::{Path, PathBuf};

/// Result alias used across store traits.
pub type StoreResult<T> = Result<T, CccError>;

/// Registration and lookup of repositories (TIS §4.1).
#[async_trait::async_trait]
pub trait RepositoryStore: Send + Sync {
    async fn insert_repository(&self, record: &RepositoryRecord) -> StoreResult<()>;
    async fn find_repository_by_path(
        &self,
        root_path: &str,
    ) -> StoreResult<Option<RepositoryRecord>>;
    async fn get_repository(&self, id: RepositoryId) -> StoreResult<Option<RepositoryRecord>>;
    async fn list_repositories(&self) -> StoreResult<Vec<RepositoryRecord>>;
    async fn update_repository_configuration(
        &self,
        id: RepositoryId,
        configuration_hash: &str,
        updated_at: &str,
    ) -> StoreResult<()>;
}

/// Snapshot identity and lifecycle (TIS §4.2).
#[async_trait::async_trait]
pub trait SnapshotStore: Send + Sync {
    /// Inserts a snapshot in `building` status.
    async fn insert_snapshot(&self, record: &SnapshotRecord) -> StoreResult<()>;
    async fn get_snapshot(&self, id: SnapshotId) -> StoreResult<Option<SnapshotRecord>>;
    /// Finds a snapshot with identical identity inputs (repo, kind, dirty,
    /// manifest hash, configuration hash, indexer version).
    async fn find_identical_snapshot(
        &self,
        repository_id: RepositoryId,
        kind: SnapshotKind,
        is_dirty: bool,
        manifest_hash: &str,
        configuration_hash: &str,
        indexer_version: &str,
    ) -> StoreResult<Option<SnapshotRecord>>;
    /// Applies a status transition, rejecting illegal transitions.
    async fn set_snapshot_status(&self, id: SnapshotId, status: SnapshotStatus) -> StoreResult<()>;
    async fn list_snapshots(&self, repository_id: RepositoryId)
        -> StoreResult<Vec<SnapshotRecord>>;
    /// Marks non-terminal snapshots (leftover from interrupted runs) failed.
    async fn fail_interrupted_snapshots(&self, repository_id: RepositoryId) -> StoreResult<u64>;
}

/// Index content: files, symbols, references, edges, generations (TIS §6/§7).
#[async_trait::async_trait]
pub trait IndexStore: Send + Sync {
    async fn insert_files(&self, records: &[FileRecord]) -> StoreResult<()>;
    async fn insert_symbols(&self, records: &[SymbolRecord]) -> StoreResult<()>;
    async fn insert_references(&self, records: &[ReferenceRecord]) -> StoreResult<()>;
    async fn insert_edges(&self, records: &[EdgeRecord]) -> StoreResult<()>;

    async fn list_files(&self, snapshot_id: SnapshotId) -> StoreResult<Vec<FileRecord>>;
    async fn get_file_by_path(
        &self,
        snapshot_id: SnapshotId,
        path: &str,
    ) -> StoreResult<Option<FileRecord>>;
    async fn list_symbols(&self, snapshot_id: SnapshotId) -> StoreResult<Vec<SymbolRecord>>;
    async fn list_symbols_for_file(
        &self,
        snapshot_id: SnapshotId,
        file_id: FileId,
    ) -> StoreResult<Vec<SymbolRecord>>;
    async fn list_edges_outgoing(
        &self,
        snapshot_id: SnapshotId,
        source: SymbolId,
    ) -> StoreResult<Vec<EdgeRecord>>;
    async fn list_edges_incoming(
        &self,
        snapshot_id: SnapshotId,
        target: SymbolId,
    ) -> StoreResult<Vec<EdgeRecord>>;

    /// Creates a generation in `building` status (TIS §4.4 step 2).
    async fn create_generation(&self, record: &GenerationRecord) -> StoreResult<()>;
    async fn get_generation(&self, id: GenerationId) -> StoreResult<Option<GenerationRecord>>;
    async fn get_active_generation(
        &self,
        repository_id: RepositoryId,
    ) -> StoreResult<Option<GenerationRecord>>;
    async fn set_generation_counters(
        &self,
        id: GenerationId,
        counters_json: &str,
    ) -> StoreResult<()>;
    async fn fail_building_generations(&self, repository_id: RepositoryId) -> StoreResult<u64>;
    /// Referential integrity check (TIS §4.4 step 4): every symbol/reference/
    /// edge row must reference files and symbols of the same snapshot. Returns
    /// the number of violations found; publication must not proceed on > 0.
    async fn count_cross_snapshot_violations(&self, snapshot_id: SnapshotId) -> StoreResult<u64>;
    /// Removes all files/symbols/references/edges for a snapshot (used when a
    /// reused snapshot identity is re-indexed).
    async fn delete_snapshot_content(&self, snapshot_id: SnapshotId) -> StoreResult<()>;
    /// Parsed-artifact cache keyed by
    /// `content_hash + language + parser_version + grammar_version + extraction_config_hash`
    /// (TIS §4.3). Returns the stored artifact hash on hit.
    async fn get_cached_parse(&self, cache_key: &str) -> StoreResult<Option<String>>;
    async fn store_cached_parse(&self, cache_key: &str, artifact_hash: &str) -> StoreResult<()>;
    /// Atomically publishes the generation: validates referential integrity,
    /// marks previous published generations superseded, flips the active
    /// pointer. Readers never observe mixed revisions (SDD §7.5).
    async fn publish_generation(&self, id: GenerationId) -> StoreResult<()>;
}

/// Lexical search index maintenance + queries (TIS §16.3). FTS5 is derived,
/// rebuildable data.
#[async_trait::async_trait]
pub trait SearchStore: Send + Sync {
    /// Replaces all FTS rows for a snapshot.
    async fn replace_snapshot_entries(
        &self,
        snapshot_id: SnapshotId,
        entries: &[SearchEntry],
    ) -> StoreResult<()>;
    async fn delete_snapshot_entries(&self, snapshot_id: SnapshotId) -> StoreResult<()>;
    async fn search(
        &self,
        snapshot_id: SnapshotId,
        query: &str,
        limit: u32,
    ) -> StoreResult<Vec<SearchEntry>>;
    /// Exact (case-sensitive) symbol-name lookup supporting the symbol channel.
    async fn lookup_exact(
        &self,
        snapshot_id: SnapshotId,
        name: &str,
        limit: u32,
    ) -> StoreResult<Vec<SearchEntry>>;
}

/// A searchable term bound to its entity and provenance.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SearchEntry {
    /// Searchable text (path, symbol name, or words from it).
    pub term: String,
    /// `file` or `symbol`.
    pub kind: String,
    /// Opaque entity reference (file_id or symbol_id string).
    pub entity_id: String,
    /// Repository-relative path of the underlying file.
    pub path: String,
    /// Optional qualified name for symbols.
    pub title: Option<String>,
}

/// Claims, evidence and their dependencies (TIS §6.3).
#[async_trait::async_trait]
pub trait EvidenceStore: Send + Sync {
    async fn insert_evidence(&self, records: &[EvidenceRecord]) -> StoreResult<()>;
    async fn insert_claims(&self, records: &[ClaimRecord]) -> StoreResult<()>;
    async fn insert_claim_dependencies(&self, links: &[ClaimDependencyRecord]) -> StoreResult<()>;
    /// Dependency links of the given claims (TIS §6.3) — the scope map used
    /// by selective invalidation and contradiction detection.
    async fn list_claim_dependencies(
        &self,
        claim_ids: &[ClaimId],
    ) -> StoreResult<Vec<ClaimDependencyRecord>>;
    async fn get_claim(&self, id: ClaimId) -> StoreResult<Option<ClaimRecord>>;
    async fn get_evidence(&self, id: EvidenceId) -> StoreResult<Option<EvidenceRecord>>;
    async fn list_claims(&self, repository_id: RepositoryId) -> StoreResult<Vec<ClaimRecord>>;
    async fn list_evidence(&self, repository_id: RepositoryId) -> StoreResult<Vec<EvidenceRecord>>;
    /// Claims that depend (transitively via claim_dependencies) on the given
    /// files or symbols. Used by selective invalidation (FR-040, TIS §8.1).
    async fn claims_depending_on(
        &self,
        repository_id: RepositoryId,
        file_ids: &[String],
        symbol_ids: &[String],
    ) -> StoreResult<Vec<ClaimRecord>>;
    /// Flips claims to `stale` unless they are already terminal or stale.
    /// Returns the ids of claims that were updated.
    async fn mark_claims_stale(
        &self,
        claim_ids: &[ClaimId],
        updated_at: &str,
    ) -> StoreResult<Vec<ClaimId>>;
    /// Applies a verification outcome to one claim (SDD §15.6): sets the
    /// status, `validated_at` (when the outcome supports the claim) and
    /// `updated_at`. Returns `false` when the claim does not exist; claims in
    /// terminal states (`superseded`, `retracted`) are never touched.
    async fn update_claim_validation(
        &self,
        claim_id: ClaimId,
        status: ClaimStatus,
        validated_at: Option<&str>,
        updated_at: &str,
    ) -> StoreResult<bool>;
    /// Flips claims to `superseded` (SDD §5.8 versioning). Terminal states
    /// (`superseded`, `retracted`) are never touched; a claim can never
    /// supersede itself. Returns the ids of claims that were updated.
    async fn mark_claims_superseded(
        &self,
        claim_ids: &[ClaimId],
        superseded_by: ClaimId,
        updated_at: &str,
    ) -> StoreResult<Vec<ClaimId>>;
}

/// Persistent engineering memory (SDD §5.11, TIS §12). Memory is a
/// projection over claims: status reuses [`ClaimStatus`], terminal states
/// (`superseded`, `retracted`) are never touched by transitions, and every
/// transition appends an immutable status event (FR-032 history).
#[async_trait::async_trait]
pub trait MemoryStore: Send + Sync {
    /// Inserts a memory with its claim links in one transaction.
    async fn insert_memory(&self, record: &MemoryRecord, claim_ids: &[ClaimId]) -> StoreResult<()>;
    async fn get_memory(&self, id: MemoryId) -> StoreResult<Option<MemoryRecord>>;
    async fn list_memories(&self, repository_id: RepositoryId) -> StoreResult<Vec<MemoryRecord>>;
    async fn list_memories_by_status(
        &self,
        repository_id: RepositoryId,
        statuses: &[ClaimStatus],
    ) -> StoreResult<Vec<MemoryRecord>>;
    /// FTS5 search over title+summary with optional status/type filters
    /// (FR-035). Unknown terms match nothing; user text never reaches FTS5
    /// syntax unquoted.
    async fn search_memories(
        &self,
        repository_id: RepositoryId,
        query: &str,
        status: Option<ClaimStatus>,
        memory_type: Option<MemoryType>,
        limit: u32,
    ) -> StoreResult<Vec<MemoryRecord>>;
    /// Claim links of one memory (the projection's supporting claims).
    async fn claim_ids_for_memory(&self, memory_id: MemoryId) -> StoreResult<Vec<ClaimId>>;
    /// Memories linked to any of the given claims (freshness projection).
    async fn memories_for_claims(&self, claim_ids: &[ClaimId]) -> StoreResult<Vec<MemoryRecord>>;
    /// Applies a status transition with actor+reason provenance (FR-032):
    /// appends a status event and updates the row. Returns `false` when the
    /// memory does not exist; terminal states are never touched.
    async fn update_memory_status(
        &self,
        memory_id: MemoryId,
        status: ClaimStatus,
        actor: &str,
        reason: Option<&str>,
        updated_at: &str,
    ) -> StoreResult<bool>;
    /// Appended transition history of one memory.
    async fn list_memory_status_events(
        &self,
        memory_id: MemoryId,
    ) -> StoreResult<Vec<MemoryStatusEvent>>;
    /// Records access (`last_accessed_at`, SDD §5.11).
    async fn touch_memory(&self, memory_id: MemoryId, accessed_at: &str) -> StoreResult<()>;
}

/// Job records (TIS §15.1).
#[async_trait::async_trait]
pub trait JobStore: Send + Sync {
    /// Inserts a job; if a job with the same (repository, idempotency key)
    /// exists, returns the existing record instead (TIS §13.3).
    async fn insert_job_idempotent(&self, record: &JobRecord) -> StoreResult<JobRecord>;
    async fn get_job(&self, id: JobId) -> StoreResult<Option<JobRecord>>;
    async fn update_job_state(
        &self,
        id: JobId,
        state: JobState,
        updated_at: &str,
        finished_at: Option<&str>,
        error_code: Option<&str>,
        error_message: Option<&str>,
    ) -> StoreResult<()>;
    async fn list_jobs(&self, repository_id: RepositoryId) -> StoreResult<Vec<JobRecord>>;
}

/// Immutable context bundles (TIS §9.7).
#[async_trait::async_trait]
pub trait BundleStore: Send + Sync {
    async fn insert_bundle(&self, record: &BundleRecord, items_json: &str) -> StoreResult<()>;
    async fn get_bundle(&self, id: BundleId) -> StoreResult<Option<BundleRecord>>;
    async fn get_bundle_items(&self, id: BundleId) -> StoreResult<String>;
    async fn list_bundles(&self, repository_id: RepositoryId) -> StoreResult<Vec<BundleRecord>>;
}

/// Change-impact reports.
#[async_trait::async_trait]
pub trait ImpactStore: Send + Sync {
    async fn insert_impact(&self, record: &ImpactRecord) -> StoreResult<()>;
    async fn get_impact(&self, id: ImpactId) -> StoreResult<Option<ImpactRecord>>;
    async fn list_impacts(&self, repository_id: RepositoryId) -> StoreResult<Vec<ImpactRecord>>;
}

/// Immutable verification records and their check rows (TIS §11).
#[async_trait::async_trait]
pub trait VerificationStore: Send + Sync {
    /// Publishes one record with its checks in a single transaction. The
    /// record is immutable once written.
    async fn insert_verification(
        &self,
        record: &VerificationRecord,
        checks: &[VerificationCheckRecord],
    ) -> StoreResult<()>;
    async fn get_verification(&self, id: VerificationId)
        -> StoreResult<Option<VerificationRecord>>;
    async fn list_verifications(
        &self,
        repository_id: RepositoryId,
    ) -> StoreResult<Vec<VerificationRecord>>;
    /// Check rows of a record, ordered by position.
    async fn list_verification_checks(
        &self,
        verification_id: VerificationId,
    ) -> StoreResult<Vec<VerificationCheckRecord>>;
}

/// Evaluation suite registry (hash-keyed, immutable) and run records
/// (SDD §20.2, Phase G).
#[async_trait::async_trait]
pub trait EvaluationStore: Send + Sync {
    /// Records a suite by content hash; re-recording an identical suite is a
    /// no-op (suites are immutable — a changed file is a new hash).
    async fn insert_suite(&self, record: &EvaluationSuiteRecord) -> StoreResult<()>;
    async fn get_suite(&self, suite_hash: &str) -> StoreResult<Option<EvaluationSuiteRecord>>;
    async fn insert_run(&self, record: &EvaluationRunRecord) -> StoreResult<()>;
    async fn get_run(&self, id: EvaluationRunId) -> StoreResult<Option<EvaluationRunRecord>>;
    async fn list_runs(&self, repository_id: RepositoryId)
        -> StoreResult<Vec<EvaluationRunRecord>>;
}

/// Content-addressed artifact metadata row (TIS §5.4).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ArtifactMeta {
    pub content_hash: String,
    pub byte_length: u64,
    pub media_type: String,
    pub retention_state: String,
}

/// Artifact metadata + blob persistence. Blobs live under `blobs/` in the
/// data dir; writes are atomic (tmp + rename).
#[async_trait::async_trait]
pub trait ArtifactStore: Send + Sync {
    /// Stores bytes if not already present; returns the content hash.
    async fn put(&self, bytes: &[u8], media_type: &str) -> StoreResult<String>;
    async fn get(&self, content_hash: &str) -> StoreResult<Option<Vec<u8>>>;
    async fn exists(&self, content_hash: &str) -> StoreResult<bool>;
    async fn get_meta(&self, content_hash: &str) -> StoreResult<Option<ArtifactMeta>>;
}

/// Filesystem blob path for a content hash: `blobs/<aa>/<hash>`.
pub fn blob_path(blobs_dir: &Path, content_hash: &str) -> PathBuf {
    let prefix = &content_hash[..2.min(content_hash.len())];
    blobs_dir.join(prefix).join(content_hash)
}
