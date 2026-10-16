-- 0002_verification_eval_down.sql — manual reverse of 0002 (sqlx 0.8 has no
-- revert so the up/down test executes this file statement-by-statement).
-- Order respects FK dependencies.

DROP TABLE IF EXISTS evaluation_runs;
DROP TABLE IF EXISTS evaluation_suites;
DROP TABLE IF EXISTS verification_checks;
DROP TABLE IF EXISTS verification_records;
