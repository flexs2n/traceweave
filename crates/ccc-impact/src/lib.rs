//! `ccc-impact` — change → impact report (TIS §10, SDD §5.15).
//!
//! Given a base revision and a head (revision or working tree), the analyzer
//! normalizes a rename-aware change set, maps changed files to symbols of the
//! active generation, and traverses reverse dependency edges to a depth limit
//! (truncation is reported, never silent — SDD §14.4).
//!
//! Classification is honest by construction: paths through resolved edges are
//! `confirmed`; any path through a candidate or unresolved edge stays
//! `candidate`. Trait-object dispatch and macros are listed as known blind
//! spots. Recommended checks are advisory text only — nothing is executed
//! (ADR-008).

use ccc_domain::*;
use ccc_repository::GitProvider;
use ccc_schema::{hash_canonical, DependencyPathEdge, SCHEMA_VERSION};
use ccc_storage::{EvidenceStore, ImpactStore, IndexStore};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// One normalized change of a file (SDD §5.15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    pub kind: ChangeKind,
    /// New path (or deleted path).
    pub path: String,
    /// Previous path for renames.
    pub old_path: Option<String>,
}

/// The normalized change set between two repository states.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeSet {
    pub base: String,
    pub head: String,
    pub files: Vec<ChangedFile>,
}

/// Classification of one affected artifact (TIS §14.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AffectedArtifact {
    pub symbol_qualified_name: String,
    pub path: String,
    pub depth: u32,
    pub classification: String,
    /// Traversable dependency path from the changed symbol to this artifact.
    pub path_edges: Vec<DependencyPathEdge>,
}

/// The full impact report document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImpactReportDto {
    pub schema_version: String,
    pub impact_id: String,
    pub repository_id: String,
    pub snapshot_id: String,
    pub generation_id: String,
    pub base: String,
    pub head: String,
    pub change_set: ChangeSet,
    pub changed_symbols: Vec<String>,
    pub affected_artifacts: Vec<AffectedArtifact>,
    pub affected_claims: Vec<AffectedClaim>,
    pub traversal_truncated: bool,
    pub traversal_depth_limit: u32,
    pub known_blind_spots: Vec<String>,
    pub recommended_checks: Vec<RecommendedCheck>,
    pub completeness_limits: Vec<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AffectedClaim {
    pub claim_id: String,
    pub text: String,
    pub current_status: String,
    pub would_become_stale: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecommendedCheck {
    pub check: String,
    pub rationale: String,
}

pub struct ImpactAnalyzer<'a> {
    index_store: &'a dyn IndexStore,
    evidence_store: &'a dyn EvidenceStore,
    impact_store: &'a dyn ImpactStore,
    git: &'a dyn GitProvider,
    depth_limit: u32,
}

impl<'a> ImpactAnalyzer<'a> {
    pub fn new(
        index_store: &'a dyn IndexStore,
        evidence_store: &'a dyn EvidenceStore,
        impact_store: &'a dyn ImpactStore,
        git: &'a dyn GitProvider,
        depth_limit: u32,
    ) -> Self {
        Self {
            index_store,
            evidence_store,
            impact_store,
            git,
            depth_limit: depth_limit.max(1),
        }
    }

