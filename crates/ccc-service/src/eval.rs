//! Offline evaluation of the retrieval pipeline (SDD §20.2, TIS §19).

use crate::{meta_for, output, resolve_repo, AppContext, OpOutput};
use ccc_domain::*;
use ccc_jobs as jobs;
use ccc_search::FtsSearchBackend;
use ccc_storage::*;
use serde::Deserialize;
use std::str::FromStr as _;

/// A retrieval-policy override file: a `[weights]` table with the policy
/// weight names (ADR-008 — experiments are recorded, never silent edits).
#[derive(Debug, Deserialize)]
struct WeightsFile {
    weights: std::collections::BTreeMap<String, i64>,
}

const KNOWN_WEIGHTS: [&str; 7] = ["w_l", "w_d", "w_t", "w_h", "w_f", "w_r", "w_x"];

fn load_weights_override(
    path: &std::path::Path,
) -> Result<std::collections::BTreeMap<String, i64>, CccError> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        CccError::invalid_request(format!(
            "cannot read policy override {}: {e}",
            path.display()
        ))
    })?;
    let file: WeightsFile = toml::from_str(&raw).map_err(|e| {
        CccError::invalid_request(format!(
            "invalid policy override {} (expected a [weights] table): {e}",
            path.display()
        ))
    })?;
    for key in file.weights.keys() {
        if !KNOWN_WEIGHTS.contains(&key.as_str()) {
            return Err(CccError::invalid_request(format!(
                "policy override {} has unknown weight {key:?}; known: {KNOWN_WEIGHTS:?}",
                path.display()
            )));
        }
    }
    Ok(file.weights)
}

pub async fn eval_run(
    ctx: &AppContext,
    repo_path: Option<&std::path::Path>,
    suite_name: &str,
    policy_path: Option<&str>,
) -> Result<OpOutput, CccError> {
    let store = &ctx.store;
    let artifacts = &*ctx.artifacts;
    let data_dir = &ctx.data_dir;
    let repo = resolve_repo(store, repo_path).await?;
    let cfg = ccc_config::load_config(data_dir)?;
    let config_hash = cfg.indexing().configuration_hash()?;
    let suite_path = ccc_evaluation::resolve_suite_path(data_dir, suite_name)?;
    let suite = ccc_evaluation::load_suite(&suite_path)?;
    let weights_override = match policy_path {
        Some(p) => Some(load_weights_override(std::path::Path::new(p))?),
        None => None,
    };
    // The effective weights hash rides in the idempotency key so an A/B
    // experiment with different weights is a distinct logical job.
    let weights_hash = match &weights_override {
        Some(w) => ccc_schema::hash_canonical(w)
            .map_err(|e| CccError::internal_error(format!("weights hash: {e}")))?,
        None => "current".into(),
    };
    let key = jobs::idempotency_key(&[
        "eval_run",
        &repo.repository_id.to_string(),
        &suite.suite_hash,
        &config_hash,
        &weights_hash,
    ]);
    let job = jobs::begin_job(&**store, repo.repository_id, "eval_run", &key, None).await?;

    let search = FtsSearchBackend::new(&**store, &**store, &**store);
    let runner = ccc_evaluation::EvalRunner::new(
        &**store, &**store, &**store, &**store, artifacts, &search, &**store,
    );
    match runner
        .run_suite(&repo, &suite, &config_hash, weights_override.as_ref())
        .await
    {
        Ok(metrics) => {
            let run = store
                .list_runs(repo.repository_id)
                .await?
                .into_iter()
                .last()
                .ok_or_else(|| CccError::internal_error("run record missing after eval run"))?;
            let _job = if job.state.is_terminal() {
                // Idempotent replay: the logical job already completed.
                job
            } else {
                jobs::finish_success(&**store, job, Some(&run.run_id.to_string())).await?
            };
            let mut human = format!(
                "Evaluation run {} — suite {:?} (hash {})\n",
                run.run_id, metrics.suite_name, metrics.suite_hash
            );
            human.push_str(&format!(
                "Policy: {} (hash {}), config: {}\n",
                metrics.retrieval_policy_version, metrics.policy_hash, metrics.config_hash
            ));
            for group in ["tuning", "held_out"] {
                let g = if group == "tuning" {
                    &metrics.tuning
                } else {
                    &metrics.held_out
                };
                if g.cases == 0 {
                    continue;
                }
                human.push_str(&format!(
                    "{group}: {} case(s) — recall {}‰, MRR {}‰, precision@{} {}‰, stale {}‰, token cost {}\n",
                    g.cases,
                    g.recall_millis / 10,
                    g.mrr_millis / 10,
                    ccc_evaluation::PRECISION_K,
                    g.precision_millis / 10,
                    g.stale_rate_millis / 10,
                    g.token_cost
                ));
            }
            for case in &metrics.cases {
                human.push_str(&format!(
                    "  case {} [{}] — items: {}, matched: {}/{} (recall {}‰), stale {}‰, tokens {}\n",
                    case.case_index,
                    case.kind,
                    case.items,
                    case.matched,
                    case.expected,
                    case.recall_millis / 10,
                    case.stale_rate_millis / 10,
                    case.estimated_tokens
                ));
            }
            let data = serde_json::json!({
                "run_id": run.run_id.to_string(),
                "metrics": metrics,
            });
            output(&data, meta_for(&repo, None, None), human)
        }
        Err(e) => {
            jobs::finish_failure(&**store, job, &e).await?;
            Err(e)
        }
    }
}

pub async fn eval_show(ctx: &AppContext, run_id: &str) -> Result<OpOutput, CccError> {
    let store: &SqliteStore = &ctx.store;
    let id = EvaluationRunId::from_str(run_id)
        .map_err(|_| CccError::invalid_request(format!("invalid run id {run_id:?}")))?;
    let record = store.get_run(id).await?.ok_or_else(|| {
        CccError::snapshot_not_found(format!("evaluation run {run_id} not found"))
    })?;
    let metrics: serde_json::Value =
        serde_json::from_str(&record.metrics_json).unwrap_or(serde_json::json!({}));
    let mut human = format!("Evaluation run {}\n", record.run_id);
    human.push_str(&format!(
        "  suite: {} (hash {})\n",
        record.suite_name, record.suite_hash
    ));
    human.push_str(&format!(
        "  policy: {} (hash {}), config: {}\n",
        record.policy_version, record.policy_hash, record.config_hash
    ));
    human.push_str(&format!("  created: {}\n", record.created_at));
    human.push_str(&format!(
        "  metrics: {}\n",
        serde_json::to_string_pretty(&metrics).unwrap_or_else(|_| record.metrics_json.clone())
    ));
    let data = serde_json::json!({
        "run_id": record.run_id.to_string(),
        "suite_name": record.suite_name,
        "suite_hash": record.suite_hash,
        "policy_version": record.policy_version,
        "policy_hash": record.policy_hash,
        "config_hash": record.config_hash,
        "created_at": record.created_at,
        "metrics": metrics,
    });
    output(&data, ccc_schema::EnvelopeMeta::default(), human)
}
