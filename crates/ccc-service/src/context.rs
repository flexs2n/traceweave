//! Context bundle compilation + inspection + diff (TIS §9.7, SDD §11.4).

use crate::{meta_for, output, output_envelope_as_human, resolve_repo, AppContext, OpOutput};
use ccc_context_compiler::{CompileRequest, ContextCompiler, HeuristicNormalizer};
use ccc_domain::*;
use ccc_jobs as jobs;
use ccc_repository::{GitCli, GitProvider};
use ccc_search::FtsSearchBackend;
use ccc_storage::*;
use std::path::PathBuf;
use std::str::FromStr as _;

pub async fn context_compile(
    ctx: &AppContext,
    repo_path: Option<&std::path::Path>,
    task: &str,
    budget: u64,
    format: &str,
) -> Result<OpOutput, CccError> {
    let store = &ctx.store;
    let artifacts = &*ctx.artifacts;
    let repo = resolve_repo(store, repo_path).await?;
    let key = jobs::idempotency_key(&[
        "context_compile",
        &repo.repository_id.to_string(),
        task,
        &budget.to_string(),
    ]);
    let job = jobs::begin_job(&**store, repo.repository_id, "context_compile", &key, None).await?;

    let search_backend = FtsSearchBackend::new(&**store, &**store, &**store);
    let compiler = ContextCompiler::new(
        &**store,
        store.as_ref(),
        store.as_ref(),
        &**store,
        artifacts,
        &search_backend,
        &HeuristicNormalizer,
    );
    let compile_started = std::time::Instant::now();
    // History recency (H feature): optional data; with w_h = 0 bundles are
    // byte-identical with or without it (T9 constraint).
    let root = PathBuf::from(&repo.root_path);
    let history_scores = GitCli
        .log_name_status(&root, 200)
        .map(|log| ccc_repository::history_recency_millis(&log))
        .unwrap_or_default();
    match compiler
        .compile(&CompileRequest {
            repository_id: repo.repository_id,
            task_text: task.to_string(),
            budget_tokens: budget,
            weights_override: None,
            history_scores: Some(history_scores),
        })
        .await
    {
        Ok(bundle) => {
            let compile_latency_ms = compile_started.elapsed().as_millis() as u64;
            let payload = serde_json::json!({
                "bundle_id": bundle.bundle_id,
                "latency_ms": compile_latency_ms,
            });
            let _job = if job.state.is_terminal() {
                // Idempotent replay: the logical job already completed.
                job
            } else {
                jobs::finish_success(&**store, job, Some(&payload.to_string())).await?
            };
            let out = if format == "markdown" {
                let human = ccc_context_compiler::bundle_markdown(&bundle);
                output(&bundle, meta_for(&repo, None, None), human)?
            } else {
                output_envelope_as_human(&bundle, meta_for(&repo, None, None))?
            };
            Ok(out)
        }
        Err(e) => {
            jobs::finish_failure(&**store, job, &e).await?;
            Err(e)
        }
    }
}

pub async fn context_show(ctx: &AppContext, bundle_id: &str) -> Result<OpOutput, CccError> {
    let store: &SqliteStore = &ctx.store;
    let id = BundleId::from_str(bundle_id)
        .map_err(|_| CccError::invalid_request(format!("invalid bundle id {bundle_id:?}")))?;
    let record = store
        .get_bundle(id)
        .await?
        .ok_or_else(|| CccError::snapshot_not_found(format!("bundle {bundle_id} not found")))?;
    let items_json = store.get_bundle_items(id).await?;
    let items: Vec<ccc_schema::BundleItem> = serde_json::from_str(&items_json).unwrap_or_default();
    let mut human = format!(
        "Bundle {} — task: {:?} — {} items, ~{} tokens ({}), stopping: {}\n",
        record.bundle_id,
        record.task_text,
        items.len(),
        record.estimated_tokens,
        record.token_estimate_basis,
        record.stopping_reason
    );
    for item in &items {
        human.push_str(&format!(
            "{}. [{}] {} — {}:{} — evidence: {} — why: {}\n",
            item.position + 1,
            item.channel,
            item.title,
            item.path,
            item.span.map(|s| s.start_line).unwrap_or(0),
            item.evidence_status,
            item.selection_reason
        ));
    }
    let data = serde_json::json!({
        "bundle": record,
        "items": items,
    });
    output(&data, ccc_schema::EnvelopeMeta::default(), human)
}

