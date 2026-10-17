//! `ccc-schema` — versioned serialized contracts (TIS §9.7, SDD §5.15).
//!
//! Every machine-readable artifact carries a `schema_version`. The canonical
//! JSON serializer (sorted keys, no floats in hash inputs) is the basis for
//! stable artifact hashes.

use ccc_domain::{blake3_hex, canonical_json};
use serde::{Deserialize, Serialize};

/// Current artifact schema version. Bump on any breaking field change.
pub const SCHEMA_VERSION: &str = "ccc.schema/1";

/// Produces canonical JSON (sorted keys, compact). Deterministic across
/// processes for the same value.
pub fn to_canonical_json<T: Serialize + ?Sized>(value: &T) -> Result<String, serde_json::Error> {
    canonical_json(value)
}

/// BLAKE3 over the canonical JSON encoding — the artifact hash basis.
/// Hash inputs must contain no floats; quantize scores to integer millis
/// before hashing (plan T13).
pub fn hash_canonical<T: Serialize + ?Sized>(value: &T) -> Result<String, serde_json::Error> {
    Ok(blake3_hex(to_canonical_json(value)?.as_bytes()))
}

/// CLI success envelope: full machine output, never silently truncated
/// (TIS §13.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope<T: Serialize> {
    pub schema_version: String,
    pub data: T,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<EnvelopeMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EnvelopeMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// FR-010 drift projection: whether the active index generation matches
    /// the repository's current state (HEAD + dirty flag). Response meta
    /// only — never part of a bundle DTO, so bundle bytes stay identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_current: Option<bool>,
}

/// CLI/API error envelope (TIS §13.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    pub schema_version: String,
    pub error: ErrorBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

impl ErrorEnvelope {
    pub fn from_ccc(err: &CccErrorAlias) -> Self {
        ErrorEnvelope {
            schema_version: SCHEMA_VERSION.into(),
            error: ErrorBody {
                code: err.code.to_string(),
                message: err.message.clone(),
                request_id: err.request_id.clone(),
            },
        }
    }
}

/// Type alias to avoid a dependency cycle in docs; identical to ccc_domain::CccError.
pub use ccc_domain::CccError as CccErrorAlias;

/// A single item inside a context bundle (TIS §9.7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BundleItem {
    pub position: u64,
    pub entity_id: String,
    /// `symbol` or `file`.
    pub item_kind: String,
    pub title: String,
    pub path: String,
    /// 1-based span when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SpanDto>,
    /// Source excerpt (capped).
    pub excerpt: String,
    pub selection_reason: String,
    /// Retrieval channel that produced the candidate.
    pub channel: String,
    /// Evidence status for the underlying claim (`candidate`, `stale`, ...).
    pub evidence_status: String,
    /// Approximate token cost (chars/4); labeled approximate at bundle level.
    pub token_cost: u64,
    pub rank: u64,
    pub provenance: ItemProvenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpanDto {
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemProvenance {
    pub repository_id: String,
    pub snapshot_id: String,
    pub generation_id: String,
    pub content_hash: String,
    pub extraction_method: String,
    /// `resolved` | `candidate` | `unresolved` for symbols/edges.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_status: Option<String>,
}

/// One hop of a dependency path carried in the bundle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DependencyPathEdge {
    pub from: String,
    pub to: String,
    pub edge_type: String,
    pub resolution_method: String,
    pub resolution_status: String,
}

/// An explicitly recorded omission (TIS §13.7, SDD omission list).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BundleOmission {
    pub entity_id: String,
    pub path: String,
    pub title: String,
    /// Integer-milli score (no floats in hashes).
    pub score_millis: i64,
    /// budget_exceeded | duplicate | lower_relevance | stale | unsupported |
    /// snapshot_mismatch | lower_priority | insufficient_provenance |
    /// excluded_by_policy
    pub reason: String,
}

/// A conflict carried in a bundle (FR-060, TIS §9.7): when selected
/// evidence is `conflicted`, both sides and their shared scope ship with the
/// bundle — never a silent preference for one side.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BundleConflict {
    /// Deterministic pair identity (sorted entity ids joined), so identical
    /// conflict sets hash identically.
    pub conflict_id: String,
    /// Entity ids (claims or memories) of both sides.
    pub entity_ids: Vec<String>,
    pub titles: Vec<String>,
    pub texts: Vec<String>,
    /// Shared scope description (e.g. `symbol:<id>` / `memory_scope:<value>`).
    pub shared_scope: String,
    /// Always `conflicted` here; scope-differing pairs are never included.
    pub status: String,
}

