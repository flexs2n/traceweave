//! `ccc-context-compiler` — task → token-budgeted context bundle
//! (TIS §9, SDD §11).
//!
//! Deterministic pipeline: normalize the task (heuristic normalizer; the LLM
//! adapter is a later opt-in extension point, NFR-012) → generate candidates
//! from lexical, symbol, dependency-graph and test channels → rank under the
//! versioned `policy-v1` weights → greedy marginal-utility selection within
//! the budget → emit an immutable bundle with provenance, dependency paths,
//! explicit omissions and limitations.
//!
//! Token counting is `chars/4` and is labeled `approximate` everywhere
//! (TIS §9.5). The bundle hash covers a canonical, float-free projection of
//! the document; `bundle_id` and `created_at` are excluded from it.

use ccc_domain::*;
use ccc_schema::{
    render_bundle_markdown, BundleDto, BundleHashInput, BundleItem, BundleOmission,
    DependencyPathEdge, ItemProvenance, SpanDto, TaskDto, SCHEMA_VERSION,
};
use ccc_search::{SearchBackend, SearchRequest};
use ccc_storage::{ArtifactStore, BundleStore, EvidenceStore, IndexStore, StoreResult};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Versioned retrieval policy (TIS §9.3). Weights are integer millis; they
/// are placeholders calibrated later through evaluation, never tuned to
/// manufacture benchmark results.
pub const RETRIEVAL_POLICY_VERSION: &str = "policy-v1";

pub fn policy_weights_millis() -> BTreeMap<String, i64> {
    BTreeMap::from([
        ("w_l".into(), 1000), // lexical relevance
        ("w_d".into(), 900),  // dependency proximity
        ("w_t".into(), 600),  // task/test relevance
        ("w_h".into(), 0),    // change history (no channel in step 1)
        ("w_f".into(), 400),  // freshness
        ("w_r".into(), 300),  // source reliability
        ("w_x".into(), 500),  // redundancy + token cost penalty
    ])
}

/// Task types the heuristic normalizer can recognize (SDD §11.1: uncertain
/// fields stay explicitly unknown).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    Bugfix,
    Feature,
    Refactor,
    Test,
    Documentation,
    Exploration,
    Unknown,
}

impl TaskType {
    fn as_str(&self) -> &'static str {
        match self {
            TaskType::Bugfix => "bugfix",
            TaskType::Feature => "feature",
            TaskType::Refactor => "refactor",
            TaskType::Test => "test",
            TaskType::Documentation => "documentation",
            TaskType::Exploration => "exploration",
            TaskType::Unknown => "unknown",
        }
    }
}

/// Task extension point (NFR-012): the default is the deterministic
/// heuristic; a cloud LLM adapter may implement this trait behind explicit
/// opt-in in a later step.
pub trait TaskNormalizer: Send + Sync {
    fn normalize(&self, task_text: &str, budget_tokens: u64) -> TaskDto;
}

/// Deterministic keyword normalizer. Original text preserved verbatim;
/// unknown fields stay `None` — acceptance criteria are never invented
/// (SDD §11.1).
pub struct HeuristicNormalizer;

