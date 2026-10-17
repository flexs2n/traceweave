//! Snapshot manifest comparison (PRD Phase 2): added/removed/changed/renamed
//! paths between two snapshots.

use crate::{output, AppContext, OpOutput};
use ccc_domain::*;
use ccc_storage::*;
use std::str::FromStr as _;

pub async fn snapshot_diff(ctx: &AppContext, a: &str, b: &str) -> Result<OpOutput, CccError> {
    let store: &ccc_storage::SqliteStore = &ctx.store;
    let id_a = SnapshotId::from_str(a)
        .map_err(|_| CccError::invalid_request(format!("invalid snapshot id {a:?}")))?;
    let id_b = SnapshotId::from_str(b)
        .map_err(|_| CccError::invalid_request(format!("invalid snapshot id {b:?}")))?;
    store
        .get_snapshot(id_a)
        .await?
        .ok_or_else(|| CccError::snapshot_not_found(format!("snapshot {a} not found")))?;
    store
        .get_snapshot(id_b)
        .await?
        .ok_or_else(|| CccError::snapshot_not_found(format!("snapshot {b} not found")))?;

    let files_a = store.list_files(id_a).await?;
    let files_b = store.list_files(id_b).await?;
    let map_a: std::collections::HashMap<&str, &str> = files_a
        .iter()
        .map(|f| (f.path.as_str(), f.content_hash.as_str()))
        .collect();
    let map_b: std::collections::HashMap<&str, &str> = files_b
        .iter()
        .map(|f| (f.path.as_str(), f.content_hash.as_str()))
        .collect();

    let mut added: Vec<String> = Vec::new();
    let mut changed: Vec<serde_json::Value> = Vec::new();
    let mut removed: Vec<String> = Vec::new();
    // Renames: a removed path and an added path sharing identical content.
    let mut renamed: Vec<serde_json::Value> = Vec::new();
    let mut consumed_targets: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut removed_paths: Vec<&str> = map_a
        .iter()
        .filter(|(p, _)| !map_b.contains_key(*p))
        .map(|(p, _)| *p)
        .collect();
    removed_paths.sort();
    let mut added_paths: Vec<&str> = map_b
        .iter()
        .filter(|(p, _)| !map_a.contains_key(*p))
        .map(|(p, _)| *p)
        .collect();
    added_paths.sort();
    for old in &removed_paths {
        let hash = map_a[*old];
        if let Some(new) = added_paths
            .iter()
            .find(|c| map_b[**c] == hash && !consumed_targets.contains(**c))
        {
            consumed_targets.insert((*new).to_string());
            renamed.push(serde_json::json!({ "from": old, "to": new }));
        } else {
            removed.push((*old).to_string());
        }
    }
    for path in added_paths {
        if !consumed_targets.contains(path) {
            added.push(path.to_string());
        }
    }
    for (path, hash) in &map_a {
        if let Some(other) = map_b.get(*path) {
            if other != hash {
                changed.push(serde_json::json!({ "path": path, "from": hash, "to": other }));
            }
        }
    }
    added.sort();
    removed.sort();
    changed.sort_by(|x, y| {
        x["path"]
            .as_str()
            .unwrap_or_default()
            .cmp(y["path"].as_str().unwrap_or_default())
    });
    renamed.sort_by(|x, y| {
        x["from"]
            .as_str()
            .unwrap_or_default()
            .cmp(y["from"].as_str().unwrap_or_default())
    });

    let mut human = format!("Snapshot diff {a} → {b}\n");
    human.push_str(&format!("added: {}\n", added.len()));
    for entry in &added {
        human.push_str(&format!("  + {entry}\n"));
    }
    human.push_str(&format!("removed: {}\n", removed.len()));
    for entry in &removed {
        human.push_str(&format!("  - {entry}\n"));
    }
    human.push_str(&format!("changed: {}\n", changed.len()));
    for c in &changed {
        human.push_str(&format!(
            "  ~ {} ({} → {})\n",
            c["path"].as_str().unwrap_or_default(),
            c["from"].as_str().unwrap_or_default(),
            c["to"].as_str().unwrap_or_default()
        ));
    }
    human.push_str(&format!("renamed: {}\n", renamed.len()));
    for r in &renamed {
        human.push_str(&format!(
            "  {} → {}\n",
            r["from"].as_str().unwrap_or_default(),
            r["to"].as_str().unwrap_or_default()
        ));
    }
    let data = serde_json::json!({
        "base_snapshot": a,
        "target_snapshot": b,
        "added": added,
        "removed": removed,
        "changed": changed,
        "renamed": renamed,
    });
    output(&data, ccc_schema::EnvelopeMeta::default(), human)
}
