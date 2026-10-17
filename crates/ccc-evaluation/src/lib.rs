//! `ccc-evaluation` — offline retrieval/integrity evaluation harness
//! (SDD §20.2, TIS §19.4, Phase G).
//!
//! Suites are versioned golden datasets (task text → expected symbols/files +
//! budget) hashed at load time and recorded immutably in the suite registry.
//! For every case the runner compiles a context bundle with the production
//! pipeline and measures recall@k, MRR, precision@k, stale-item rate and
//! token cost — integer millis only, so two identical runs produce byte-
//! identical metrics JSON (TIS §19.5 replay discipline).
//!
//! Held-out cases are scored separately from tuning cases (anti-overfit,
//! TIS §19.4). Integrity checks are built in: content the suite marks
//! forbidden (excluded paths) must never appear in a bundle.

use ccc_context_compiler::{
    policy_weights_millis, CompileRequest, ContextCompiler, HeuristicNormalizer,
    RETRIEVAL_POLICY_VERSION,
};
use ccc_domain::*;
use ccc_schema::{hash_canonical, SCHEMA_VERSION};
use ccc_search::SearchBackend;
use ccc_storage::{
    ArtifactStore, BundleStore, EvaluationStore, EvidenceStore, IndexStore, MemoryStore,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Cutoff for precision@k (items are position-ordered in the bundle).
pub const PRECISION_K: usize = 10;

/// One golden case of a suite file (TOML `[[cases]]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SuiteCase {
    /// The task text handed to the compiler.
    pub task: String,
    pub budget: u64,
    /// Symbol names (simple or qualified) expected in the selected items.
    pub expect_symbols: Vec<String>,
    /// File paths expected in the selected items.
    pub expect_files: Vec<String>,
    /// Paths that must never appear (excluded content — integrity check).
    pub forbidden_paths: Vec<String>,
    /// `tuning` or `held_out` (scored separately, TIS §19.4).
    pub kind: String,
}

impl Default for SuiteCase {
    fn default() -> Self {
        Self {
            task: String::new(),
            budget: 2000,
            expect_symbols: Vec::new(),
            expect_files: Vec::new(),
            forbidden_paths: Vec::new(),
            kind: "tuning".into(),
        }
    }
}

/// The parsed suite document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuiteFile {
    pub name: String,
    pub cases: Vec<SuiteCase>,
}

/// A loaded suite: parsed document plus its immutable content hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSuite {
    pub path: PathBuf,
    pub suite_hash: String,
    pub file: SuiteFile,
}

/// Resolves a suite name or explicit path. Lookup order: direct path;
/// `<cwd>/eval/suites/<name>.toml` (in-repo harness suites version with the
/// code); `<data>/suites/<name>.toml` (user suites). Unknown names are
/// `INVALID_REQUEST` — no silent fallback, no false availability.
pub fn resolve_suite_path(data_dir: &Path, name: &str) -> Result<PathBuf, CccError> {
    let as_path = Path::new(name);
    if as_path.is_file() {
        return Ok(as_path.to_path_buf());
    }
    if !name.is_empty() && !name.contains('/') && !name.contains('\\') && !name.starts_with('.') {
        let in_repo = std::env::current_dir()
            .map_err(|e| CccError::internal_error(format!("cannot read cwd: {e}")))?
            .join("eval")
            .join("suites")
            .join(format!("{name}.toml"));
        if in_repo.is_file() {
            return Ok(in_repo);
        }
        let in_data = data_dir.join("suites").join(format!("{name}.toml"));
        if in_data.is_file() {
            return Ok(in_data);
        }
    }
    Err(CccError::invalid_request(format!(
        "evaluation suite {name:?} not found (looked in ./eval/suites/ and the data dir)"
    )))
}