impl TaskNormalizer for HeuristicNormalizer {
    fn normalize(&self, task_text: &str, _budget_tokens: u64) -> TaskDto {
        let lower = task_text.to_ascii_lowercase();
        let task_type = if [
            "fix",
            "bug",
            "error",
            "crash",
            "fails",
            "broken",
            "regression",
        ]
        .iter()
        .any(|k| lower.contains(k))
        {
            TaskType::Bugfix
        } else if ["add", "feature", "implement", "support", "introduce"]
            .iter()
            .any(|k| lower.contains(k))
        {
            TaskType::Feature
        } else if ["refactor", "rename", "move", "cleanup", "extract"]
            .iter()
            .any(|k| lower.contains(k))
        {
            TaskType::Refactor
        } else if ["test", "coverage", "spec"]
            .iter()
            .any(|k| lower.contains(k))
        {
            TaskType::Test
        } else if ["document", "readme", "docs"]
            .iter()
            .any(|k| lower.contains(k))
        {
            TaskType::Documentation
        } else if ["how", "where", "find", "understand", "explain", "explore"]
            .iter()
            .any(|k| lower.contains(k))
        {
            TaskType::Exploration
        } else {
            TaskType::Unknown
        };
        TaskDto {
            original_text: task_text.to_string(),
            task_type: task_type.as_str().into(),
            constraints: Vec::new(),
            suspected_files: None,
            suspected_symbols: None,
            target_subsystem: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Channel {
    Symbol,
    Lexical,
    Dependency(u32),
    Test,
}

impl Channel {
    fn as_str(&self) -> String {
        match self {
            Channel::Symbol => "symbol".into(),
            Channel::Lexical => "lexical".into(),
            Channel::Dependency(d) => format!("dependency_depth_{d}"),
            Channel::Test => "test".into(),
        }
    }
    /// Priority tier (TIS §9.4): task-critical → deps → tests.
    fn tier(&self) -> u32 {
        match self {
            Channel::Symbol | Channel::Lexical => 0,
            Channel::Dependency(1) => 1,
            Channel::Dependency(_) => 2,
            Channel::Test => 3,
        }
    }
}

#[derive(Debug, Clone)]
struct Candidate {
    entity_id: String,
    item_kind: &'static str,
    title: String,
    path: String,
    span: Option<SpanDto>,
    excerpt: String,
    content_hash: String,
    resolution_status: Option<String>,
    channel: Channel,
    score_millis: i64,
    score: f64,
    token_cost: u64,
    evidence_status: String,
}

/// Compile request inputs (TIS §9.1 subset used in step 1).
#[derive(Debug, Clone)]
pub struct CompileRequest {
    pub repository_id: RepositoryId,
    pub task_text: String,
    pub budget_tokens: u64,
    /// Optional retrieval-policy override (ADR-008 experiments). When absent,
    /// the current `policy_weights_millis()` apply; overrides are recorded in
    /// the bundle's `weights_millis` so every bundle carries its policy.
    pub weights_override: Option<BTreeMap<String, i64>>,
}

pub struct ContextCompiler<'a> {
    index_store: &'a dyn IndexStore,
    evidence_store: &'a dyn EvidenceStore,
    bundle_store: &'a dyn BundleStore,
    artifacts: &'a dyn ArtifactStore,
    search_backend: &'a dyn SearchBackend,
    normalizer: &'a dyn TaskNormalizer,
}

impl<'a> ContextCompiler<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        index_store: &'a dyn IndexStore,
        evidence_store: &'a dyn EvidenceStore,
        bundle_store: &'a dyn BundleStore,
        artifacts: &'a dyn ArtifactStore,
        search_backend: &'a dyn SearchBackend,
        normalizer: &'a dyn TaskNormalizer,
    ) -> Self {
        Self {
            index_store,
            evidence_store,
            bundle_store,
            artifacts,
            search_backend,
            normalizer,
        }
    }

