DROP TABLE IF EXISTS memory_status_events;
DROP TABLE IF EXISTS memory_claims;
DROP TABLE IF EXISTS memory_fts;
DROP TABLE IF EXISTS memory_records;
ALTER TABLE claims DROP COLUMN supersedes_claim_id;

-- 0003_memory_down.sql — manual reverse of 0003 (sqlx 0.8 has no revert so
-- the up/down test executes this file statement-by-statement, and statements
-- come FIRST because a chunk that begins with a comment line is skipped).
-- `supersedes_claim_id` is droppable because it carries no REFERENCES clause
-- (SQLite cannot DROP a column involved in a foreign key) — see
-- 0003_memory_up.sql.