pub async fn context_diff(ctx: &AppContext, a: &str, b: &str) -> Result<OpOutput, CccError> {
    let store: &SqliteStore = &ctx.store;
    let id_a = BundleId::from_str(a)
        .map_err(|_| CccError::invalid_request(format!("invalid bundle id {a:?}")))?;
    let id_b = BundleId::from_str(b)
        .map_err(|_| CccError::invalid_request(format!("invalid bundle id {b:?}")))?;
    let bundle_a = store
        .get_bundle(id_a)
        .await?
        .ok_or_else(|| CccError::snapshot_not_found(format!("bundle {a} not found")))?;
    let bundle_b = store
        .get_bundle(id_b)
        .await?
        .ok_or_else(|| CccError::snapshot_not_found(format!("bundle {b} not found")))?;
    let items_a: Vec<ccc_schema::BundleItem> =
        serde_json::from_str(&store.get_bundle_items(id_a).await?).unwrap_or_default();
    let items_b: Vec<ccc_schema::BundleItem> =
        serde_json::from_str(&store.get_bundle_items(id_b).await?).unwrap_or_default();

    // Join key is the logical identity (kind + path + title), stable across
    // snapshot-scoped entity ids — a reindex regenerates ids, not items.
    let by_item = |items: &[ccc_schema::BundleItem]| {
        items
            .iter()
            .map(|i| (format!("{}|{}|{}", i.item_kind, i.path, i.title), i.clone()))
            .collect::<std::collections::HashMap<String, ccc_schema::BundleItem>>()
    };
    let map_a = by_item(&items_a);
    let map_b = by_item(&items_b);

    let added: Vec<&ccc_schema::BundleItem> = items_b
        .iter()
        .filter(|i| !map_a.contains_key(&format!("{}|{}|{}", i.item_kind, i.path, i.title)))
        .collect();
    let removed: Vec<&ccc_schema::BundleItem> = items_a
        .iter()
        .filter(|i| !map_b.contains_key(&format!("{}|{}|{}", i.item_kind, i.path, i.title)))
        .collect();
    let mut changed: Vec<serde_json::Value> = Vec::new();
    for item in &items_b {
        let key = format!("{}|{}|{}", item.item_kind, item.path, item.title);
        if let Some(old) = map_a.get(&key) {
            let content_changed = old.provenance.content_hash != item.provenance.content_hash;
            let status_changed = old.evidence_status != item.evidence_status;
            if content_changed || status_changed {
                changed.push(serde_json::json!({
                    "entity_id": item.entity_id,
                    "title": item.title,
                    "path": item.path,
                    "content_changed": content_changed,
                    "evidence_status": {
                        "from": old.evidence_status,
                        "to": item.evidence_status,
                    },
                }));
            }
        }
    }

    let mut human = format!(
        "Context diff {} → {}\n",
        bundle_a.bundle_id, bundle_b.bundle_id
    );
    human.push_str(&format!("added ({}):\n", added.len()));
    for i in &added {
        human.push_str(&format!(
            "  + {} ({}, evidence: {})\n",
            i.title, i.path, i.evidence_status
        ));
    }
    human.push_str(&format!("removed ({}):\n", removed.len()));
    for i in &removed {
        human.push_str(&format!(
            "  - {} ({}, evidence: {})\n",
            i.title, i.path, i.evidence_status
        ));
    }
    human.push_str(&format!("changed ({}):\n", changed.len()));
    for c in &changed {
        human.push_str(&format!(
            "  ~ {} — evidence: {} → {}{}\n",
            c["title"].as_str().unwrap_or_default(),
            c["evidence_status"]["from"].as_str().unwrap_or_default(),
            c["evidence_status"]["to"].as_str().unwrap_or_default(),
            if c["content_changed"].as_bool().unwrap_or(false) {
                " (content changed)"
            } else {
                ""
            }
        ));
    }
    let data = serde_json::json!({
        "bundle_a": a,
        "bundle_b": b,
        "added": added,
        "removed": removed,
        "changed": changed,
    });
    output(&data, ccc_schema::EnvelopeMeta::default(), human)
}