    /// Compiles, persists, and returns the immutable bundle.
    pub async fn compile(&self, request: &CompileRequest) -> Result<BundleDto, CccError> {
        if request.task_text.trim().is_empty() {
            return Err(CccError::invalid_request("task text must not be empty"));
        }
        if request.budget_tokens == 0 {
            return Err(CccError::invalid_request("token budget must be positive"));
        }
        let active = self
            .index_store
            .get_active_generation(request.repository_id)
            .await?
            .ok_or_else(|| CccError::snapshot_not_found("no published index generation"))?;

        let normalized = self
            .normalizer
            .normalize(&request.task_text, request.budget_tokens);

        // Candidate generation channels (TIS §9.2).
        let file_by_id = self.file_index(active.snapshot_id).await?;
        let symbol_by_id = self.symbol_index(active.snapshot_id).await?;
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut seen_entities: HashSet<String> = HashSet::new();

        // Channel 1+2: lexical + symbol lookup (via SearchBackend).
        let search_response = self
            .search_backend
            .search(SearchRequest {
                repository_id: request.repository_id,
                query: request.task_text.clone(),
                snapshot_id: Some(active.snapshot_id),
                limit: 50,
            })
            .await?;
        for hit in &search_response.hits {
            let channel = match (hit.kind.as_str(), hit.match_class) {
                ("symbol", _) => Channel::Symbol,
                _ => Channel::Lexical,
            };
            if let Some(candidate) = self
                .candidate_from_hit(hit, channel, &file_by_id, &symbol_by_id, &mut seen_entities)
                .await?
            {
                candidates.push(candidate);
            }
        }

        // Channels 3+4: dependency traversal + test association from the
        // seed symbols (depth-limited, TIS §16.4).
        let seeds: Vec<SymbolId> = candidates
            .iter()
            .filter(|c| c.item_kind == "symbol")
            .filter_map(|c| SymbolId::from_str(&c.entity_id).ok())
            .collect();
        let (dep_candidates, dep_paths) = self
            .dependency_candidates(
                request.repository_id,
                active.snapshot_id,
                &seeds,
                &symbol_by_id,
                &file_by_id,
                &mut seen_entities,
            )
            .await?;
        candidates.extend(dep_candidates);
        // Deduplicate hop records (an edge can be seen from both directions).
        let mut seen_paths: HashSet<String> = HashSet::new();
        let dependency_paths: Vec<DependencyPathEdge> = dep_paths
            .into_iter()
            .filter(|p| {
                let key = format!(
                    "{}|{}|{}|{}",
                    p.from, p.to, p.edge_type, p.resolution_status
                );
                seen_paths.insert(key)
            })
            .collect();

        // Evidence freshness per candidate (selective, capped count).
        for candidate in &mut candidates {
            candidate.evidence_status = self
                .evidence_status_for(request.repository_id, &candidate.entity_id)
                .await;
        }

        // Ranking (TIS §9.3): S(c) = w_l·L + w_d·D + w_t·T + w_h·H + w_f·F + w_r·R − w_x·X.
        let weights = request
            .weights_override
            .clone()
            .unwrap_or_else(policy_weights_millis);
        for c in &mut candidates {
            let f = features_for(c);
            let w = |k: &str| weights.get(k).copied().unwrap_or(0) as f64 / 1000.0;
            let score = w("w_l") * f.l
                + w("w_d") * f.d
                + w("w_t") * f.t
                + w("w_h") * f.h
                + w("w_f") * f.f
                + w("w_r") * f.r
                - w("w_x") * f.x;
            c.score = score;
            c.score_millis = (score * 1000.0).round() as i64;
        }

        // Deterministic pre-order: tier → score desc → token asc → entity id.
        candidates.sort_by(|a, b| {
            a.channel
                .tier()
                .cmp(&b.channel.tier())
                .then(b.score_millis.cmp(&a.score_millis))
                .then(a.token_cost.cmp(&b.token_cost))
                .then(a.entity_id.cmp(&b.entity_id))
        });

        // Budget selection: greedy marginal utility (score/token), duplicate
        // penalization, explicit omissions (TIS §9.4, §13.7).
        let (selected, omissions, stopping_reason) =
            select_within_budget(&candidates, request.budget_tokens);

        // Assemble the bundle document.
        let bundle_id = BundleId::generate();
        let created_at = now_rfc3339();
        let mut items = Vec::new();
        let mut estimated_tokens: u64 = 0;
        for (rank, c) in selected.iter().enumerate() {
            estimated_tokens = estimated_tokens.saturating_add(c.token_cost);
            items.push(BundleItem {
                position: rank as u64,
                entity_id: c.entity_id.clone(),
                item_kind: c.item_kind.into(),
                title: c.title.clone(),
                path: c.path.clone(),
                span: c.span,
                excerpt: c.excerpt.clone(),
                selection_reason: c.channel.as_str(),
                channel: c.channel.as_str(),
                evidence_status: c.evidence_status.clone(),
                token_cost: c.token_cost,
                rank: rank as u64,
                provenance: ItemProvenance {
                    repository_id: request.repository_id.to_string(),
                    snapshot_id: active.snapshot_id.to_string(),
                    generation_id: active.generation_id.to_string(),
                    content_hash: c.content_hash.clone(),
                    extraction_method: "tree_sitter".into(),
                    resolution_status: c.resolution_status.clone(),
                },
            });
        }

        let task = TaskDto {
            original_text: normalized.original_text.clone(),
            task_type: normalized.task_type.clone(),
            constraints: normalized.constraints.clone(),
            suspected_files: normalized.suspected_files.clone(),
            suspected_symbols: normalized.suspected_symbols.clone(),
            target_subsystem: normalized.target_subsystem.clone(),
        };

        let limitations = vec![
            "Dependency edges are syntax-level: name-level references remain candidates; \
             trait-object dispatch, macros and reflection are known blind spots."
                .into(),
            "The absence of a discovered dependency is not proof that no dependency exists.".into(),
            "Token counts are approximate (chars/4) under an unknown tokenizer.".into(),
        ];

        let hash_input = BundleHashInput {
            schema_version: SCHEMA_VERSION,
            repository_id: &request.repository_id.to_string(),
            snapshot_id: &active.snapshot_id.to_string(),
            generation_id: &active.generation_id.to_string(),
            task: &task,
            retrieval_policy_version: RETRIEVAL_POLICY_VERSION,
            weights_millis: &weights,
            budget_tokens: request.budget_tokens,
            token_estimate_basis: "approximate_chars_div4",
            stopping_reason: &stopping_reason,
            items: &items,
            dependency_paths: &dependency_paths,
            omissions: &omissions,
            limitations: &limitations,
        };
        let bundle_hash = ccc_schema::hash_canonical(&hash_input)
            .map_err(|e| CccError::internal_error(format!("canonical hashing failed: {e}")))?;

        let bundle = BundleDto {
            schema_version: SCHEMA_VERSION.into(),
            bundle_id: bundle_id.to_string(),
            repository_id: request.repository_id.to_string(),
            snapshot_id: active.snapshot_id.to_string(),
            generation_id: active.generation_id.to_string(),
            task,
            retrieval_policy_version: RETRIEVAL_POLICY_VERSION.into(),
            weights_millis: weights,
            budget_tokens: request.budget_tokens,
            estimated_tokens,
            token_estimate_basis: "approximate_chars_div4".into(),
            stopping_reason,
            items,
            dependency_paths,
            omissions,
            limitations,
            created_at,
        };

        // Immutable artifact + rows (TIS §9.7: recompilation creates a new bundle).
        let canonical = ccc_schema::to_canonical_json(&bundle)
            .map_err(|e| CccError::internal_error(format!("serialize bundle: {e}")))?;
        let artifact_hash = self
            .artifacts
            .put(canonical.as_bytes(), "application/json+ccc-bundle")
            .await?;
        let items_json = serde_json::to_string(&bundle.items)
            .map_err(|e| CccError::internal_error(format!("serialize items: {e}")))?;
        self.bundle_store
            .insert_bundle(
                &BundleRecord {
                    bundle_id,
                    repository_id: request.repository_id,
                    snapshot_id: active.snapshot_id,
                    generation_id: active.generation_id,
                    schema_version: bundle.schema_version.clone(),
                    task_text: bundle.task.original_text.clone(),
                    normalized_json: serde_json::to_string(&bundle.task)
                        .unwrap_or_else(|_| "{}".into()),
                    retrieval_policy_version: bundle.retrieval_policy_version.clone(),
                    budget_tokens: bundle.budget_tokens,
                    estimated_tokens: bundle.estimated_tokens,
                    token_estimate_basis: bundle.token_estimate_basis.clone(),
                    stopping_reason: bundle.stopping_reason.clone(),
                    bundle_hash: bundle_hash.clone(),
                    artifact_hash,
                    created_at: bundle.created_at.clone(),
                },
                &items_json,
            )
            .await?;

        Ok(bundle)
    }

