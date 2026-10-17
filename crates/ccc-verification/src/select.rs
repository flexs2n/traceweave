//! Test selection (SDD §15.2).
//!
//! Candidates come from the Step 1 dependency graph as-is: `tests` edges from
//! synthetic test modules to code under test, plus reverse dependency paths.
//! Every selected check carries an honest relation label — `directly`,
//! `transitively`, `historically` (no history channel in this step), or
//! `fallback` — and a selected test set is a recommendation, never proof that
//! every regression path is covered.

use ccc_domain::*;
use ccc_storage::{IndexStore, StoreResult};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

/// One selected check suggestion with its relation label (SDD §15.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedCheck {
    pub command: String,
    pub relation: TestRelation,
    pub rationale: String,
}

/// Selects verification checks for the changed symbols of a snapshot.
///
/// - `cargo build` is `directly` related whenever changed symbols exist
///   (compilation directly covers the changed code).
/// - Each test module with a `tests` edge straight to a changed symbol is
///   `directly` related.
/// - Test modules covering symbols reached through reverse dependency paths
///   (callers/importers, breadth-limited) are `transitively` related.
/// - When no indexed test association covers the change, the broad suite is
///   offered as `fallback` — explicitly labeled as the broad net it is.
pub async fn select_checks(
    index: &dyn IndexStore,
    snapshot_id: SnapshotId,
    changed_symbol_ids: &[SymbolId],
    depth_limit: u32,
) -> StoreResult<Vec<SelectedCheck>> {
    let mut direct_tests: BTreeMap<String, String> = BTreeMap::new();
    let mut transitive_tests: BTreeMap<String, String> = BTreeMap::new();

    // Test modules are the sources of `tests` edges. Direct: edge targets a
    // changed symbol. Transitive: edge targets a symbol reached by walking
    // reverse (caller) edges from the changed set.
    let mut visited: HashSet<SymbolId> = changed_symbol_ids.iter().copied().collect();
    let mut frontier: Vec<SymbolId> = changed_symbol_ids.to_vec();
    let mut depth = 0u32;
    let limit = depth_limit.max(1);
    while !frontier.is_empty() && depth < limit {
        let mut next: Vec<SymbolId> = Vec::new();
        for symbol_id in &frontier {
            for edge in index.list_edges_incoming(snapshot_id, *symbol_id).await? {
                if edge.edge_type == RelationshipType::Tests {
                    // The source is a synthetic test module (FR-016); only
                    // edges mapping to a real cargo target are suggested.
                    if let Some(name) = test_target_name(&edge) {
                        if changed_symbol_ids.contains(symbol_id) {
                            direct_tests.insert(name.clone(), name);
                        } else {
                            transitive_tests.insert(name.clone(), name);
                        }
                    }
                    continue;
                }
                if visited.insert(edge.source_symbol_id) {
                    next.push(edge.source_symbol_id);
                }
            }
        }
        frontier = next;
        depth += 1;
    }

    let mut checks: Vec<SelectedCheck> = Vec::new();
    if !changed_symbol_ids.is_empty() {
        checks.push(SelectedCheck {
            command: "cargo build".into(),
            relation: TestRelation::Directly,
            rationale: "compilation directly covers the changed symbols; it catches signature \
                        and type breakage"
                .into(),
        });
    }
    for name in direct_tests.keys() {
        checks.push(SelectedCheck {
            command: format!("cargo test --test {name}"),
            relation: TestRelation::Directly,
            rationale: "an indexed tests edge ties this test module to a changed symbol".into(),
        });
    }
    for name in transitive_tests.keys() {
        checks.push(SelectedCheck {
            command: format!("cargo test --test {name}"),
            relation: TestRelation::Transitively,
            rationale: "the test module covers a symbol reached through a dependency path from \
                        the change"
                .into(),
        });
    }
    if direct_tests.is_empty() && transitive_tests.is_empty() && !changed_symbol_ids.is_empty() {
        checks.push(SelectedCheck {
            command: "cargo test".into(),
            relation: TestRelation::Fallback,
            rationale: "no indexed test association covers the changed symbols; the broad suite \
                        is a fallback, not a targeted selection"
                .into(),
        });
    }
    Ok(checks)
}

/// Derives the cargo integration-test target name from a `tests` edge's
/// provenance path. Only real cargo targets are suggested (a nested path
/// without a `main.rs` is NOT a target — dogfood finding D-1):
/// - `tests/smoke.rs` → `smoke`
/// - `tests/it/main.rs` → `it`
/// - `tests/fixtures/rust-sample/lib.rs` → none (not a target)
fn test_target_name(edge: &EdgeRecord) -> Option<String> {
    let path = edge.provenance_ref.as_ref()?;
    let rest = path.strip_prefix("tests/")?;
    let stem = rest.strip_suffix(".rs")?;
    if !stem.contains('/') {
        return Some(stem.to_string());
    }
    if let Some(dir) = stem.strip_suffix("/main") {
        let name = dir.split('/').next().unwrap_or(dir);
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    None
}
