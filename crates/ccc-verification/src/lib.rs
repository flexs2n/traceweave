//! `ccc-verification` — patch verification under an execution policy
//! (TIS §11, SDD §15, ADR-008).
//!
//! The engine closes the loop Step 1 opened: *change → predicted impact →
//! verification → evidence updates*. Modes 1–2 (static inspection, suggested
//! verification) never spawn a subprocess; mode 3 (authorized execution) runs
//! only commands permitted by a data-dir policy through exact-prefix argv
//! allowlisting, with timeouts, output caps, and a minimal environment.
//!
//! Honesty rules encoded here (TIS §11.3): a check that was skipped, could
//! not start, or produced an ambiguous result is never `passed`; timeouts and
//! missing infrastructure are `timed_out`/`unavailable`, never `failed`; and
//! a selected test set is a recommendation, not proof of coverage.

pub mod executor;
pub mod policy;
pub mod revalidate;
pub mod select;

use ccc_domain::*;
use ccc_impact::{AffectedArtifact, ChangeSet, ImpactAnalyzer};
use ccc_repository::GitProvider;
use ccc_schema::{hash_canonical, SCHEMA_VERSION};
use ccc_storage::{
    ArtifactStore, EvidenceStore, ImpactStore, IndexStore, StoreResult, VerificationStore,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr as _;

pub use policy::{
    list_policy_names, load_policy, VerificationPolicy, DEFAULT_MAX_OUTPUT_BYTES,
    DEFAULT_POLICY_NAME,
};
pub use select::{select_checks, SelectedCheck};

/// One check of a verification report (serialized contract).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationCheckDto {
    pub check_id: String,
    pub position: u32,
    pub command: String,
    /// `directly` | `transitively` | `historically` | `fallback` (SDD §15.2).
    pub relation: String,
    /// TIS §11.3 vocabulary.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// `true` when captured output was capped at the policy limit.
    pub truncated: bool,
    /// Human note (e.g. why a suggestion was not executed).
    pub note: String,
}

/// Patch-analysis result (SDD §15.5 lite): the actual diff compared with the
/// previously recorded verification for the same repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactComparison {
    /// Verification id of the compared prior record, when one existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_verification_id: Option<String>,
    /// Symbols changed in both the prior and current patch.
    pub overlapping_symbols: Vec<String>,
    /// Symbols new to this patch (absent from the prior record).
    pub new_symbols: Vec<String>,
}

/// The full verification report document (TIS §11.1 step 9).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerificationReportDto {
    pub schema_version: String,
    pub verification_id: String,
    pub repository_id: String,
    pub snapshot_id: String,
    pub generation_id: String,
    pub base: String,
    pub head: String,
    pub policy_name: String,
    pub policy_hash: String,
    pub mode: VerificationMode,
    pub change_set: ChangeSet,
    pub changed_symbols: Vec<String>,
    pub affected_artifacts: Vec<AffectedArtifact>,
    /// Claims that the impact analysis expects to become stale for this patch
    /// (TIS §11.1 step 4).
    pub newly_stale_claims: Vec<String>,
    pub impact_comparison: ImpactComparison,
    pub checks: Vec<VerificationCheckDto>,
    /// Per-status counts over `checks` (TIS §11.3).
    pub statuses: BTreeMap<String, u32>,
    /// Observed-vs-predicted comparison notes (TIS §11.1 step 8). Surfaced
    /// for human judgment — never used to auto-accept a patch (SDD §15.5).
    pub observed_vs_predicted: Vec<String>,
    /// Evidence revalidation outcome (TIS §11.1 step 10, SDD §15.6): scoped
    /// claim flips with their provenance links.
    pub revalidation: revalidate::RevalidationSummary,
    pub limitations: Vec<String>,
    pub created_at: String,
}

/// Verification engine (TIS §11.1 workflow).
pub struct VerificationEngine<'a> {
    index: &'a dyn IndexStore,
    evidence: &'a dyn EvidenceStore,
    verifications: &'a dyn VerificationStore,
    impacts: &'a dyn ImpactStore,
    artifacts: &'a dyn ArtifactStore,
    git: &'a dyn GitProvider,
    /// Reverse-dependency traversal depth (shared with impact analysis).
    depth_limit: u32,
}

