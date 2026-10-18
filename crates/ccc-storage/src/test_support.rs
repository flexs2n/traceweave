//! In-memory store implementations for tests in other crates.
//!
//! These live behind the `test-support` feature and are never compiled into
//! production builds. They implement the same trait contracts as the SQLite
//! adapter, including status-transition validation where relevant.

#![allow(clippy::new_without_default)]

use crate::traits::*;
use ccc_domain::*;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Mutex;

/// Shared interior state for all in-memory stores.
#[derive(Default)]
struct SnapshotState {
    snapshots: HashMap<SnapshotId, SnapshotRecord>,
}

/// In-memory [`SnapshotStore`].
pub struct InMemorySnapshotStore(Mutex<SnapshotState>);

impl InMemorySnapshotStore {
    pub fn new() -> Self {
        Self(Mutex::new(SnapshotState::default()))
    }
}

impl Default for InMemorySnapshotStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl SnapshotStore for InMemorySnapshotStore {
    async fn insert_snapshot(&self, record: &SnapshotRecord) -> StoreResult<()> {
        self.0
            .lock()
            .unwrap()
            .snapshots
            .insert(record.snapshot_id, record.clone());
        Ok(())
    }

    async fn get_snapshot(&self, id: SnapshotId) -> StoreResult<Option<SnapshotRecord>> {
        Ok(self.0.lock().unwrap().snapshots.get(&id).cloned())
    }

    async fn find_identical_snapshot(
        &self,
        repository_id: RepositoryId,
        kind: SnapshotKind,
        is_dirty: bool,
        manifest_hash: &str,
        configuration_hash: &str,
        indexer_version: &str,
    ) -> StoreResult<Option<SnapshotRecord>> {
        let state = self.0.lock().unwrap();
        Ok(state
            .snapshots
            .values()
            .find(|s| {
                s.repository_id == repository_id
                    && s.kind == kind
                    && s.is_dirty == is_dirty
                    && s.manifest_hash == manifest_hash
                    && s.configuration_hash == configuration_hash
                    && s.indexer_version == indexer_version
            })
            .cloned())
    }

    async fn set_snapshot_status(&self, id: SnapshotId, status: SnapshotStatus) -> StoreResult<()> {
        let mut state = self.0.lock().unwrap();
        if let Some(record) = state.snapshots.get_mut(&id) {
            record.status = status;
            Ok(())
        } else {
            Err(CccError::snapshot_not_found("snapshot not found"))
        }
    }

    async fn list_snapshots(
        &self,
        repository_id: RepositoryId,
    ) -> StoreResult<Vec<SnapshotRecord>> {
        let state = self.0.lock().unwrap();
        Ok(state
            .snapshots
            .values()
            .filter(|s| s.repository_id == repository_id)
            .cloned()
            .collect())
    }

    async fn fail_interrupted_snapshots(&self, repository_id: RepositoryId) -> StoreResult<u64> {
        let mut state = self.0.lock().unwrap();
        let mut count = 0;
        for record in state.snapshots.values_mut() {
            if record.repository_id == repository_id && record.status == SnapshotStatus::Building {
                record.status = SnapshotStatus::Failed;
                count += 1;
            }
        }
        Ok(count)
    }
}

/// In-memory [`RepositoryStore`].
#[derive(Default)]
pub struct InMemoryRepositoryStore(Mutex<HashMap<RepositoryId, RepositoryRecord>>);

impl InMemoryRepositoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl RepositoryStore for InMemoryRepositoryStore {
    async fn insert_repository(&self, record: &RepositoryRecord) -> StoreResult<()> {
        self.0
            .lock()
            .unwrap()
            .insert(record.repository_id, record.clone());
        Ok(())
    }

    async fn find_repository_by_path(
        &self,
        root_path: &str,
    ) -> StoreResult<Option<RepositoryRecord>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .values()
            .find(|r| r.root_path == root_path)
            .cloned())
    }

    async fn get_repository(&self, id: RepositoryId) -> StoreResult<Option<RepositoryRecord>> {
        Ok(self.0.lock().unwrap().get(&id).cloned())
    }

    async fn list_repositories(&self) -> StoreResult<Vec<RepositoryRecord>> {
        Ok(self.0.lock().unwrap().values().cloned().collect())
    }

    async fn update_repository_configuration(
        &self,
        id: RepositoryId,
        configuration_hash: &str,
        updated_at: &str,
    ) -> StoreResult<()> {
        if let Some(record) = self.0.lock().unwrap().get_mut(&id) {
            record.configuration_hash = configuration_hash.to_string();
            record.updated_at = updated_at.to_string();
        }
        Ok(())
    }
}

