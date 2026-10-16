//! `ccc-indexer` — incremental indexing pipeline (TIS §7, SDD §7.5).
//!
//! Flow: cleanup interrupted runs → classify discovered files (exclusions are
//! recorded with reasons, never silent) → parse `.rs` files through the
//! content-addressed artifact cache → resolve edge targets conservatively
//! (exact-name-unique matches only) → write files/symbols/references/edges →
//! rebuild derived search entries → record evidence → publish the generation
//! atomically. A failed or interrupted run leaves the previous generation
//! active.

use ccc_config::IndexingConfig;
use ccc_domain::*;
use ccc_evidence::{invalidate_for_changes, record_symbol_facts, SymbolFact};
use ccc_storage::{
    ArtifactStore, EvidenceStore, IndexStore, SearchEntry, SearchStore, SnapshotStore, StoreResult,
};
use ccc_tree_sitter::{
    ExtractedFile, ExtractedReference, RustExtractor, EXTRACTION_CONFIG_HASH, GRAMMAR_VERSION,
    PARSER_VERSION,
};
use globset::{GlobSet, GlobSetBuilder};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

/// Counters surfaced to `ccc index` output and metrics (TIS §18).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IndexCounters {
    /// Files actually parsed with tree-sitter this run.
    pub parsed: u64,
    /// Files whose extraction came from the artifact cache.
    pub reused: u64,
    /// Files excluded for any reason (patterns, binary, oversized, unsupported).
    pub excluded: u64,
    /// Excluded files with no language support (visible in `ccc status`).
    pub unsupported: u64,
    /// Files that could not be read.
    pub failed: u64,
}

impl IndexCounters {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IndexOutcome {
    pub snapshot_id: SnapshotId,
    pub generation_id: GenerationId,
    pub counters: IndexCounters,
}

/// Parsed artifact cached per content hash (TIS §4.3).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CachedExtraction {
    symbols: Vec<ccc_tree_sitter::ExtractedSymbol>,
    references: Vec<ExtractedReference>,
    parse_error: bool,
}

/// Canonical lookup key from an extraction symbol to its generated id.
fn symbol_key(path: &str, qualified: &str, signature_hash: &str) -> String {
    format!("{path}|{qualified}|{signature_hash}")
}

