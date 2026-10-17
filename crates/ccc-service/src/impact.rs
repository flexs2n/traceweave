//! Change-impact analysis (FR-046–048, SDD §12).

use crate::{meta_for, output, output_envelope_as_human, resolve_repo, AppContext, OpOutput};
use ccc_domain::CccError;
use ccc_impact::{render_impact_markdown, ImpactAnalyzer};
use ccc_repository::GitCli;
use std::path::PathBuf;

pub async fn impact_analyze(
    ctx: &AppContext,
    repo_path: Option<&std::path::Path>,
    base: &str,
    head: Option<&str>,
    format: &str,
) -> Result<OpOutput, CccError> {
    let store = &ctx.store;
    let repo = resolve_repo(store, repo_path).await?;
    let root = PathBuf::from(&repo.root_path);
    let key = ccc_jobs::idempotency_key(&[
        "impact_analyze",
        &repo.repository_id.to_string(),
        base,
        head.unwrap_or("working_tree"),
    ]);
    let job =
        ccc_jobs::begin_job(&**store, repo.repository_id, "impact_analyze", &key, None).await?;

    let analyzer = ImpactAnalyzer::new(&**store, &**store, &**store, &GitCli, 3);
    match analyzer
        .analyze(repo.repository_id, &root, base, head)
        .await
    {
        Ok(report) => {
            let _job = if job.state.is_terminal() {
                // Idempotent replay: the logical job already completed.
                job
            } else {
                ccc_jobs::finish_success(&**store, job, Some(&report.impact_id)).await?
            };
            let out = if format == "markdown" {
                let human = render_impact_markdown(&report);
                output(&report, meta_for(&repo, None, None), human)?
            } else {
                output_envelope_as_human(&report, meta_for(&repo, None, None))?
            };
            Ok(out)
        }
        Err(e) => {
            ccc_jobs::finish_failure(&**store, job, &e).await?;
            Err(e)
        }
    }
}
