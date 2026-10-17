//! Engineering memory (FR-032/033/034/035, SDD §9) + conflict listing
//! (ui.md UI-018).

use crate::{meta_for, output, parse_claim_status, resolve_repo, AppContext, OpOutput};
use ccc_domain::*;
use ccc_jobs as jobs;
use ccc_storage::*;
use std::str::FromStr as _;

#[allow(clippy::too_many_arguments)]
pub async fn memory_add(
    ctx: &AppContext,
    repo_path: Option<&std::path::Path>,
    title: &str,
    summary: &str,
    memory_type: &str,
    scope: &str,
    created_by: &str,
    sharing_policy: &str,
    sensitivity: &str,
    claim_ids: &[String],
    idempotency_key: Option<&str>,
) -> Result<OpOutput, CccError> {
    let store = &ctx.store;
    let repo = resolve_repo(store, repo_path).await?;
    let snapshot_id = crate::active_snapshot_id(store, &repo).await?;
    let claim_ids = parse_claim_ids(claim_ids)?;
    let draft = memory_draft_from(
        title,
        summary,
        memory_type,
        scope,
        created_by,
        sharing_policy,
        sensitivity,
    )?;
    let mut key_parts: Vec<String> = vec![
        "memory_add".into(),
        repo.repository_id.to_string(),
        draft.title.clone(),
        draft.summary.clone(),
        draft.memory_type.as_str().into(),
        draft.scope.clone(),
        draft.created_by.clone(),
        claim_ids
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(","),
    ];
    key_parts.sort();
    let key = idempotency_key.map(str::to_string).unwrap_or_else(|| {
        jobs::idempotency_key(&key_parts.iter().map(String::as_str).collect::<Vec<_>>())
    });
    let job = jobs::begin_job(&**store, repo.repository_id, "memory_add", &key, None).await?;

    match ccc_memory::create_memory_with_detection(
        store.as_ref(),
        store.as_ref(),
        repo.repository_id,
        snapshot_id,
        &claim_ids,
        draft,
    )
    .await
    {
        Ok(mutation) => {
            let _job = if job.state.is_terminal() {
                job
            } else {
                jobs::finish_success(&**store, job, Some(&mutation.record.memory_id.to_string()))
                    .await?
            };
            memory_mutation_output(&mutation, &repo, Some(snapshot_id))
        }
        Err(e) => {
            jobs::finish_failure(&**store, job, &e).await?;
            Err(e)
        }
    }
}