/// Runs the full index pipeline for an already-created `building` snapshot.
///
/// `manifest` maps repo-relative paths to content hashes; files are read
/// under `root`. Publication is atomic: on any error the previous active
/// generation stays published (SDD §7.5). `full_rebuild` skips incremental
/// claim invalidation bookkeeping and is the recovery path (TIS §7.5).
#[allow(clippy::too_many_arguments)]
pub async fn run_index(
    index_store: &dyn IndexStore,
    snapshot_store: &dyn SnapshotStore,
    search_store: &dyn SearchStore,
    artifacts: &dyn ArtifactStore,
    evidence_store: &dyn EvidenceStore,
    repo: &RepositoryRecord,
    snapshot: &SnapshotRecord,
    root: &Path,
    manifest: &BTreeMap<String, String>,
    cfg: &IndexingConfig,
    full_rebuild: bool,
) -> Result<IndexOutcome, CccError> {
    // 1. Recovery: anything left `building` by an interrupted run is failed
    //    explicitly; the previously published generation remains active.
    index_store
        .fail_building_generations(repo.repository_id)
        .await?;
    snapshot_store
        .fail_interrupted_snapshots(repo.repository_id)
        .await?;

    // 2. A reused snapshot identity replaces its content rows (idempotent rerun).
    index_store
        .delete_snapshot_content(snapshot.snapshot_id)
        .await?;
    search_store
        .delete_snapshot_entries(snapshot.snapshot_id)
        .await?;

    // 3. Isolated generation record (TIS §4.4 step 2).
    let generation = GenerationRecord {
        generation_id: GenerationId::generate(),
        repository_id: repo.repository_id,
        snapshot_id: snapshot.snapshot_id,
        status: GenerationStatus::Building,
        counters_json: IndexCounters::default().to_json(),
        created_at: now_rfc3339(),
        published_at: None,
    };
    index_store.create_generation(&generation).await?;

    // 4. Classification — exclusions are recorded, never silent (TIS §7.1).
    let mut counters = IndexCounters::default();
    let exclude_matcher = build_exclude_matcher(&cfg.exclude_globs)?;
    let mut classified: Vec<ClassifiedFile> = Vec::new();
    for (path, content_hash) in manifest {
        let cf = classify_file(
            repo.repository_id,
            snapshot.snapshot_id,
            root,
            path,
            content_hash,
            &exclude_matcher,
            cfg,
            &mut counters,
        )?;
        classified.push(cf);
    }
    classified.sort_by(|a, b| a.record.path.cmp(&b.record.path));

    // Content-addressed blob per included file (used later for excerpts).
    for cf in &classified {
        if let Some(bytes) = &cf.source {
            artifacts
                .put(bytes, media_type_for(&cf.record.path))
                .await?;
        }
    }
    index_store
        .insert_files(
            &classified
                .iter()
                .map(|cf| cf.record.clone())
                .collect::<Vec<_>>(),
        )
        .await?;

    // 5. Extraction through the artifact cache (bounded concurrency, TIS §15).
    let included: Vec<&ClassifiedFile> = classified
        .iter()
        .filter(|cf| cf.record.inclusion == FileInclusion::Included)
        .collect();
    let extractions = extract_files(
        artifacts,
        index_store,
        &included,
        root,
        cfg.parse_concurrency,
        &mut counters,
    )
    .await?;

    // 6. Symbol/reference/edge records with conservative resolution.
    let (symbols, _symbol_ids) = build_symbols(repo, snapshot, &classified, &extractions);
    let (test_modules, test_edges) =
        build_test_associations(repo, snapshot, root, &classified, &symbols);
    let mut all_symbols = symbols.clone();
    all_symbols.extend(test_modules.clone());
    let references = build_references(snapshot, &classified, &extractions);
    let mut edges =
        build_calls_and_impls_edges(repo, snapshot, &classified, &extractions, &all_symbols);
    edges.extend(test_edges);
    let edge_type_key = |e: &EdgeRecord| serde_json::to_string(&e.edge_type).unwrap_or_default();
    edges.sort_by(|a, b| {
        (
            a.source_symbol_id.to_string(),
            edge_type_key(a),
            a.provenance_ref.clone().unwrap_or_default(),
        )
            .cmp(&(
                b.source_symbol_id.to_string(),
                edge_type_key(b),
                b.provenance_ref.clone().unwrap_or_default(),
            ))
    });
    index_store.insert_symbols(&all_symbols).await?;
    index_store.insert_references(&references).await?;
    index_store.insert_edges(&edges).await?;

    // 7. Derived search entries (rebuildable, never authoritative — TIS §5.2).
    let entries = build_search_entries(&classified, &all_symbols);
    search_store
        .replace_snapshot_entries(snapshot.snapshot_id, &entries)
        .await?;

    // 8. Evidence ledger: one source_fact + candidate claim per real symbol.
    let file_by_id: HashMap<FileId, &FileRecord> = classified
        .iter()
        .map(|cf| (cf.record.file_id, &cf.record))
        .collect();
    let mut facts = Vec::new();
    for s in &symbols {
        // Synthetic heuristic modules (tests/) do not become claims.
        if s.resolver != ResolutionMethod::TreeSitter {
            continue;
        }
        let Some(fr) = file_by_id.get(&s.file_id) else {
            continue;
        };
        facts.push(SymbolFact {
            symbol_id: &s.symbol_id,
            file_id: &s.file_id,
            qualified_name: &s.qualified_name,
            kind: kind_label(s.kind),
            path: &fr.path,
            start_line: s.start_line,
            end_line: s.end_line,
            content_hash: &fr.content_hash,
        });
    }
    record_symbol_facts(
        evidence_store,
        repo.repository_id,
        snapshot.snapshot_id,
        &facts,
        PARSER_VERSION,
    )
    .await?;

    // 9. Selective invalidation of claims depending on changed files (FR-040).
    if !full_rebuild {
        invalidate_previous_claims(index_store, evidence_store, repo, manifest).await?;
    }

    // 10. Atomic publication (TIS §4.4 steps 4–6, SDD §7.5).
    index_store
        .set_generation_counters(generation.generation_id, &counters.to_json())
        .await?;
    index_store
        .publish_generation(generation.generation_id)
        .await?;

    Ok(IndexOutcome {
        snapshot_id: snapshot.snapshot_id,
        generation_id: generation.generation_id,
        counters,
    })
}

struct ClassifiedFile {
    record: FileRecord,
    source: Option<Vec<u8>>,
}

fn build_exclude_matcher(patterns: &[String]) -> Result<GlobSet, CccError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| {
                CccError::invalid_request(format!("invalid exclude glob {pattern:?}: {e}"))
            })?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|e| CccError::internal_error(format!("cannot build glob set: {e}")))
}

#[allow(clippy::too_many_arguments)]
fn classify_file(
    repository_id: RepositoryId,
    snapshot_id: SnapshotId,
    root: &Path,
    path: &str,
    content_hash: &str,
    exclude_matcher: &GlobSet,
    cfg: &IndexingConfig,
    counters: &mut IndexCounters,
) -> Result<ClassifiedFile, CccError> {
    let mut record = FileRecord {
        file_id: FileId::generate(),
        snapshot_id,
        repository_id,
        path: path.to_string(),
        content_hash: content_hash.to_string(),
        size_bytes: 0,
        language: None,
        binary: false,
        inclusion: FileInclusion::Included,
        exclusion_reason: None,
    };

    if exclude_matcher.is_match(path) {
        record.inclusion = FileInclusion::Excluded;
        record.exclusion_reason = Some(ExclusionReason::ExcludedByPattern);
        counters.excluded += 1;
        return Ok(ClassifiedFile {
            record,
            source: None,
        });
    }

    let bytes = std::fs::read(root.join(path)).map_err(|_| {
        // Unreadable discovered file: counted explicitly, not silent.
        counters.failed += 1;
        CccError::internal_error(format!("unreadable file {path}"))
    })?;

    record.size_bytes = bytes.len() as u64;
    if record.size_bytes > cfg.max_file_size_bytes {
        record.inclusion = FileInclusion::Excluded;
        record.exclusion_reason = Some(ExclusionReason::Oversized);
        counters.excluded += 1;
        return Ok(ClassifiedFile {
            record,
            source: None,
        });
    }
    if bytes.iter().take(8192).any(|b| *b == 0) {
        record.binary = true;
        record.inclusion = FileInclusion::Excluded;
        record.exclusion_reason = Some(ExclusionReason::Binary);
        counters.excluded += 1;
        return Ok(ClassifiedFile {
            record,
            source: None,
        });
    }
    if path.ends_with(".rs") {
        record.language = Some("rust".into());
        Ok(ClassifiedFile {
            record,
            source: Some(bytes),
        })
    } else {
        record.inclusion = FileInclusion::Excluded;
        record.exclusion_reason = Some(ExclusionReason::UnsupportedLanguage);
        counters.excluded += 1;
        counters.unsupported += 1;
        Ok(ClassifiedFile {
            record,
            source: None,
        })
    }
}

