//! `ccc-search` — lexical search and symbol lookup (TIS §14, §16.3).
//!
//! The FTS5 implementation is a derived, rebuildable index (TIS §5.2): the
//! canonical provenance (repository, snapshot, generation) is attached to
//! every hit, and requesting a snapshot other than the active generation
//! fails with `INDEX_STALE` rather than returning mixed revisions.

use ccc_domain::*;
use ccc_storage::{IndexStore, SearchStore, SnapshotStore};
use serde::{Deserialize, Serialize};

/// Search boundary (TIS §14).
#[async_trait::async_trait]
pub trait SearchBackend: Send + Sync {
    async fn search(&self, request: SearchRequest) -> Result<SearchResponse, CccError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchRequest {
    pub repository_id: RepositoryId,
    pub query: String,
    /// Defaults to the active generation's snapshot when omitted.
    pub snapshot_id: Option<SnapshotId>,
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResponse {
    pub repository_id: RepositoryId,
    pub snapshot_id: SnapshotId,
    pub generation_id: GenerationId,
    /// Exact name matches first, then lexical matches.
    pub hits: Vec<SearchHit>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub entity_id: String,
    /// `file` or `symbol`.
    pub kind: String,
    pub path: String,
    pub title: Option<String>,
    /// `exact` when the query equals the symbol name/path, `lexical` otherwise.
    pub match_class: MatchClass,
    pub repository_id: RepositoryId,
    pub snapshot_id: SnapshotId,
    pub generation_id: GenerationId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchClass {
    Exact,
    Lexical,
}

/// FTS5-backed search over paths and symbol names.
///
/// The snapshot store handle is retained for future provenance enrichment;
/// snapshot identity currently flows through the active generation.
pub struct FtsSearchBackend<'a> {
    search_store: &'a dyn SearchStore,
    #[allow(dead_code)]
    snapshot_store: &'a dyn SnapshotStore,
    index_store: &'a dyn IndexStore,
}

impl<'a> FtsSearchBackend<'a> {
    pub fn new(
        search_store: &'a dyn SearchStore,
        snapshot_store: &'a dyn SnapshotStore,
        index_store: &'a dyn IndexStore,
    ) -> Self {
        Self {
            search_store,
            snapshot_store,
            index_store,
        }
    }
}

#[async_trait::async_trait]
impl SearchBackend for FtsSearchBackend<'_> {
    async fn search(&self, request: SearchRequest) -> Result<SearchResponse, CccError> {
        let active = self
            .index_store
            .get_active_generation(request.repository_id)
            .await?
            .ok_or_else(|| CccError::snapshot_not_found("no published index generation"))?;

        // Freshness gate (TIS §13.2): a requested snapshot that is not the
        // active generation must fail loudly, never return mixed revisions.
        if let Some(requested) = request.snapshot_id {
            if requested != active.snapshot_id {
                return Err(CccError::index_stale(format!(
                    "requested snapshot {requested} is not the active generation; \
                     active snapshot is {}",
                    active.snapshot_id
                )));
            }
        }

        let limit = request.limit.max(1);
        let mut hits: Vec<SearchHit> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        let query_trimmed = request.query.trim();
        if query_trimmed.is_empty() {
            return Ok(SearchResponse {
                repository_id: request.repository_id,
                snapshot_id: active.snapshot_id,
                generation_id: active.generation_id,
                hits,
            });
        }

        // Exact matches rank above partial (plan T8 acceptance).
        for entry in self
            .search_store
            .lookup_exact(active.snapshot_id, query_trimmed, limit)
            .await?
        {
            if seen.insert(entry.entity_id.clone()) {
                hits.push(SearchHit {
                    entity_id: entry.entity_id,
                    kind: entry.kind,
                    path: entry.path,
                    title: entry.title,
                    match_class: MatchClass::Exact,
                    repository_id: request.repository_id,
                    snapshot_id: active.snapshot_id,
                    generation_id: active.generation_id,
                });
            }
        }
        for entry in self
            .search_store
            .search(active.snapshot_id, query_trimmed, limit)
            .await?
        {
            if seen.insert(entry.entity_id.clone()) {
                hits.push(SearchHit {
                    entity_id: entry.entity_id,
                    kind: entry.kind,
                    path: entry.path,
                    title: entry.title,
                    match_class: MatchClass::Lexical,
                    repository_id: request.repository_id,
                    snapshot_id: active.snapshot_id,
                    generation_id: active.generation_id,
                });
            }
        }
        hits.truncate(limit as usize);
        Ok(SearchResponse {
            repository_id: request.repository_id,
            snapshot_id: active.snapshot_id,
            generation_id: active.generation_id,
            hits,
        })
    }
}

use std::collections::HashSet;

#[cfg(test)]
mod tests {
    use super::*;
    use ccc_storage::test_support::{
        InMemoryIndexStore, InMemorySearchStore, InMemorySnapshotStore,
    };
    use ccc_storage::SearchEntry;