impl<'a> VerificationEngine<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        index: &'a dyn IndexStore,
        evidence: &'a dyn EvidenceStore,
        verifications: &'a dyn VerificationStore,
        impacts: &'a dyn ImpactStore,
        artifacts: &'a dyn ArtifactStore,
        git: &'a dyn GitProvider,
        depth_limit: u32,
    ) -> Self {
        Self {
            index,
            evidence,
            verifications,
            impacts,
            artifacts,
            git,
            depth_limit: depth_limit.max(1),
        }
    }

    /// Runs verification for `base → head` (head `None` = working tree) under
    /// the given policy and publishes an immutable record.
    pub async fn verify(
        &self,
        repo: &RepositoryRecord,
        repo_root: &Path,
        base: &str,
        head: Option<&str>,
        policy: &VerificationPolicy,
    ) -> Result<VerificationReportDto, CccError> {
        // Steps 1–4 (TIS §11.1): baseline snapshot, patch normalization,
        // predicted impact, potentially-stale evidence. The impact analyzer
        // validates revisions and persists the predicted-impact record.
        let impact = ImpactAnalyzer::new(
            self.index,
            self.evidence,
            self.impacts,
            self.git,
            self.depth_limit,
        )
        .analyze(repo.repository_id, repo_root, base, head)
        .await?;
        let snapshot_id = SnapshotId::from_str(&impact.snapshot_id)?;

        // Changed files/symbols of the active generation (execution coverage).
        let changed = self.changed_scope(&impact.change_set, snapshot_id).await?;

        // Step 5: checks selected by the dependency graph, labeled honestly.
        let selected = select::select_checks(
            self.index,
            snapshot_id,
            &changed.symbol_ids,
            self.depth_limit,
        )
        .await?;

        // Steps 6–7: execution is policy-gated. The default policy spawns
        // nothing; suggestions are recorded `not_run` (never `passed`).
        let mode = if policy.allow_execution {
            VerificationMode::Authorized
        } else {
            VerificationMode::Analysis
        };
        let verification_id = VerificationId::generate();
        let mut checks: Vec<VerificationCheckDto> = Vec::new();
        let mut check_records: Vec<VerificationCheckRecord> = Vec::new();
        for (position, candidate) in selected.iter().enumerate() {
            let (mut dto, mut record) = if policy.allow_execution {
                let (dto, record) = executor::execute_check(
                    repo_root,
                    policy,
                    self.artifacts,
                    candidate,
                    &verification_id,
                    position as u32,
                    // No interactive cancellation surface in the Step 2 CLI;
                    // the seam exists for job cancellation (SDD §15.3).
                    None,
                )
                .await?;
                (dto, record)
            } else {
                let check_id = VerificationId::generate();
                (
                    VerificationCheckDto {
                        check_id: check_id.to_string(),
                        position: position as u32,
                        command: candidate.command.clone(),
                        relation: candidate.relation.as_str().into(),
                        status: VerificationStatus::NotRun.as_str().into(),
                        exit_code: None,
                        duration_ms: None,
                        truncated: false,
                        note: "recommendation — not executed (analysis policy; suggestions are \
                               advisory)"
                            .into(),
                    },
                    VerificationCheckRecord {
                        check_id,
                        verification_id,
                        position: position as u32,
                        suggested_command: candidate.command.clone(),
                        relation: candidate.relation.as_str().into(),
                        status: VerificationStatus::NotRun,
                        exit_code: None,
                        duration_ms: None,
                        stdout_ref: None,
                        stderr_ref: None,
                        truncated: false,
                        env_summary_json: "{}".into(),
                    },
                )
            };
            dto.position = position as u32;
            record.position = position as u32;
            checks.push(dto);
            check_records.push(record);
        }

        let statuses = statuses_summary(&checks);

        // Step 8 + SDD §15.5 lite: observed vs predicted, and the current diff
        // compared with the previously recorded verification (if any).
        let observed_vs_predicted = self.compare_observed_with_predicted(&impact, &checks);
        let impact_comparison = self
            .compare_with_prior(repo.repository_id, &impact.changed_symbols)
            .await?;

        // Step 10: scoped evidence revalidation (SDD §15.6). Analysis runs
        // have no completed checks, so this is an explicit no-op with a note.
        let revalidation = revalidate::revalidate(revalidate::RevalidationInput {
            evidence: self.evidence,
            repository_id: repo.repository_id,
            verification_id,
            policy_name: &policy.name,
            policy_allows_revalidation: policy.revalidate_claims,
            scope_file_ids: &changed.file_ids,
            scope_symbol_ids: &changed.symbol_ids,
            checks: &checks,
        })
        .await?;

        let mut newly_stale_claims: Vec<String> = impact
            .affected_claims
            .iter()
            .filter(|c| c.would_become_stale)
            .map(|c| c.claim_id.clone())
            .collect();
        newly_stale_claims.sort();

        let mut limitations = vec![
            "Selected checks are recommendations; running them does not prove that every \
             regression path is covered (SDD §15.2)."
                .into(),
            "Passing checks support narrowly scoped claims only; repeated or cheap success is \
             not `verified` status (SDD §15.6)."
                .into(),
            "Static impact derives from syntax-level indexing; trait-object dispatch and \
             macro-generated call sites are blind spots."
                .into(),
        ];
        if !policy.allow_execution {
            limitations.insert(
                0,
                "Analysis policy: no command was executed. Executable suggestions are reported \
                 `not_run`; predicted impact is unverified by execution."
                    .into(),
            );
        }

        let report = VerificationReportDto {
            schema_version: SCHEMA_VERSION.into(),
            verification_id: verification_id.to_string(),
            repository_id: repo.repository_id.to_string(),
            snapshot_id: impact.snapshot_id.clone(),
            generation_id: impact.generation_id.clone(),
            base: impact.base.clone(),
            head: impact.head.clone(),
            policy_name: policy.name.clone(),
            policy_hash: policy.hash(),
            mode,
            change_set: impact.change_set.clone(),
            changed_symbols: impact.changed_symbols.clone(),
            affected_artifacts: impact.affected_artifacts.clone(),
            newly_stale_claims,
            impact_comparison,
            checks,
            statuses,
            observed_vs_predicted,
            revalidation,
            limitations,
            created_at: now_rfc3339(),
        };

        // Step 9: publish the immutable record. The report artifact is stored
        // content-addressed so later patch analysis can compare against it.
        let canonical = ccc_schema::to_canonical_json(&report)
            .map_err(|e| CccError::internal_error(format!("serialize report: {e}")))?;
        let artifact_hash = self
            .artifacts
            .put(canonical.as_bytes(), "application/json")
            .await?;
        self.verifications
            .insert_verification(
                &VerificationRecord {
                    verification_id,
                    repository_id: repo.repository_id,
                    snapshot_id,
                    base: report.base.clone(),
                    head: report.head.clone(),
                    policy_name: policy.name.clone(),
                    policy_hash: report.policy_hash.clone(),
                    mode: report.mode,
                    statuses_json: serde_json::to_string(&report.statuses)
                        .unwrap_or_else(|_| "{}".into()),
                    artifact_hash,
                    created_at: report.created_at.clone(),
                },
                &check_records,
            )
            .await?;

        Ok(report)
    }

    /// Changed files/symbols of the active generation for a change set.
    async fn changed_scope(
        &self,
        change_set: &ChangeSet,
        snapshot_id: SnapshotId,
    ) -> StoreResult<ChangedScope> {
        let files = self.index.list_files(snapshot_id).await?;
        let mut changed_paths: Vec<String> = Vec::new();
        for change in &change_set.files {
            changed_paths.push(change.path.clone());
            if let Some(old) = &change.old_path {
                changed_paths.push(old.clone());
            }
        }
        changed_paths.sort();
        changed_paths.dedup();
        let mut file_ids = Vec::new();
        let mut symbol_ids = Vec::new();
        for path in &changed_paths {
            if let Some(file) = files.iter().find(|f| &f.path == path) {
                file_ids.push(file.file_id);
                for sym in self
                    .index
                    .list_symbols_for_file(snapshot_id, file.file_id)
                    .await?
                {
                    symbol_ids.push(sym.symbol_id);
                }
            }
        }
        Ok(ChangedScope {
            file_ids,
            symbol_ids,
        })
    }

    /// TIS §11.1 step 8: compare observed check outcomes with the predicted
    /// impact. Notes only — never an auto-accept (SDD §15.5).
    fn compare_observed_with_predicted(
        &self,
        impact: &ccc_impact::ImpactReportDto,
        checks: &[VerificationCheckDto],
    ) -> Vec<String> {
        let mut notes = Vec::new();
        let executed: Vec<&VerificationCheckDto> = checks
            .iter()
            .filter(|c| c.status == VerificationStatus::Passed.as_str())
            .collect();
        let failed: Vec<&VerificationCheckDto> = checks
            .iter()
            .filter(|c| c.status == VerificationStatus::Failed.as_str())
            .collect();
        let predicted_affected = !impact.affected_artifacts.is_empty();
        if !executed.is_empty() && failed.is_empty() {
            notes.push(format!(
                "{} check(s) passed while the impact prediction listed {} affected artifact(s); \
                 agreement with the prediction is observational, not proof (SDD §15.5).",
                executed.len(),
                impact.affected_artifacts.len()
            ));
        }
        if !failed.is_empty() {
            notes.push(format!(
                "{} check(s) failed; the predicted affected set ({}) should be reviewed against \
                 the failures before the patch is accepted.",
                failed.len(),
                if predicted_affected {
                    format!("{} artifact(s)", impact.affected_artifacts.len())
                } else {
                    "no affected artifacts predicted".into()
                }
            ));
        }
        if executed.is_empty() && failed.is_empty() {
            notes.push(
                "No check was executed; the predicted impact is unverified by execution.".into(),
            );
        }
        notes
    }

    /// SDD §15.5 lite: diff the current changed-symbol set against the most
    /// recent prior verification record for this repository. Prior records
    /// written by earlier builds may have unretrievable artifacts; the
    /// comparison degrades to "new symbols" rather than failing.
    async fn compare_with_prior(
        &self,
        repository_id: RepositoryId,
        changed_symbols: &[String],
    ) -> StoreResult<ImpactComparison> {
        let prior = self
            .verifications
            .list_verifications(repository_id)
            .await?
            .into_iter()
            .last();
        let Some(prior) = prior else {
            return Ok(ImpactComparison {
                prior_verification_id: None,
                overlapping_symbols: Vec::new(),
                new_symbols: changed_symbols.to_vec(),
            });
        };
        let prior_symbols: Vec<String> = match self.artifacts.get(&prior.artifact_hash).await? {
            Some(bytes) => serde_json::from_slice::<VerificationReportDto>(&bytes)
                .map(|r| r.changed_symbols)
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let overlapping: Vec<String> = changed_symbols
            .iter()
            .filter(|s| prior_symbols.contains(s))
            .cloned()
            .collect();
        let new_symbols: Vec<String> = changed_symbols
            .iter()
            .filter(|s| !prior_symbols.contains(s))
            .cloned()
            .collect();
        Ok(ImpactComparison {
            prior_verification_id: Some(prior.verification_id.to_string()),
            overlapping_symbols: overlapping,
            new_symbols,
        })
    }
}

