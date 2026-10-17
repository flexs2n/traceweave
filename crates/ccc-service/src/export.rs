//! Export bundles + evidence as JSON (FR-064).

use crate::{meta_for, output, resolve_repo, AppContext, OpOutput};
use ccc_domain::CccError;
use ccc_schema::SCHEMA_VERSION;
use ccc_storage::*;

pub async fn export_data(
    ctx: &AppContext,
    repo_path: Option<&std::path::Path>,
    output_path: &std::path::Path,
) -> Result<OpOutput, CccError> {
    let store: &ccc_storage::SqliteStore = &ctx.store;
    let repo = resolve_repo(store, repo_path).await?;
    let repositories = vec![repo.clone()];
    let claims = store.list_claims(repo.repository_id).await?;
    let evidence = store.list_evidence(repo.repository_id).await?;
    let memories = store.list_memories(repo.repository_id).await?;
    let mut memory_docs = Vec::new();
    for m in &memories {
        memory_docs.push(serde_json::json!({
            "memory": m,
            "claim_ids": store.claim_ids_for_memory(m.memory_id).await?,
            "status_events": store.list_memory_status_events(m.memory_id).await?,
        }));
    }
    let bundles = store.list_bundles(repo.repository_id).await?;
    let mut bundle_docs = Vec::new();
    for b in &bundles {
        let items_json = store.get_bundle_items(b.bundle_id).await?;
        bundle_docs.push(serde_json::json!({
            "bundle": b,
            "items": serde_json::from_str::<serde_json::Value>(&items_json).unwrap_or_default(),
        }));
    }
    let export = serde_json::json!({
        "schema_version": SCHEMA_VERSION,
        "repositories": repositories,
        "claims": claims,
        "evidence": evidence,
        "memories": memory_docs,
        "bundles": bundle_docs,
    });
    let text = serde_json::to_string_pretty(&export)
        .map_err(|e| CccError::internal_error(format!("export serialization: {e}")))?;
    std::fs::write(output_path, text)
        .map_err(|e| CccError::internal_error(format!("cannot write export: {e}")))?;
    let human = format!(
        "Exported {} claim(s), {} evidence record(s), {} memory record(s), {} bundle(s) to {}\n",
        claims.len(),
        evidence.len(),
        memories.len(),
        bundles.len(),
        output_path.display()
    );
    let data = serde_json::json!({
        "output": output_path.to_string_lossy(),
        "claims": claims.len(),
        "evidence": evidence.len(),
        "memories": memories.len(),
        "bundles": bundles.len(),
    });
    output(&data, meta_for(&repo, None, None), human)
}