/// In-memory [`IndexStore`].
#[derive(Default)]
pub struct InMemoryIndexStore {
    inner: Mutex<IndexInner>,
}

#[derive(Default)]
struct IndexInner {
    files: Vec<FileRecord>,
    symbols: Vec<SymbolRecord>,
    references: Vec<ReferenceRecord>,
    edges: Vec<EdgeRecord>,
    generations: HashMap<GenerationId, GenerationRecord>,
    active: HashMap<RepositoryId, GenerationId>,
}

impl InMemoryIndexStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl IndexStore for InMemoryIndexStore {
    async fn insert_files(&self, records: &[FileRecord]) -> StoreResult<()> {
        self.inner
            .lock()
            .unwrap()
            .files
            .extend(records.iter().cloned());
        Ok(())
    }
    async fn insert_symbols(&self, records: &[SymbolRecord]) -> StoreResult<()> {
        self.inner
            .lock()
            .unwrap()
            .symbols
            .extend(records.iter().cloned());
        Ok(())
    }
    async fn insert_references(&self, records: &[ReferenceRecord]) -> StoreResult<()> {
        self.inner
            .lock()
            .unwrap()
            .references
            .extend(records.iter().cloned());
        Ok(())
    }
    async fn insert_edges(&self, records: &[EdgeRecord]) -> StoreResult<()> {
        self.inner
            .lock()
            .unwrap()
            .edges
            .extend(records.iter().cloned());
        Ok(())
    }
    async fn list_files(&self, snapshot_id: SnapshotId) -> StoreResult<Vec<FileRecord>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .files
            .iter()
            .filter(|f| f.snapshot_id == snapshot_id)
            .cloned()
            .collect())
    }
    async fn get_file_by_path(
        &self,
        snapshot_id: SnapshotId,
        path: &str,
    ) -> StoreResult<Option<FileRecord>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .files
            .iter()
            .find(|f| f.snapshot_id == snapshot_id && f.path == path)
            .cloned())
    }
    async fn list_symbols(&self, snapshot_id: SnapshotId) -> StoreResult<Vec<SymbolRecord>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .symbols
            .iter()
            .filter(|s| s.snapshot_id == snapshot_id)
            .cloned()
            .collect())
    }
    async fn list_symbols_for_file(
        &self,
        snapshot_id: SnapshotId,
        file_id: FileId,
    ) -> StoreResult<Vec<SymbolRecord>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .symbols
            .iter()
            .filter(|s| s.snapshot_id == snapshot_id && s.file_id == file_id)
            .cloned()
            .collect())
    }
    async fn list_edges_outgoing(
        &self,
        snapshot_id: SnapshotId,
        source: SymbolId,
    ) -> StoreResult<Vec<EdgeRecord>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .edges
            .iter()
            .filter(|e| e.snapshot_id == snapshot_id && e.source_symbol_id == source)
            .cloned()
            .collect())
    }
    async fn list_edges_incoming(
        &self,
        snapshot_id: SnapshotId,
        target: SymbolId,
    ) -> StoreResult<Vec<EdgeRecord>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .edges
            .iter()
            .filter(|e| e.snapshot_id == snapshot_id && e.target_symbol_id == Some(target))
            .cloned()
            .collect())
    }
    async fn delete_snapshot_content(&self, snapshot_id: SnapshotId) -> StoreResult<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.files.retain(|f| f.snapshot_id != snapshot_id);
        inner.symbols.retain(|s| s.snapshot_id != snapshot_id);
        inner.references.retain(|r| r.snapshot_id != snapshot_id);
        inner.edges.retain(|e| e.snapshot_id != snapshot_id);
        Ok(())
    }
    async fn get_cached_parse(&self, _cache_key: &str) -> StoreResult<Option<String>> {
        Ok(None)
    }
    async fn store_cached_parse(&self, _cache_key: &str, _artifact_hash: &str) -> StoreResult<()> {
        Ok(())
    }
    async fn create_generation(&self, record: &GenerationRecord) -> StoreResult<()> {
        self.inner
            .lock()
            .unwrap()
            .generations
            .insert(record.generation_id, record.clone());
        Ok(())
    }
    async fn get_generation(&self, id: GenerationId) -> StoreResult<Option<GenerationRecord>> {
        Ok(self.inner.lock().unwrap().generations.get(&id).cloned())
    }
    async fn get_active_generation(
        &self,
        repository_id: RepositoryId,
    ) -> StoreResult<Option<GenerationRecord>> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .active
            .get(&repository_id)
            .and_then(|id| inner.generations.get(id))
            .cloned())
    }
    async fn set_generation_counters(
        &self,
        id: GenerationId,
        counters_json: &str,
    ) -> StoreResult<()> {
        if let Some(gen) = self.inner.lock().unwrap().generations.get_mut(&id) {
            gen.counters_json = counters_json.to_string();
        }
        Ok(())
    }
    async fn fail_building_generations(&self, repository_id: RepositoryId) -> StoreResult<u64> {
        let mut inner = self.inner.lock().unwrap();
        let mut count = 0;
        for gen in inner.generations.values_mut() {
            if gen.repository_id == repository_id && gen.status == GenerationStatus::Building {
                gen.status = GenerationStatus::Failed;
                count += 1;
            }
        }
        Ok(count)
    }
    async fn count_cross_snapshot_violations(&self, snapshot_id: SnapshotId) -> StoreResult<u64> {
        let inner = self.inner.lock().unwrap();
        let files: HashMap<String, SnapshotId> = inner
            .files
            .iter()
            .map(|f| (f.file_id.to_string(), f.snapshot_id))
            .collect();
        let symbols: HashMap<String, SnapshotId> = inner
            .symbols
            .iter()
            .map(|s| (s.symbol_id.to_string(), s.snapshot_id))
            .collect();
        let mut violations = 0u64;
        for s in &inner.symbols {
            if s.snapshot_id != snapshot_id {
                continue;
            }
            if files.get(&s.file_id.to_string()) != Some(&snapshot_id) {
                violations += 1;
            }
        }
        for e in &inner.edges {
            if e.snapshot_id != snapshot_id {
                continue;
            }
            if symbols.get(&e.source_symbol_id.to_string()) != Some(&snapshot_id) {
                violations += 1;
            }
            if let Some(t) = e.target_symbol_id {
                if symbols.get(&t.to_string()) != Some(&snapshot_id) {
                    violations += 1;
                }
            }
        }
        Ok(violations)
    }
    async fn publish_generation(&self, id: GenerationId) -> StoreResult<()> {
        let (snapshot_id, repository_id) = {
            let inner = self.inner.lock().unwrap();
            let gen = inner
                .generations
                .get(&id)
                .ok_or_else(|| CccError::snapshot_not_found("generation not found"))?
                .clone();
            (gen.snapshot_id, gen.repository_id)
        };
        // referential check
        let violations = self.count_cross_snapshot_violations(snapshot_id).await?;
        if violations > 0 {
            return Err(CccError::internal_error(format!(
                "refusing to publish: {violations} cross-snapshot violations"
            )));
        }
        let mut inner = self.inner.lock().unwrap();
        for other in inner.generations.values_mut() {
            if other.repository_id == repository_id && other.status == GenerationStatus::Published {
                other.status = GenerationStatus::Superseded;
            }
        }
        let gen_now = inner
            .generations
            .get_mut(&id)
            .ok_or_else(|| CccError::snapshot_not_found("generation not found"))?;
        gen_now.status = GenerationStatus::Published;
        gen_now.published_at = Some(now_rfc3339());
        inner.active.insert(repository_id, id);
        Ok(())
    }
}