/// The full context bundle document (TIS §9.7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BundleDto {
    pub schema_version: String,
    pub bundle_id: String,
    pub repository_id: String,
    pub snapshot_id: String,
    pub generation_id: String,
    pub task: TaskDto,
    pub retrieval_policy_version: String,
    pub weights_millis: BTreeMapWeight,
    pub budget_tokens: u64,
    pub estimated_tokens: u64,
    /// Always `approximate_chars_div4` in this step — token counting is a
    /// labeled approximation under an unknown tokenizer (TIS §9.5).
    pub token_estimate_basis: String,
    pub stopping_reason: String,
    pub items: Vec<BundleItem>,
    pub dependency_paths: Vec<DependencyPathEdge>,
    pub omissions: Vec<BundleOmission>,
    pub limitations: Vec<String>,
    /// Conflicted evidence touching the selected items (FR-060). Absent when
    /// empty so pre-Step-3 bundles stay byte-identical.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<BundleConflict>,
    pub created_at: String,
}

/// Weight map with integer-milli values (hash inputs carry no floats).
pub type BTreeMapWeight = std::collections::BTreeMap<String, i64>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskDto {
    pub original_text: String,
    pub task_type: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspected_files: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspected_symbols: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_subsystem: Option<String>,
}

/// The bundle hash input: everything identity-relevant, with floats banned
/// and nondeterministic metadata (bundle_id, created_at) excluded. Serialize
/// only — never deserialized.
#[derive(Debug, Clone, Serialize)]
pub struct BundleHashInput<'a> {
    pub schema_version: &'a str,
    pub repository_id: &'a str,
    pub snapshot_id: &'a str,
    pub generation_id: &'a str,
    pub task: &'a TaskDto,
    pub retrieval_policy_version: &'a str,
    pub weights_millis: &'a BTreeMapWeight,
    pub budget_tokens: u64,
    pub token_estimate_basis: &'a str,
    pub stopping_reason: &'a str,
    pub items: &'a [BundleItem],
    pub dependency_paths: &'a [DependencyPathEdge],
    pub omissions: &'a [BundleOmission],
    pub limitations: &'a [String],
    /// Excluded from the hash when empty: bundles without conflicts hash
    /// identically to pre-Step-3 bundles (the w_h = 0 precedent).
    #[serde(skip_serializing_if = "conflicts_are_empty")]
    pub conflicts: &'a [BundleConflict],
}

fn conflicts_are_empty(conflicts: &&[BundleConflict]) -> bool {
    conflicts.is_empty()
}

/// Renders a bundle as readable Markdown (FR-063).
pub fn render_bundle_markdown(bundle: &BundleDto) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Context bundle {}\n\n", bundle.bundle_id));
    out.push_str(&format!(
        "- Snapshot: `{}` (generation `{}`)\n",
        bundle.snapshot_id, bundle.generation_id
    ));
    out.push_str(&format!(
        "- Task ({}): {}\n",
        bundle.task.task_type, bundle.task.original_text
    ));
    out.push_str(&format!(
        "- Budget: {} tokens (estimated ~{}, basis: {})\n",
        bundle.budget_tokens, bundle.estimated_tokens, bundle.token_estimate_basis
    ));
    out.push_str(&format!(
        "- Stopping reason: {}\n\n",
        bundle.stopping_reason
    ));
    out.push_str("## Selected items\n\n");
    for item in &bundle.items {
        out.push_str(&format!(
            "{}. `{}` — {} ({}:{}, {} tokens, evidence: {}, why: {})\n",
            item.position + 1,
            item.title,
            item.item_kind,
            item.path,
            item.span.map(|s| s.start_line).unwrap_or(0),
            item.token_cost,
            item.evidence_status,
            item.selection_reason,
        ));
    }
    if !bundle.dependency_paths.is_empty() {
        out.push_str("\n## Dependency paths\n\n");
        for edge in &bundle.dependency_paths {
            out.push_str(&format!(
                "- `{}` --{}--> `{}` ({}/{})\n",
                edge.from, edge.edge_type, edge.to, edge.resolution_method, edge.resolution_status
            ));
        }
    }
    if !bundle.omissions.is_empty() {
        out.push_str("\n## Omitted candidates\n\n");
        for o in &bundle.omissions {
            out.push_str(&format!(
                "- `{}` ({}) — score {}e-3, reason: {}\n",
                o.title, o.path, o.score_millis, o.reason
            ));
        }
    }
    if !bundle.conflicts.is_empty() {
        out.push_str("\n## Conflicts (both sides shown, never auto-resolved)\n\n");
        for c in &bundle.conflicts {
            out.push_str(&format!(
                "- {} — scope: {}, sides: {}\n",
                c.conflict_id,
                c.shared_scope,
                c.titles
                    .iter()
                    .map(|t| format!("`{t}`"))
                    .collect::<Vec<_>>()
                    .join(" vs ")
            ));
        }
    }
    out.push_str("\n## Limitations\n\n");
    for l in &bundle.limitations {
        out.push_str(&format!("- {l}\n"));
    }
    out
}