fn media_type_for(path: &str) -> &'static str {
    if path.ends_with(".rs") {
        "text/x-rust"
    } else {
        "application/octet-stream"
    }
}

fn artifact_cache_key(content_hash: &str) -> String {
    // Key composition per TIS §4.3:
    // content_hash + language + parser_version + grammar_version + extraction_config_hash
    blake3_hex(
        format!(
            "v1|{content_hash}|rust|{PARSER_VERSION}|{GRAMMAR_VERSION}|{EXTRACTION_CONFIG_HASH}"
        )
        .as_bytes(),
    )
}

/// Extraction phase: sequential cache lookups and reads, then bounded
/// parallel parsing (store access stays outside the spawned tasks).
async fn extract_files(
    artifacts: &dyn ArtifactStore,
    index_store: &dyn IndexStore,
    files: &[&ClassifiedFile],
    root: &Path,
    concurrency: usize,
    counters: &mut IndexCounters,
) -> Result<HashMap<FileId, ExtractedFile>, CccError> {
    let mut out: HashMap<FileId, ExtractedFile> = HashMap::new();
    let mut parse_jobs: Vec<(FileId, String, Vec<u8>)> = Vec::new();

    // Phase A: cache lookups + reads (sequential, deterministic order).
    for cf in files {
        let cache_key = artifact_cache_key(&cf.record.content_hash);
        let mut reused = false;
        if let Some(artifact_hash) = index_store.get_cached_parse(&cache_key).await? {
            if let Some(bytes) = artifacts.get(&artifact_hash).await? {
                if let Ok(cached) = serde_json::from_slice::<CachedExtraction>(&bytes) {
                    counters.reused += 1;
                    out.insert(
                        cf.record.file_id,
                        ExtractedFile {
                            symbols: cached.symbols,
                            references: cached.references,
                            parse_error: cached.parse_error,
                        },
                    );
                    reused = true;
                }
            }
        }
        if !reused {
            let path = root.join(&cf.record.path);
            let bytes = tokio::fs::read(&path)
                .await
                .map_err(|e| CccError::internal_error(format!("cannot read {path:?}: {e}")))?;
            parse_jobs.push((cf.record.file_id, cache_key, bytes));
        }
    }

    // Phase B: bounded parallel parsing (pure CPU inside tasks).
    let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency.max(1)));
    let mut tasks = tokio::task::JoinSet::new();
    for (file_id, cache_key, bytes) in parse_jobs {
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| CccError::internal_error(format!("semaphore closed: {e}")))?;
        tasks.spawn(async move {
            let _permit = permit;
            let extraction = tokio::task::spawn_blocking(move || {
                let mut extractor = RustExtractor::new();
                extractor.extract(&bytes)
            })
            .await
            .map_err(|e| CccError::internal_error(format!("parse task failed: {e}")))?;
            Ok::<(FileId, String, ExtractedFile), CccError>((file_id, cache_key, extraction))
        });
    }

    // Phase C: persist artifacts in file order (sequential on the stores).
    let mut parsed: Vec<(FileId, String, ExtractedFile)> = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        parsed.push(joined.map_err(|e| CccError::internal_error(format!("join failed: {e}")))??);
    }
    parsed.sort_by_key(|(file_id, _, _)| file_id.to_string());
    for (file_id, cache_key, extraction) in parsed {
        let artifact = CachedExtraction {
            symbols: extraction.symbols.clone(),
            references: extraction.references.clone(),
            parse_error: extraction.parse_error,
        };
        let bytes = serde_json::to_vec(&artifact)
            .map_err(|e| CccError::internal_error(format!("serialize: {e}")))?;
        let artifact_hash = artifacts
            .put(&bytes, "application/json+ccc-extraction")
            .await?;
        index_store
            .store_cached_parse(&cache_key, &artifact_hash)
            .await?;
        counters.parsed += 1;
        out.insert(file_id, extraction);
    }
    Ok(out)
}

fn kind_label(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Function => "function",
        SymbolKind::Struct => "struct",
        SymbolKind::Enum => "enum",
        SymbolKind::Trait => "trait",
        SymbolKind::Impl => "impl",
        SymbolKind::Module => "module",
        SymbolKind::UseDeclaration => "use_declaration",
        SymbolKind::TypeAlias => "type_alias",
        SymbolKind::Const => "const",
        SymbolKind::Static => "static",
        SymbolKind::Macro => "macro",
        SymbolKind::Field => "field",
        SymbolKind::Variant => "variant",
        SymbolKind::Other => "other",
    }
}

type SymbolIdMap = HashMap<String, SymbolId>;