/// In-memory [`SearchStore`].
#[derive(Default)]
pub struct InMemorySearchStore(Mutex<BTreeMap<SnapshotId, Vec<SearchEntry>>>);

impl InMemorySearchStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl SearchStore for InMemorySearchStore {
    async fn replace_snapshot_entries(
        &self,
        snapshot_id: SnapshotId,
        entries: &[SearchEntry],
    ) -> StoreResult<()> {
        self.0.lock().unwrap().insert(snapshot_id, entries.to_vec());
        Ok(())
    }
    async fn delete_snapshot_entries(&self, snapshot_id: SnapshotId) -> StoreResult<()> {
        self.0.lock().unwrap().remove(&snapshot_id);
        Ok(())
    }
    async fn search(
        &self,
        snapshot_id: SnapshotId,
        query: &str,
        limit: u32,
    ) -> StoreResult<Vec<SearchEntry>> {
        let map = self.0.lock().unwrap();
        let terms: Vec<String> = query
            .to_ascii_lowercase()
            .split(|c: char| !c.is_alphanumeric() && c != '_' && c != ':')
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect();
        let mut hits: Vec<SearchEntry> = map
            .get(&snapshot_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|e| {
                let lower = e.term.to_ascii_lowercase();
                terms.iter().any(|t| lower.contains(t))
            })
            .collect();
        hits.truncate(limit as usize);
        Ok(hits)
    }
    async fn lookup_exact(
        &self,
        snapshot_id: SnapshotId,
        name: &str,
        limit: u32,
    ) -> StoreResult<Vec<SearchEntry>> {
        let map = self.0.lock().unwrap();
        let hits: Vec<SearchEntry> = map
            .get(&snapshot_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|e| e.term == name || e.title.as_deref() == Some(name))
            .take(limit as usize)
            .collect();
        Ok(hits)
    }
}