    async fn file_index(
        &self,
        snapshot_id: SnapshotId,
    ) -> StoreResult<HashMap<FileId, FileRecord>> {
        let files = self.index_store.list_files(snapshot_id).await?;
        Ok(files.into_iter().map(|f| (f.file_id, f)).collect())
    }

    async fn symbol_index(
        &self,
        snapshot_id: SnapshotId,
    ) -> StoreResult<HashMap<SymbolId, SymbolRecord>> {
        let symbols = self.index_store.list_symbols(snapshot_id).await?;
        Ok(symbols.into_iter().map(|s| (s.symbol_id, s)).collect())
    }

    async fn candidate_from_hit(
        &self,
        hit: &ccc_search::SearchHit,
        channel: Channel,
        file_by_id: &HashMap<FileId, FileRecord>,
        symbol_by_id: &HashMap<SymbolId, SymbolRecord>,
        seen: &mut HashSet<String>,
    ) -> StoreResult<Option<Candidate>> {
        if !seen.insert(hit.entity_id.clone()) {
            return Ok(None);
        }
        let (title, span, content_hash, excerpt) = match hit.kind.as_str() {
            "symbol" => {
                let Some(symbol_id) = SymbolId::from_str(&hit.entity_id).ok() else {
                    return Ok(None);
                };
                let Some(sym) = symbol_by_id.get(&symbol_id) else {
                    return Ok(None);
                };
                let file = file_by_id.get(&sym.file_id);
                let excerpt = match file {
                    Some(f) => excerpt_for(f, sym.start_line, sym.end_line, self.artifacts).await?,
                    None => String::new(),
                };
                (
                    sym.qualified_name.clone(),
                    Some(SpanDto {
                        start_line: sym.start_line,
                        start_column: sym.start_column,
                        end_line: sym.end_line,
                        end_column: sym.end_column,
                    }),
                    file.map(|f| f.content_hash.clone()).unwrap_or_default(),
                    excerpt,
                )
            }
            _ => {
                let Some(file_id) = FileId::from_str(&hit.entity_id).ok() else {
                    return Ok(None);
                };
                let Some(file) = file_by_id.get(&file_id) else {
                    return Ok(None);
                };
                let excerpt = excerpt_for(file, 1, u32::MAX, self.artifacts).await?;
                (file.path.clone(), None, file.content_hash.clone(), excerpt)
            }
        };
        Ok(Some(Candidate {
            entity_id: hit.entity_id.clone(),
            item_kind: if hit.kind == "symbol" {
                "symbol"
            } else {
                "file"
            },
            title,
            path: hit.path.clone(),
            span,
            excerpt,
            content_hash,
            resolution_status: hit.title.as_ref().map(|_| "resolved".to_string()),
            channel,
            score_millis: 0,
            score: 0.0,
            token_cost: 0, // set after excerpt known
            evidence_status: "candidate".into(),
        })
        .map(|mut c| {
            c.token_cost = estimate_tokens(&format!("{} {}", c.title, c.excerpt));
            c
        }))
    }