/// Derives the module path prefix for symbols from their file location:
/// `src/lib.rs`/`src/main.rs` → `crate`, `src/util.rs` → `crate::util`,
/// `tests/it.rs` → `tests::it`.
fn file_module_prefix(path: &str) -> String {
    let no_ext = path.trim_end_matches(".rs");
    if no_ext == "src/lib" || no_ext == "src/main" {
        return "crate".into();
    }
    if let Some(rest) = no_ext.strip_prefix("src/") {
        return format!("crate::{}", rest.replace('/', "::"));
    }
    if let Some(rest) = no_ext.strip_prefix("tests/") {
        return format!("tests::{}", rest.replace('/', "::"));
    }
    no_ext.replace('/', "::")
}

/// Remaps an extracted qualified name (rooted at `crate`) under the file's
/// module prefix.
fn remap_qualified(extracted: &str, prefix: &str) -> String {
    let Some(rest) = extracted.strip_prefix("crate") else {
        return extracted.to_string();
    };
    let rest = rest.strip_prefix("::").unwrap_or(rest);
    if prefix == "crate" {
        format!("crate::{rest}")
    } else {
        format!("{prefix}::{rest}")
    }
}

fn build_symbols(
    repo: &RepositoryRecord,
    snapshot: &SnapshotRecord,
    files: &[ClassifiedFile],
    extractions: &HashMap<FileId, ExtractedFile>,
) -> (Vec<SymbolRecord>, SymbolIdMap) {
    let mut records = Vec::new();
    let mut id_map: SymbolIdMap = HashMap::new();
    for cf in files {
        if cf.record.inclusion != FileInclusion::Included {
            continue;
        }
        let Some(extraction) = extractions.get(&cf.record.file_id) else {
            continue;
        };
        let prefix = file_module_prefix(&cf.record.path);
        for sym in &extraction.symbols {
            let qualified = remap_qualified(&sym.qualified_name, &prefix);
            let symbol_id = SymbolId::generate();
            id_map.insert(
                symbol_key(&cf.record.path, &qualified, &sym.signature_hash),
                symbol_id,
            );
            records.push(SymbolRecord {
                symbol_id,
                repository_id: repo.repository_id,
                snapshot_id: snapshot.snapshot_id,
                file_id: cf.record.file_id,
                symbol_key: format!("rust|{qualified}|{}", sym.signature_hash),
                qualified_name: qualified,
                kind: sym.kind,
                language: "rust".into(),
                start_line: sym.start_line,
                start_column: sym.start_column,
                end_line: sym.end_line,
                end_column: sym.end_column,
                signature_hash: sym.signature_hash.clone(),
                resolver: ResolutionMethod::TreeSitter,
                // The definition itself is syntactically established; edge
                // targets get the conservative treatment separately.
                resolution_status: ResolutionStatus::Resolved,
            });
        }
    }
    records.sort_by(|a, b| a.qualified_name.cmp(&b.qualified_name));
    (records, id_map)
}

fn build_references(
    snapshot: &SnapshotRecord,
    files: &[ClassifiedFile],
    extractions: &HashMap<FileId, ExtractedFile>,
) -> Vec<ReferenceRecord> {
    let mut records = Vec::new();
    for cf in files {
        if cf.record.inclusion != FileInclusion::Included {
            continue;
        }
        let Some(extraction) = extractions.get(&cf.record.file_id) else {
            continue;
        };
        for r in &extraction.references {
            records.push(ReferenceRecord {
                reference_id: ReferenceId::generate(),
                snapshot_id: snapshot.snapshot_id,
                file_id: cf.record.file_id,
                source_symbol_id: None,
                target_name: r.target_name.clone(),
                relationship: r.relationship,
                start_line: r.start_line,
                start_column: r.start_column,
                end_line: r.end_line,
                end_column: r.end_column,
                resolution_method: ResolutionMethod::TreeSitter,
                // Name-level syntax references are candidates: syntax is not
                // semantic resolution (TIS §7.2).
                resolution_status: ResolutionStatus::Candidate,
            });
        }
    }
    records
}

/// Resolves an edge target conservatively (TIS §7.2): `resolved` only when
/// the referenced qualified name matches exactly one symbol of this snapshot;
/// unique simple-name matches stay `candidate`; everything else unresolved.
fn resolve_target(
    target_name: &str,
    by_qualified: &HashMap<&str, Vec<&SymbolRecord>>,
    by_simple: &HashMap<&str, Vec<&SymbolRecord>>,
) -> Option<(SymbolId, ResolutionStatus)> {
    if let Some(list) = by_qualified.get(target_name) {
        if list.len() == 1 {
            return Some((list[0].symbol_id, ResolutionStatus::Resolved));
        }
    }
    let simple = target_name.rsplit("::").next()?;
    if let Some(list) = by_simple.get(simple) {
        if list.len() == 1 {
            return Some((list[0].symbol_id, ResolutionStatus::Candidate));
        }
    }
    None
}