/// In-memory [`EvidenceStore`].
#[derive(Default)]
pub struct InMemoryEvidenceStore {
    inner: Mutex<EvidenceInner>,
}

#[derive(Default)]
struct EvidenceInner {
    evidence: Vec<EvidenceRecord>,
    claims: Vec<ClaimRecord>,
    dependencies: Vec<ClaimDependencyRecord>,
}

impl InMemoryEvidenceStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl EvidenceStore for InMemoryEvidenceStore {
    async fn insert_evidence(&self, records: &[EvidenceRecord]) -> StoreResult<()> {
        self.inner
            .lock()
            .unwrap()
            .evidence
            .extend(records.iter().cloned());
        Ok(())
    }
    async fn insert_claims(&self, records: &[ClaimRecord]) -> StoreResult<()> {
        self.inner
            .lock()
            .unwrap()
            .claims
            .extend(records.iter().cloned());
        Ok(())
    }
    async fn insert_claim_dependencies(&self, links: &[ClaimDependencyRecord]) -> StoreResult<()> {
        self.inner
            .lock()
            .unwrap()
            .dependencies
            .extend(links.iter().cloned());
        Ok(())
    }
    async fn list_claim_dependencies(
        &self,
        claim_ids: &[ClaimId],
    ) -> StoreResult<Vec<ClaimDependencyRecord>> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .dependencies
            .iter()
            .filter(|d| claim_ids.contains(&d.claim_id))
            .cloned()
            .collect())
    }
    async fn get_claim(&self, id: ClaimId) -> StoreResult<Option<ClaimRecord>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .claims
            .iter()
            .find(|c| c.claim_id == id)
            .cloned())
    }
    async fn get_evidence(&self, id: EvidenceId) -> StoreResult<Option<EvidenceRecord>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .evidence
            .iter()
            .find(|e| e.evidence_id == id)
            .cloned())
    }
    async fn list_claims(&self, repository_id: RepositoryId) -> StoreResult<Vec<ClaimRecord>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .claims
            .iter()
            .filter(|c| c.repository_id == repository_id)
            .cloned()
            .collect())
    }
    async fn list_evidence(&self, repository_id: RepositoryId) -> StoreResult<Vec<EvidenceRecord>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .evidence
            .iter()
            .filter(|e| e.repository_id == repository_id)
            .cloned()
            .collect())
    }
    async fn claims_depending_on(
        &self,
        repository_id: RepositoryId,
        file_ids: &[String],
        symbol_ids: &[String],
    ) -> StoreResult<Vec<ClaimRecord>> {
        let inner = self.inner.lock().unwrap();
        let matched: BTreeSet<String> = inner
            .dependencies
            .iter()
            .filter(|d| {
                (d.depends_on_kind == "file" && file_ids.contains(&d.depends_on))
                    || (d.depends_on_kind == "symbol" && symbol_ids.contains(&d.depends_on))
            })
            .map(|d| d.claim_id.to_string())
            .collect();
        Ok(inner
            .claims
            .iter()
            .filter(|c| {
                c.repository_id == repository_id && matched.contains(&c.claim_id.to_string())
            })
            .cloned()
            .collect())
    }
    async fn mark_claims_stale(
        &self,
        claim_ids: &[ClaimId],
        updated_at: &str,
    ) -> StoreResult<Vec<ClaimId>> {
        let mut inner = self.inner.lock().unwrap();
        let mut updated = Vec::new();
        for claim in &mut inner.claims {
            if claim_ids.contains(&claim.claim_id)
                && matches!(
                    claim.status,
                    ClaimStatus::Candidate | ClaimStatus::Supported | ClaimStatus::Verified
                )
            {
                claim.status = ClaimStatus::Stale;
                claim.updated_at = updated_at.to_string();
                updated.push(claim.claim_id);
            }
        }
        Ok(updated)
    }

    async fn mark_claims_superseded(
        &self,
        claim_ids: &[ClaimId],
        superseded_by: ClaimId,
        updated_at: &str,
    ) -> StoreResult<Vec<ClaimId>> {
        let mut inner = self.inner.lock().unwrap();
        let mut updated = Vec::new();
        for claim in &mut inner.claims {
            if claim_ids.contains(&claim.claim_id)
                && claim.claim_id != superseded_by
                && !matches!(
                    claim.status,
                    ClaimStatus::Superseded | ClaimStatus::Retracted
                )
            {
                claim.status = ClaimStatus::Superseded;
                claim.updated_at = updated_at.to_string();
                updated.push(claim.claim_id);
            }
        }
        Ok(updated)
    }

    async fn update_claim_validation(
        &self,
        claim_id: ClaimId,
        status: ClaimStatus,
        validated_at: Option<&str>,
        updated_at: &str,
    ) -> StoreResult<bool> {
        let mut inner = self.inner.lock().unwrap();
        match inner.claims.iter_mut().find(|c| c.claim_id == claim_id) {
            Some(claim)
                if !matches!(
                    claim.status,
                    ClaimStatus::Superseded | ClaimStatus::Retracted
                ) =>
            {
                claim.status = status;
                if let Some(at) = validated_at {
                    claim.validated_at = Some(at.to_string());
                }
                claim.updated_at = updated_at.to_string();
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

/// In-memory [`MemoryStore`] (same transition semantics as the SQLite
/// adapter: terminal states are never touched, events append immutably).
#[derive(Default)]
pub struct InMemoryMemoryStore {
    inner: Mutex<MemoryInner>,
}

#[derive(Default)]
struct MemoryInner {
    memories: Vec<MemoryRecord>,
    claim_links: Vec<(MemoryId, ClaimId)>,
    events: Vec<MemoryStatusEvent>,
}

impl InMemoryMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl MemoryStore for InMemoryMemoryStore {
    async fn insert_memory(&self, record: &MemoryRecord, claim_ids: &[ClaimId]) -> StoreResult<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.memories.push(record.clone());
        for claim_id in claim_ids {
            inner.claim_links.push((record.memory_id, *claim_id));
        }
        Ok(())
    }

    async fn get_memory(&self, id: MemoryId) -> StoreResult<Option<MemoryRecord>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .memories
            .iter()
            .find(|m| m.memory_id == id)
            .cloned())
    }

    async fn list_memories(&self, repository_id: RepositoryId) -> StoreResult<Vec<MemoryRecord>> {
        let inner = self.inner.lock().unwrap();
        let mut memories: Vec<MemoryRecord> = inner
            .memories
            .iter()
            .filter(|m| m.repository_id == repository_id)
            .cloned()
            .collect();
        memories.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(memories)
    }

    async fn list_memories_by_status(
        &self,
        repository_id: RepositoryId,
        statuses: &[ClaimStatus],
    ) -> StoreResult<Vec<MemoryRecord>> {
        let all = self.list_memories(repository_id).await?;
        Ok(all
            .into_iter()
            .filter(|m| statuses.contains(&m.status))
            .collect())
    }

    async fn search_memories(
        &self,
        repository_id: RepositoryId,
        query: &str,
        status: Option<ClaimStatus>,
        memory_type: Option<MemoryType>,
        limit: u32,
    ) -> StoreResult<Vec<MemoryRecord>> {
        let terms: Vec<String> = query
            .to_ascii_lowercase()
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|t| t.len() >= 2)
            .map(str::to_string)
            .collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let inner = self.inner.lock().unwrap();
        let mut hits: Vec<MemoryRecord> = inner
            .memories
            .iter()
            .filter(|m| m.repository_id == repository_id)
            // FR-036: retraction removes a record from active retrieval —
            // queryable only when explicitly requested by status.
            .filter(|m| match status {
                Some(_) => true,
                None => m.status != ClaimStatus::Retracted,
            })
            .filter(|m| {
                let haystack = format!("{} {}", m.title, m.summary).to_ascii_lowercase();
                terms.iter().any(|t| haystack.contains(t.as_str()))
            })
            .filter(|m| status.is_none_or(|s| m.status == s))
            .filter(|m| memory_type.is_none_or(|t| m.memory_type == t))
            .cloned()
            .collect();
        hits.reverse(); // newest first, mirroring the SQLite ORDER BY created_at DESC
        hits.truncate(limit as usize);
        Ok(hits)
    }

    async fn claim_ids_for_memory(&self, memory_id: MemoryId) -> StoreResult<Vec<ClaimId>> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .claim_links
            .iter()
            .filter(|(m, _)| *m == memory_id)
            .map(|(_, c)| *c)
            .collect())
    }

    async fn memories_for_claims(&self, claim_ids: &[ClaimId]) -> StoreResult<Vec<MemoryRecord>> {
        let inner = self.inner.lock().unwrap();
        let linked: Vec<MemoryId> = inner
            .claim_links
            .iter()
            .filter(|(_, c)| claim_ids.contains(c))
            .map(|(m, _)| *m)
            .collect();
        let mut memories: Vec<MemoryRecord> = inner
            .memories
            .iter()
            .filter(|m| linked.contains(&m.memory_id))
            .cloned()
            .collect();
        memories.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(memories)
    }

    async fn update_memory_status(
        &self,
        memory_id: MemoryId,
        status: ClaimStatus,
        actor: &str,
        reason: Option<&str>,
        updated_at: &str,
    ) -> StoreResult<bool> {
        let mut inner = self.inner.lock().unwrap();
        let Some(record) = inner.memories.iter_mut().find(|m| m.memory_id == memory_id) else {
            return Ok(false);
        };
        if matches!(
            record.status,
            ClaimStatus::Superseded | ClaimStatus::Retracted
        ) {
            return Ok(false);
        }
        record.status = status;
        record.stale_cause = match status {
            ClaimStatus::Stale | ClaimStatus::Conflicted => reason.map(str::to_string),
            _ => None,
        };
        record.updated_at = updated_at.to_string();
        inner.events.push(MemoryStatusEvent {
            event_id: MemoryEventId::generate(),
            memory_id,
            status,
            actor: actor.to_string(),
            reason: reason.map(str::to_string),
            created_at: updated_at.to_string(),
        });
        Ok(true)
    }

    async fn list_memory_status_events(
        &self,
        memory_id: MemoryId,
    ) -> StoreResult<Vec<MemoryStatusEvent>> {
        let inner = self.inner.lock().unwrap();
        let mut events: Vec<MemoryStatusEvent> = inner
            .events
            .iter()
            .filter(|e| e.memory_id == memory_id)
            .cloned()
            .collect();
        events.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(events)
    }

    async fn touch_memory(&self, memory_id: MemoryId, accessed_at: &str) -> StoreResult<()> {
        if let Some(record) = self
            .inner
            .lock()
            .unwrap()
            .memories
            .iter_mut()
            .find(|m| m.memory_id == memory_id)
        {
            record.last_accessed_at = accessed_at.to_string();
        }
        Ok(())
    }
}