    /// Dependency-graph traversal (default depth 3, bounded) plus incoming
    /// `tests` edges. Both directions are traversed: callees and callers.
    async fn dependency_candidates(
        &self,
        repository_id: RepositoryId,
        snapshot_id: SnapshotId,
        seeds: &[SymbolId],
        symbol_by_id: &HashMap<SymbolId, SymbolRecord>,
        file_by_id: &HashMap<FileId, FileRecord>,
        seen: &mut HashSet<String>,
    ) -> StoreResult<(Vec<Candidate>, Vec<DependencyPathEdge>)> {
        let depth_limit = 3u32;
        let mut candidates = Vec::new();
        let mut visited: HashSet<SymbolId> = HashSet::new();
        let mut frontier: Vec<SymbolId> = seeds.to_vec();
        let mut dep_paths: Vec<DependencyPathEdge> = Vec::new();

        for depth in 1..=depth_limit {
            let mut next_frontier: Vec<SymbolId> = Vec::new();
            for symbol_id in &frontier {
                // Outgoing (callees) and incoming (callers) — deterministic
                // ordering by edge id string.
                let mut edges = self
                    .index_store
                    .list_edges_outgoing(snapshot_id, *symbol_id)
                    .await?;
                edges.extend(
                    self.index_store
                        .list_edges_incoming(snapshot_id, *symbol_id)
                        .await?,
                );
                edges.sort_by_key(|a| a.edge_id.to_string());
                for edge in edges {
                    let outgoing = edge.source_symbol_id == *symbol_id;
                    let other = if outgoing {
                        edge.target_symbol_id
                    } else {
                        Some(edge.source_symbol_id)
                    };
                    let Some(other_id) = other else { continue };
                    dep_paths.push(DependencyPathEdge {
                        from: symbol_by_id
                            .get(&edge.source_symbol_id)
                            .map(|s| s.qualified_name.clone())
                            .unwrap_or_default(),
                        to: edge
                            .target_symbol_id
                            .and_then(|t| symbol_by_id.get(&t))
                            .map(|s| s.qualified_name.clone())
                            .unwrap_or_else(|| edge.target_name.clone().unwrap_or_default()),
                        edge_type: serde_json::to_string(&edge.edge_type)
                            .unwrap_or_default()
                            .trim_matches('"')
                            .to_string(),
                        resolution_method: serde_json::to_string(&edge.resolution_method)
                            .unwrap_or_default()
                            .trim_matches('"')
                            .to_string(),
                        resolution_status: serde_json::to_string(&edge.resolution_status)
                            .unwrap_or_default()
                            .trim_matches('"')
                            .to_string(),
                    });
                    if edge.edge_type == RelationshipType::Tests {
                        // Test associations arrive via the test channel below.
                        if let Some(test_symbol) = symbol_by_id.get(&other_id) {
                            if let Some(candidate) = self
                                .candidate_from_symbol(test_symbol, Channel::Test, file_by_id, seen)
                                .await?
                            {
                                candidates.push(candidate);
                            }
                        }
                        continue;
                    }
                    if visited.contains(&other_id) || seeds.contains(&other_id) {
                        continue;
                    }
                    visited.insert(other_id);
                    if let Some(sym) = symbol_by_id.get(&other_id) {
                        if let Some(candidate) = self
                            .candidate_from_symbol(
                                sym,
                                Channel::Dependency(depth),
                                file_by_id,
                                seen,
                            )
                            .await?
                        {
                            candidates.push(candidate);
                            next_frontier.push(other_id);
                        }
                    }
                }
            }
            frontier = next_frontier;
            if frontier.is_empty() {
                break;
            }
        }
        let _ = repository_id;
        Ok((candidates, dep_paths))
    }

    #[allow(clippy::too_many_arguments)]
    async fn candidate_from_symbol(
        &self,
        sym: &SymbolRecord,
        channel: Channel,
        file_by_id: &HashMap<FileId, FileRecord>,
        seen: &mut HashSet<String>,
    ) -> StoreResult<Option<Candidate>> {
        if !seen.insert(sym.symbol_id.to_string()) {
            return Ok(None);
        }
        let file = file_by_id.get(&sym.file_id);
        let excerpt = match file {
            Some(f) => excerpt_for(f, sym.start_line, sym.end_line, self.artifacts).await?,
            None => String::new(),
        };
        let token_cost = estimate_tokens(&format!("{} {}", sym.qualified_name, excerpt));
        Ok(Some(Candidate {
            entity_id: sym.symbol_id.to_string(),
            item_kind: "symbol",
            title: sym.qualified_name.clone(),
            path: file.map(|f| f.path.clone()).unwrap_or_default(),
            span: Some(SpanDto {
                start_line: sym.start_line,
                start_column: sym.start_column,
                end_line: sym.end_line,
                end_column: sym.end_column,
            }),
            excerpt,
            content_hash: file.map(|f| f.content_hash.clone()).unwrap_or_default(),
            resolution_status: Some(
                serde_json::to_string(&sym.resolution_status)
                    .unwrap_or_default()
                    .trim_matches('"')
                    .to_string(),
            ),
            channel,
            score_millis: 0,
            score: 0.0,
            token_cost,
            evidence_status: "candidate".into(),
        }))
    }

    /// Evidence status for a candidate: stale if any dependent claim of the
    /// symbol is stale, else `candidate` (TIS §8.1, FR-060: stale items are
    /// carried visibly, never silently dropped).
    async fn evidence_status_for(&self, repository_id: RepositoryId, entity_id: &str) -> String {
        let Ok(symbol_id) = SymbolId::from_str(entity_id) else {
            return "candidate".into();
        };
        match self
            .evidence_store
            .claims_depending_on(repository_id, &[], &[symbol_id.to_string()])
            .await
        {
            Ok(claims) if claims.iter().any(|c| c.status == ClaimStatus::Stale) => "stale".into(),
            _ => "candidate".into(),
        }
    }
}