fn build_calls_and_impls_edges(
    repo: &RepositoryRecord,
    snapshot: &SnapshotRecord,
    files: &[ClassifiedFile],
    extractions: &HashMap<FileId, ExtractedFile>,
    symbols: &[SymbolRecord],
) -> Vec<EdgeRecord> {
    let mut by_qualified: HashMap<&str, Vec<&SymbolRecord>> = HashMap::new();
    let mut id_by_qualified: HashMap<&str, SymbolId> = HashMap::new();
    let mut by_simple: HashMap<&str, Vec<&SymbolRecord>> = HashMap::new();
    for s in symbols {
        by_qualified
            .entry(s.qualified_name.as_str())
            .or_default()
            .push(s);
        id_by_qualified
            .entry(s.qualified_name.as_str())
            .or_insert(s.symbol_id);
        if let Some(simple) = s.qualified_name.rsplit("::").next() {
            by_simple.entry(simple).or_default().push(s);
        }
    }

    let mut edges = Vec::new();
    let now = now_rfc3339();
    for cf in files {
        if cf.record.inclusion != FileInclusion::Included {
            continue;
        }
        let Some(extraction) = extractions.get(&cf.record.file_id) else {
            continue;
        };
        let prefix = file_module_prefix(&cf.record.path);
        for r in &extraction.references {
            let Some(from_q) = &r.from_qualified else {
                continue;
            };
            let from_qualified = remap_qualified(from_q, &prefix);
            let Some(&source_id) = id_by_qualified.get(from_qualified.as_str()) else {
                continue;
            };
            match r.relationship {
                RelationshipType::Calls => {
                    let target = resolve_target(&r.target_name, &by_qualified, &by_simple);
                    edges.push(EdgeRecord {
                        edge_id: EdgeId::generate(),
                        repository_id: repo.repository_id,
                        snapshot_id: snapshot.snapshot_id,
                        source_symbol_id: source_id,
                        target_symbol_id: target.as_ref().map(|(id, _)| *id),
                        target_name: Some(r.target_name.clone()),
                        edge_type: RelationshipType::Calls,
                        resolution_method: ResolutionMethod::TreeSitter,
                        resolution_status: target
                            .as_ref()
                            .map(|(_, status)| *status)
                            .unwrap_or(ResolutionStatus::Candidate),
                        confidence: if target.is_some() {
                            ConfidenceCategory::Confirmed
                        } else {
                            ConfidenceCategory::Unverified
                        },
                        inferred: false,
                        provenance_ref: Some(format!("{}#L{}", cf.record.path, r.start_line)),
                        created_at: now.clone(),
                    });
                }
                RelationshipType::Implements => {
                    // Trait implementations stay candidate by construction:
                    // syntax cannot confirm trait resolution (FR-049).
                    edges.push(EdgeRecord {
                        edge_id: EdgeId::generate(),
                        repository_id: repo.repository_id,
                        snapshot_id: snapshot.snapshot_id,
                        source_symbol_id: source_id,
                        target_symbol_id: None,
                        target_name: Some(r.target_name.clone()),
                        edge_type: RelationshipType::Implements,
                        resolution_method: ResolutionMethod::TreeSitter,
                        resolution_status: ResolutionStatus::Candidate,
                        confidence: ConfidenceCategory::Inferred,
                        inferred: false,
                        provenance_ref: Some(format!("{}#L{}", cf.record.path, r.start_line)),
                        created_at: now.clone(),
                    });
                }
                _ => {}
            }
        }
    }
    edges
}

/// Test association heuristic (FR-016): each file under `tests/` gets a
/// synthetic module symbol, and inferred `tests` edges link it to src symbols
/// whose simple names appear in the test file. Explicitly labeled inferred.
fn build_test_associations(
    repo: &RepositoryRecord,
    snapshot: &SnapshotRecord,
    root: &Path,
    files: &[ClassifiedFile],
    symbols: &[SymbolRecord],
) -> (Vec<SymbolRecord>, Vec<EdgeRecord>) {
    let mut modules = Vec::new();
    let mut edges = Vec::new();
    let now = now_rfc3339();

    for cf in files {
        if cf.record.inclusion != FileInclusion::Included {
            continue;
        }
        if !cf.record.path.starts_with("tests/") || !cf.record.path.ends_with(".rs") {
            continue;
        }
        let stem = cf
            .record
            .path
            .trim_end_matches(".rs")
            .trim_start_matches("tests/")
            .replace('/', "::");
        let module_qname = format!("tests::{stem}");
        let module_symbol = SymbolRecord {
            symbol_id: SymbolId::generate(),
            repository_id: repo.repository_id,
            snapshot_id: snapshot.snapshot_id,
            file_id: cf.record.file_id,
            symbol_key: format!("rust|{module_qname}|{}", cf.record.content_hash),
            qualified_name: module_qname,
            kind: SymbolKind::Module,
            language: "rust".into(),
            start_line: 1,
            start_column: 0,
            end_line: 1,
            end_column: 0,
            signature_hash: cf.record.content_hash.clone(),
            resolver: ResolutionMethod::Heuristic,
            resolution_status: ResolutionStatus::Resolved,
        };
        modules.push(module_symbol.clone());

        let test_text = std::fs::read_to_string(root.join(&cf.record.path)).unwrap_or_default();
        for s in symbols {
            let simple = s.qualified_name.rsplit("::").next().unwrap_or("");
            if simple.len() < 3
                || s.kind == SymbolKind::Impl
                || s.resolver != ResolutionMethod::TreeSitter
            {
                continue;
            }
            if contains_word(&test_text, simple) {
                edges.push(EdgeRecord {
                    edge_id: EdgeId::generate(),
                    repository_id: repo.repository_id,
                    snapshot_id: snapshot.snapshot_id,
                    source_symbol_id: module_symbol.symbol_id,
                    target_symbol_id: Some(s.symbol_id),
                    target_name: Some(s.qualified_name.clone()),
                    edge_type: RelationshipType::Tests,
                    resolution_method: ResolutionMethod::Heuristic,
                    resolution_status: ResolutionStatus::Candidate,
                    confidence: ConfidenceCategory::Inferred,
                    inferred: true,
                    provenance_ref: Some(cf.record.path.clone()),
                    created_at: now.clone(),
                });
            }
        }
    }
    (modules, edges)
}