/// In-memory [`JobStore`].
#[derive(Default)]
pub struct InMemoryJobStore(Mutex<Vec<JobRecord>>);

impl InMemoryJobStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl JobStore for InMemoryJobStore {
    async fn insert_job_idempotent(&self, record: &JobRecord) -> StoreResult<JobRecord> {
        let mut jobs = self.0.lock().unwrap();
        if let Some(existing) = jobs.iter().find(|j| {
            j.repository_id == record.repository_id && j.idempotency_key == record.idempotency_key
        }) {
            return Ok(existing.clone());
        }
        jobs.push(record.clone());
        Ok(record.clone())
    }
    async fn get_job(&self, id: JobId) -> StoreResult<Option<JobRecord>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .iter()
            .find(|j| j.job_id == id)
            .cloned())
    }
    async fn update_job_state(
        &self,
        id: JobId,
        state: JobState,
        updated_at: &str,
        finished_at: Option<&str>,
        error_code: Option<&str>,
        error_message: Option<&str>,
    ) -> StoreResult<()> {
        if let Some(job) = self.0.lock().unwrap().iter_mut().find(|j| j.job_id == id) {
            job.state = state;
            job.updated_at = updated_at.to_string();
            job.finished_at = finished_at.map(str::to_string);
            job.error_code = error_code.map(str::to_string);
            job.error_message = error_message.map(str::to_string);
        }
        Ok(())
    }
    async fn list_jobs(&self, repository_id: RepositoryId) -> StoreResult<Vec<JobRecord>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|j| j.repository_id == repository_id)
            .cloned()
            .collect())
    }
    async fn set_job_payload(&self, id: JobId, payload_json: &str) -> StoreResult<()> {
        if let Some(job) = self.0.lock().unwrap().iter_mut().find(|j| j.job_id == id) {
            job.payload_json = Some(payload_json.to_string());
        }
        Ok(())
    }
}