    async fn seed_snapshot(
        store: &InMemorySnapshotStore,
        repository_id: RepositoryId,
    ) -> SnapshotId {
        let snapshot = SnapshotRecord {
            snapshot_id: SnapshotId::generate(),
            repository_id,
            commit_hash: None,
            branch: Some("main".into()),
            kind: SnapshotKind::WorkingTree,
            is_dirty: false,
            manifest_hash: "h".into(),
            indexer_version: "test".into(),
            configuration_hash: "c".into(),
            status: SnapshotStatus::Published,
            created_at: now_rfc3339(),
        };
        store.insert_snapshot(&snapshot).await.unwrap();
        snapshot.snapshot_id
    }

    async fn seed_generation(
        index: &InMemoryIndexStore,
        repository_id: RepositoryId,
        snapshot_id: SnapshotId,
    ) -> GenerationId {
        let gen = GenerationRecord {
            generation_id: GenerationId::generate(),
            repository_id,
            snapshot_id,
            status: GenerationStatus::Building,
            counters_json: "{}".into(),
            created_at: now_rfc3339(),
            published_at: None,
        };
        index.create_generation(&gen).await.unwrap();
        index.publish_generation(gen.generation_id).await.unwrap();
        gen.generation_id
    }

    #[tokio::test]
    async fn exact_match_ranks_above_lexical_and_carries_provenance() {
        let snapshots = InMemorySnapshotStore::new();
        let search = InMemorySearchStore::new();
        let index = InMemoryIndexStore::new();
        let repo = RepositoryId::generate();
        let snap = seed_snapshot(&snapshots, repo).await;
        let gen = seed_generation(&index, repo, snap).await;

        search
            .replace_snapshot_entries(
                snap,
                &[
                    SearchEntry {
                        term: "config".into(),
                        path: "src/settings.rs".into(),
                        kind: "file".into(),
                        entity_id: "file-1".into(),
                        title: None,
                    },
                    SearchEntry {
                        term: "read_config".into(),
                        path: "src/settings.rs".into(),
                        kind: "symbol".into(),
                        entity_id: "symbol-1".into(),
                        title: Some("crate::settings::read_config".into()),
                    },
                ],
            )
            .await
            .unwrap();

        let backend = FtsSearchBackend::new(&search, &snapshots, &index);
        let response = backend
            .search(SearchRequest {
                repository_id: repo,
                query: "read_config".into(),
                snapshot_id: None,
                limit: 10,
            })
            .await
            .unwrap();

        assert_eq!(response.snapshot_id, snap);
        assert_eq!(response.generation_id, gen);
        assert_eq!(response.hits[0].match_class, MatchClass::Exact);
        assert_eq!(response.hits[0].entity_id, "symbol-1");
        // Every hit carries full provenance.
        assert!(response
            .hits
            .iter()
            .all(|h| h.repository_id == repo && h.snapshot_id == snap && h.generation_id == gen));
    }

    #[tokio::test]
    async fn requesting_inactive_snapshot_is_index_stale() {
        let snapshots = InMemorySnapshotStore::new();
        let search = InMemorySearchStore::new();
        let index = InMemoryIndexStore::new();
        let repo = RepositoryId::generate();
        let active = seed_snapshot(&snapshots, repo).await;
        seed_generation(&index, repo, active).await;
        let older = seed_snapshot(&snapshots, repo).await;

        let backend = FtsSearchBackend::new(&search, &snapshots, &index);
        let err = backend
            .search(SearchRequest {
                repository_id: repo,
                query: "anything".into(),
                snapshot_id: Some(older),
                limit: 10,
            })
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::IndexStale);
    }

    #[tokio::test]
    async fn search_without_index_is_snapshot_not_found() {
        let snapshots = InMemorySnapshotStore::new();
        let search = InMemorySearchStore::new();
        let index = InMemoryIndexStore::new();
        let backend = FtsSearchBackend::new(&search, &snapshots, &index);
        let err = backend
            .search(SearchRequest {
                repository_id: RepositoryId::generate(),
                query: "x".into(),
                snapshot_id: None,
                limit: 10,
            })
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::SnapshotNotFound);
    }

    #[tokio::test]
    async fn excluded_paths_never_enter_results() {
        // The indexer only inserts entries for included files; this test pins
        // the contract on the search side: entries absent from FTS never appear.
        let snapshots = InMemorySnapshotStore::new();
        let search = InMemorySearchStore::new();
        let index = InMemoryIndexStore::new();
        let repo = RepositoryId::generate();
        let snap = seed_snapshot(&snapshots, repo).await;
        seed_generation(&index, repo, snap).await;
        search
            .replace_snapshot_entries(
                snap,
                &[SearchEntry {
                    term: "src/lib.rs".into(),
                    path: "src/lib.rs".into(),
                    kind: "file".into(),
                    entity_id: "file-1".into(),
                    title: None,
                }],
            )
            .await
            .unwrap();

        let backend = FtsSearchBackend::new(&search, &snapshots, &index);
        let response = backend
            .search(SearchRequest {
                repository_id: repo,
                query: "target/debug".into(),
                snapshot_id: None,
                limit: 10,
            })
            .await
            .unwrap();
        assert!(response.hits.is_empty());
    }
}