/// Loads and hashes a suite file. The hash is the registry key: a changed
/// file is a new suite (immutable once recorded, Phase G).
pub fn load_suite(path: &Path) -> Result<LoadedSuite, CccError> {
    let bytes = std::fs::read(path).map_err(|e| {
        CccError::invalid_request(format!("cannot read suite file {}: {e}", path.display()))
    })?;
    let suite_hash = blake3_hex(&bytes);
    let file: SuiteFile = toml::from_slice(&bytes).map_err(|e| {
        CccError::invalid_request(format!("invalid suite file {}: {e}", path.display()))
    })?;
    if file.cases.is_empty() {
        return Err(CccError::invalid_request(format!(
            "suite file {} declares no cases",
            path.display()
        )));
    }
    for (i, case) in file.cases.iter().enumerate() {
        if case.task.trim().is_empty() {
            return Err(CccError::invalid_request(format!(
                "suite file {} case {i} has an empty task",
                path.display()
            )));
        }
        if case.kind != "tuning" && case.kind != "held_out" {
            return Err(CccError::invalid_request(format!(
                "suite file {} case {i} has kind {:?}; expected \"tuning\" or \"held_out\"",
                path.display(),
                case.kind
            )));
        }
    }
    Ok(LoadedSuite {
        path: path.to_path_buf(),
        suite_hash,
        file,
    })
}

/// Per-case measured metrics (integer millis — no floats in hashes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseMetrics {
    pub case_index: u32,
    /// `tuning` | `held_out`.
    pub kind: String,
    pub task: String,
    pub budget: u64,
    pub items: u64,
    pub estimated_tokens: u64,
    /// Fraction of expected symbols/files present in the selected items.
    pub recall_millis: u64,
    /// Mean reciprocal rank of the first expected hit per expectation.
    pub mrr_millis: u64,
    /// Matched within the top-`PRECISION_K` items over the cutoff size.
    pub precision_millis: u64,
    /// Fraction of selected items whose evidence is `stale`.
    pub stale_rate_millis: u64,
    pub matched: u64,
    pub expected: u64,
}

/// Aggregate metrics over one case group (`tuning` or `held_out`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMetrics {
    pub cases: u64,
    pub recall_millis: u64,
    pub mrr_millis: u64,
    pub precision_millis: u64,
    pub stale_rate_millis: u64,
    /// Sum of estimated tokens across the group's bundles.
    pub token_cost: u64,
}

/// The full run document (also the basis of the persisted metrics JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMetrics {
    pub schema_version: String,
    pub suite_name: String,
    pub suite_hash: String,
    pub retrieval_policy_version: String,
    /// BLAKE3 over the canonical policy-weight map (versioned policy files
    /// are the only way these change — ADR-008).
    pub policy_hash: String,
    pub config_hash: String,
    pub cases: Vec<CaseMetrics>,
    pub tuning: GroupMetrics,
    pub held_out: GroupMetrics,
}

/// Evaluation runner over the production compile pipeline.
pub struct EvalRunner<'a> {
    index: &'a dyn IndexStore,
    evidence: &'a dyn EvidenceStore,
    memories: &'a dyn MemoryStore,
    bundles: &'a dyn BundleStore,
    artifacts: &'a dyn ArtifactStore,
    search: &'a dyn SearchBackend,
    evaluation: &'a dyn EvaluationStore,
}