/// In-memory [`BundleStore`].
#[derive(Default)]
pub struct InMemoryBundleStore {
    inner: Mutex<BundlesInner>,
}

#[derive(Default)]
struct BundlesInner {
    bundles: Vec<BundleRecord>,
    items: HashMap<BundleId, String>,
}

impl InMemoryBundleStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl BundleStore for InMemoryBundleStore {
    async fn insert_bundle(&self, record: &BundleRecord, items_json: &str) -> StoreResult<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.bundles.push(record.clone());
        inner.items.insert(record.bundle_id, items_json.to_string());
        Ok(())
    }
    async fn get_bundle(&self, id: BundleId) -> StoreResult<Option<BundleRecord>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .bundles
            .iter()
            .find(|b| b.bundle_id == id)
            .cloned())
    }
    async fn get_bundle_items(&self, id: BundleId) -> StoreResult<String> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .items
            .get(&id)
            .cloned()
            .unwrap_or_else(|| "[]".to_string()))
    }
    async fn list_bundles(&self, repository_id: RepositoryId) -> StoreResult<Vec<BundleRecord>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .bundles
            .iter()
            .filter(|b| b.repository_id == repository_id)
            .cloned()
            .collect())
    }
}

/// In-memory [`ImpactStore`].
#[derive(Default)]
pub struct InMemoryImpactStore(Mutex<Vec<ImpactRecord>>);

