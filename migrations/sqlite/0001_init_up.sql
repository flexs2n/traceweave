-- 0001_init_up.sql — canonical schema (TIS §5, §6).
-- The FTS5 virtual table is created FIRST: it is the plan's top technical
-- risk (R1). If the bundled SQLite lacks FTS5 this migration fails loudly
-- before anything builds on it.

CREATE VIRTUAL TABLE search_fts USING fts5(
    term,
    path UNINDEXED,
    kind UNINDEXED,
    entity_id UNINDEXED,
    title UNINDEXED,
    snapshot_id UNINDEXED
);

CREATE TABLE repositories (
    repository_id      TEXT PRIMARY KEY,
    root_path          TEXT NOT NULL UNIQUE,
    canonical_uri      TEXT,
    default_branch     TEXT,
    configuration_hash TEXT NOT NULL,
    limitations_json   TEXT NOT NULL DEFAULT '[]',
    created_at         TEXT NOT NULL,
    updated_at         TEXT NOT NULL
);

CREATE TABLE snapshots (
    snapshot_id        TEXT PRIMARY KEY,
    repository_id      TEXT NOT NULL REFERENCES repositories(repository_id),
    commit_hash        TEXT,
    branch             TEXT,
    snapshot_kind      TEXT NOT NULL,
    is_dirty           INTEGER NOT NULL DEFAULT 0,
    manifest_hash      TEXT NOT NULL,
    indexer_version    TEXT NOT NULL,
    configuration_hash TEXT NOT NULL,
    status             TEXT NOT NULL,
    created_at         TEXT NOT NULL
);

CREATE UNIQUE INDEX snapshots_identity
    ON snapshots(repository_id, snapshot_kind, is_dirty, manifest_hash, configuration_hash, indexer_version);

CREATE TABLE files (
    file_id          TEXT PRIMARY KEY,
    snapshot_id      TEXT NOT NULL REFERENCES snapshots(snapshot_id) ON DELETE CASCADE,
    repository_id    TEXT NOT NULL REFERENCES repositories(repository_id),
    path             TEXT NOT NULL,
    content_hash     TEXT NOT NULL,
    size_bytes       INTEGER NOT NULL,
    language         TEXT,
    binary           INTEGER NOT NULL DEFAULT 0,
    inclusion        TEXT NOT NULL,
    exclusion_reason TEXT,
    UNIQUE (snapshot_id, path)
);

CREATE TABLE symbols (
    symbol_id         TEXT PRIMARY KEY,
    repository_id     TEXT NOT NULL REFERENCES repositories(repository_id),
    snapshot_id       TEXT NOT NULL REFERENCES snapshots(snapshot_id) ON DELETE CASCADE,
    file_id           TEXT NOT NULL REFERENCES files(file_id),
    symbol_key        TEXT NOT NULL,
    qualified_name    TEXT NOT NULL,
    kind              TEXT NOT NULL,
    language          TEXT NOT NULL,
    start_line        INTEGER NOT NULL,
    start_column      INTEGER NOT NULL,
    end_line          INTEGER NOT NULL,
    end_column        INTEGER NOT NULL,
    signature_hash    TEXT NOT NULL,
    resolver          TEXT NOT NULL,
    resolution_status TEXT NOT NULL
);

CREATE INDEX symbols_by_snapshot ON symbols(snapshot_id);
CREATE INDEX symbols_by_qname ON symbols(snapshot_id, qualified_name);
CREATE INDEX symbols_by_file ON symbols(file_id);

CREATE TABLE "references" (
    reference_id      TEXT PRIMARY KEY,
    snapshot_id       TEXT NOT NULL REFERENCES snapshots(snapshot_id) ON DELETE CASCADE,
    file_id           TEXT NOT NULL REFERENCES files(file_id),
    source_symbol_id  TEXT REFERENCES symbols(symbol_id),
    target_name       TEXT NOT NULL,
    relationship      TEXT NOT NULL,
    start_line        INTEGER NOT NULL,
    start_column      INTEGER NOT NULL,
    end_line          INTEGER NOT NULL,
    end_column        INTEGER NOT NULL,
    resolution_method TEXT NOT NULL,
    resolution_status TEXT NOT NULL
);

CREATE INDEX references_by_snapshot ON "references"(snapshot_id);
CREATE INDEX references_by_file ON "references"(file_id);

CREATE TABLE dependency_edges (
    edge_id            TEXT PRIMARY KEY,
    repository_id      TEXT NOT NULL REFERENCES repositories(repository_id),
    snapshot_id        TEXT NOT NULL REFERENCES snapshots(snapshot_id) ON DELETE CASCADE,
    source_symbol_id   TEXT NOT NULL REFERENCES symbols(symbol_id),
    target_symbol_id   TEXT REFERENCES symbols(symbol_id),
    target_name        TEXT,
    edge_type          TEXT NOT NULL,
    resolution_method  TEXT NOT NULL,
    resolution_status  TEXT NOT NULL,
    confidence         TEXT NOT NULL,
    inferred           INTEGER NOT NULL DEFAULT 0,
    provenance_ref     TEXT,
    created_at         TEXT NOT NULL
);

CREATE INDEX edges_by_snapshot ON dependency_edges(snapshot_id);
CREATE INDEX edges_by_target ON dependency_edges(snapshot_id, target_symbol_id);
CREATE INDEX edges_by_source ON dependency_edges(snapshot_id, source_symbol_id);