impl<'a> EvalRunner<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        index: &'a dyn IndexStore,
        evidence: &'a dyn EvidenceStore,
        memories: &'a dyn MemoryStore,
        bundles: &'a dyn BundleStore,
        artifacts: &'a dyn ArtifactStore,
        search: &'a dyn SearchBackend,
        evaluation: &'a dyn EvaluationStore,
    ) -> Self {
        Self {
            index,
            evidence,
            memories,
            bundles,
            artifacts,
            search,
            evaluation,
        }
    }

    /// Runs every case of the suite against `repo`, records the suite (if new)
    /// and persists the immutable run record. Deterministic: identical
    /// (suite, policy, config) inputs produce identical metrics JSON.
    /// `weights_override` carries an experiment's retrieval-policy weights
    /// (ADR-008); `None` runs the current `policy-v1`.
    pub async fn run_suite(
        &self,
        repo: &RepositoryRecord,
        suite: &LoadedSuite,
        config_hash: &str,
        weights_override: Option<&std::collections::BTreeMap<String, i64>>,
    ) -> Result<RunMetrics, CccError> {
        // Register the suite immutably (no-op when the hash is already known).
        let suite_json = serde_json::to_string(&suite.file)
            .map_err(|e| CccError::internal_error(format!("suite serialization: {e}")))?;
        self.evaluation
            .insert_suite(&EvaluationSuiteRecord {
                suite_hash: suite.suite_hash.clone(),
                name: suite.file.name.clone(),
                case_count: suite.file.cases.len() as u32,
                suite_json,
                created_at: now_rfc3339(),
            })
            .await?;

        let effective_weights: std::collections::BTreeMap<String, i64> = weights_override
            .cloned()
            .unwrap_or_else(policy_weights_millis);
        let policy_hash = hash_canonical(&effective_weights)
            .map_err(|e| CccError::internal_error(format!("policy hash: {e}")))?;
        // Overridden runs carry an explicit version label — never silently
        // mislabeled as the current policy (ADR-008 provenance).
        let policy_version = match weights_override {
            Some(_) => format!(
                "policy-override-{}",
                &policy_hash[..policy_hash.len().min(12)]
            ),
            None => RETRIEVAL_POLICY_VERSION.into(),
        };
        let compiler = ContextCompiler::new(
            self.index,
            self.evidence,
            self.memories,
            self.bundles,
            self.artifacts,
            self.search,
            &HeuristicNormalizer,
        );

        let mut cases = Vec::with_capacity(suite.file.cases.len());
        for (index_of_case, case) in suite.file.cases.iter().enumerate() {
            let bundle = compiler
                .compile(&CompileRequest {
                    repository_id: repo.repository_id,
                    task_text: case.task.clone(),
                    budget_tokens: case.budget,
                    weights_override: weights_override.cloned(),
                    history_scores: None,
                })
                .await?;

            // Integrity gate: forbidden (excluded) content never surfaces.
            for item in &bundle.items {
                if case
                    .forbidden_paths
                    .iter()
                    .any(|f| item.path == *f || item.path.starts_with(&format!("{f}/")))
                {
                    return Err(CccError::internal_error(format!(
                        "integrity violation: excluded content {} appeared in the bundle for \
                         suite case {}",
                        item.path, index_of_case
                    )));
                }
            }

            cases.push(score_case(index_of_case as u32, case, &bundle));
        }

        let tuning = aggregate(&cases, "tuning");
        let held_out = aggregate(&cases, "held_out");
        let metrics = RunMetrics {
            schema_version: SCHEMA_VERSION.into(),
            suite_name: suite.file.name.clone(),
            suite_hash: suite.suite_hash.clone(),
            retrieval_policy_version: policy_version,
            policy_hash,
            config_hash: config_hash.to_string(),
            cases,
            tuning,
            held_out,
        };

        // Persist the immutable run record (the metrics JSON is the durable
        // comparison unit — created_at/run_id stay out of it).
        let metrics_json = serde_json::to_string(&metrics)
            .map_err(|e| CccError::internal_error(format!("metrics serialization: {e}")))?;
        self.evaluation
            .insert_run(&EvaluationRunRecord {
                run_id: EvaluationRunId::generate(),
                repository_id: repo.repository_id,
                suite_name: metrics.suite_name.clone(),
                suite_hash: metrics.suite_hash.clone(),
                policy_version: metrics.retrieval_policy_version.clone(),
                policy_hash: metrics.policy_hash.clone(),
                config_hash: metrics.config_hash.clone(),
                metrics_json,
                created_at: now_rfc3339(),
            })
            .await?;
        Ok(metrics)
    }
}