fn contains_word(haystack: &str, needle: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs = start + pos;
        let before_ok = haystack[..abs]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        let after = abs + needle.len();
        let after_ok = haystack[after..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        if before_ok && after_ok {
            return true;
        }
        start = abs + needle.len();
        if start >= haystack.len() {
            break;
        }
    }
    false
}

fn build_search_entries(files: &[ClassifiedFile], symbols: &[SymbolRecord]) -> Vec<SearchEntry> {
    let mut entries = Vec::new();
    let path_by_file: HashMap<FileId, &str> = files
        .iter()
        .map(|f| (f.record.file_id, f.record.path.as_str()))
        .collect();
    for cf in files {
        if cf.record.inclusion != FileInclusion::Included {
            continue;
        }
        entries.push(SearchEntry {
            term: cf.record.path.clone(),
            path: cf.record.path.clone(),
            kind: "file".into(),
            entity_id: cf.record.file_id.to_string(),
            title: None,
        });
    }
    for s in symbols {
        let simple = s
            .qualified_name
            .rsplit("::")
            .next()
            .unwrap_or("")
            .to_string();
        let path = path_by_file
            .get(&s.file_id)
            .map(|p| (*p).to_string())
            .unwrap_or_default();
        for term in [simple, s.qualified_name.clone()] {
            entries.push(SearchEntry {
                term,
                path: path.clone(),
                kind: "symbol".into(),
                entity_id: s.symbol_id.to_string(),
                title: Some(s.qualified_name.clone()),
            });
        }
    }
    entries
}