/// Files/symbols the verification covers (SDD §15.6 revalidation scope).
#[derive(Debug, Default)]
pub struct ChangedScope {
    pub file_ids: Vec<FileId>,
    pub symbol_ids: Vec<SymbolId>,
}

/// Per-status counts over the checks (TIS §11.3).
fn statuses_summary(checks: &[VerificationCheckDto]) -> BTreeMap<String, u32> {
    let mut summary: BTreeMap<String, u32> = BTreeMap::new();
    for status in [
        VerificationStatus::Passed,
        VerificationStatus::Failed,
        VerificationStatus::TimedOut,
        VerificationStatus::Blocked,
        VerificationStatus::NotRun,
        VerificationStatus::Unavailable,
        VerificationStatus::Cancelled,
    ] {
        summary.insert(status.as_str().into(), 0);
    }
    for check in checks {
        *summary.entry(check.status.clone()).or_insert(0) += 1;
    }
    summary
}

/// Renders a verification report as readable Markdown (ui.md §3.2: no false
/// availability — suggestions are labeled as never executed).
pub fn render_verification_markdown(report: &VerificationReportDto) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Verification {} → {} ({})\n\n",
        report.base, report.head, report.policy_name
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
        "- Changed symbols: {}\n",
        if report.changed_symbols.is_empty() {
            "(none indexed)".to_string()
        } else {
            report.changed_symbols.join(", ")
        }
    ));
    out.push_str(&format!(
        "- Statuses: {}\n\n",
        report
            .statuses
            .iter()
            .filter(|(_, n)| **n > 0)
            .map(|(s, n)| format!("{s}: {n}"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    out.push_str("## Checks (recommendations, not proof)\n\n");
    for check in &report.checks {
        out.push_str(&format!(
            "- `{}` — relation: {}, status: **{}**{}\n",
            check.command,
            check.relation,
            check.status,
            if check.note.is_empty() {
                String::new()
            } else {
                format!(" — {}", check.note)
            }
        ));
    }
    if !report.observed_vs_predicted.is_empty() {
        out.push_str("\n## Observed vs predicted\n\n");
        for note in &report.observed_vs_predicted {
            out.push_str(&format!("- {note}\n"));
        }
    }
    if !report.newly_stale_claims.is_empty() {
        out.push_str("\n## Newly stale claims\n\n");
        for id in &report.newly_stale_claims {
            out.push_str(&format!("- `{id}`\n"));
        }
    }
    out.push_str("\n## Evidence revalidation (scoped, SDD §15.6)\n\n");
    if !report.revalidation.supported_claims.is_empty() {
        out.push_str(&format!(
            "- supported: {} claim(s) now carry `validated_at` + this record's link\n",
            report.revalidation.supported_claims.len()
        ));
    }
    if !report.revalidation.conflicted_claims.is_empty() {
        out.push_str(&format!(
            "- conflicted: {} claim(s) marked by failing checks (negative results preserved)\n",
            report.revalidation.conflicted_claims.len()
        ));
    }
    if report.revalidation.ambiguous {
        out.push_str(&format!(
            "- no claim was touched: {} (ambiguous outcomes establish nothing)\n",
            if report.revalidation.note.is_empty() {
                "outcome not established".to_string()
            } else {
                report.revalidation.note.clone()
            }
        ));
    }
    out.push_str("\n## Limitations\n\n");
    for l in &report.limitations {
        out.push_str(&format!("- {l}\n"));
    }
    out
}

/// Serialization helper used by tests and callers that need the report hash
/// basis (canonical, float-free).
pub fn hash_report(report: &VerificationReportDto) -> Result<String, CccError> {
    hash_canonical(report).map_err(|e| CccError::internal_error(format!("hash failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccc_config::IndexingConfig;
    use ccc_indexer::run_index;
    use ccc_repository::{ChangeEntry, GitCli};
    use ccc_snapshot::{find_or_create_snapshot, SnapshotBuilder};
    use ccc_storage::{FsArtifactStore, SqliteStore};
    use std::path::PathBuf;
    use std::process::Command as StdCommand;
    use std::sync::Arc;

    const LIB_V1: &str = "pub mod util;\npub fn run_app() { util::read_config(); }\n";
    const UTIL_V1: &str = "pub fn read_config() -> u8 { 1 }\n";
    const ISOLATED_V1: &str = "pub fn lonely() -> u8 { 7 }\n";
    const SMOKE_V1: &str = "use crate::run_app;\n#[test]\nfn smoke_run_app() { run_app(); }\n";

    /// A GitProvider that never spawns a process (for the poisoned-PATH test
    /// that proves analysis mode creates zero subprocesses).
    struct StubGit {
        entries: Vec<ChangeEntry>,
    }
    impl GitProvider for StubGit {
        fn is_git_repository(&self, _root: &Path) -> bool {
            true
        }
        fn rev_parse_head(&self, _root: &Path) -> Result<Option<String>, CccError> {
            Ok(Some("0123456789abcdef0123456789abcdef01234567".into()))
        }
        fn current_branch(&self, _root: &Path) -> Result<Option<String>, CccError> {
            Ok(Some("main".into()))
        }
        fn status_porcelain(
            &self,
            _root: &Path,
        ) -> Result<Vec<ccc_repository::StatusEntry>, CccError> {
            Ok(Vec::new())
        }
        fn ls_files(
            &self,
            _root: &Path,
            _include_untracked: bool,
        ) -> Result<Vec<String>, CccError> {
            Ok(Vec::new())
        }
        fn diff_name_status(
            &self,
            _root: &Path,
            _base: &str,
            _head: Option<&str>,
        ) -> Result<Vec<ChangeEntry>, CccError> {
            Ok(self.entries.clone())
        }
        fn rev_exists(&self, _root: &Path, _rev: &str) -> Result<bool, CccError> {
            Ok(true)
        }
    }

    struct Fixture {
        _guard: tempfile::TempDir,
        root: PathBuf,
        store: Arc<SqliteStore>,
        artifacts: FsArtifactStore,
        repo: RepositoryRecord,
        cfg: IndexingConfig,
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
        std::fs::create_dir_all(root.join("tests")).unwrap();
        run(&root, &["init", "--initial-branch=main"]);
        run(&root, &["config", "user.email", "t@e.com"]);
        run(&root, &["config", "user.name", "t"]);
        std::fs::write(root.join("src/lib.rs"), LIB_V1).unwrap();
        std::fs::write(root.join("src/util.rs"), UTIL_V1).unwrap();
        std::fs::write(root.join("src/isolated.rs"), ISOLATED_V1).unwrap();
        std::fs::write(root.join("tests/smoke.rs"), SMOKE_V1).unwrap();
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

    fn util_modified() -> Vec<ChangeEntry> {
        vec![ChangeEntry {
            kind: ChangeKind::Modified,
            path: "src/util.rs".into(),
            old_path: None,
        }]
    }

    fn lib_modified() -> Vec<ChangeEntry> {
        vec![ChangeEntry {
            kind: ChangeKind::Modified,
            path: "src/lib.rs".into(),
            old_path: None,
        }]
    }

    fn isolated_modified() -> Vec<ChangeEntry> {
        vec![ChangeEntry {
            kind: ChangeKind::Modified,
            path: "src/isolated.rs".into(),
            old_path: None,
        }]
    }

    fn engine<'a>(
        fx: &'a Fixture,
        git: &'a dyn GitProvider,
        verifications: &'a dyn VerificationStore,
    ) -> VerificationEngine<'a> {
        VerificationEngine::new(
            &*fx.store,
            &*fx.store,
            verifications,
            &*fx.store,
            &fx.artifacts,
            git,
            fx.cfg.traversal_depth,
        )
    }

    /// Environment variables are process-global; tests that poison them must
    /// not run concurrently with each other.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn analysis_mode_spawns_zero_subprocesses_and_reports_not_run() {
        let _env_guard = ENV_LOCK.lock().await;
        let fx = setup().await;
        // Change util.rs (not referenced by the smoke test) and commit.
        std::fs::write(
            fx.root.join("src/util.rs"),
            "pub fn read_config() -> u8 { 2 }\n",
        )
        .unwrap();
        run(&fx.root, &["add", "-A"]);
        run(&fx.root, &["commit", "-m", "change util"]);
        // Reindex so the active generation maps util.rs to symbols.
        index_current(&fx.store, &fx.artifacts, &fx.repo, &fx.cfg, &fx.root).await;

        let verifications = ccc_storage::test_support::InMemoryVerificationStore::new();
        let git = StubGit {
            entries: util_modified(),
        };
        // Poisoned PATH: ANY subprocess spawn (git, cargo, shell) fails. The
        // engine must succeed purely on stored analysis (TIS §11.2 default).
        let saved_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", "");
        let result = engine(&fx, &git, &verifications)
            .verify(
                &fx.repo,
                &fx.root,
                "HEAD~1",
                Some("HEAD"),
                &VerificationPolicy::analysis(),
            )
            .await;
        std::env::set_var("PATH", saved_path);

        let report = result.expect("analysis-mode verify must succeed with zero subprocesses");
        assert_eq!(report.mode, VerificationMode::Analysis);
        assert!(!report.checks.is_empty(), "suggestions are present");
        assert!(
            report
                .checks
                .iter()
                .all(|c| c.status == VerificationStatus::NotRun.as_str()),
            "nothing executed under the analysis policy: {:?}",
            report.checks
        );
        assert_eq!(
            report.statuses.get("not_run"),
            Some(&(report.checks.len() as u32))
        );
        assert_eq!(report.statuses.get("passed"), Some(&0));
        assert!(report
            .changed_symbols
            .iter()
            .any(|s| s.contains("read_config")));
        assert!(
            report
                .limitations
                .iter()
                .any(|l| l.contains("no command was executed")),
            "the analysis limitation is stated, never hidden"
        );

        // The record is published and its artifact retrievable.
        let record = verifications
            .get_verification(VerificationId::from_str(&report.verification_id).unwrap())
            .await
            .unwrap()
            .expect("verification record persisted");
        assert_eq!(record.policy_name, "analysis");
        assert_eq!(record.policy_hash, report.policy_hash);
        let artifact = fx
            .artifacts
            .get(&record.artifact_hash)
            .await
            .unwrap()
            .unwrap();
        let round_trip: VerificationReportDto = serde_json::from_slice(&artifact).unwrap();
        assert_eq!(round_trip.verification_id, report.verification_id);
    }

    #[tokio::test]
    async fn selection_labels_direct_tests_and_fallback_honestly() {
        let _env_guard = ENV_LOCK.lock().await;
        let fx = setup().await;
        index_current(&fx.store, &fx.artifacts, &fx.repo, &fx.cfg, &fx.root).await;

        // Changing run_app (exercised by tests/smoke.rs) yields a directly
        // related test module plus the always-present build check.
        let verifications = ccc_storage::test_support::InMemoryVerificationStore::new();
        let git = StubGit {
            entries: lib_modified(),
        };
        let report = engine(&fx, &git, &verifications)
            .verify(
                &fx.repo,
                &fx.root,
                "HEAD",
                Some("HEAD"),
                &VerificationPolicy::analysis(),
            )
            .await
            .unwrap();
        let smoke = report
            .checks
            .iter()
            .find(|c| c.command.contains("--test smoke"))
            .expect("smoke test module selected via tests edge");
        assert_eq!(smoke.relation, "directly");
        assert!(report
            .checks
            .iter()
            .any(|c| c.command == "cargo build" && c.relation == "directly"));
        assert!(
            !report.checks.iter().any(|c| c.relation == "fallback"),
            "a targeted selection exists, so no fallback is offered"
        );

        // Changing isolated.rs (no callers, no test references at all) yields
        // the broad suite as an explicitly labeled fallback.
        let git = StubGit {
            entries: isolated_modified(),
        };
        let report = engine(&fx, &git, &verifications)
            .verify(
                &fx.repo,
                &fx.root,
                "HEAD",
                Some("HEAD"),
                &VerificationPolicy::analysis(),
            )
            .await
            .unwrap();
        let fallback = report
            .checks
            .iter()
            .find(|c| c.command == "cargo test")
            .expect("broad suite offered when nothing targeted exists");
        assert_eq!(fallback.relation, "fallback");
        assert!(fallback.note.contains("not executed") || !fallback.note.is_empty());
        // `historically` is reserved for the history channel (not in this step).
        assert!(report.checks.iter().all(|c| c.relation != "historically"));
    }

    #[tokio::test]
    async fn patch_analysis_compares_with_prior_and_lists_newly_stale_claims() {
        let _env_guard = ENV_LOCK.lock().await;
        let fx = setup().await;

        // First patch: change util.rs → claims depending on it go stale.
        std::fs::write(
            fx.root.join("src/util.rs"),
            "pub fn read_config() -> u8 { 2 }\n",
        )
        .unwrap();
        run(&fx.root, &["add", "-A"]);
        run(&fx.root, &["commit", "-m", "change util"]);
        index_current(&fx.store, &fx.artifacts, &fx.repo, &fx.cfg, &fx.root).await;

        let verifications = ccc_storage::test_support::InMemoryVerificationStore::new();
        let git = StubGit {
            entries: util_modified(),
        };
        let first = engine(&fx, &git, &verifications)
            .verify(
                &fx.repo,
                &fx.root,
                "HEAD~1",
                Some("HEAD"),
                &VerificationPolicy::analysis(),
            )
            .await
            .unwrap();
        assert!(
            !first.newly_stale_claims.is_empty(),
            "claims depending on changed files are listed as newly stale"
        );
        assert!(first.impact_comparison.prior_verification_id.is_none());
        assert!(!first.impact_comparison.new_symbols.is_empty());

        // Second patch touches lib.rs: comparison against the prior record.
        std::fs::write(
            fx.root.join("src/lib.rs"),
            "pub mod util;\npub fn run_app() { util::read_config(); }\npub fn extra() {}\n",
        )
        .unwrap();
        run(&fx.root, &["add", "-A"]);
        run(&fx.root, &["commit", "-m", "extend lib"]);
        index_current(&fx.store, &fx.artifacts, &fx.repo, &fx.cfg, &fx.root).await;

        let git = StubGit {
            entries: lib_modified(),
        };
        let second = engine(&fx, &git, &verifications)
            .verify(
                &fx.repo,
                &fx.root,
                "HEAD~1",
                Some("HEAD"),
                &VerificationPolicy::analysis(),
            )
            .await
            .unwrap();
        let prior_id = second
            .impact_comparison
            .prior_verification_id
            .expect("prior verification compared");
        assert_eq!(prior_id, first.verification_id);
        assert!(
            second
                .impact_comparison
                .new_symbols
                .iter()
                .all(|s| !first.changed_symbols.contains(s)),
            "new symbols were absent from the prior record"
        );
        assert!(
            second
                .observed_vs_predicted
                .iter()
                .any(|n| n.contains("unverified by execution")),
            "analysis mode is explicit that predictions are unverified"
        );
    }

    // -- T4: evidence revalidation with a real cargo project ----------------

    const CARGO_TOML: &str = "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n";
    const GITIGNORE: &str = "/target\nCargo.lock\n";

    /// Fixture that is a real cargo project so authorized `cargo build` works.
    async fn setup_cargo() -> Fixture {
        let guard = tempfile::tempdir().unwrap();
        let root = guard.path().join("repo");
        std::fs::create_dir_all(root.join("src")).unwrap();
        run(&root, &["init", "--initial-branch=main"]);
        run(&root, &["config", "user.email", "t@e.com"]);
        run(&root, &["config", "user.name", "t"]);
        std::fs::write(root.join("Cargo.toml"), CARGO_TOML).unwrap();
        std::fs::write(root.join(".gitignore"), GITIGNORE).unwrap();
        std::fs::write(root.join("src/lib.rs"), LIB_V1).unwrap();
        std::fs::write(root.join("src/util.rs"), UTIL_V1).unwrap();
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
        }
    }

    fn cargo_policy() -> VerificationPolicy {
        VerificationPolicy {
            name: "local-build".into(),
            allow_execution: true,
            allowlist: vec!["cargo".into()],
            disallowlist: vec![],
            timeout_secs: 300,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            env_allowlist: vec!["PATH".into(), "CARGO_HOME".into(), "RUSTUP_HOME".into()],
            revalidate_claims: true,
        }
    }

    #[tokio::test]
    async fn authorized_pass_revalidates_covered_claims_scoped() {
        let _env_guard = ENV_LOCK.lock().await;
        let fx = setup_cargo().await;
        // Patch util.rs, commit, reindex: old claims go stale, the active
        // generation gains fresh candidate claims.
        std::fs::write(
            fx.root.join("src/util.rs"),
            "pub fn read_config() -> u8 { 2 }\n",
        )
        .unwrap();
        run(&fx.root, &["add", "-A"]);
        run(&fx.root, &["commit", "-m", "change util"]);
        index_current(&fx.store, &fx.artifacts, &fx.repo, &fx.cfg, &fx.root).await;

        let verifications = ccc_storage::test_support::InMemoryVerificationStore::new();
        let git = StubGit {
            entries: util_modified(),
        };
        let report = engine(&fx, &git, &verifications)
            .verify(&fx.repo, &fx.root, "HEAD~1", Some("HEAD"), &cargo_policy())
            .await
            .unwrap();

        // cargo build + the selected checks actually ran and passed.
        assert_eq!(report.mode, VerificationMode::Authorized);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.command == "cargo build" && c.status == "passed"),
            "cargo build executed and passed: {:?}",
            report.checks
        );
        assert!(
            report.checks.iter().all(|c| c.status == "passed"),
            "nothing skipped under the covering allowlist: {:?}",
            report.checks
        );

        // Covered claims (active generation, util.rs) are now supported with
        // validated_at; unrelated claims (lib.rs) are untouched candidates.
        let claims = fx.store.list_claims(fx.repo.repository_id).await.unwrap();
        let util_claims: Vec<&ClaimRecord> = claims
            .iter()
            .filter(|c| c.text.contains("read_config") && c.status == ClaimStatus::Supported)
            .collect();
        assert!(
            !util_claims.is_empty(),
            "at least one util.rs claim became supported"
        );
        for claim in &util_claims {
            assert!(claim.validated_at.is_some(), "validated_at recorded");
        }
        // The verification link chains via evidence rows (type verification).
        let evidence = fx.store.list_evidence(fx.repo.repository_id).await.unwrap();
        let links: Vec<&EvidenceRecord> = evidence
            .iter()
            .filter(|e| {
                e.evidence_type == EvidenceType::Verification
                    && e.source_ref
                        .starts_with(&format!("verification:{}", report.verification_id))
            })
            .collect();
        assert!(
            links.len() >= util_claims.len(),
            "each supported claim chains to the verification record"
        );
        assert!(links
            .iter()
            .all(|e| e.validation_outcome.as_deref() == Some("passed")));

        // Scoped: claims that depend only on lib.rs stay candidates.
        let lib_claims: Vec<&ClaimRecord> = claims
            .iter()
            .filter(|c| c.text.contains("run_app"))
            .collect();
        assert!(
            lib_claims
                .iter()
                .all(|c| c.status == ClaimStatus::Candidate),
            "unrelated claims are untouched by the verified change: {:?}",
            lib_claims.iter().map(|c| c.status).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn failing_check_conflicts_claims_and_negatives_survive_a_later_pass() {
        let _env_guard = ENV_LOCK.lock().await;
        let fx = setup_cargo().await;
        // Broken patch: util.rs no longer compiles.
        std::fs::write(
            fx.root.join("src/util.rs"),
            "pub fn read_config() -> u8 { absolutely_not_defined(); }\n",
        )
        .unwrap();
        run(&fx.root, &["add", "-A"]);
        run(&fx.root, &["commit", "-m", "broken change"]);
        index_current(&fx.store, &fx.artifacts, &fx.repo, &fx.cfg, &fx.root).await;

        let verifications = ccc_storage::test_support::InMemoryVerificationStore::new();
        let git = StubGit {
            entries: util_modified(),
        };
        let failed = engine(&fx, &git, &verifications)
            .verify(&fx.repo, &fx.root, "HEAD~1", Some("HEAD"), &cargo_policy())
            .await
            .unwrap();
        assert!(failed.statuses.get("failed").is_some_and(|n| *n >= 1));
        assert_eq!(failed.statuses.get("passed"), Some(&0));
        assert!(
            !failed.revalidation.conflicted_claims.is_empty(),
            "covered claims are conflicted by the failing check"
        );
        let claims = fx.store.list_claims(fx.repo.repository_id).await.unwrap();
        assert!(claims
            .iter()
            .any(|c| c.text.contains("read_config") && c.status == ClaimStatus::Conflicted));

        // The negative result is an evidence row with outcome failed.
        let evidence_count_failed = fx
            .store
            .list_evidence(fx.repo.repository_id)
            .await
            .unwrap()
            .iter()
            .filter(|e| e.validation_outcome.as_deref() == Some("failed"))
            .count();
        assert!(evidence_count_failed > 0, "negative results are recorded");

        // Repair: a later pass revalidates the covered claims...
        std::fs::write(
            fx.root.join("src/util.rs"),
            "pub fn read_config() -> u8 { 2 }\n",
        )
        .unwrap();
        run(&fx.root, &["add", "-A"]);
        run(&fx.root, &["commit", "-m", "fix util"]);
        index_current(&fx.store, &fx.artifacts, &fx.repo, &fx.cfg, &fx.root).await;
        let git = StubGit {
            entries: util_modified(),
        };
        let repaired = engine(&fx, &git, &verifications)
            .verify(&fx.repo, &fx.root, "HEAD~1", Some("HEAD"), &cargo_policy())
            .await
            .unwrap();
        assert!(repaired.statuses.get("passed").is_some_and(|n| *n >= 1));
        assert!(!repaired.revalidation.supported_claims.is_empty());

        // ...and the earlier failed verification record stays queryable.
        let records = verifications
            .list_verifications(fx.repo.repository_id)
            .await
            .unwrap();
        assert_eq!(records.len(), 2, "both runs are recorded immutably");
        let negatives = fx
            .store
            .list_evidence(fx.repo.repository_id)
            .await
            .unwrap()
            .iter()
            .filter(|e| e.validation_outcome.as_deref() == Some("failed"))
            .count();
        assert_eq!(
            negatives, evidence_count_failed,
            "negative evidence rows are preserved after a later pass"
        );
    }
}