use std::str::FromStr as _;

#[derive(Debug, Clone, Copy)]
struct Features {
    l: f64,
    d: f64,
    t: f64,
    h: f64,
    f: f64,
    r: f64,
    x: f64,
}

/// Feature normalization to documented [0,1] ranges (TIS §9.3).
fn features_for(c: &Candidate) -> Features {
    let l = match c.channel {
        Channel::Symbol => 1.0,
        Channel::Lexical => 0.7,
        Channel::Dependency(_) => 0.4,
        Channel::Test => 0.3,
    };
    let d = match c.channel {
        Channel::Dependency(1) => 1.0,
        Channel::Dependency(2) => 0.66,
        Channel::Dependency(3) => 0.33,
        Channel::Dependency(_) => 0.25,
        Channel::Symbol => 1.0,
        Channel::Lexical => 0.25,
        Channel::Test => 0.5,
    };
    let t = match c.channel {
        Channel::Test => 1.0,
        _ => 0.0,
    };
    let h = 0.0; // history channel: not implemented in step 1
    let f = if c.evidence_status == "stale" {
        0.0
    } else {
        1.0
    };
    let r = match c.channel {
        Channel::Symbol | Channel::Lexical => 0.9,
        Channel::Dependency(_) => 0.7,
        Channel::Test => 0.4,
    };
    let x = (c.token_cost as f64 / 2048.0).clamp(0.0, 1.0); // normalized cost pressure
    Features {
        l,
        d,
        t,
        h,
        f,
        r,
        x,
    }
}

/// Greedy marginal-utility selection with duplicate penalization. Returns
/// (selected, omissions, stopping_reason). Deterministic: utility is
/// recomputed against the live selection state each step; ties break by
/// entity id.
fn select_within_budget(
    candidates: &[Candidate],
    budget_tokens: u64,
) -> (Vec<Candidate>, Vec<BundleOmission>, String) {
    let mut selected: Vec<Candidate> = Vec::new();
    let mut selected_files: HashSet<String> = HashSet::new();
    let mut remaining_budget = budget_tokens;
    let mut omissions: Vec<BundleOmission> = Vec::new();
    let mut any_budget_omission = false;

    let max_score = candidates
        .iter()
        .map(|c| c.score)
        .fold(f64::MIN, f64::max)
        .max(0.0);
    let high_threshold = 0.5 * max_score;

    let utility = |c: &Candidate, selected_files: &HashSet<String>| {
        let dup = if selected_files.contains(&c.path) {
            0.5 * c.score
        } else {
            0.0
        };
        ((c.score - dup) / (c.token_cost.max(1) as f64) * 1000.0) as i64
    };

    let mut remaining: Vec<usize> = (0..candidates.len()).collect();
    while !remaining.is_empty() {
        // Pick the best remaining candidate by marginal utility, ties by id.
        remaining.sort_by(|&i, &j| {
            let ui = utility(&candidates[i], &selected_files);
            let uj = utility(&candidates[j], &selected_files);
            uj.cmp(&ui)
                .then(candidates[i].entity_id.cmp(&candidates[j].entity_id))
        });
        let best = *remaining.last().unwrap();
        remaining.pop();
        let c = &candidates[best];
        if c.token_cost <= remaining_budget {
            remaining_budget -= c.token_cost;
            selected_files.insert(c.path.clone());
            selected.push(c.clone());
        } else if c.score >= high_threshold {
            omissions.push(BundleOmission {
                entity_id: c.entity_id.clone(),
                path: c.path.clone(),
                title: c.title.clone(),
                score_millis: c.score_millis,
                reason: "budget_exceeded".into(),
            });
            any_budget_omission = true;
        } else if selected_files.contains(&c.path) {
            omissions.push(BundleOmission {
                entity_id: c.entity_id.clone(),
                path: c.path.clone(),
                title: c.title.clone(),
                score_millis: c.score_millis,
                reason: "duplicate".into(),
            });
        } else {
            omissions.push(BundleOmission {
                entity_id: c.entity_id.clone(),
                path: c.path.clone(),
                title: c.title.clone(),
                score_millis: c.score_millis,
                reason: "lower_relevance".into(),
            });
        }
    }
    let stopping_reason = if selected.is_empty() && candidates.is_empty() {
        "no_candidates".to_string()
    } else if any_budget_omission {
        "budget_exhausted".to_string()
    } else {
        "candidates_exhausted".to_string()
    };
    (selected, omissions, stopping_reason)
}

/// Approximate token estimate: chars/4, rounded up (TIS §9.5 labeled
/// approximation; never presented as exact).
fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

const MAX_EXCERPT_CHARS: usize = 4000;