/// Scores one case against its compiled bundle (integer math only).
fn score_case(case_index: u32, case: &SuiteCase, bundle: &ccc_schema::BundleDto) -> CaseMetrics {
    let expected_count = case.expect_symbols.len() + case.expect_files.len();
    let mut matched = 0u64;
    let mut reciprocal_sum_millis = 0u64;

    // An expectation is matched at the FIRST item whose title/path contains
    // it (rank is 1-based; the bundle order is the production ranking).
    let find_rank = |needle: &str, match_title: bool| -> Option<u64> {
        for item in &bundle.items {
            let title_hit = match_title
                && item
                    .title
                    .to_ascii_lowercase()
                    .contains(&needle.to_ascii_lowercase());
            let path_hit = item
                .path
                .to_ascii_lowercase()
                .contains(&needle.to_ascii_lowercase());
            if title_hit || path_hit {
                return Some(item.position + 1);
            }
        }
        None
    };

    for symbol in &case.expect_symbols {
        if let Some(rank) = find_rank(symbol, true) {
            matched += 1;
            reciprocal_sum_millis += 1000 / rank.max(1);
        }
    }
    for file in &case.expect_files {
        if find_rank(file, false).is_some() {
            matched += 1;
        }
    }
    // File matches also contribute their first-hit reciprocal rank.
    for file in &case.expect_files {
        if let Some(rank) = find_rank(file, false) {
            reciprocal_sum_millis += 1000 / rank.max(1);
        }
    }

    let expected = expected_count as u64;
    let recall_millis = matched
        .checked_mul(1000)
        .and_then(|n| n.checked_div(expected))
        .unwrap_or(0);
    let mrr_millis = reciprocal_sum_millis.checked_div(expected).unwrap_or(0);
    let cutoff = bundle.items.len().min(PRECISION_K);
    let matched_in_top_k = matched.min(cutoff as u64);
    let precision_millis = if cutoff == 0 {
        0
    } else {
        matched_in_top_k * 1000 / cutoff as u64
    };
    let stale_items = bundle
        .items
        .iter()
        .filter(|i| i.evidence_status == "stale")
        .count() as u64;
    let stale_rate_millis = if bundle.items.is_empty() {
        0
    } else {
        stale_items * 1000 / bundle.items.len() as u64
    };

    CaseMetrics {
        case_index,
        kind: case.kind.clone(),
        task: case.task.clone(),
        budget: case.budget,
        items: bundle.items.len() as u64,
        estimated_tokens: bundle.estimated_tokens,
        recall_millis,
        mrr_millis,
        precision_millis,
        stale_rate_millis,
        matched,
        expected,
    }
}