impl InMemoryImpactStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl ImpactStore for InMemoryImpactStore {
    async fn insert_impact(&self, record: &ImpactRecord) -> StoreResult<()> {
        self.0.lock().unwrap().push(record.clone());
        Ok(())
    }
    async fn get_impact(&self, id: ImpactId) -> StoreResult<Option<ImpactRecord>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.impact_id == id)
            .cloned())
    }
    async fn list_impacts(&self, repository_id: RepositoryId) -> StoreResult<Vec<ImpactRecord>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.repository_id == repository_id)
            .cloned()
            .collect())
    }
}

/// In-memory [`VerificationStore`].
#[derive(Default)]
pub struct InMemoryVerificationStore {
    inner: Mutex<VerificationInner>,
}

#[derive(Default)]
struct VerificationInner {
    records: Vec<VerificationRecord>,
    checks: Vec<VerificationCheckRecord>,
}

impl InMemoryVerificationStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl VerificationStore for InMemoryVerificationStore {
    async fn insert_verification(
        &self,
        record: &VerificationRecord,
        checks: &[VerificationCheckRecord],
    ) -> StoreResult<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.records.push(record.clone());
        inner.checks.extend(checks.iter().cloned());
        Ok(())
    }

    async fn get_verification(
        &self,
        id: VerificationId,
    ) -> StoreResult<Option<VerificationRecord>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .records
            .iter()
            .find(|r| r.verification_id == id)
            .cloned())
    }

    async fn list_verifications(
        &self,
        repository_id: RepositoryId,
    ) -> StoreResult<Vec<VerificationRecord>> {
        let inner = self.inner.lock().unwrap();
        let mut records: Vec<VerificationRecord> = inner
            .records
            .iter()
            .filter(|r| r.repository_id == repository_id)
            .cloned()
            .collect();
        records.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(records)
    }

    async fn list_verification_checks(
        &self,
        verification_id: VerificationId,
    ) -> StoreResult<Vec<VerificationCheckRecord>> {
        let inner = self.inner.lock().unwrap();
        let mut checks: Vec<VerificationCheckRecord> = inner
            .checks
            .iter()
            .filter(|c| c.verification_id == verification_id)
            .cloned()
            .collect();
        checks.sort_by_key(|c| c.position);
        Ok(checks)
    }
}

