//! Dependency tracing for the MCP `trace_dependencies` tool (TIS §13.5):
//! breadth-first traversal of name-resolved edges from a symbol, against the
//! active snapshot. Read-only, bounded depth.

use crate::{meta_for, output, resolve_repo_ref, AppContext, OpOutput};
use ccc_domain::*;
use ccc_storage::*;
use std::collections::{HashMap, HashSet};
use std::str::FromStr as _;

pub async fn trace_dependencies(
    ctx: &AppContext,
    repo_ref: &str,
    symbol: &str,
    depth: u32,
) -> Result<OpOutput, CccError> {
    if depth == 0 || depth > 5 {
        return Err(CccError::invalid_request(format!(
            "depth must be 1..=5 (got {depth})"
        )));
    }
    let store: &SqliteStore = &ctx.store;
    let repo = resolve_repo_ref(store, repo_ref).await?;
    let snapshot = crate::active_snapshot_id(store, &repo).await?;

    // Snapshot-scale symbol load (in-memory map), then an exact
    // qualified-name match — no fuzzy guessing for a trace root.
    let symbols = store.list_symbols(snapshot).await?;
    let by_id: HashMap<SymbolId, &SymbolRecord> =
        symbols.iter().map(|s| (s.symbol_id, s)).collect();
    let root = symbols
        .iter()
        .find(|s| s.qualified_name == symbol)
        .ok_or_else(|| {
            CccError::snapshot_not_found(format!("symbol {symbol:?} not found in active snapshot"))
        })?;

    let mut visited: HashSet<SymbolId> = HashSet::new();
    let mut nodes: Vec<serde_json::Value> = Vec::new();
    let mut edges: Vec<serde_json::Value> = Vec::new();

    let mut frontier: Vec<&SymbolRecord> = vec![root];
    visited.insert(root.symbol_id);
    let mut depth_reached = 0u32;
    while depth_reached < depth && !frontier.is_empty() {
        let mut next: Vec<&SymbolRecord> = Vec::new();
        for current in &frontier {
            for edge in store
                .list_edges_outgoing(snapshot, current.symbol_id)
                .await?
            {
                let (target_id, target_name): (Option<SymbolId>, String) =
                    match (edge.target_symbol_id, &edge.target_name) {
                        (Some(id), _) => (
                            Some(id),
                            by_id
                                .get(&id)
                                .map(|s| s.qualified_name.clone())
                                .unwrap_or_default(),
                        ),
                        (None, Some(name)) => (None, name.clone()),
                        (None, None) => continue,
                    };
                edges.push(serde_json::json!({
                    "from": current.qualified_name,
                    "to": target_name,
                    "edge_type": serde_json::to_string(&edge.edge_type).unwrap_or_default().trim_matches('"'),
                    "resolved": target_id.is_some(),
                    "inferred": edge.inferred,
                }));
                if let Some(id) = target_id {
                    if visited.insert(id) {
                        if let Some(target) = by_id.get(&id) {
                            nodes.push(symbol_node(target, depth_reached + 1));
                            next.push(target);
                        }
                    }
                }
            }
        }
        frontier = next;
        depth_reached += 1;
    }

    nodes.insert(0, symbol_node(root, 0));
    let human = format!(
        "Trace of {symbol}: {} node(s), {} edge(s), depth {} (snapshot {})\n",
        nodes.len(),
        edges.len(),
        depth_reached,
        snapshot
    );
    let data = serde_json::json!({
        "repository_id": repo.repository_id.to_string(),
        "snapshot_id": snapshot.to_string(),
        "root": symbol,
        "depth_reached": depth_reached,
        "symbols": nodes,
        "edges": edges,
    });
    output(&data, meta_for(&repo, Some(snapshot), None), human)
}

fn symbol_node(record: &SymbolRecord, depth: u32) -> serde_json::Value {
    serde_json::json!({
        "symbol_id": record.symbol_id.to_string(),
        "qualified_name": record.qualified_name,
        "kind": serde_json::to_string(&record.kind).unwrap_or_default().trim_matches('"'),
        "language": record.language,
        "start_line": record.start_line,
        "depth": depth,
    })
}

/// Verification results for the MCP `get_verification_results` tool (TIS
/// §13.5): the latest reports for a repository, or one report with its check
/// rows by id.
pub async fn verification_results(
    ctx: &AppContext,
    repo_ref: Option<&str>,
    verification_id: Option<&str>,
) -> Result<OpOutput, CccError> {
    let store: &SqliteStore = &ctx.store;
    if let Some(id) = verification_id {
        let vid = VerificationId::from_str(id)
            .map_err(|_| CccError::invalid_request(format!("invalid verification id {id:?}")))?;
        let record = store
            .get_verification(vid)
            .await?
            .ok_or_else(|| CccError::snapshot_not_found(format!("verification {id} not found")))?;
        let checks = store.list_verification_checks(vid).await?;
        let repo = store
            .get_repository(record.repository_id)
            .await?
            .ok_or_else(|| {
                CccError::snapshot_not_found("verification's repository no longer registered")
            })?;
        let human = format!(
            "Verification {} — mode {} ({} check(s))\n",
            record.verification_id,
            serde_json::to_string(&record.mode)
                .unwrap_or_default()
                .trim_matches('"'),
            checks.len()
        );
        let data = serde_json::json!({
            "verification": record,
            "checks": checks,
        });
        return output(&data, meta_for(&repo, None, None), human);
    }
    let repo = resolve_repo_ref(
        store,
        repo_ref.ok_or_else(|| CccError::invalid_request("verification_id or repo is required"))?,
    )
    .await?;
    let mut records = store.list_verifications(repo.repository_id).await?;
    records.reverse();
    records.truncate(10);
    let mut human = String::new();
    if records.is_empty() {
        human.push_str("No verification runs recorded for this repository.\n");
    }
    for r in &records {
        human.push_str(&format!(
            "{} — mode {} at {}\n",
            r.verification_id,
            serde_json::to_string(&r.mode)
                .unwrap_or_default()
                .trim_matches('"'),
            r.created_at
        ));
    }
    output(&records, meta_for(&repo, None, None), human)
}