/// Builds a memory-mutation output; the detected-conflict count is part of
/// the output — new disagreements surface immediately, never silently
/// (FR-034).
fn memory_mutation_output(
    mutation: &ccc_memory::MemoryMutation,
    repo: &RepositoryRecord,
    snapshot_id: Option<SnapshotId>,
) -> Result<OpOutput, CccError> {
    let detected = mutation.conflicts.conflicted.len();
    let value = serde_json::to_value(&mutation.record)
        .map_err(|e| CccError::internal_error(format!("serialize memory: {e}")))?;
    let mut object = match value {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    object.insert("detected_conflicts".into(), serde_json::json!(detected));
    let data = serde_json::Value::Object(object);
    let human = memory_human(&mutation.record) + &memory_conflict_note(detected);
    output(&data, meta_for(repo, snapshot_id, None), human)
}

fn memory_conflict_note(detected: usize) -> String {
    if detected > 0 {
        format!(
            "Detected {detected} conflict pair(s) — inspect with `ccc conflicts`; \
             conflicts are never auto-resolved.\n"
        )
    } else {
        String::new()
    }
}

fn memory_human(record: &MemoryRecord) -> String {
    let mut human = String::new();
    human.push_str(&format!("Memory {}\n", record.memory_id));
    human.push_str(&format!("  title: {}\n", record.title));
    human.push_str(&format!("  type: {}\n", record.memory_type.as_str()));
    human.push_str(&format!(
        "  status: {}\n",
        serde_json::to_string(&record.status)
            .unwrap_or_default()
            .trim_matches('"')
    ));
    human.push_str(&format!(
        "  scope: {}\n",
        if record.scope.is_empty() {
            "(repository-wide)"
        } else {
            &record.scope
        }
    ));
    human.push_str(&format!("  created_by: {}\n", record.created_by));
    human.push_str(&format!("  summary: {}\n", record.summary));
    human
}

pub async fn memory_list(
    ctx: &AppContext,
    repo_path: Option<&std::path::Path>,
    status: Option<&str>,
    memory_type: Option<&str>,
    limit: usize,
) -> Result<OpOutput, CccError> {
    let store: &SqliteStore = &ctx.store;
    let repo = resolve_repo(store, repo_path).await?;
    let status_filter = status.map(parse_claim_status).transpose()?;
    let type_filter = memory_type.map(MemoryType::parse).transpose()?;
    let mut records = match status_filter {
        Some(s) => {
            store
                .list_memories_by_status(repo.repository_id, &[s])
                .await?
        }
        None => store.list_memories(repo.repository_id).await?,
    };
    if let Some(t) = type_filter {
        records.retain(|m| m.memory_type == t);
    }
    records.truncate(limit);
    let mut human = String::new();
    if records.is_empty() {
        human.push_str("No memory records.\n");
    }
    for m in &records {
        human.push_str(&format!(
            "{} [{}] {} ({}) — {}\n",
            m.memory_id,
            serde_json::to_string(&m.status)
                .unwrap_or_default()
                .trim_matches('"'),
            m.title,
            m.memory_type.as_str(),
            m.summary
        ));
    }
    output(&records, meta_for(&repo, None, None), human)
}

pub async fn memory_show(ctx: &AppContext, memory_id: &str) -> Result<OpOutput, CccError> {
    let store: &SqliteStore = &ctx.store;
    let id = MemoryId::from_str(memory_id)
        .map_err(|_| CccError::invalid_request(format!("invalid memory id {memory_id:?}")))?;
    let record = store
        .get_memory(id)
        .await?
        .ok_or_else(|| CccError::snapshot_not_found(format!("memory {memory_id} not found")))?;
    let claim_ids = store.claim_ids_for_memory(id).await?;
    let events = store.list_memory_status_events(id).await?;
    // last_accessed_at records retrieval (SDD §5.11).
    store.touch_memory(id, &now_rfc3339()).await?;
    let mut human = String::new();
    human.push_str(&format!("Memory {}\n", record.memory_id));
    human.push_str(&format!("  title: {}\n", record.title));
    human.push_str(&format!(
        "  status: {}\n",
        serde_json::to_string(&record.status)
            .unwrap_or_default()
            .trim_matches('"')
    ));
    if let Some(cause) = &record.stale_cause {
        human.push_str(&format!("  cause: {cause}\n"));
    }
    human.push_str(&format!("  type: {}\n", record.memory_type.as_str()));
    human.push_str(&format!(
        "  scope: {}\n",
        if record.scope.is_empty() {
            "(repository-wide)"
        } else {
            &record.scope
        }
    ));
    human.push_str(&format!(
        "  created_by: {} at {}\n",
        record.created_by, record.created_at
    ));
    human.push_str(&format!(
        "  last_accessed_at: {}\n",
        record.last_accessed_at
    ));
    human.push_str(&format!("  summary: {}\n", record.summary));
    if let Some(superseded_id) = record.supersedes_memory_id {
        human.push_str(&format!("  supersedes: {superseded_id}\n"));
    }
    human.push_str(&format!(
        "  claims: {}\n",
        if claim_ids.is_empty() {
            "(none — unverified note)"
        } else {
            ""
        }
    ));
    for c in &claim_ids {
        human.push_str(&format!("    - {c}\n"));
    }
    for e in &events {
        human.push_str(&format!(
            "  event: {} → {} by {}{}\n",
            e.created_at,
            serde_json::to_string(&e.status)
                .unwrap_or_default()
                .trim_matches('"'),
            e.actor,
            e.reason
                .as_deref()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default()
        ));
    }
    let data = serde_json::json!({
        "memory": record,
        "claim_ids": claim_ids.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
        "status_events": events,
    });
    output(&data, ccc_schema::EnvelopeMeta::default(), human)
}

pub async fn memory_search(
    ctx: &AppContext,
    repo_path: Option<&std::path::Path>,
    query: &str,
    status: Option<&str>,
    memory_type: Option<&str>,
    limit: u32,
) -> Result<OpOutput, CccError> {
    let store: &SqliteStore = &ctx.store;
    let repo = resolve_repo(store, repo_path).await?;
    let status_filter = status.map(parse_claim_status).transpose()?;
    let type_filter = memory_type.map(MemoryType::parse).transpose()?;
    let records = store
        .search_memories(repo.repository_id, query, status_filter, type_filter, limit)
        .await?;
    let now = now_rfc3339();
    for m in &records {
        store.touch_memory(m.memory_id, &now).await?;
    }
    let mut human = String::new();
    if records.is_empty() {
        human.push_str(&format!("No memory hits for {query:?}.\n"));
    }
    for m in &records {
        human.push_str(&format!(
            "{} [{}] {} — {}\n",
            m.memory_id,
            serde_json::to_string(&m.status)
                .unwrap_or_default()
                .trim_matches('"'),
            m.title,
            m.summary
        ));
    }
    output(&records, meta_for(&repo, None, None), human)
}

#[allow(clippy::too_many_arguments)]
pub async fn memory_supersede(
    ctx: &AppContext,
    repo_path: Option<&std::path::Path>,
    memory_id: &str,
    title: &str,
    summary: &str,
    memory_type: &str,
    scope: &str,
    created_by: &str,
    sharing_policy: &str,
    sensitivity: &str,
    claim_ids: &[String],
    reason: Option<&str>,
    idempotency_key: Option<&str>,
) -> Result<OpOutput, CccError> {
    let store = &ctx.store;
    let repo = resolve_repo(store, repo_path).await?;
    let snapshot_id = crate::active_snapshot_id(store, &repo).await?;
    let old_id = MemoryId::from_str(memory_id)
        .map_err(|_| CccError::invalid_request(format!("invalid memory id {memory_id:?}")))?;
    let claim_ids = parse_claim_ids(claim_ids)?;
    let draft = memory_draft_from(
        title,
        summary,
        memory_type,
        scope,
        created_by,
        sharing_policy,
        sensitivity,
    )?;
    let key = idempotency_key.map(str::to_string).unwrap_or_else(|| {
        jobs::idempotency_key(&[
            "memory_supersede",
            &repo.repository_id.to_string(),
            &old_id.to_string(),
            &draft.title,
            &draft.summary,
            draft.memory_type.as_str(),
            &draft.scope,
            &draft.created_by,
            reason.unwrap_or(""),
        ])
    });
    let job = jobs::begin_job(&**store, repo.repository_id, "memory_supersede", &key, None).await?;

    match ccc_memory::supersede_memory(
        store.as_ref(),
        store.as_ref(),
        repo.repository_id,
        snapshot_id,
        old_id,
        &claim_ids,
        ccc_memory::MemoryEdit {
            draft,
            reason: reason.map(str::to_string),
        },
    )
    .await
    {
        Ok(mutation) => {
            let _job = if job.state.is_terminal() {
                job
            } else {
                jobs::finish_success(&**store, job, Some(&mutation.record.memory_id.to_string()))
                    .await?
            };
            memory_mutation_output(&mutation, &repo, Some(snapshot_id))
        }
        Err(e) => {
            jobs::finish_failure(&**store, job, &e).await?;
            Err(e)
        }
    }
}

pub async fn memory_retract(
    ctx: &AppContext,
    repo_path: Option<&std::path::Path>,
    memory_id: &str,
    actor: &str,
    reason: Option<&str>,
    idempotency_key: Option<&str>,
) -> Result<OpOutput, CccError> {
    let store = &ctx.store;
    let repo = resolve_repo(store, repo_path).await?;
    let id = MemoryId::from_str(memory_id)
        .map_err(|_| CccError::invalid_request(format!("invalid memory id {memory_id:?}")))?;
    let key = idempotency_key.map(str::to_string).unwrap_or_else(|| {
        jobs::idempotency_key(&[
            "memory_retract",
            &repo.repository_id.to_string(),
            &id.to_string(),
            actor,
            reason.unwrap_or(""),
        ])
    });
    let job = jobs::begin_job(&**store, repo.repository_id, "memory_retract", &key, None).await?;

    match ccc_memory::retract_memory(store.as_ref(), id, actor, reason).await {
        Ok(record) => {
            let _job = if job.state.is_terminal() {
                job
            } else {
                jobs::finish_success(&**store, job, Some(&record.memory_id.to_string())).await?
            };
            let human = memory_human(&record);
            output(&record, meta_for(&repo, None, None), human)
        }
        Err(e) => {
            jobs::finish_failure(&**store, job, &e).await?;
            Err(e)
        }
    }
}

pub async fn memory_resolve(
    ctx: &AppContext,
    repo_path: Option<&std::path::Path>,
    keep: &str,
    drop: &str,
    rationale: &str,
    actor: &str,
    idempotency_key: Option<&str>,
) -> Result<OpOutput, CccError> {
    let store = &ctx.store;
    let repo = resolve_repo(store, repo_path).await?;
    let keep_id = MemoryId::from_str(keep)
        .map_err(|_| CccError::invalid_request(format!("invalid memory id {keep:?}")))?;
    let drop_id = MemoryId::from_str(drop)
        .map_err(|_| CccError::invalid_request(format!("invalid memory id {drop:?}")))?;
    let key = idempotency_key.map(str::to_string).unwrap_or_else(|| {
        jobs::idempotency_key(&[
            "memory_resolve",
            &repo.repository_id.to_string(),
            &keep_id.to_string(),
            &drop_id.to_string(),
            rationale,
            actor,
        ])
    });
    let job = jobs::begin_job(&**store, repo.repository_id, "memory_resolve", &key, None).await?;

    match ccc_memory::resolve_conflict(
        store.as_ref(),
        repo.repository_id,
        keep_id,
        drop_id,
        rationale,
        actor,
    )
    .await
    {
        Ok(resolution) => {
            let _job = if job.state.is_terminal() {
                job
            } else {
                jobs::finish_success(
                    &**store,
                    job,
                    Some(&resolution.dropped.memory_id.to_string()),
                )
                .await?
            };
            let human = format!(
                "Resolution recorded: {} retracted; {} kept (returns to candidate).\n",
                resolution.dropped.memory_id, resolution.kept.memory_id
            );
            output(&resolution, meta_for(&repo, None, None), human)
        }
        Err(e) => {
            jobs::finish_failure(&**store, job, &e).await?;
            Err(e)
        }
    }
}

pub async fn conflicts(
    ctx: &AppContext,
    repo_path: Option<&std::path::Path>,
) -> Result<OpOutput, CccError> {
    let store: &SqliteStore = &ctx.store;
    let repo = resolve_repo(store, repo_path).await?;
    let pairs = ccc_memory::list_conflicts(store, store, repo.repository_id).await?;
    let mut human = String::new();
    if pairs.is_empty() {
        human.push_str("No conflicted evidence.\n");
    }
    for p in &pairs {
        human.push_str(&format!(
            "{} {} — scope: {} (snapshots {})\n",
            p.kind,
            p.label,
            p.shared_scope,
            if p.snapshot_match { "match" } else { "differ" }
        ));
        human.push_str(&format!(
            "  left  {} [{}]: {}\n",
            p.left_id, p.left_title, p.left_text
        ));
        human.push_str(&format!(
            "  right {} [{}]: {}\n",
            p.right_id, p.right_title, p.right_text
        ));
        human.push_str(
            "  resolution: `ccc memory resolve --keep <id> --drop <id> --rationale … --actor …`\n",
        );
    }
    output(&pairs, meta_for(&repo, None, None), human)
}

fn memory_draft_from(
    title: &str,
    summary: &str,
    memory_type: &str,
    scope: &str,
    created_by: &str,
    sharing_policy: &str,
    sensitivity: &str,
) -> Result<ccc_memory::MemoryDraft, CccError> {
    Ok(ccc_memory::MemoryDraft {
        title: title.to_string(),
        summary: summary.to_string(),
        memory_type: MemoryType::parse(memory_type)?,
        scope: scope.to_string(),
        created_by: created_by.to_string(),
        sharing_policy: sharing_policy.to_string(),
        sensitivity: sensitivity.to_string(),
    })
}

fn parse_claim_ids(raw: &[String]) -> Result<Vec<ClaimId>, CccError> {
    raw.iter()
        .map(|s| {
            ClaimId::from_str(s)
                .map_err(|_| CccError::invalid_request(format!("invalid claim id {s:?}")))
        })
        .collect()
}