/// Extracts a source excerpt for a line span from the file's content blob.
/// `end_line == u32::MAX` means "to end of file".
async fn excerpt_for(
    file: &FileRecord,
    start_line: u32,
    end_line: u32,
    artifacts: &dyn ArtifactStore,
) -> StoreResult<String> {
    let Some(bytes) = artifacts.get(&file.content_hash).await? else {
        return Ok(String::new());
    };
    let text = String::from_utf8_lossy(&bytes);
    let selected = if end_line == u32::MAX {
        text.into_owned()
    } else {
        let start_idx = start_line.saturating_sub(1) as usize;
        let take = (end_line.saturating_sub(start_line) as usize).saturating_add(1);
        text.lines()
            .skip(start_idx)
            .take(take)
            .collect::<Vec<_>>()
            .join("\n")
    };
    if selected.chars().count() > MAX_EXCERPT_CHARS {
        Ok(selected.chars().take(MAX_EXCERPT_CHARS).collect())
    } else {
        Ok(selected)
    }
}

/// Convenience: renders the persisted bundle to Markdown (FR-063).
pub fn bundle_markdown(bundle: &BundleDto) -> String {
    render_bundle_markdown(bundle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccc_config::IndexingConfig;
    use ccc_indexer::run_index;
    use ccc_repository::GitCli;
    use ccc_snapshot::{find_or_create_snapshot, SnapshotBuilder};
    use ccc_storage::test_support::InMemoryEvidenceStore;
    use ccc_storage::{FsArtifactStore, SqliteStore};
    use std::path::Path;
    use std::process::Command;
    use std::sync::Arc;

    fn run(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?} failed");
    }

    const LIB: &str = r#"pub mod util;
pub fn run_app() { util::read_config(); }
"#;
    const UTIL: &str = r#"pub fn read_config() -> u8 { 1 }
"#;
    const TESTS: &str = r#"use app::run_app;
#[test]
fn smoke_run_app() { run_app(); }
"#;

    struct Fixture {
        _guard: tempfile::TempDir,
        root: std::path::PathBuf,
        store: Arc<SqliteStore>,
        artifacts: FsArtifactStore,
        evidence: InMemoryEvidenceStore,
        repo: RepositoryRecord,
        cfg: IndexingConfig,
    }

    async fn fixture_with_files(files: &[(&str, &str)]) -> Fixture {
        let guard = tempfile::tempdir().unwrap();
        let root = guard.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        run(&root, &["init", "--initial-branch=main"]);
        run(&root, &["config", "user.email", "t@e.com"]);
        run(&root, &["config", "user.name", "t"]);
        for (rel, content) in files {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        run(&root, &["add", "-A"]);
        run(&root, &["commit", "-m", "fixture"]);

        let store = Arc::new(
            SqliteStore::connect(&guard.path().join("ccc.db"))
                .await
                .unwrap(),
        );
        let artifacts = FsArtifactStore::new(guard.path().join("blobs"), store.clone());
        let evidence = InMemoryEvidenceStore::new();
        let registry = ccc_repository::RepositoryRegistry::new(&*store, &GitCli);
        let cfg = IndexingConfig::default();
        let repo = registry
            .register(&root, &cfg.configuration_hash().unwrap())
            .await
            .unwrap();
        Fixture {
            _guard: guard,
            root,
            store,
            artifacts,
            evidence,
            repo,
            cfg,
        }
    }

    async fn index_fixture(fixture: &Fixture) {
        let builder = SnapshotBuilder::new(&GitCli);
        let draft = builder
            .build_working_tree(
                fixture.repo.repository_id,
                &fixture.root,
                &fixture.cfg.configuration_hash().unwrap(),
                fixture.cfg.include_untracked,
            )
            .unwrap();
        let manifest = draft.manifest.clone();
        let snapshot = find_or_create_snapshot(&*fixture.store, draft)
            .await
            .unwrap();
        run_index(
            &*fixture.store,
            &*fixture.store,
            &*fixture.store,
            &fixture.artifacts,
            &fixture.evidence,
            &fixture.repo,
            &snapshot,
            &fixture.root,
            &manifest,
            &fixture.cfg,
            false,
        )
        .await
        .unwrap();
    }

    #[test]
    fn heuristic_normalizer_preserves_text_and_marks_unknown_fields() {
        let n = HeuristicNormalizer;
        let task = n.normalize("Fix the crash in read_config", 500);
        assert_eq!(task.original_text, "Fix the crash in read_config");
        assert_eq!(task.task_type, "bugfix");
        assert!(task.suspected_files.is_none(), "unknown fields stay None");
        assert!(task.target_subsystem.is_none());
        let explore = n.normalize("how does snapshot identity get computed", 100);
        assert_eq!(explore.task_type, "exploration");
        let unknown = n.normalize("xyzzy", 100);
        assert_eq!(unknown.task_type, "unknown");
    }

    #[tokio::test]
    async fn identical_compiles_produce_identical_bundle_hash() {
        let fx = fixture_with_files(&[
            ("src/lib.rs", LIB),
            ("src/util.rs", UTIL),
            ("tests/smoke.rs", TESTS),
        ])
        .await;
        index_fixture(&fx).await;
        let search_backend = ccc_search::FtsSearchBackend::new(&*fx.store, &*fx.store, &*fx.store);
        let compiler = ContextCompiler::new(
            &*fx.store,
            &fx.evidence,
            &*fx.store,
            &fx.artifacts,
            &search_backend,
            &HeuristicNormalizer,
        );
        let request = CompileRequest {
            repository_id: fx.repo.repository_id,
            task_text: "how does read_config get called".into(),
            budget_tokens: 2000,
            weights_override: None,
        };
        let first = compiler.compile(&request).await.unwrap();
        let second = compiler.compile(&request).await.unwrap();
        // bundle_id and created_at may differ; the content hash must not.
        assert_ne!(first.bundle_id, second.bundle_id);
        let hash_a = bundle_content_hash(&first);
        let hash_b = bundle_content_hash(&second);
        assert_eq!(hash_a, hash_b, "determinism (TIS §18.1)");
        // Items ordered deterministically with provenance.
        assert!(!first.items.is_empty());
        assert!(first
            .items
            .iter()
            .all(|i| !i.provenance.snapshot_id.is_empty()));
        assert_eq!(first.token_estimate_basis, "approximate_chars_div4");
    }

    fn bundle_content_hash(bundle: &BundleDto) -> String {
        ccc_schema::hash_canonical(&BundleHashInput {
            schema_version: &bundle.schema_version,
            repository_id: &bundle.repository_id,
            snapshot_id: &bundle.snapshot_id,
            generation_id: &bundle.generation_id,
            task: &bundle.task,
            retrieval_policy_version: &bundle.retrieval_policy_version,
            weights_millis: &bundle.weights_millis,
            budget_tokens: bundle.budget_tokens,
            token_estimate_basis: &bundle.token_estimate_basis,
            stopping_reason: &bundle.stopping_reason,
            items: &bundle.items,
            dependency_paths: &bundle.dependency_paths,
            omissions: &bundle.omissions,
            limitations: &bundle.limitations,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn budget_is_never_exceeded_and_omissions_are_recorded() {
        let fx = fixture_with_files(&[
            ("src/lib.rs", LIB),
            ("src/util.rs", UTIL),
            ("tests/smoke.rs", TESTS),
        ])
        .await;
        index_fixture(&fx).await;
        let search_backend = ccc_search::FtsSearchBackend::new(&*fx.store, &*fx.store, &*fx.store);
        let compiler = ContextCompiler::new(
            &*fx.store,
            &fx.evidence,
            &*fx.store,
            &fx.artifacts,
            &search_backend,
            &HeuristicNormalizer,
        );
        let request = CompileRequest {
            repository_id: fx.repo.repository_id,
            task_text: "read_config run_app smoke".into(),
            budget_tokens: 40,
            weights_override: None,
        };
        let bundle = compiler.compile(&request).await.unwrap();
        assert!(
            bundle.estimated_tokens <= bundle.budget_tokens,
            "budget never exceeded by estimate"
        );
        assert_eq!(bundle.token_estimate_basis, "approximate_chars_div4");
        if bundle.stopping_reason == "budget_exhausted" {
            assert!(
                bundle
                    .omissions
                    .iter()
                    .any(|o| o.reason == "budget_exceeded"),
                "high-scoring rejections recorded explicitly"
            );
        }
        // Markdown renders all sections.
        let md = bundle_markdown(&bundle);
        assert!(md.contains("# Context bundle"));
        assert!(md.contains("Selected items"));
        assert!(md.contains("Limitations"));
    }

    #[tokio::test]
    async fn stale_evidence_is_carried_visibly() {
        let fx = fixture_with_files(&[("src/lib.rs", LIB), ("src/util.rs", UTIL)]).await;
        index_fixture(&fx).await;
        // Flip the read_config claim stale manually (freshness seam output).
        let claims = fx
            .evidence
            .list_claims(fx.repo.repository_id)
            .await
            .unwrap();
        let target = claims
            .iter()
            .find(|c| c.text.contains("crate::util::read_config"))
            .expect("util claim exists");
        fx.evidence
            .mark_claims_stale(&[target.claim_id], &now_rfc3339())
            .await
            .unwrap();

        let search_backend = ccc_search::FtsSearchBackend::new(&*fx.store, &*fx.store, &*fx.store);
        let compiler = ContextCompiler::new(
            &*fx.store,
            &fx.evidence,
            &*fx.store,
            &fx.artifacts,
            &search_backend,
            &HeuristicNormalizer,
        );
        let bundle = compiler
            .compile(&CompileRequest {
                repository_id: fx.repo.repository_id,
                task_text: "read_config".into(),
                budget_tokens: 4000,
                weights_override: None,
            })
            .await
            .unwrap();
        let stale_item = bundle
            .items
            .iter()
            .find(|i| i.title == "crate::util::read_config");
        assert!(
            stale_item.is_some(),
            "stale item carried, not dropped (FR-060)"
        );
        assert_eq!(stale_item.unwrap().evidence_status, "stale");
    }
}
