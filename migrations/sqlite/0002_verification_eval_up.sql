-- 0002_verification_eval_up.sql — Step 2: verification records and offline
-- evaluation registries (TIS §11.3, SDD §20.2, SDD Phase G).
--
-- Checks are rows for queryability; check output lives as capped blob
-- artifacts referenced by stdout_ref/stderr_ref (never inline).

CREATE TABLE verification_records (
    verification_id TEXT PRIMARY KEY,
    repository_id   TEXT NOT NULL REFERENCES repositories(repository_id),
    snapshot_id     TEXT NOT NULL REFERENCES snapshots(snapshot_id),
    base            TEXT NOT NULL,
    head            TEXT NOT NULL,
    policy_name     TEXT NOT NULL,
    policy_hash     TEXT NOT NULL,
    mode            TEXT NOT NULL,
    statuses_json   TEXT NOT NULL DEFAULT '{}',
    artifact_hash   TEXT NOT NULL,
    created_at      TEXT NOT NULL
);

CREATE INDEX verifications_by_repo ON verification_records(repository_id, created_at);

CREATE TABLE verification_checks (
    check_id          TEXT PRIMARY KEY,
    verification_id   TEXT NOT NULL REFERENCES verification_records(verification_id) ON DELETE CASCADE,
    position          INTEGER NOT NULL,
    suggested_command TEXT NOT NULL,
    relation          TEXT NOT NULL,
    status            TEXT NOT NULL,
    exit_code         INTEGER,
    duration_ms       INTEGER,
    stdout_ref        TEXT,
    stderr_ref        TEXT,
    truncated         INTEGER NOT NULL DEFAULT 0,
    env_summary_json  TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX checks_by_verification ON verification_checks(verification_id, position);
CREATE INDEX checks_by_status ON verification_checks(verification_id, status);

CREATE TABLE evaluation_suites (
    suite_hash  TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    case_count  INTEGER NOT NULL,
    suite_json  TEXT NOT NULL,
    created_at  TEXT NOT NULL
);

CREATE TABLE evaluation_runs (
    run_id         TEXT PRIMARY KEY,
    repository_id  TEXT NOT NULL REFERENCES repositories(repository_id),
    suite_name     TEXT NOT NULL,
    suite_hash     TEXT NOT NULL REFERENCES evaluation_suites(suite_hash),
    policy_version TEXT NOT NULL,
    policy_hash    TEXT NOT NULL,
    config_hash    TEXT NOT NULL,
    metrics_json   TEXT NOT NULL,
    created_at     TEXT NOT NULL
);

CREATE INDEX eval_runs_by_repo ON evaluation_runs(repository_id, created_at);