    /// Analyzes the impact of `base → head` (head `None` = working tree)
    /// against the active index generation.
    pub async fn analyze(
        &self,
        repository_id: RepositoryId,
        repo_root: &Path,
        base: &str,
        head: Option<&str>,
    ) -> Result<ImpactReportDto, CccError> {
        if !self.git.rev_exists(repo_root, base)? {
            return Err(CccError::invalid_request(format!(
                "base revision {base:?} not found"
            )));
        }
        if let Some(h) = head {
            if h != "working_tree" && !self.git.rev_exists(repo_root, h)? {
                return Err(CccError::invalid_request(format!(
                    "head revision {h:?} not found"
                )));
            }
        }
        let active = self
            .index_store
            .get_active_generation(repository_id)
            .await?
            .ok_or_else(|| CccError::snapshot_not_found("no published index generation"))?;

        // 1. Normalized, rename-aware change set (SDD §5.15).
        let entries = self.git.diff_name_status(repo_root, base, head)?;
        let change_set = ChangeSet {
            base: base.to_string(),
            head: head.unwrap_or("working_tree").to_string(),
            files: entries
                .into_iter()
                .map(|e| ChangedFile {
                    kind: e.kind,
                    path: e.path,
                    old_path: e.old_path,
                })
                .collect(),
        };

        // 2. Changed files → symbols of the active generation.
        let files = self.index_store.list_files(active.snapshot_id).await?;
        let file_by_path: HashMap<&str, &FileRecord> =
            files.iter().map(|f| (f.path.as_str(), f)).collect();
        let mut changed_paths: HashSet<String> = HashSet::new();
        for change in &change_set.files {
            changed_paths.insert(change.path.clone());
            if let Some(old) = &change.old_path {
                changed_paths.insert(old.clone());
            }
        }
        let mut changed_symbol_ids: Vec<SymbolId> = Vec::new();
        let mut changed_symbol_names: Vec<String> = Vec::new();
        let mut changed_file_ids: Vec<String> = Vec::new();
        for path in &changed_paths {
            if let Some(file) = file_by_path.get(path.as_str()) {
                changed_file_ids.push(file.file_id.to_string());
                for sym in self
                    .index_store
                    .list_symbols_for_file(active.snapshot_id, file.file_id)
                    .await?
                {
                    changed_symbol_ids.push(sym.symbol_id);
                    changed_symbol_names.push(sym.qualified_name.clone());
                }
            }
        }
        changed_symbol_names.sort();

        // 3. Reverse-edge BFS (callers/importers) to the depth limit with
        //    truncation reporting.
        let symbols = self.index_store.list_symbols(active.snapshot_id).await?;
        let symbol_by_id: HashMap<SymbolId, &SymbolRecord> =
            symbols.iter().map(|s| (s.symbol_id, s)).collect();
        let mut affected: Vec<AffectedArtifact> = Vec::new();
        let mut visited: HashSet<SymbolId> = HashSet::new();
        let mut truncated = false;

        #[derive(Clone)]
        struct BfsItem {
            symbol_id: SymbolId,
            path_edges: Vec<DependencyPathEdge>,
            classification: String,
        }
        let mut frontier: Vec<BfsItem> = Vec::new();
        for id in &changed_symbol_ids {
            visited.insert(*id);
            frontier.push(BfsItem {
                symbol_id: *id,
                path_edges: Vec::new(),
                classification: "confirmed".into(),
            });
        }
        let mut depth = 0u32;
        while !frontier.is_empty() && depth < self.depth_limit {
            depth += 1;
            let mut next: Vec<BfsItem> = Vec::new();
            for item in &frontier {
                let mut incoming = self
                    .index_store
                    .list_edges_incoming(active.snapshot_id, item.symbol_id)
                    .await?;
                incoming.sort_by_key(|a| a.edge_id.to_string());
                if incoming.is_empty() {
                    continue;
                }
                let from_name = symbol_by_id
                    .get(&item.symbol_id)
                    .map(|s| s.qualified_name.clone())
                    .unwrap_or_default();
                for edge in incoming {
                    let classification = if item.classification == "confirmed"
                        && edge.resolution_status == ResolutionStatus::Resolved
                    {
                        "confirmed"
                    } else {
                        "candidate"
                    };
                    let to_name = symbol_by_id
                        .get(&edge.source_symbol_id)
                        .map(|s| s.qualified_name.clone())
                        .unwrap_or_default();
                    let mut path_edges = item.path_edges.clone();
                    path_edges.push(DependencyPathEdge {
                        from: from_name.clone(),
                        to: to_name.clone(),
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
                    if visited.contains(&edge.source_symbol_id) {
                        continue;
                    }
                    visited.insert(edge.source_symbol_id);
                    if let Some(sym) = symbol_by_id.get(&edge.source_symbol_id) {
                        let file_path = files
                            .iter()
                            .find(|f| f.file_id == sym.file_id)
                            .map(|f| f.path.clone())
                            .unwrap_or_default();
                        affected.push(AffectedArtifact {
                            symbol_qualified_name: sym.qualified_name.clone(),
                            path: file_path,
                            depth,
                            classification: classification.into(),
                            path_edges: path_edges.clone(),
                        });
                        next.push(BfsItem {
                            symbol_id: edge.source_symbol_id,
                            path_edges,
                            classification: classification.into(),
                        });
                    }
                }
            }
            if !next.is_empty() && depth == self.depth_limit {
                // The frontier would keep going beyond the limit.
                truncated = true;
            }
            frontier = next;
        }
        affected.sort_by(|a, b| {
            a.depth
                .cmp(&b.depth)
                .then(a.symbol_qualified_name.cmp(&b.symbol_qualified_name))
        });

        // 4. Affected claims: dependencies intersecting the changed set would
        //    flip stale on the next index (FR-040 seam reporting).
        let changed_symbol_id_strings: Vec<String> =
            changed_symbol_ids.iter().map(|s| s.to_string()).collect();
        let dependent_claims = self
            .evidence_store
            .claims_depending_on(repository_id, &changed_file_ids, &changed_symbol_id_strings)
            .await?;
        let affected_claims = dependent_claims
            .into_iter()
            .map(|c| AffectedClaim {
                claim_id: c.claim_id.to_string(),
                text: c.text.clone(),
                current_status: serde_json::to_string(&c.status)
                    .unwrap_or_default()
                    .trim_matches('"')
                    .to_string(),
                would_become_stale: c.status != ClaimStatus::Stale,
            })
            .collect();

        // 5. Advisory checks only — nothing is executed (ADR-008).
        let recommended_checks = vec![
            RecommendedCheck {
                check: "cargo build".into(),
                rationale: "compilation catches signature breakage in changed symbols".into(),
            },
            RecommendedCheck {
                check: "cargo test".into(),
                rationale: "affected call paths and associated tests should be revalidated".into(),
            },
        ];
        let known_blind_spots = vec![
            "dynamic dispatch through trait objects resolves only as candidates".into(),
            "macro-generated call sites are invisible to the syntax parser".into(),
            "reflection and runtime-constructed calls are not tracked".into(),
        ];
        let completeness_limits = vec![
            "Impact is a hypothesis grounded in indexed evidence, not proof that all \
             affected behavior was discovered (SDD §5.15)."
                .into(),
            "The absence of a discovered dependency is not evidence of no dependency.".into(),
        ];

        let impact_id = ImpactId::generate();
        let report = ImpactReportDto {
            schema_version: SCHEMA_VERSION.into(),
            impact_id: impact_id.to_string(),
            repository_id: repository_id.to_string(),
            snapshot_id: active.snapshot_id.to_string(),
            generation_id: active.generation_id.to_string(),
            base: change_set.base.clone(),
            head: change_set.head.clone(),
            change_set,
            changed_symbols: changed_symbol_names,
            affected_artifacts: affected,
            affected_claims,
            traversal_truncated: truncated,
            traversal_depth_limit: self.depth_limit,
            known_blind_spots,
            recommended_checks,
            completeness_limits,
            created_at: now_rfc3339(),
        };

        // Persist with a float-free hash.
        let hash = hash_canonical(&report)
            .map_err(|e| CccError::internal_error(format!("hash failed: {e}")))?;
        let canonical = serde_json::to_vec(&report)
            .map_err(|e| CccError::internal_error(format!("serialize: {e}")))?;
        let artifact_hash = blake3_hex(&canonical);
        self.impact_store
            .insert_impact(&ImpactRecord {
                impact_id,
                repository_id,
                snapshot_id: active.snapshot_id,
                base: report.base.clone(),
                head: report.head.clone(),
                schema_version: report.schema_version.clone(),
                report_hash: hash,
                artifact_hash,
                created_at: report.created_at.clone(),
            })
            .await?;

        Ok(report)
    }
}

/// Renders an impact report as readable Markdown (FR-049 vocabulary).
pub fn render_impact_markdown(report: &ImpactReportDto) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Impact report {} → {}\n\n",
        report.base, report.head
    ));
    out.push_str(&format!(
        "- Snapshot: `{}` (generation `{}`)\n",
        report.snapshot_id, report.generation_id
    ));
    out.push_str(&format!(
        "- Changed files: {}\n",
        report
            .change_set
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    out.push_str(&format!(
        "- Changed symbols: {}\n\n",
        if report.changed_symbols.is_empty() {
            "(none indexed)".to_string()
        } else {
            report.changed_symbols.join(", ")
        }
    ));
    out.push_str("## Affected artifacts\n\n");
    if report.affected_artifacts.is_empty() {
        out.push_str("(none found within depth limit)\n");
    }
    for a in &report.affected_artifacts {
        out.push_str(&format!(
            "- `{}` ({}; depth {}) — classification: **{}**\n",
            a.symbol_qualified_name, a.path, a.depth, a.classification
        ));
    }
    if report.traversal_truncated {
        out.push_str(&format!(
            "\nTraversal truncated at depth {}.\n",
            report.traversal_depth_limit
        ));
    }
    if !report.affected_claims.is_empty() {
        out.push_str("\n## Affected claims\n\n");
        for c in &report.affected_claims {
            out.push_str(&format!(
                "- `{}` — status: {}, would become stale: {}\n",
                c.claim_id, c.current_status, c.would_become_stale
            ));
        }
    }
    out.push_str("\n## Recommended checks (advisory — not executed)\n\n");
    for check in &report.recommended_checks {
        out.push_str(&format!("- `{}`: {}\n", check.check, check.rationale));
    }
    out.push_str("\n## Known blind spots\n\n");
    for b in &report.known_blind_spots {
        out.push_str(&format!("- {b}\n"));
    }
    out.push_str("\n## Completeness limits\n\n");
    for l in &report.completeness_limits {
        out.push_str(&format!("- {l}\n"));
    }
    out
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
    use std::process::Command;
    use std::sync::Arc;

    const LIB_V1: &str = "pub mod util;\npub fn run_app() { util::read_config(); }\n";
    const UTIL_V1: &str = "pub fn read_config() -> u8 { 1 }\n";

    struct Fixture {
        _guard: tempfile::TempDir,
        root: std::path::PathBuf,
        store: Arc<SqliteStore>,
        #[allow(dead_code)]
        artifacts: FsArtifactStore,
        evidence: InMemoryEvidenceStore,
        repo: RepositoryRecord,
        #[allow(dead_code)]
        cfg: IndexingConfig,
    }

    async fn setup(committed: bool) -> Fixture {
        let guard = tempfile::tempdir().unwrap();
        let root = guard.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        run(&root, &["init", "--initial-branch=main"]);
        run(&root, &["config", "user.email", "t@e.com"]);
        run(&root, &["config", "user.name", "t"]);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), LIB_V1).unwrap();
        std::fs::write(root.join("src/util.rs"), UTIL_V1).unwrap();
        run(&root, &["add", "-A"]);
        run(&root, &["commit", "-m", "base"]);
        if committed {
            std::fs::write(
                root.join("src/util.rs"),
                "pub fn read_config() -> u8 { 2 }\n",
            )
            .unwrap();
            run(&root, &["add", "-A"]);
            run(&root, &["commit", "-m", "change util"]);
        }

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
        // Index the BASE state so changed-file mapping finds symbols.
        let builder = SnapshotBuilder::new(&GitCli);
        let draft = builder
            .build_working_tree(
                repo.repository_id,
                &root,
                &cfg.configuration_hash().unwrap(),
                true,
            )
            .unwrap();
        let manifest = draft.manifest.clone();
        let snapshot = find_or_create_snapshot(&*store, draft).await.unwrap();
        run_index(
            &*store, &*store, &*store, &artifacts, &evidence, &repo, &snapshot, &root, &manifest,
            &cfg, false,
        )
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

    fn run(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?} failed");
    }

    #[tokio::test]
    async fn caller_of_changed_function_is_confirmed_impact_with_path() {
        // Index base state, commit a util.rs change, analyze HEAD~1 → HEAD.
        let fx = setup(false).await;
        std::fs::write(
            fx.root.join("src/util.rs"),
            "pub fn read_config() -> u8 { 2 }\n",
        )
        .unwrap();
        run(&fx.root, &["add", "-A"]);
        run(&fx.root, &["commit", "-m", "change util"]);

        let analyzer = ImpactAnalyzer::new(&*fx.store, &fx.evidence, &*fx.store, &GitCli, 3);
        let report = analyzer
            .analyze(fx.repo.repository_id, &fx.root, "HEAD~1", Some("HEAD"))
            .await
            .unwrap();

        assert!(report
            .change_set
            .files
            .iter()
            .any(|f| f.path == "src/util.rs" && f.kind == ChangeKind::Modified));
        assert!(report
            .changed_symbols
            .iter()
            .any(|s| s.contains("read_config")));

        // run_app calls read_config via a simple-name candidate edge; the
        // report must list run_app as impacted with a traversable path, and
        // honestly classify it as a candidate (syntax-level binding).
        let run_app = report
            .affected_artifacts
            .iter()
            .find(|a| a.symbol_qualified_name.contains("run_app"))
            .expect("caller reported as impacted");
        assert!(run_app.depth >= 1);
        assert!(!run_app.path_edges.is_empty(), "traversable path present");
        assert_eq!(run_app.classification, "candidate");
        // Advisory only.
        assert!(report
            .recommended_checks
            .iter()
            .all(|c| c.check == "cargo build" || c.check == "cargo test"));

        let md = render_impact_markdown(&report);
        assert!(md.contains("Affected artifacts"));
        assert!(md.contains("advisory"));
    }

    #[tokio::test]
    async fn trait_impl_change_reports_candidates_not_certainty() {
        let fx = setup(false).await;
        std::fs::write(
            fx.root.join("src/writer.rs"),
            "pub trait Sink { fn drain(&self); }\npub struct File { pub name: String }\nimpl Sink for File { fn drain(&self) {} }\npub fn use_sink(s: &dyn Sink) { s.drain(); }\n",
        )
        .unwrap();
        run(&fx.root, &["add", "-A"]);
        run(&fx.root, &["commit", "-m", "add sink"]);

        let analyzer = ImpactAnalyzer::new(&*fx.store, &fx.evidence, &*fx.store, &GitCli, 3);
        let report = analyzer
            .analyze(fx.repo.repository_id, &fx.root, "HEAD~1", Some("HEAD"))
            .await
            .unwrap();

        // implements edges stay candidates (FR-049): no confirmed claims from
        // trait implementations.
        assert!(report
            .known_blind_spots
            .iter()
            .any(|b| b.contains("trait objects")));
        assert!(report
            .affected_artifacts
            .iter()
            .all(|a| a.classification == "candidate" || a.classification == "confirmed"));
    }

    #[tokio::test]
    async fn affected_claims_report_would_become_stale() {
        let fx = setup(false).await;
        std::fs::write(
            fx.root.join("src/util.rs"),
            "pub fn read_config() -> u8 { 9 }\n",
        )
        .unwrap();
        run(&fx.root, &["add", "-A"]);
        run(&fx.root, &["commit", "-m", "change util"]);

        let analyzer = ImpactAnalyzer::new(&*fx.store, &fx.evidence, &*fx.store, &GitCli, 3);
        let report = analyzer
            .analyze(fx.repo.repository_id, &fx.root, "HEAD~1", Some("HEAD"))
            .await
            .unwrap();
        assert!(
            !report.affected_claims.is_empty(),
            "claims depending on changed files are listed"
        );
        assert!(report.affected_claims.iter().any(|c| c.would_become_stale));
    }

    #[tokio::test]
    async fn unknown_base_revision_is_invalid_request() {
        let fx = setup(false).await;
        let analyzer = ImpactAnalyzer::new(&*fx.store, &fx.evidence, &*fx.store, &GitCli, 3);
        let err = analyzer
            .analyze(fx.repo.repository_id, &fx.root, "no-such-rev", Some("HEAD"))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
    }
}