/// Selective claim invalidation (FR-040, TIS §8.1): flips dependent claims of
/// files (and their symbols) whose content changed since the previously
/// published generation.
async fn invalidate_previous_claims(
    index_store: &dyn IndexStore,
    evidence_store: &dyn EvidenceStore,
    repo: &RepositoryRecord,
    current_manifest: &BTreeMap<String, String>,
) -> StoreResult<()> {
    let Some(previous) = index_store
        .get_active_generation(repo.repository_id)
        .await?
    else {
        return Ok(());
    };
    let Some(previous_generation) = index_store.get_generation(previous.generation_id).await?
    else {
        return Ok(());
    };
    let previous_files = index_store
        .list_files(previous_generation.snapshot_id)
        .await?;

    let mut changed_file_ids: Vec<String> = Vec::new();
    for f in &previous_files {
        match current_manifest.get(&f.path) {
            Some(hash) if hash == &f.content_hash => {}
            _ => changed_file_ids.push(f.file_id.to_string()),
        }
    }
    if changed_file_ids.is_empty() {
        return Ok(());
    }
    let mut changed_symbol_ids = Vec::new();
    for file_id_str in &changed_file_ids {
        if let Ok(file_id) = <FileId as std::str::FromStr>::from_str(file_id_str) {
            for sym in index_store
                .list_symbols_for_file(previous_generation.snapshot_id, file_id)
                .await?
            {
                changed_symbol_ids.push(sym.symbol_id.to_string());
            }
        }
    }
    invalidate_for_changes(
        evidence_store,
        repo.repository_id,
        &changed_file_ids,
        &changed_symbol_ids,
    )
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ccc_repository::GitCli;
    use ccc_snapshot::{find_or_create_snapshot, SnapshotBuilder};
    use ccc_storage::test_support::InMemoryEvidenceStore;
    use ccc_storage::{FsArtifactStore, SqliteStore};
    use std::collections::HashSet;
    use std::process::Command;
    use std::sync::Arc;

    fn run(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
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

    fn init_repo(dir: &Path) {
        run(dir, &["init", "--initial-branch=main"]);
        run(dir, &["config", "user.email", "test@example.com"]);
        run(dir, &["config", "user.name", "test"]);
    }

    fn write(dir: &Path, rel: &str, content: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    struct Fixture {
        // Holds the temp root alive for the fixture's lifetime.
        _guard: tempfile::TempDir,
        root: std::path::PathBuf,
        store: Arc<SqliteStore>,
        artifacts: FsArtifactStore,
        evidence: InMemoryEvidenceStore,
        repo: RepositoryRecord,
        cfg: IndexingConfig,
    }

    async fn fixture_with_files(files: &[(&str, &str)]) -> Fixture {
        // The repo lives in a subdirectory; the database and blob store live
        // outside it so the fixture manifest only sees repo files.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        init_repo(&root);
        for (rel, content) in files {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        run(&root, &["add", "-A"]);
        run(&root, &["commit", "-m", "fixture"]);
        let git_dir = dir.path();

        let db_path = git_dir.join("ccc.db");
        let store = Arc::new(SqliteStore::connect(&db_path).await.unwrap());
        let artifacts = FsArtifactStore::new(git_dir.join("blobs"), store.clone());
        let evidence = InMemoryEvidenceStore::new();
        let registry = ccc_repository::RepositoryRegistry::new(&*store, &GitCli);
        let cfg = IndexingConfig::default();
        let repo = registry
            .register(&root, &cfg.configuration_hash().unwrap())
            .await
            .unwrap();
        Fixture {
            _guard: dir,
            root,
            store,
            artifacts,
            evidence,
            repo,
            cfg,
        }
    }

    async fn build_snapshot(fixture: &Fixture) -> (SnapshotRecord, BTreeMap<String, String>) {
        let builder = SnapshotBuilder::new(&GitCli);
        let draft = builder
            .build_working_tree(
                fixture.repo.repository_id,
                &fixture.root,
                &fixture.cfg.configuration_hash().unwrap(),
                fixture.cfg.include_untracked,
            )
            .unwrap();
        let manifest = draft.manifest.clone();
        let snapshot = find_or_create_snapshot(&*fixture.store, draft)
            .await
            .unwrap();
        (snapshot, manifest)
    }

    async fn index_fixture(fixture: &Fixture, full: bool) -> IndexOutcome {
        let (snapshot, manifest) = build_snapshot(fixture).await;
        run_index(
            &*fixture.store,
            &*fixture.store,
            &*fixture.store,
            &fixture.artifacts,
            &fixture.evidence,
            &fixture.repo,
            &snapshot,
            &fixture.root,
            &manifest,
            &fixture.cfg,
            full,
        )
        .await
        .unwrap()
    }

    const LIB: &str = r#"pub mod util;
pub fn run_app() { util::read_config(); }
"#;
    const UTIL: &str = r#"pub fn read_config() -> u8 { 1 }
"#;

    #[tokio::test]
    async fn indexes_symbols_edges_and_provenance() {
        let fx = fixture_with_files(&[
            ("src/lib.rs", LIB),
            ("src/util.rs", UTIL),
            (
                "tests/smoke.rs",
                "use trace_lib::run_app;\n#[test]\nfn smoke_run_app() { run_app(); }\n",
            ),
            ("notes.md", "# notes"),
            ("excluded/generated.rs", "pub fn generated() {}"),
            ("data.bin", "\0\0binary\0"),
        ])
        .await;
        // generated/ matches the default vendor/dist globs? No — add a config glob:
        let outcome = index_fixture(&fx, false).await;

        // Counters: 3 rust files parsed (lib, util, tests/smoke), notes.md and
        // data.bin excluded (unsupported + binary), generated.rs excluded by pattern
        // is configured below, so first run sees it parsed? It is .rs and not excluded
        // by default config; keep expectations aligned with default config.
        assert_eq!(
            outcome.counters.parsed, 4,
            "lib, util, tests/smoke, generated.rs"
        );
        assert_eq!(outcome.counters.reused, 0);
        assert_eq!(outcome.counters.unsupported, 1, "notes.md");
        assert_eq!(outcome.counters.excluded, 2, "notes.md + data.bin");

        let snapshot_id = outcome.snapshot_id;
        let files = fx.store.list_files(snapshot_id).await.unwrap();
        let by_path: HashMap<String, &FileRecord> =
            files.iter().map(|f| (f.path.clone(), f)).collect();
        let lib = by_path.get("src/lib.rs").unwrap();
        assert_eq!(lib.inclusion, FileInclusion::Included);
        let notes = by_path.get("notes.md").unwrap();
        assert_eq!(
            notes.exclusion_reason,
            Some(ExclusionReason::UnsupportedLanguage)
        );
        let bin = by_path.get("data.bin").unwrap();
        assert_eq!(bin.exclusion_reason, Some(ExclusionReason::Binary));
        assert!(bin.binary);

        // Symbols with provenance.
        let symbols = fx.store.list_symbols(snapshot_id).await.unwrap();
        let qnames: HashSet<&str> = symbols.iter().map(|s| s.qualified_name.as_str()).collect();
        assert!(qnames.contains("crate::run_app"), "{qnames:?}");
        assert!(qnames.contains("crate::util::read_config"), "{qnames:?}");
        let run_app = symbols
            .iter()
            .find(|s| s.qualified_name == "crate::run_app")
            .unwrap();
        assert_eq!(run_app.start_line, 2, "1-based line numbers");

        // Call edge: run_app -> read_config resolved? Name-level simple match is
        // unique simple name (read_config defined once) → candidate with target.
        let edges = fx
            .store
            .list_edges_outgoing(snapshot_id, run_app.symbol_id)
            .await
            .unwrap();
        let call = edges
            .iter()
            .find(|e| e.edge_type == RelationshipType::Calls)
            .expect("call edge");
        assert_eq!(call.resolution_status, ResolutionStatus::Candidate);
        assert!(
            call.target_symbol_id.is_some(),
            "unique simple name binds the target row"
        );
        assert_eq!(
            call.provenance_ref.as_deref(),
            Some("src/lib.rs#L2"),
            "provenance carries file and span"
        );

        // Tests edge: heuristic + inferred + candidate.
        let smoke = symbols
            .iter()
            .find(|s| s.qualified_name == "tests::smoke")
            .expect("synthetic test module");
        let test_edges = fx
            .store
            .list_edges_outgoing(snapshot_id, smoke.symbol_id)
            .await
            .unwrap();
        let tests_edge = test_edges
            .iter()
            .find(|e| e.edge_type == RelationshipType::Tests)
            .expect("tests edge");
        assert!(tests_edge.inferred);
        assert_eq!(tests_edge.resolution_method, ResolutionMethod::Heuristic);
        assert_eq!(tests_edge.resolution_status, ResolutionStatus::Candidate);

        // Search entries include symbol + file terms.
        let hits = fx
            .store
            .search(snapshot_id, "read_config", 10)
            .await
            .unwrap();
        assert!(!hits.is_empty());

        // Evidence: one candidate claim per symbol.
        let claims = fx
            .evidence
            .list_claims(fx.repo.repository_id)
            .await
            .unwrap();
        assert!(!claims.is_empty());
        assert!(claims.iter().all(|c| c.status == ClaimStatus::Candidate));

        // Active generation published.
        let active = fx
            .store
            .get_active_generation(fx.repo.repository_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(active.generation_id, outcome.generation_id);
    }

    #[tokio::test]
    async fn incremental_reindex_reparses_only_changed_file() {
        let fx = fixture_with_files(&[("src/lib.rs", LIB), ("src/util.rs", UTIL)]).await;
        let first = index_fixture(&fx, false).await;
        assert_eq!(first.counters.parsed, 2);

        write(
            &fx.root,
            "src/util.rs",
            "pub fn read_config() -> u8 { 2 }\n",
        );
        run(&fx.root, &["add", "-A"]);
        run(&fx.root, &["commit", "-m", "tweak util"]);

        let second = index_fixture(&fx, false).await;
        assert_eq!(second.counters.parsed, 1, "exactly one file reparsed");
        assert_eq!(
            second.counters.reused, 1,
            "unchanged file reused from cache"
        );
    }

    #[tokio::test]
    async fn changed_file_flips_only_dependent_claims_stale() {
        let fx = fixture_with_files(&[("src/lib.rs", LIB), ("src/util.rs", UTIL)]).await;
        index_fixture(&fx, false).await;
        let claims_before = fx
            .evidence
            .list_claims(fx.repo.repository_id)
            .await
            .unwrap();
        let util_claim = claims_before
            .iter()
            .find(|c| c.text.contains("crate::util::read_config"))
            .expect("util claim");
        let lib_claim = claims_before
            .iter()
            .find(|c| c.text.contains("crate::run_app"))
            .expect("lib claim");

        write(
            &fx.root,
            "src/util.rs",
            "pub fn read_config() -> u8 { 3 }\n",
        );
        run(&fx.root, &["add", "-A"]);
        run(&fx.root, &["commit", "-m", "change util"]);
        index_fixture(&fx, false).await;

        let after = fx
            .evidence
            .get_claim(util_claim.claim_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after.status,
            ClaimStatus::Stale,
            "changed file's claim flips stale"
        );
        let untouched = fx
            .evidence
            .get_claim(lib_claim.claim_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            untouched.status,
            ClaimStatus::Candidate,
            "unrelated claim untouched"
        );
    }

    #[tokio::test]
    async fn interrupted_index_leaves_previous_generation_active() {
        let fx = fixture_with_files(&[("src/lib.rs", LIB), ("src/util.rs", UTIL)]).await;
        let first = index_fixture(&fx, false).await;

        // Simulate a killed run: a new generation + snapshot left `building`.
        let (snapshot, _manifest) = build_snapshot(&fx).await;
        let generation = GenerationRecord {
            generation_id: GenerationId::generate(),
            repository_id: fx.repo.repository_id,
            snapshot_id: snapshot.snapshot_id,
            status: GenerationStatus::Building,
            counters_json: "{}".into(),
            created_at: now_rfc3339(),
            published_at: None,
        };
        fx.store.create_generation(&generation).await.unwrap();

        // Next index run cleans up and publishes fresh state.
        let third = index_fixture(&fx, false).await;
        let failed_gen = fx
            .store
            .get_generation(generation.generation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            failed_gen.status,
            GenerationStatus::Failed,
            "stale building generation failed"
        );
        let active = fx
            .store
            .get_active_generation(fx.repo.repository_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(active.generation_id, third.generation_id);
        assert_ne!(active.generation_id, first.generation_id);

        // No mixed revisions: every symbol references a file of the same snapshot.
        let violations = fx
            .store
            .count_cross_snapshot_violations(third.snapshot_id)
            .await
            .unwrap();
        assert_eq!(violations, 0);
    }

    #[tokio::test]
    async fn manifest_globs_exclude_patterns() {
        let fx =
            fixture_with_files(&[("src/lib.rs", LIB), ("vendor/dep.rs", "pub fn v() {}\n")]).await;
        // vendor/** is in the default exclude globs.
        let outcome = index_fixture(&fx, false).await;
        let files = fx.store.list_files(outcome.snapshot_id).await.unwrap();
        let vendored = files.iter().find(|f| f.path == "vendor/dep.rs").unwrap();
        assert_eq!(vendored.inclusion, FileInclusion::Excluded);
        assert_eq!(
            vendored.exclusion_reason,
            Some(ExclusionReason::ExcludedByPattern)
        );
    }
}
