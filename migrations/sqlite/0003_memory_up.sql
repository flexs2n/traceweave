-- 0003_memory_up.sql — Step 3: engineering memory, supersession, memory FTS
-- (SDD §5.11/§9.3, TIS §12, FR-032/033/034/035).
--
-- The FTS5 virtual table is created FIRST, mirroring 0001's discipline
-- (plan risk R1): if the bundled SQLite lacks FTS5 this fails loudly before
-- anything builds on it.

CREATE VIRTUAL TABLE memory_fts USING fts5(
    term,
    memory_id UNINDEXED,
    repository_id UNINDEXED
);

CREATE TABLE memory_records (
    memory_id            TEXT PRIMARY KEY,
    repository_id        TEXT NOT NULL REFERENCES repositories(repository_id),
    snapshot_id          TEXT NOT NULL REFERENCES snapshots(snapshot_id),
    memory_type          TEXT NOT NULL,
    title                TEXT NOT NULL,
    summary              TEXT NOT NULL,
    scope                TEXT NOT NULL DEFAULT '',
    created_by           TEXT NOT NULL,
    created_at           TEXT NOT NULL,
    last_accessed_at     TEXT NOT NULL,
    status               TEXT NOT NULL,
    sharing_policy       TEXT NOT NULL DEFAULT 'private',
    sensitivity          TEXT NOT NULL DEFAULT 'none',
    supersedes_memory_id TEXT REFERENCES memory_records(memory_id),
    stale_cause          TEXT,
    updated_at           TEXT NOT NULL
);

CREATE INDEX memories_by_repo ON memory_records(repository_id, status);
CREATE INDEX memories_by_scope ON memory_records(repository_id, scope);

-- Claim links: memory is a projection over claims, never a second source of
-- truth (SDD §5.11 — a memory without linked claims is unverified/candidate).
CREATE TABLE memory_claims (
    memory_id TEXT NOT NULL REFERENCES memory_records(memory_id) ON DELETE CASCADE,
    claim_id  TEXT NOT NULL REFERENCES claims(claim_id),
    PRIMARY KEY (memory_id, claim_id)
);

CREATE INDEX memory_claims_by_claim ON memory_claims(claim_id);

-- Appended history trail for memory status transitions (FR-032: edits
-- preserve history and provenance).
CREATE TABLE memory_status_events (
    event_id   TEXT PRIMARY KEY,
    memory_id  TEXT NOT NULL REFERENCES memory_records(memory_id) ON DELETE CASCADE,
    status     TEXT NOT NULL,
    actor      TEXT NOT NULL,
    reason     TEXT,
    created_at TEXT NOT NULL
);

CREATE INDEX memory_events_by_memory ON memory_status_events(memory_id, created_at);

-- Supersession on claims (SDD §5.8). Additive nullable column. Deliberately
-- WITHOUT a REFERENCES clause: SQLite cannot DROP a column involved in a
-- foreign key, and the manual down migration must restore the exact 0002
-- schema. Integrity is enforced in ccc-memory — the superseding flow loads
-- and flips the target claim in the same operation, so a dangling pointer
-- cannot arise through the API.
ALTER TABLE claims ADD COLUMN supersedes_claim_id TEXT;