/// Round-trip + tolerance tests (plan T13 acceptance).
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn sample_item() -> BundleItem {
        BundleItem {
            position: 0,
            entity_id: "symbol-1".into(),
            item_kind: "symbol".into(),
            title: "crate::util::read_config".into(),
            path: "src/util.rs".into(),
            span: Some(SpanDto {
                start_line: 1,
                start_column: 0,
                end_line: 2,
                end_column: 1,
            }),
            excerpt: "pub fn read_config() -> u8 { 1 }".into(),
            selection_reason: "task_match".into(),
            channel: "symbol".into(),
            evidence_status: "candidate".into(),
            token_cost: 8,
            rank: 0,
            provenance: ItemProvenance {
                repository_id: "repo-1".into(),
                snapshot_id: "snap-1".into(),
                generation_id: "gen-1".into(),
                content_hash: "hash".into(),
                extraction_method: "tree_sitter".into(),
                resolution_status: Some("resolved".into()),
            },
        }
    }

    #[test]
    fn bundle_item_round_trips() {
        let item = sample_item();
        let json = serde_json::to_string(&item).unwrap();
        let back: BundleItem = serde_json::from_str(&json).unwrap();
        assert_eq!(back, item);
    }

    #[test]
    fn canonical_json_is_sorted_and_hash_stable() {
        let value = serde_json::json!({"b": 1, "a": {"z": true, "y": false}});
        let first = to_canonical_json(&value).unwrap();
        let second = to_canonical_json(&value).unwrap();
        assert_eq!(first, second);
        assert_eq!(first, r#"{"a":{"y":false,"z":true},"b":1}"#);
        assert_eq!(
            hash_canonical(&value).unwrap(),
            hash_canonical(&value).unwrap()
        );
    }

    #[test]
    fn error_envelope_uses_tis_codes() {
        let err = ccc_domain::CccError::index_stale("stale").with_request_id("r1");
        let envelope = ErrorEnvelope::from_ccc(&err);
        let json = serde_json::to_string(&envelope).unwrap();
        assert!(json.contains("INDEX_STALE"));
        let back: ErrorEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.error.code, "INDEX_STALE");
        assert_eq!(back.error.request_id.as_deref(), Some("r1"));
    }

    #[test]
    fn old_serialized_sample_with_added_fields_still_parses() {
        // Simulates an older artifact: missing `limitations`, `span` absent.
        let old = serde_json::json!({
            "position": 0,
            "entity_id": "s1",
            "item_kind": "symbol",
            "title": "t",
            "path": "p.rs",
            "excerpt": "",
            "selection_reason": "task_match",
            "channel": "symbol",
            "evidence_status": "candidate",
            "token_cost": 3,
            "rank": 0,
            "provenance": {
                "repository_id": "r", "snapshot_id": "s", "generation_id": "g",
                "content_hash": "h", "extraction_method": "tree_sitter"
            }
        });
        let parsed: BundleItem = serde_json::from_value(old).unwrap();
        assert_eq!(parsed.title, "t");
        assert!(parsed.span.is_none());
        let full = serde_json::to_value(&parsed).unwrap();
        // Additive field from a NEWER writer must not break OLD readers that
        // use default handling; serde ignores unknown fields by default.
        let mut with_new_field = full.clone();
        if let Value::Object(ref mut map) = with_new_field {
            map.insert("future_field".into(), Value::from(1));
        }
        let reparsed: BundleItem = serde_json::from_value(with_new_field).unwrap();
        assert_eq!(reparsed, parsed);
    }
}