/// In-memory [`EvaluationStore`].
#[derive(Default)]
pub struct InMemoryEvaluationStore {
    inner: Mutex<EvaluationInner>,
}

#[derive(Default)]
struct EvaluationInner {
    suites: Vec<EvaluationSuiteRecord>,
    runs: Vec<EvaluationRunRecord>,
}

impl InMemoryEvaluationStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl EvaluationStore for InMemoryEvaluationStore {
    async fn insert_suite(&self, record: &EvaluationSuiteRecord) -> StoreResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if !inner
            .suites
            .iter()
            .any(|s| s.suite_hash == record.suite_hash)
        {
            inner.suites.push(record.clone());
        }
        Ok(())
    }

    async fn get_suite(&self, suite_hash: &str) -> StoreResult<Option<EvaluationSuiteRecord>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .suites
            .iter()
            .find(|s| s.suite_hash == suite_hash)
            .cloned())
    }

    async fn insert_run(&self, record: &EvaluationRunRecord) -> StoreResult<()> {
        self.inner.lock().unwrap().runs.push(record.clone());
        Ok(())
    }

    async fn get_run(&self, id: EvaluationRunId) -> StoreResult<Option<EvaluationRunRecord>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .runs
            .iter()
            .find(|r| r.run_id == id)
            .cloned())
    }

    async fn list_runs(
        &self,
        repository_id: RepositoryId,
    ) -> StoreResult<Vec<EvaluationRunRecord>> {
        let inner = self.inner.lock().unwrap();
        let mut runs: Vec<EvaluationRunRecord> = inner
            .runs
            .iter()
            .filter(|r| r.repository_id == repository_id)
            .cloned()
            .collect();
        runs.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(runs)
    }
}

/// In-memory [`ArtifactStore`] (kept in process memory).
#[derive(Default)]
pub struct InMemoryArtifactStore(Mutex<HashMap<String, (Vec<u8>, ArtifactMeta)>>);
impl InMemoryArtifactStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl ArtifactStore for InMemoryArtifactStore {
    async fn put(&self, bytes: &[u8], media_type: &str) -> StoreResult<String> {
        let hash = blake3_hex(bytes);
        let meta = ArtifactMeta {
            content_hash: hash.clone(),
            byte_length: bytes.len() as u64,
            media_type: media_type.to_string(),
            retention_state: "retained".to_string(),
        };
        self.0
            .lock()
            .unwrap()
            .insert(hash.clone(), (bytes.to_vec(), meta));
        Ok(hash)
    }
    async fn get(&self, content_hash: &str) -> StoreResult<Option<Vec<u8>>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .get(content_hash)
            .map(|(bytes, _)| bytes.clone()))
    }
    async fn exists(&self, content_hash: &str) -> StoreResult<bool> {
        Ok(self.0.lock().unwrap().contains_key(content_hash))
    }
    async fn get_meta(&self, content_hash: &str) -> StoreResult<Option<ArtifactMeta>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .get(content_hash)
            .map(|(_, meta)| meta.clone()))
    }
}