CREATE TABLE index_generations (
    generation_id TEXT PRIMARY KEY,
    repository_id TEXT NOT NULL REFERENCES repositories(repository_id),
    snapshot_id   TEXT NOT NULL REFERENCES snapshots(snapshot_id),
    status        TEXT NOT NULL,
    counters_json TEXT NOT NULL DEFAULT '{}',
    created_at    TEXT NOT NULL,
    published_at  TEXT
);

CREATE INDEX generations_by_repo ON index_generations(repository_id, status);

CREATE TABLE repository_state (
    repository_id        TEXT PRIMARY KEY REFERENCES repositories(repository_id),
    active_generation_id TEXT NOT NULL REFERENCES index_generations(generation_id)
);

CREATE TABLE artifacts (
    content_hash    TEXT PRIMARY KEY,
    byte_length     INTEGER NOT NULL,
    media_type      TEXT NOT NULL,
    retention_state TEXT NOT NULL DEFAULT 'retained',
    created_at      TEXT NOT NULL
);

CREATE TABLE parse_cache (
    cache_key     TEXT PRIMARY KEY,
    artifact_hash TEXT NOT NULL,
    created_at    TEXT NOT NULL
);

CREATE TABLE jobs (
    job_id           TEXT PRIMARY KEY,
    repository_id    TEXT NOT NULL REFERENCES repositories(repository_id),
    job_type         TEXT NOT NULL,
    state            TEXT NOT NULL,
    idempotency_key  TEXT NOT NULL,
    payload_json     TEXT,
    retry_count      INTEGER NOT NULL DEFAULT 0,
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL,
    finished_at      TEXT,
    error_code       TEXT,
    error_message    TEXT,
    UNIQUE (repository_id, idempotency_key)
);

CREATE TABLE claims (
    claim_id            TEXT PRIMARY KEY,
    repository_id       TEXT NOT NULL REFERENCES repositories(repository_id),
    snapshot_id         TEXT NOT NULL REFERENCES snapshots(snapshot_id),
    claim_type          TEXT NOT NULL,
    text                TEXT NOT NULL,
    status              TEXT NOT NULL,
    source_kind         TEXT NOT NULL,
    confidence_category TEXT NOT NULL,
    created_at          TEXT NOT NULL,
    validated_at        TEXT,
    updated_at          TEXT NOT NULL
);

CREATE INDEX claims_by_repo ON claims(repository_id, status);

CREATE TABLE evidence (
    evidence_id        TEXT PRIMARY KEY,
    repository_id      TEXT NOT NULL REFERENCES repositories(repository_id),
    snapshot_id        TEXT NOT NULL REFERENCES snapshots(snapshot_id),
    evidence_type      TEXT NOT NULL,
    source_ref         TEXT NOT NULL,
    source_hash        TEXT,
    extraction_method  TEXT NOT NULL,
    extraction_version TEXT NOT NULL,
    actor              TEXT NOT NULL,
    validation_method  TEXT,
    validation_outcome TEXT,
    created_at         TEXT NOT NULL
);

CREATE INDEX evidence_by_repo ON evidence(repository_id);

CREATE TABLE claim_dependencies (
    claim_id        TEXT NOT NULL REFERENCES claims(claim_id),
    depends_on      TEXT NOT NULL,
    depends_on_kind TEXT NOT NULL,
    PRIMARY KEY (claim_id, depends_on, depends_on_kind)
);

CREATE INDEX claim_deps_by_target ON claim_dependencies(depends_on, depends_on_kind);

CREATE TABLE context_bundles (
    bundle_id                 TEXT PRIMARY KEY,
    repository_id             TEXT NOT NULL REFERENCES repositories(repository_id),
    snapshot_id               TEXT NOT NULL REFERENCES snapshots(snapshot_id),
    generation_id             TEXT NOT NULL REFERENCES index_generations(generation_id),
    schema_version            TEXT NOT NULL,
    task_text                 TEXT NOT NULL,
    normalized_json           TEXT NOT NULL,
    retrieval_policy_version  TEXT NOT NULL,
    budget_tokens             INTEGER NOT NULL,
    estimated_tokens          INTEGER NOT NULL,
    token_estimate_basis      TEXT NOT NULL,
    stopping_reason           TEXT NOT NULL,
    bundle_hash               TEXT NOT NULL,
    artifact_hash             TEXT NOT NULL,
    created_at                TEXT NOT NULL
);

CREATE INDEX bundles_by_repo ON context_bundles(repository_id);

CREATE TABLE bundle_items (
    bundle_id TEXT NOT NULL REFERENCES context_bundles(bundle_id) ON DELETE CASCADE,
    position  INTEGER NOT NULL,
    item_json TEXT NOT NULL,
    PRIMARY KEY (bundle_id, position)
);

CREATE TABLE impact_reports (
    impact_id      TEXT PRIMARY KEY,
    repository_id  TEXT NOT NULL REFERENCES repositories(repository_id),
    snapshot_id    TEXT NOT NULL REFERENCES snapshots(snapshot_id),
    base           TEXT NOT NULL,
    head           TEXT NOT NULL,
    schema_version TEXT NOT NULL,
    report_hash    TEXT NOT NULL,
    artifact_hash  TEXT NOT NULL,
    created_at     TEXT NOT NULL
);

CREATE INDEX impacts_by_repo ON impact_reports(repository_id);