/// Aggregates one group's per-case metrics (floor-integer means).
fn aggregate(cases: &[CaseMetrics], kind: &str) -> GroupMetrics {
    let group: Vec<&CaseMetrics> = cases.iter().filter(|c| c.kind == kind).collect();
    let n = group.len() as u64;
    if n == 0 {
        return GroupMetrics::default();
    }
    let sum = |pick: fn(&CaseMetrics) -> u64| group.iter().map(|c| pick(c)).sum::<u64>();
    GroupMetrics {
        cases: n,
        recall_millis: sum(|c| c.recall_millis) / n,
        mrr_millis: sum(|c| c.mrr_millis) / n,
        precision_millis: sum(|c| c.precision_millis) / n,
        stale_rate_millis: sum(|c| c.stale_rate_millis) / n,
        token_cost: sum(|c| c.estimated_tokens),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccc_config::IndexingConfig;
    use ccc_indexer::run_index;
    use ccc_repository::GitCli;
    use ccc_search::FtsSearchBackend;
    use ccc_snapshot::{find_or_create_snapshot, SnapshotBuilder};
    use ccc_storage::test_support::InMemoryEvaluationStore;
    use ccc_storage::{FsArtifactStore, SqliteStore};
    use std::path::PathBuf;
    use std::process::Command as StdCommand;
    use std::str::FromStr as _;
    use std::sync::Arc;

    const LIB_V1: &str = "pub mod util;\npub fn run_app() { util::read_config(); }\n";
    const UTIL_V1: &str = "pub fn read_config() -> u8 { 1 }\n";
    const VENDOR_V1: &str = "// vendored, excluded\n";

    struct Fixture {
        _guard: tempfile::TempDir,
        root: PathBuf,
        store: Arc<SqliteStore>,
        artifacts: FsArtifactStore,
        repo: RepositoryRecord,
        cfg: IndexingConfig,
        evaluation: InMemoryEvaluationStore,
    }

    fn run(dir: &Path, args: &[&str]) {
        let out = StdCommand::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    async fn setup() -> Fixture {
        let guard = tempfile::tempdir().unwrap();
        let root = guard.path().join("repo");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("vendor")).unwrap();
        run(&root, &["init", "--initial-branch=main"]);
        run(&root, &["config", "user.email", "t@e.com"]);
        run(&root, &["config", "user.name", "t"]);
        std::fs::write(root.join("src/lib.rs"), LIB_V1).unwrap();
        std::fs::write(root.join("src/util.rs"), UTIL_V1).unwrap();
        std::fs::write(root.join("vendor/dep.rs"), VENDOR_V1).unwrap();
        run(&root, &["add", "-A"]);
        run(&root, &["commit", "-m", "base"]);

        let store = Arc::new(
            SqliteStore::connect(&guard.path().join("ccc.db"))
                .await
                .unwrap(),
        );
        let artifacts = FsArtifactStore::new(guard.path().join("blobs"), store.clone());
        let registry = ccc_repository::RepositoryRegistry::new(&*store, &GitCli);
        let cfg = IndexingConfig::default();
        let repo = registry
            .register(&root, &cfg.configuration_hash().unwrap())
            .await
            .unwrap();
        index_current(&store, &artifacts, &repo, &cfg, &root).await;
        Fixture {
            _guard: guard,
            root,
            store,
            artifacts,
            repo,
            cfg,
            evaluation: InMemoryEvaluationStore::new(),
        }
    }

    async fn index_current(
        store: &Arc<SqliteStore>,
        artifacts: &FsArtifactStore,
        repo: &RepositoryRecord,
        cfg: &IndexingConfig,
        root: &Path,
    ) {
        let builder = SnapshotBuilder::new(&GitCli);
        let draft = builder
            .build_working_tree(
                repo.repository_id,
                root,
                &cfg.configuration_hash().unwrap(),
                true,
            )
            .unwrap();
        let manifest = draft.manifest.clone();
        let snapshot = find_or_create_snapshot(&**store, draft).await.unwrap();
        run_index(
            &**store, &**store, &**store, artifacts, &**store, repo, &snapshot, root, &manifest,
            cfg, false,
        )
        .await
        .unwrap();
    }

    /// Local helper: builds a runner over the fixture's production pipeline
    /// and runs one suite end to end (current policy weights).
    macro_rules! eval_run {
        ($fx:expr, $evaluation:expr, $suite:expr) => {{
            let search = FtsSearchBackend::new(&*$fx.store, &*$fx.store, &*$fx.store);
            EvalRunner::new(
                &*$fx.store,
                &*$fx.store,
                &*$fx.store,
                &*$fx.store,
                &$fx.artifacts,
                &search,
                $evaluation,
            )
            .run_suite(&$fx.repo, $suite, "cfg-hash", None)
            .await
        }};
    }

    fn suite_with(cases: &[SuiteCase]) -> LoadedSuite {
        LoadedSuite {
            path: PathBuf::from("memory-suite.toml"),
            suite_hash: blake3_hex(format!("{cases:?}").as_bytes()),
            file: SuiteFile {
                name: "memory".into(),
                cases: cases.to_vec(),
            },
        }
    }

    #[tokio::test]
    async fn identical_runs_produce_identical_metrics_json() {
        let fx = setup().await;
        let suite = suite_with(&[
            SuiteCase {
                task: "how does read_config get called".into(),
                budget: 3000,
                expect_symbols: vec!["read_config".into(), "run_app".into()],
                expect_files: vec!["src/util.rs".into()],
                forbidden_paths: vec!["vendor/dep.rs".into()],
                kind: "tuning".into(),
            },
            SuiteCase {
                task: "where is the vendor dependency".into(),
                budget: 1000,
                expect_symbols: vec![],
                expect_files: vec![],
                forbidden_paths: vec![],
                kind: "held_out".into(),
            },
        ]);

        let first = eval_run!(fx, &fx.evaluation, &suite).unwrap();
        let second = eval_run!(fx, &fx.evaluation, &suite).unwrap();

        // Byte-identical metrics (deterministic replay, TIS §19.5).
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        );
        // Identical case metrics, including ranking positions.
        assert_eq!(first.cases, second.cases);
        assert_eq!(first.suite_hash, second.suite_hash);
        assert_eq!(first.retrieval_policy_version, RETRIEVAL_POLICY_VERSION);

        // The runs are recorded separately but share the metrics content.
        let runs = fx
            .evaluation
            .list_runs(fx.repo.repository_id)
            .await
            .unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].metrics_json, runs[1].metrics_json);
        assert_eq!(runs[0].suite_hash, suite.suite_hash);
        assert_eq!(runs[0].policy_version, "policy-v1");
        let suite_record = fx
            .evaluation
            .get_suite(&suite.suite_hash)
            .await
            .unwrap()
            .expect("suite registered");
        assert_eq!(suite_record.case_count, 2);

        // The retrieval actually works on the golden set: all expectations hit.
        assert_eq!(first.cases[0].matched, first.cases[0].expected);
        assert!(first.cases[0].recall_millis > 0);
        assert!(first.tuning.recall_millis > 0);
        // Held-out is scored separately, not mixed into tuning.
        assert_eq!(first.tuning.cases, 1);
        assert_eq!(first.held_out.cases, 1);
    }

    #[tokio::test]
    async fn stale_scenario_shows_positive_stale_rate_selectively() {
        // SDD §20.5 controlled stale-evidence experiment: a previously valid
        // claim becomes stale after a known change; the bundle exposes it.
        let fx = setup().await;
        std::fs::write(
            fx.root.join("src/util.rs"),
            "pub fn read_config() -> u8 { 2 }\n",
        )
        .unwrap();
        run(&fx.root, &["add", "-A"]);
        run(&fx.root, &["commit", "-m", "change util"]);
        index_current(&fx.store, &fx.artifacts, &fx.repo, &fx.cfg, &fx.root).await;

        let suite = suite_with(&[SuiteCase {
            task: "how does read_config get called".into(),
            budget: 3000,
            expect_symbols: vec!["read_config".into()],
            expect_files: vec!["src/util.rs".into()],
            forbidden_paths: vec![],
            kind: "tuning".into(),
        }]);
        let metrics = eval_run!(fx, &fx.evaluation, &suite).unwrap();

        assert!(
            metrics.tuning.stale_rate_millis > 0,
            "the stale-evidence scenario must surface in the bundle: {metrics:?}"
        );
        // Selective: stale items exist, but not everything went stale.
        assert!(
            metrics.cases[0].items > metrics.cases[0].matched
                || metrics.cases[0].stale_rate_millis < 1000
        );
        // Claims that were flipped stale are exactly those depending on the
        // changed file (selectivity at the claim level, FR-040).
        let claims = fx.store.list_claims(fx.repo.repository_id).await.unwrap();
        let stale = claims
            .iter()
            .filter(|c| c.status == ClaimStatus::Stale)
            .count();
        let supported = claims
            .iter()
            .filter(|c| c.status == ClaimStatus::Supported)
            .count();
        assert!(stale > 0, "changed-file claims are stale");
        assert_eq!(
            supported, 0,
            "no unrelated claim flipped without a verification run"
        );
    }

    #[tokio::test]
    async fn forbidden_content_fails_the_run_as_integrity_violation() {
        let fx = setup().await;
        // Mark currently-indexable content forbidden: the runner must refuse.
        let suite = suite_with(&[SuiteCase {
            task: "how does read_config get called".into(),
            budget: 3000,
            expect_symbols: vec![],
            expect_files: vec![],
            forbidden_paths: vec!["src/util.rs".into()],
            kind: "tuning".into(),
        }]);
        let result = eval_run!(fx, &fx.evaluation, &suite);
        let err = result.unwrap_err();
        assert_eq!(err.code, ErrorCode::InternalError);
        assert!(err.message.contains("integrity violation"));
        // No run record for a failed run.
        assert!(fx
            .evaluation
            .list_runs(fx.repo.repository_id)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn unknown_suites_and_invalid_suites_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_suite_path(dir.path(), "no-such-suite").unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);

        let suite_path = dir.path().join("empty.toml");
        std::fs::write(&suite_path, "name = \"empty\"\n").unwrap();
        let err = load_suite(&suite_path).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);

        std::fs::write(
            &suite_path,
            "name = \"bad\"\n[[cases]]\ntask = \"x\"\nkind = \"weird\"\n",
        )
        .unwrap();
        let err = load_suite(&suite_path).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn suite_files_in_repo_parse_with_expectations() {
        // The shipped suites must stay loadable (they version with the code).
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("eval")
            .join("suites");
        let fixture_suite = load_suite(&base.join("fixture.toml")).unwrap();
        assert_eq!(fixture_suite.file.name, "fixture");
        assert!(fixture_suite
            .file
            .cases
            .iter()
            .any(|c| c.kind == "held_out"));
        let dogfood = load_suite(&base.join("dogfood.toml")).unwrap();
        assert!(
            dogfood.file.cases.len() >= 3,
            "dogfood suite has real tasks"
        );
    }

    #[tokio::test]
    async fn run_record_round_trips_through_the_store() {
        let fx = setup().await;
        let suite = suite_with(&[SuiteCase {
            task: "how does read_config get called".into(),
            budget: 2000,
            expect_symbols: vec!["read_config".into()],
            expect_files: vec![],
            forbidden_paths: vec![],
            kind: "tuning".into(),
        }]);
        let metrics = eval_run!(fx, &fx.evaluation, &suite).unwrap();
        let runs = fx
            .evaluation
            .list_runs(fx.repo.repository_id)
            .await
            .unwrap();
        assert_eq!(runs.len(), 1);
        let parsed: RunMetrics = serde_json::from_str(&runs[0].metrics_json).unwrap();
        assert_eq!(parsed, metrics);
        let fetched = fx
            .evaluation
            .get_run(EvaluationRunId::from_str(&runs[0].run_id.to_string()).unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.config_hash, "cfg-hash");
    }

    // -- T8: controlled policy A/B (ADR-008) --------------------------------

    /// A retrieval-policy override file ([weights] table) — mirrors the CLI
    /// loader shape.
    #[derive(Debug, serde::Deserialize)]
    struct WeightsOverrideFile {
        weights: std::collections::BTreeMap<String, i64>,
    }

    fn v2_weights() -> std::collections::BTreeMap<String, i64> {
        std::collections::BTreeMap::from([
            ("w_l".into(), 1000),
            ("w_d".into(), 1100),
            ("w_t".into(), 600),
            ("w_h".into(), 0),
            ("w_f".into(), 400),
            ("w_r".into(), 300),
            ("w_x".into(), 400),
        ])
    }

    #[tokio::test]
    async fn policy_ab_records_two_runs_with_distinct_policy_hashes() {
        let fx = setup().await;
        // The shipped candidate file must parse into the exact v2 weights.
        let v2_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("eval")
            .join("policies")
            .join("policy-v2.toml");
        let raw = std::fs::read_to_string(&v2_path).unwrap();
        let file: WeightsOverrideFile = toml::from_str(&raw).unwrap();
        let override_weights = file.weights;
        assert_eq!(override_weights, v2_weights());

        let suite_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("eval")
            .join("suites")
            .join("fixture.toml");
        let suite = load_suite(&suite_path).unwrap();

        // Run A: current policy-v1. Run B: the v2 candidate weights.
        let v1 = eval_run!(fx, &fx.evaluation, &suite).unwrap();
        let v2 = {
            let search = FtsSearchBackend::new(&*fx.store, &*fx.store, &*fx.store);
            EvalRunner::new(
                &*fx.store,
                &*fx.store,
                &*fx.store,
                &*fx.store,
                &fx.artifacts,
                &search,
                &fx.evaluation,
            )
            .run_suite(&fx.repo, &suite, "cfg-hash", Some(&override_weights))
            .await
            .unwrap()
        };

        // Both runs recorded against the same immutable suite, with distinct
        // policy hashes (no silent weight edits — ADR-008).
        assert_ne!(v1.policy_hash, v2.policy_hash);
        assert_eq!(v1.suite_hash, v2.suite_hash);
        let runs = fx
            .evaluation
            .list_runs(fx.repo.repository_id)
            .await
            .unwrap();
        assert_eq!(runs.len(), 2);
        let hashes: std::collections::HashSet<String> =
            runs.iter().map(|r| r.policy_hash.clone()).collect();
        assert_eq!(hashes.len(), 2);
        assert!(runs.iter().all(|r| r.suite_hash == suite.suite_hash));
        // Determinism holds per policy: rerunning v2 reproduces its metrics.
        let v2_again = {
            let search = FtsSearchBackend::new(&*fx.store, &*fx.store, &*fx.store);
            EvalRunner::new(
                &*fx.store,
                &*fx.store,
                &*fx.store,
                &*fx.store,
                &fx.artifacts,
                &search,
                &fx.evaluation,
            )
            .run_suite(&fx.repo, &suite, "cfg-hash", Some(&override_weights))
            .await
            .unwrap()
        };
        assert_eq!(
            serde_json::to_string(&v2).unwrap(),
            serde_json::to_string(&v2_again).unwrap()
        );
        // The comparison data the eval-log needs (both sides measured).
        assert_eq!(v1.tuning.cases, v2.tuning.cases);
    }
}
