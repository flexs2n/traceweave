//! SQLite adapter implementing the store traits (TIS §5.1).
//!
//! - WAL journaling, `foreign_keys=ON`, explicit busy timeout.
//! - Two bounded pools: a single-connection writer pool (serialized writes,
//!   TIS §5.1 forbids unbounded write pools) and a small reader pool.
//! - Runtime-prepared queries only (`sqlx::query`), per plan decision.
//! - Short transactions; publication is a single atomic transaction.

use crate::traits::*;
use ccc_domain::*;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow};
use sqlx::{Pool, Row, Sqlite, SqlitePool, Transaction};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

/// SQLite-backed store facade.
pub struct SqliteStore {
    reader: SqlitePool,
    writer: SqlitePool,
}

impl SqliteStore {
    /// Opens (creating if needed) the database and applies migrations.
    ///
    /// The first storage action is the FTS5 smoke test (plan risk R1): if the
    /// bundled SQLite lacks FTS5 this returns `DEPENDENCY_UNAVAILABLE` before
    /// any migration runs.
    pub async fn connect(db_path: &Path) -> StoreResult<Self> {
        if let Some(parent) = db_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| CccError::internal_error(format!("cannot create data dir: {e}")))?;
        }
        smoke_test_fts5().await?;

        let options = SqliteConnectOptions::from_str(&format!(
            "sqlite://{}",
            db_path.to_string_lossy().replace('\\', "/")
        ))
        .map_err(|e| CccError::internal_error(format!("invalid db path: {e}")))?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));

        let reader = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options.clone())
            .await
            .map_err(|e| CccError::dependency_unavailable(format!("cannot open database: {e}")))?;
        // Single serialized writer connection (TIS §5.1).
        let writer = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .map_err(|e| CccError::dependency_unavailable(format!("cannot open database: {e}")))?;

        let store = Self { reader, writer };
        store.run_migrations().await?;
        Ok(store)
    }

    async fn run_migrations(&self) -> StoreResult<()> {
        let migrator = sqlx::migrate!("../../migrations/sqlite");
        migrator
            .run(&self.writer)
            .await
            .map_err(|e| CccError::internal_error(format!("migrations failed: {e}")))?;
        Ok(())
    }
}

fn db_err(e: sqlx::Error) -> CccError {
    match &e {
        sqlx::Error::Database(db) => {
            // 2067 = UNIQUE constraint violation in SQLite extended codes.
            if db.code().as_deref() == Some("2067") {
                CccError::job_conflict("a record with this unique key already exists")
            } else {
                CccError::internal_error(format!("database error: {db}"))
            }
        }
        _ => CccError::internal_error(format!("database error: {e}")),
    }
}

fn not_found(message: &'static str) -> CccError {
    CccError::snapshot_not_found(message)
}

/// Verifies FTS5 availability on a scratch in-memory database (plan risk R1).
async fn smoke_test_fts5() -> StoreResult<()> {
    let options = SqliteConnectOptions::from_str("sqlite::memory:")
        .map_err(|e| CccError::internal_error(format!("sqlite options: {e}")))?;
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .map_err(|e| CccError::dependency_unavailable(format!("sqlite unavailable: {e}")))?;
    sqlx::query("CREATE VIRTUAL TABLE fts5_smoke USING fts5(content)")
        .execute(&pool)
        .await
        .map_err(|_| {
            CccError::dependency_unavailable(
                "the SQLite build in use does not provide FTS5; ccc requires it for lexical search",
            )
        })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn opt_string(row: &SqliteRow, col: &str) -> Option<String> {
    row.try_get::<Option<String>, _>(col).unwrap_or(None)
}

fn string(row: &SqliteRow, col: &str) -> String {
    row.try_get::<String, _>(col).unwrap_or_default()
}

fn i64_or(row: &SqliteRow, col: &str, default: i64) -> i64 {
    row.try_get::<Option<i64>, _>(col)
        .ok()
        .flatten()
        .unwrap_or(default)
}

fn snapshot_kind_from(s: &str) -> SnapshotKind {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .unwrap_or(SnapshotKind::WorkingTree)
}

fn status_from(s: &str) -> SnapshotStatus {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .unwrap_or(SnapshotStatus::Failed)
}

fn gen_status_from(s: &str) -> GenerationStatus {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .unwrap_or(GenerationStatus::Failed)
}

fn job_state_from(s: &str) -> JobState {
    serde_json::from_value(serde_json::Value::String(s.to_string())).unwrap_or(JobState::Failed)
}

type Tx<'c> = Transaction<'c, Sqlite>;

#[async_trait::async_trait]
impl RepositoryStore for SqliteStore {
    async fn insert_repository(&self, record: &RepositoryRecord) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO repositories (repository_id, root_path, canonical_uri, default_branch, \
             configuration_hash, limitations_json, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(record.repository_id.to_string())
        .bind(&record.root_path)
        .bind(record.canonical_uri.clone())
        .bind(record.default_branch.clone())
        .bind(&record.configuration_hash)
        .bind(serde_json::to_string(&record.limitations).unwrap_or_else(|_| "[]".into()))
        .bind(&record.created_at)
        .bind(&record.updated_at)
        .execute(&self.writer)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn find_repository_by_path(
        &self,
        root_path: &str,
    ) -> StoreResult<Option<RepositoryRecord>> {
        let row = sqlx::query("SELECT * FROM repositories WHERE root_path = ?")
            .bind(root_path)
            .fetch_optional(&self.reader)
            .await
            .map_err(db_err)?;
        row.map(|r| row_to_repository(&r)).transpose()
    }

    async fn get_repository(&self, id: RepositoryId) -> StoreResult<Option<RepositoryRecord>> {
        let row = sqlx::query("SELECT * FROM repositories WHERE repository_id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.reader)
            .await
            .map_err(db_err)?;
        row.map(|r| row_to_repository(&r)).transpose()
    }

    async fn list_repositories(&self) -> StoreResult<Vec<RepositoryRecord>> {
        let rows = sqlx::query("SELECT * FROM repositories ORDER BY created_at")
            .fetch_all(&self.reader)
            .await
            .map_err(db_err)?;
        rows.iter().map(row_to_repository).collect()
    }

    async fn update_repository_configuration(
        &self,
        id: RepositoryId,
        configuration_hash: &str,
        updated_at: &str,
    ) -> StoreResult<()> {
        sqlx::query("UPDATE repositories SET configuration_hash = ?, updated_at = ? WHERE repository_id = ?")
            .bind(configuration_hash)
            .bind(updated_at)
            .bind(id.to_string())
            .execute(&self.writer)
            .await
            .map_err(db_err)?;
        Ok(())
    }
}

fn row_to_repository(row: &SqliteRow) -> StoreResult<RepositoryRecord> {
    Ok(RepositoryRecord {
        repository_id: RepositoryId::from_str(&string(row, "repository_id"))?,
        root_path: string(row, "root_path"),
        canonical_uri: opt_string(row, "canonical_uri"),
        default_branch: opt_string(row, "default_branch"),
        configuration_hash: string(row, "configuration_hash"),
        limitations: serde_json::from_str(&string(row, "limitations_json")).unwrap_or_default(),
        created_at: string(row, "created_at"),
        updated_at: string(row, "updated_at"),
    })
}

const SNAPSHOT_COLS: &str = "snapshot_id, repository_id, commit_hash, branch, snapshot_kind, \
     is_dirty, manifest_hash, indexer_version, configuration_hash, status, created_at";

fn row_to_snapshot(row: &SqliteRow) -> StoreResult<SnapshotRecord> {
    Ok(SnapshotRecord {
        snapshot_id: SnapshotId::from_str(&string(row, "snapshot_id"))?,
        repository_id: RepositoryId::from_str(&string(row, "repository_id"))?,
        commit_hash: opt_string(row, "commit_hash"),
        branch: opt_string(row, "branch"),
        kind: snapshot_kind_from(&string(row, "snapshot_kind")),
        is_dirty: i64_or(row, "is_dirty", 0) != 0,
        manifest_hash: string(row, "manifest_hash"),
        indexer_version: string(row, "indexer_version"),
        configuration_hash: string(row, "configuration_hash"),
        status: status_from(&string(row, "status")),
        created_at: string(row, "created_at"),
    })
}

#[async_trait::async_trait]
impl SnapshotStore for SqliteStore {
    async fn insert_snapshot(&self, record: &SnapshotRecord) -> StoreResult<()> {
        sqlx::query(&format!(
            "INSERT INTO snapshots ({SNAPSHOT_COLS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        ))
        .bind(record.snapshot_id.to_string())
        .bind(record.repository_id.to_string())
        .bind(record.commit_hash.clone())
        .bind(record.branch.clone())
        .bind(
            serde_json::to_string(&record.kind)
                .unwrap_or_default()
                .trim_matches('"')
                .to_string(),
        )
        .bind(record.is_dirty as i64)
        .bind(&record.manifest_hash)
        .bind(&record.indexer_version)
        .bind(&record.configuration_hash)
        .bind(
            serde_json::to_string(&record.status)
                .unwrap_or_default()
                .trim_matches('"')
                .to_string(),
        )
        .bind(&record.created_at)
        .execute(&self.writer)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn get_snapshot(&self, id: SnapshotId) -> StoreResult<Option<SnapshotRecord>> {
        let row = sqlx::query("SELECT * FROM snapshots WHERE snapshot_id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.reader)
            .await
            .map_err(db_err)?;
        row.map(|r| row_to_snapshot(&r)).transpose()
    }

    async fn find_identical_snapshot(
        &self,
        repository_id: RepositoryId,
        kind: SnapshotKind,
        is_dirty: bool,
        manifest_hash: &str,
        configuration_hash: &str,
        indexer_version: &str,
    ) -> StoreResult<Option<SnapshotRecord>> {
        let row = sqlx::query(
            "SELECT * FROM snapshots WHERE repository_id = ? AND snapshot_kind = ? AND is_dirty = ? \
             AND manifest_hash = ? AND configuration_hash = ? AND indexer_version = ? \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(repository_id.to_string())
        .bind(serde_json::to_string(&kind).unwrap_or_default().trim_matches('"').to_string())
        .bind(is_dirty as i64)
        .bind(manifest_hash)
        .bind(configuration_hash)
        .bind(indexer_version)
        .fetch_optional(&self.reader)
        .await
        .map_err(db_err)?;
        row.map(|r| row_to_snapshot(&r)).transpose()
    }

    async fn set_snapshot_status(&self, id: SnapshotId, status: SnapshotStatus) -> StoreResult<()> {
        let current = self
            .get_snapshot(id)
            .await?
            .ok_or_else(|| not_found("snapshot not found"))?;
        let legal = matches!(
            (current.status, status),
            (SnapshotStatus::Building, SnapshotStatus::Published)
                | (SnapshotStatus::Building, SnapshotStatus::Failed)
                | (SnapshotStatus::Building, SnapshotStatus::Superseded)
                | (SnapshotStatus::Published, SnapshotStatus::Superseded)
                | (SnapshotStatus::Failed, SnapshotStatus::Superseded)
                | (SnapshotStatus::Failed, SnapshotStatus::Building)
                | (SnapshotStatus::Superseded, SnapshotStatus::Building)
        );
        if !legal {
            return Err(CccError::job_conflict(format!(
                "illegal snapshot status transition {:?} -> {:?}",
                current.status, status
            )));
        }
        sqlx::query("UPDATE snapshots SET status = ? WHERE snapshot_id = ?")
            .bind(
                serde_json::to_string(&status)
                    .unwrap_or_default()
                    .trim_matches('"')
                    .to_string(),
            )
            .bind(id.to_string())
            .execute(&self.writer)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn list_snapshots(
        &self,
        repository_id: RepositoryId,
    ) -> StoreResult<Vec<SnapshotRecord>> {
        let rows =
            sqlx::query("SELECT * FROM snapshots WHERE repository_id = ? ORDER BY created_at DESC")
                .bind(repository_id.to_string())
                .fetch_all(&self.reader)
                .await
                .map_err(db_err)?;
        rows.iter().map(row_to_snapshot).collect()
    }

    async fn fail_interrupted_snapshots(&self, repository_id: RepositoryId) -> StoreResult<u64> {
        let result =
            sqlx::query("UPDATE snapshots SET status = 'failed' WHERE repository_id = ? AND status = 'building'")
                .bind(repository_id.to_string())
                .execute(&self.writer)
                .await
                .map_err(db_err)?;
        Ok(result.rows_affected())
    }
}

const GEN_COLS: &str =
    "generation_id, repository_id, snapshot_id, status, counters_json, created_at, published_at";

fn row_to_generation(row: &SqliteRow) -> StoreResult<GenerationRecord> {
    Ok(GenerationRecord {
        generation_id: GenerationId::from_str(&string(row, "generation_id"))?,
        repository_id: RepositoryId::from_str(&string(row, "repository_id"))?,
        snapshot_id: SnapshotId::from_str(&string(row, "snapshot_id"))?,
        status: gen_status_from(&string(row, "status")),
        counters_json: string(row, "counters_json"),
        created_at: string(row, "created_at"),
        published_at: opt_string(row, "published_at"),
    })
}

#[async_trait::async_trait]
impl IndexStore for SqliteStore {
    async fn insert_files(&self, records: &[FileRecord]) -> StoreResult<()> {
        let mut tx = self.writer.begin().await.map_err(db_err)?;
        for f in records {
            sqlx::query(
                "INSERT INTO files (file_id, snapshot_id, repository_id, path, content_hash, size_bytes, language, binary, inclusion, exclusion_reason) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(f.file_id.to_string())
            .bind(f.snapshot_id.to_string())
            .bind(f.repository_id.to_string())
            .bind(&f.path)
            .bind(&f.content_hash)
            .bind(f.size_bytes as i64)
            .bind(f.language.clone())
            .bind(f.binary as i64)
            .bind(serde_json::to_string(&f.inclusion).unwrap_or_default().trim_matches('"').to_string())
            .bind(f.exclusion_reason.map(|r| serde_json::to_string(&r).unwrap_or_default().trim_matches('"').to_string()))
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)
    }

    async fn insert_symbols(&self, records: &[SymbolRecord]) -> StoreResult<()> {
        let mut tx = self.writer.begin().await.map_err(db_err)?;
        for s in records {
            sqlx::query(
                "INSERT INTO symbols (symbol_id, repository_id, snapshot_id, file_id, symbol_key, qualified_name, kind, language, \
                 start_line, start_column, end_line, end_column, signature_hash, resolver, resolution_status) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(s.symbol_id.to_string())
            .bind(s.repository_id.to_string())
            .bind(s.snapshot_id.to_string())
            .bind(s.file_id.to_string())
            .bind(&s.symbol_key)
            .bind(&s.qualified_name)
            .bind(serde_json::to_string(&s.kind).unwrap_or_default().trim_matches('"').to_string())
            .bind(&s.language)
            .bind(s.start_line as i64)
            .bind(s.start_column as i64)
            .bind(s.end_line as i64)
            .bind(s.end_column as i64)
            .bind(&s.signature_hash)
            .bind(serde_json::to_string(&s.resolver).unwrap_or_default().trim_matches('"').to_string())
            .bind(serde_json::to_string(&s.resolution_status).unwrap_or_default().trim_matches('"').to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)
    }

    async fn insert_references(&self, records: &[ReferenceRecord]) -> StoreResult<()> {
        let mut tx = self.writer.begin().await.map_err(db_err)?;
        for r in records {
            sqlx::query(
                "INSERT INTO \"references\" (reference_id, snapshot_id, file_id, source_symbol_id, target_name, relationship, \
                 start_line, start_column, end_line, end_column, resolution_method, resolution_status) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(r.reference_id.to_string())
            .bind(r.snapshot_id.to_string())
            .bind(r.file_id.to_string())
            .bind(r.source_symbol_id.map(|s| s.to_string()))
            .bind(&r.target_name)
            .bind(serde_json::to_string(&r.relationship).unwrap_or_default().trim_matches('"').to_string())
            .bind(r.start_line as i64)
            .bind(r.start_column as i64)
            .bind(r.end_line as i64)
            .bind(r.end_column as i64)
            .bind(serde_json::to_string(&r.resolution_method).unwrap_or_default().trim_matches('"').to_string())
            .bind(serde_json::to_string(&r.resolution_status).unwrap_or_default().trim_matches('"').to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)
    }

    async fn insert_edges(&self, records: &[EdgeRecord]) -> StoreResult<()> {
        let mut tx = self.writer.begin().await.map_err(db_err)?;
        for e in records {
            sqlx::query(
                "INSERT INTO dependency_edges (edge_id, repository_id, snapshot_id, source_symbol_id, target_symbol_id, \
                 target_name, edge_type, resolution_method, resolution_status, confidence, inferred, provenance_ref, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(e.edge_id.to_string())
            .bind(e.repository_id.to_string())
            .bind(e.snapshot_id.to_string())
            .bind(e.source_symbol_id.to_string())
            .bind(e.target_symbol_id.map(|s| s.to_string()))
            .bind(e.target_name.clone())
            .bind(serde_json::to_string(&e.edge_type).unwrap_or_default().trim_matches('"').to_string())
            .bind(serde_json::to_string(&e.resolution_method).unwrap_or_default().trim_matches('"').to_string())
            .bind(serde_json::to_string(&e.resolution_status).unwrap_or_default().trim_matches('"').to_string())
            .bind(serde_json::to_string(&e.confidence).unwrap_or_default().trim_matches('"').to_string())
            .bind(e.inferred as i64)
            .bind(e.provenance_ref.clone())
            .bind(&e.created_at)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)
    }

    async fn list_files(&self, snapshot_id: SnapshotId) -> StoreResult<Vec<FileRecord>> {
        let rows = sqlx::query("SELECT * FROM files WHERE snapshot_id = ? ORDER BY path")
            .bind(snapshot_id.to_string())
            .fetch_all(&self.reader)
            .await
            .map_err(db_err)?;
        rows.iter().map(row_to_file).collect()
    }

    async fn get_file_by_path(
        &self,
        snapshot_id: SnapshotId,
        path: &str,
    ) -> StoreResult<Option<FileRecord>> {
        let row = sqlx::query("SELECT * FROM files WHERE snapshot_id = ? AND path = ?")
            .bind(snapshot_id.to_string())
            .bind(path)
            .fetch_optional(&self.reader)
            .await
            .map_err(db_err)?;
        row.map(|r| row_to_file(&r)).transpose()
    }

    async fn list_symbols(&self, snapshot_id: SnapshotId) -> StoreResult<Vec<SymbolRecord>> {
        let rows =
            sqlx::query("SELECT * FROM symbols WHERE snapshot_id = ? ORDER BY qualified_name")
                .bind(snapshot_id.to_string())
                .fetch_all(&self.reader)
                .await
                .map_err(db_err)?;
        rows.iter().map(row_to_symbol).collect()
    }

    async fn list_symbols_for_file(
        &self,
        snapshot_id: SnapshotId,
        file_id: FileId,
    ) -> StoreResult<Vec<SymbolRecord>> {
        let rows = sqlx::query(
            "SELECT * FROM symbols WHERE snapshot_id = ? AND file_id = ? ORDER BY qualified_name",
        )
        .bind(snapshot_id.to_string())
        .bind(file_id.to_string())
        .fetch_all(&self.reader)
        .await
        .map_err(db_err)?;
        rows.iter().map(row_to_symbol).collect()
    }

    async fn list_edges_outgoing(
        &self,
        snapshot_id: SnapshotId,
        source: SymbolId,
    ) -> StoreResult<Vec<EdgeRecord>> {
        let rows = sqlx::query(
            "SELECT * FROM dependency_edges WHERE snapshot_id = ? AND source_symbol_id = ?",
        )
        .bind(snapshot_id.to_string())
        .bind(source.to_string())
        .fetch_all(&self.reader)
        .await
        .map_err(db_err)?;
        rows.iter().map(row_to_edge).collect()
    }

    async fn list_edges_incoming(
        &self,
        snapshot_id: SnapshotId,
        target: SymbolId,
    ) -> StoreResult<Vec<EdgeRecord>> {
        let rows = sqlx::query(
            "SELECT * FROM dependency_edges WHERE snapshot_id = ? AND target_symbol_id = ?",
        )
        .bind(snapshot_id.to_string())
        .bind(target.to_string())
        .fetch_all(&self.reader)
        .await
        .map_err(db_err)?;
        rows.iter().map(row_to_edge).collect()
    }

    async fn create_generation(&self, record: &GenerationRecord) -> StoreResult<()> {
        sqlx::query(&format!(
            "INSERT INTO index_generations ({GEN_COLS}) VALUES (?, ?, ?, ?, ?, ?, ?)"
        ))
        .bind(record.generation_id.to_string())
        .bind(record.repository_id.to_string())
        .bind(record.snapshot_id.to_string())
        .bind(
            serde_json::to_string(&record.status)
                .unwrap_or_default()
                .trim_matches('"')
                .to_string(),
        )
        .bind(&record.counters_json)
        .bind(&record.created_at)
        .bind(record.published_at.clone())
        .execute(&self.writer)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn get_generation(&self, id: GenerationId) -> StoreResult<Option<GenerationRecord>> {
        let row = sqlx::query("SELECT * FROM index_generations WHERE generation_id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.reader)
            .await
            .map_err(db_err)?;
        row.map(|r| row_to_generation(&r)).transpose()
    }

    async fn get_active_generation(
        &self,
        repository_id: RepositoryId,
    ) -> StoreResult<Option<GenerationRecord>> {
        let row = sqlx::query(
            "SELECT g.* FROM index_generations g \
             JOIN repository_state s ON s.active_generation_id = g.generation_id \
             WHERE s.repository_id = ?",
        )
        .bind(repository_id.to_string())
        .fetch_optional(&self.reader)
        .await
        .map_err(db_err)?;
        row.map(|r| row_to_generation(&r)).transpose()
    }

    async fn set_generation_counters(
        &self,
        id: GenerationId,
        counters_json: &str,
    ) -> StoreResult<()> {
        sqlx::query("UPDATE index_generations SET counters_json = ? WHERE generation_id = ?")
            .bind(counters_json)
            .bind(id.to_string())
            .execute(&self.writer)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn fail_building_generations(&self, repository_id: RepositoryId) -> StoreResult<u64> {
        let result = sqlx::query(
            "UPDATE index_generations SET status = 'failed' WHERE repository_id = ? AND status = 'building'",
        )
        .bind(repository_id.to_string())
        .execute(&self.writer)
        .await
        .map_err(db_err)?;
        Ok(result.rows_affected())
    }

    async fn count_cross_snapshot_violations(&self, snapshot_id: SnapshotId) -> StoreResult<u64> {
        let row = sqlx::query(
            "SELECT \
               (SELECT COUNT(*) FROM symbols s JOIN files f ON f.file_id = s.file_id \
                  WHERE s.snapshot_id = ? AND f.snapshot_id != s.snapshot_id) + \
               (SELECT COUNT(*) FROM dependency_edges e \
                  JOIN symbols src ON src.symbol_id = e.source_symbol_id \
                  WHERE e.snapshot_id = ? AND src.snapshot_id != e.snapshot_id) + \
               (SELECT COUNT(*) FROM dependency_edges e \
                  JOIN symbols tgt ON tgt.symbol_id = e.target_symbol_id \
                  WHERE e.snapshot_id = ? AND tgt.snapshot_id != e.snapshot_id) AS violations",
        )
        .bind(snapshot_id.to_string())
        .bind(snapshot_id.to_string())
        .bind(snapshot_id.to_string())
        .fetch_one(&self.reader)
        .await
        .map_err(db_err)?;
        Ok(i64_or(&row, "violations", 0) as u64)
    }

    async fn delete_snapshot_content(&self, snapshot_id: SnapshotId) -> StoreResult<()> {
        let mut tx = self.writer.begin().await.map_err(db_err)?;
        sqlx::query("DELETE FROM dependency_edges WHERE snapshot_id = ?")
            .bind(snapshot_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        sqlx::query("DELETE FROM \"references\" WHERE snapshot_id = ?")
            .bind(snapshot_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        sqlx::query("DELETE FROM symbols WHERE snapshot_id = ?")
            .bind(snapshot_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        sqlx::query("DELETE FROM files WHERE snapshot_id = ?")
            .bind(snapshot_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        tx.commit().await.map_err(db_err)
    }

    async fn get_cached_parse(&self, cache_key: &str) -> StoreResult<Option<String>> {
        let row = sqlx::query("SELECT artifact_hash FROM parse_cache WHERE cache_key = ?")
            .bind(cache_key)
            .fetch_optional(&self.reader)
            .await
            .map_err(db_err)?;
        Ok(row.map(|r| string(&r, "artifact_hash")))
    }

    async fn store_cached_parse(&self, cache_key: &str, artifact_hash: &str) -> StoreResult<()> {
        sqlx::query("INSERT OR IGNORE INTO parse_cache (cache_key, artifact_hash, created_at) VALUES (?, ?, ?)")
            .bind(cache_key)
            .bind(artifact_hash)
            .bind(now_rfc3339())
            .execute(&self.writer)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn publish_generation(&self, id: GenerationId) -> StoreResult<()> {
        // TIS §4.4 / SDD §7.5: publication is one transaction on the single
        // writer connection; readers keep using the previous active generation.
        let mut tx: Tx = self.writer.begin().await.map_err(db_err)?;
        let gen = sqlx::query("SELECT * FROM index_generations WHERE generation_id = ?")
            .bind(id.to_string())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("generation not found"))?;
        let repository_id = string(&gen, "repository_id");
        let snapshot_id = string(&gen, "snapshot_id");
        let status = string(&gen, "status");
        if status != "building" {
            return Err(CccError::job_conflict(format!(
                "generation is not publishable from status {status:?}"
            )));
        }

        // Referential integrity gate (TIS §4.4 step 4).
        let violations = sqlx::query(
            "SELECT \
               (SELECT COUNT(*) FROM symbols s JOIN files f ON f.file_id = s.file_id \
                  WHERE s.snapshot_id = ? AND f.snapshot_id != s.snapshot_id) + \
               (SELECT COUNT(*) FROM dependency_edges e \
                  JOIN symbols src ON src.symbol_id = e.source_symbol_id \
                  WHERE e.snapshot_id = ? AND src.snapshot_id != e.snapshot_id) + \
               (SELECT COUNT(*) FROM dependency_edges e \
                  JOIN symbols tgt ON tgt.symbol_id = e.target_symbol_id \
                  WHERE e.snapshot_id = ? AND tgt.snapshot_id != e.snapshot_id) AS violations",
        )
        .bind(&snapshot_id)
        .bind(&snapshot_id)
        .bind(&snapshot_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        let violation_count = i64_or(&violations, "violations", 0);
        if violation_count > 0 {
            return Err(CccError::internal_error(format!(
                "refusing to publish generation with {violation_count} cross-snapshot referential violations"
            )));
        }

        let now = now_rfc3339();
        sqlx::query(
            "UPDATE index_generations SET status = 'superseded' WHERE repository_id = ? AND status = 'published'",
        )
        .bind(&repository_id)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        sqlx::query(
            "UPDATE snapshots SET status = 'superseded' WHERE repository_id = ? AND status = 'published' AND snapshot_id != ?",
        )
        .bind(&repository_id)
        .bind(&snapshot_id)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        sqlx::query("UPDATE index_generations SET status = 'published', published_at = ? WHERE generation_id = ?")
            .bind(&now)
            .bind(id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        sqlx::query("UPDATE snapshots SET status = 'published' WHERE snapshot_id = ?")
            .bind(&snapshot_id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        sqlx::query(
            "INSERT INTO repository_state (repository_id, active_generation_id) VALUES (?, ?) \
             ON CONFLICT(repository_id) DO UPDATE SET active_generation_id = excluded.active_generation_id",
        )
        .bind(&repository_id)
        .bind(id.to_string())
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)
    }
}

fn row_to_file(row: &SqliteRow) -> StoreResult<FileRecord> {
    Ok(FileRecord {
        file_id: FileId::from_str(&string(row, "file_id"))?,
        snapshot_id: SnapshotId::from_str(&string(row, "snapshot_id"))?,
        repository_id: RepositoryId::from_str(&string(row, "repository_id"))?,
        path: string(row, "path"),
        content_hash: string(row, "content_hash"),
        size_bytes: i64_or(row, "size_bytes", 0) as u64,
        language: opt_string(row, "language"),
        binary: i64_or(row, "binary", 0) != 0,
        inclusion: serde_json::from_value(serde_json::Value::String(string(row, "inclusion")))
            .unwrap_or(FileInclusion::Excluded),
        exclusion_reason: opt_string(row, "exclusion_reason")
            .and_then(|r| serde_json::from_value(serde_json::Value::String(r)).ok()),
    })
}

fn row_to_symbol(row: &SqliteRow) -> StoreResult<SymbolRecord> {
    Ok(SymbolRecord {
        symbol_id: SymbolId::from_str(&string(row, "symbol_id"))?,
        repository_id: RepositoryId::from_str(&string(row, "repository_id"))?,
        snapshot_id: SnapshotId::from_str(&string(row, "snapshot_id"))?,
        file_id: FileId::from_str(&string(row, "file_id"))?,
        symbol_key: string(row, "symbol_key"),
        qualified_name: string(row, "qualified_name"),
        kind: serde_json::from_value(serde_json::Value::String(string(row, "kind")))
            .unwrap_or(SymbolKind::Other),
        language: string(row, "language"),
        start_line: i64_or(row, "start_line", 0) as u32,
        start_column: i64_or(row, "start_column", 0) as u32,
        end_line: i64_or(row, "end_line", 0) as u32,
        end_column: i64_or(row, "end_column", 0) as u32,
        signature_hash: string(row, "signature_hash"),
        resolver: serde_json::from_value(serde_json::Value::String(string(row, "resolver")))
            .unwrap_or(ResolutionMethod::TreeSitter),
        resolution_status: serde_json::from_value(serde_json::Value::String(string(
            row,
            "resolution_status",
        )))
        .unwrap_or(ResolutionStatus::Candidate),
    })
}

fn row_to_edge(row: &SqliteRow) -> StoreResult<EdgeRecord> {
    Ok(EdgeRecord {
        edge_id: EdgeId::from_str(&string(row, "edge_id"))?,
        repository_id: RepositoryId::from_str(&string(row, "repository_id"))?,
        snapshot_id: SnapshotId::from_str(&string(row, "snapshot_id"))?,
        source_symbol_id: SymbolId::from_str(&string(row, "source_symbol_id"))?,
        target_symbol_id: opt_string(row, "target_symbol_id")
            .and_then(|s| SymbolId::from_str(&s).ok()),
        target_name: opt_string(row, "target_name"),
        edge_type: serde_json::from_value(serde_json::Value::String(string(row, "edge_type")))
            .unwrap_or(RelationshipType::References),
        resolution_method: serde_json::from_value(serde_json::Value::String(string(
            row,
            "resolution_method",
        )))
        .unwrap_or(ResolutionMethod::TreeSitter),
        resolution_status: serde_json::from_value(serde_json::Value::String(string(
            row,
            "resolution_status",
        )))
        .unwrap_or(ResolutionStatus::Candidate),
        confidence: serde_json::from_value(serde_json::Value::String(string(row, "confidence")))
            .unwrap_or(ConfidenceCategory::Unverified),
        inferred: i64_or(row, "inferred", 0) != 0,
        provenance_ref: opt_string(row, "provenance_ref"),
        created_at: string(row, "created_at"),
    })
}

#[async_trait::async_trait]
impl SearchStore for SqliteStore {
    async fn replace_snapshot_entries(
        &self,
        snapshot_id: SnapshotId,
        entries: &[SearchEntry],
    ) -> StoreResult<()> {
        let mut tx = self.writer.begin().await.map_err(db_err)?;
        sqlx::query("DELETE FROM search_fts WHERE snapshot_id = ?")
            .bind(snapshot_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        for e in entries {
            sqlx::query(
                "INSERT INTO search_fts (term, path, kind, entity_id, title, snapshot_id) VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(&e.term)
            .bind(&e.path)
            .bind(&e.kind)
            .bind(&e.entity_id)
            .bind(e.title.clone())
            .bind(snapshot_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)
    }

    async fn delete_snapshot_entries(&self, snapshot_id: SnapshotId) -> StoreResult<()> {
        sqlx::query("DELETE FROM search_fts WHERE snapshot_id = ?")
            .bind(snapshot_id.to_string())
            .execute(&self.writer)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn search(
        &self,
        snapshot_id: SnapshotId,
        query: &str,
        limit: u32,
    ) -> StoreResult<Vec<SearchEntry>> {
        // Sanitize: each whitespace-separated term becomes a quoted FTS5 token,
        // OR-combined. User text never reaches FTS5 syntax unquoted.
        let match_query = quote_fts(query);
        if match_query.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "SELECT term, path, kind, entity_id, title FROM search_fts \
             WHERE snapshot_id = ? AND search_fts MATCH ? ORDER BY rank LIMIT ?",
        )
        .bind(snapshot_id.to_string())
        .bind(&match_query)
        .bind(limit as i64)
        .fetch_all(&self.reader)
        .await
        .map_err(db_err)?;
        Ok(rows.iter().map(row_to_entry).collect())
    }

    async fn lookup_exact(
        &self,
        snapshot_id: SnapshotId,
        name: &str,
        limit: u32,
    ) -> StoreResult<Vec<SearchEntry>> {
        let rows = sqlx::query(
            "SELECT term, path, kind, entity_id, title FROM search_fts \
             WHERE snapshot_id = ? AND (term = ? OR title = ?) LIMIT ?",
        )
        .bind(snapshot_id.to_string())
        .bind(name)
        .bind(name)
        .bind(limit as i64)
        .fetch_all(&self.reader)
        .await
        .map_err(db_err)?;
        Ok(rows.iter().map(row_to_entry).collect())
    }
}

/// Quotes each term for FTS5 MATCH; drops empty/whitespace terms.
fn quote_fts(query: &str) -> String {
    query
        .split(|c: char| c.is_whitespace() || c == ',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn row_to_entry(row: &SqliteRow) -> SearchEntry {
    SearchEntry {
        term: string(row, "term"),
        path: string(row, "path"),
        kind: string(row, "kind"),
        entity_id: string(row, "entity_id"),
        title: opt_string(row, "title"),
    }
}

#[async_trait::async_trait]
impl EvidenceStore for SqliteStore {
    async fn insert_evidence(&self, records: &[EvidenceRecord]) -> StoreResult<()> {
        let mut tx = self.writer.begin().await.map_err(db_err)?;
        for e in records {
            sqlx::query(
                "INSERT INTO evidence (evidence_id, repository_id, snapshot_id, evidence_type, source_ref, source_hash, \
                 extraction_method, extraction_version, actor, validation_method, validation_outcome, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(e.evidence_id.to_string())
            .bind(e.repository_id.to_string())
            .bind(e.snapshot_id.to_string())
            .bind(serde_json::to_string(&e.evidence_type).unwrap_or_default().trim_matches('"').to_string())
            .bind(&e.source_ref)
            .bind(e.source_hash.clone())
            .bind(serde_json::to_string(&e.extraction_method).unwrap_or_default().trim_matches('"').to_string())
            .bind(&e.extraction_version)
            .bind(&e.actor)
            .bind(e.validation_method.clone())
            .bind(e.validation_outcome.clone())
            .bind(&e.created_at)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)
    }

    async fn insert_claims(&self, records: &[ClaimRecord]) -> StoreResult<()> {
        let mut tx = self.writer.begin().await.map_err(db_err)?;
        for c in records {
            sqlx::query(
                "INSERT INTO claims (claim_id, repository_id, snapshot_id, claim_type, text, status, source_kind, \
                 confidence_category, created_at, validated_at, updated_at, supersedes_claim_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(c.claim_id.to_string())
            .bind(c.repository_id.to_string())
            .bind(c.snapshot_id.to_string())
            .bind(&c.claim_type)
            .bind(&c.text)
            .bind(serde_json::to_string(&c.status).unwrap_or_default().trim_matches('"').to_string())
            .bind(&c.source_kind)
            .bind(serde_json::to_string(&c.confidence_category).unwrap_or_default().trim_matches('"').to_string())
            .bind(&c.created_at)
            .bind(c.validated_at.clone())
            .bind(&c.updated_at)
            .bind(c.supersedes_claim_id.map(|id| id.to_string()))
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)
    }

    async fn insert_claim_dependencies(&self, links: &[ClaimDependencyRecord]) -> StoreResult<()> {
        let mut tx = self.writer.begin().await.map_err(db_err)?;
        for d in links {
            sqlx::query("INSERT OR IGNORE INTO claim_dependencies (claim_id, depends_on, depends_on_kind) VALUES (?, ?, ?)")
                .bind(d.claim_id.to_string())
                .bind(&d.depends_on)
                .bind(&d.depends_on_kind)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)
    }

    async fn list_claim_dependencies(
        &self,
        claim_ids: &[ClaimId],
    ) -> StoreResult<Vec<ClaimDependencyRecord>> {
        if claim_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; claim_ids.len()].join(", ");
        let sql = format!(
            "SELECT claim_id, depends_on, depends_on_kind FROM claim_dependencies \
             WHERE claim_id IN ({placeholders}) ORDER BY claim_id, depends_on"
        );
        let mut query = sqlx::query(&sql);
        for claim_id in claim_ids {
            query = query.bind(claim_id.to_string());
        }
        let rows = query.fetch_all(&self.reader).await.map_err(db_err)?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                Some(ClaimDependencyRecord {
                    claim_id: ClaimId::from_str(&string(r, "claim_id")).ok()?,
                    depends_on: string(r, "depends_on"),
                    depends_on_kind: string(r, "depends_on_kind"),
                })
            })
            .collect())
    }

    async fn get_claim(&self, id: ClaimId) -> StoreResult<Option<ClaimRecord>> {
        let row = sqlx::query("SELECT * FROM claims WHERE claim_id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.reader)
            .await
            .map_err(db_err)?;
        row.map(|r| row_to_claim(&r)).transpose()
    }

    async fn get_evidence(&self, id: EvidenceId) -> StoreResult<Option<EvidenceRecord>> {
        let row = sqlx::query("SELECT * FROM evidence WHERE evidence_id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.reader)
            .await
            .map_err(db_err)?;
        row.map(|r| row_to_evidence(&r)).transpose()
    }

    async fn list_claims(&self, repository_id: RepositoryId) -> StoreResult<Vec<ClaimRecord>> {
        let rows = sqlx::query("SELECT * FROM claims WHERE repository_id = ? ORDER BY created_at")
            .bind(repository_id.to_string())
            .fetch_all(&self.reader)
            .await
            .map_err(db_err)?;
        rows.iter().map(row_to_claim).collect()
    }

    async fn list_evidence(&self, repository_id: RepositoryId) -> StoreResult<Vec<EvidenceRecord>> {
        let rows =
            sqlx::query("SELECT * FROM evidence WHERE repository_id = ? ORDER BY created_at")
                .bind(repository_id.to_string())
                .fetch_all(&self.reader)
                .await
                .map_err(db_err)?;
        rows.iter().map(row_to_evidence).collect()
    }

    async fn claims_depending_on(
        &self,
        repository_id: RepositoryId,
        file_ids: &[String],
        symbol_ids: &[String],
    ) -> StoreResult<Vec<ClaimRecord>> {
        let mut all_ids: Vec<String> = file_ids.to_vec();
        all_ids.extend(symbol_ids.iter().cloned());
        if all_ids.is_empty() {
            return Ok(Vec::new());
        }
        // Bound-parameter IN list; ids are opaque strings, never interpolated.
        let placeholders = vec!["?"; all_ids.len()].join(", ");
        let sql = format!(
            "SELECT DISTINCT c.* FROM claims c \
             JOIN claim_dependencies d ON d.claim_id = c.claim_id \
             WHERE c.repository_id = ? AND d.depends_on_kind IN ('file', 'symbol') \
             AND d.depends_on IN ({placeholders}) \
             ORDER BY c.created_at"
        );
        let mut query = sqlx::query(&sql).bind(repository_id.to_string());
        for id in &all_ids {
            query = query.bind(id);
        }
        let rows = query.fetch_all(&self.reader).await.map_err(db_err)?;
        rows.iter().map(row_to_claim).collect()
    }

    async fn mark_claims_stale(
        &self,
        claim_ids: &[ClaimId],
        updated_at: &str,
    ) -> StoreResult<Vec<ClaimId>> {
        let mut tx = self.writer.begin().await.map_err(db_err)?;
        let mut updated = Vec::new();
        for claim_id in claim_ids {
            let result = sqlx::query(
                "UPDATE claims SET status = 'stale', updated_at = ? \
                 WHERE claim_id = ? AND status IN ('candidate', 'supported', 'verified')",
            )
            .bind(updated_at)
            .bind(claim_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            if result.rows_affected() > 0 {
                updated.push(*claim_id);
            }
        }
        tx.commit().await.map_err(db_err)?;
        Ok(updated)
    }

    async fn update_claim_validation(
        &self,
        claim_id: ClaimId,
        status: ClaimStatus,
        validated_at: Option<&str>,
        updated_at: &str,
    ) -> StoreResult<bool> {
        // Terminal claims (superseded/retracted) are never revalidated, and
        // `validated_at` is only written for outcomes that support the claim.
        let status_str = serde_json::to_string(&status)
            .unwrap_or_default()
            .trim_matches('"')
            .to_string();
        let result = sqlx::query(
            "UPDATE claims SET status = ?, validated_at = COALESCE(?, validated_at), updated_at = ? \
             WHERE claim_id = ? AND status NOT IN ('superseded', 'retracted')",
        )
        .bind(&status_str)
        .bind(validated_at)
        .bind(updated_at)
        .bind(claim_id.to_string())
        .execute(&self.writer)
        .await
        .map_err(db_err)?;
        Ok(result.rows_affected() > 0)
    }

    async fn mark_claims_superseded(
        &self,
        claim_ids: &[ClaimId],
        superseded_by: ClaimId,
        updated_at: &str,
    ) -> StoreResult<Vec<ClaimId>> {
        let mut tx = self.writer.begin().await.map_err(db_err)?;
        let mut updated = Vec::new();
        for claim_id in claim_ids {
            // A claim can never supersede itself (SDD §5.8 versioning).
            if *claim_id == superseded_by {
                continue;
            }
            let result = sqlx::query(
                "UPDATE claims SET status = 'superseded', updated_at = ? \
                 WHERE claim_id = ? AND status NOT IN ('superseded', 'retracted')",
            )
            .bind(updated_at)
            .bind(claim_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            if result.rows_affected() > 0 {
                updated.push(*claim_id);
            }
        }
        tx.commit().await.map_err(db_err)?;
        Ok(updated)
    }
}

fn row_to_claim(row: &SqliteRow) -> StoreResult<ClaimRecord> {
    Ok(ClaimRecord {
        claim_id: ClaimId::from_str(&string(row, "claim_id"))?,
        repository_id: RepositoryId::from_str(&string(row, "repository_id"))?,
        snapshot_id: SnapshotId::from_str(&string(row, "snapshot_id"))?,
        claim_type: string(row, "claim_type"),
        text: string(row, "text"),
        status: serde_json::from_value(serde_json::Value::String(string(row, "status")))
            .unwrap_or(ClaimStatus::Candidate),
        source_kind: string(row, "source_kind"),
        confidence_category: serde_json::from_value(serde_json::Value::String(string(
            row,
            "confidence_category",
        )))
        .unwrap_or(ConfidenceCategory::Unverified),
        created_at: string(row, "created_at"),
        validated_at: opt_string(row, "validated_at"),
        updated_at: string(row, "updated_at"),
        supersedes_claim_id: opt_string(row, "supersedes_claim_id")
            .and_then(|s| ClaimId::from_str(&s).ok()),
    })
}

fn row_to_evidence(row: &SqliteRow) -> StoreResult<EvidenceRecord> {
    Ok(EvidenceRecord {
        evidence_id: EvidenceId::from_str(&string(row, "evidence_id"))?,
        repository_id: RepositoryId::from_str(&string(row, "repository_id"))?,
        snapshot_id: SnapshotId::from_str(&string(row, "snapshot_id"))?,
        evidence_type: serde_json::from_value(serde_json::Value::String(string(
            row,
            "evidence_type",
        )))
        .unwrap_or(EvidenceType::SourceFact),
        source_ref: string(row, "source_ref"),
        source_hash: opt_string(row, "source_hash"),
        extraction_method: serde_json::from_value(serde_json::Value::String(string(
            row,
            "extraction_method",
        )))
        .unwrap_or(ResolutionMethod::TreeSitter),
        extraction_version: string(row, "extraction_version"),
        actor: string(row, "actor"),
        validation_method: opt_string(row, "validation_method"),
        validation_outcome: opt_string(row, "validation_outcome"),
        created_at: string(row, "created_at"),
    })
}

fn memory_type_from(s: &str) -> MemoryType {
    MemoryType::parse(s).unwrap_or(MemoryType::UnresolvedQuestion)
}

fn memory_claim_status_from(s: &str) -> ClaimStatus {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .unwrap_or(ClaimStatus::Candidate)
}

const MEMORY_COLS: &str = "memory_id, repository_id, snapshot_id, memory_type, title, summary, \
     scope, created_by, created_at, last_accessed_at, status, sharing_policy, sensitivity, \
     supersedes_memory_id, stale_cause, updated_at";

fn row_to_memory(row: &SqliteRow) -> StoreResult<MemoryRecord> {
    Ok(MemoryRecord {
        memory_id: MemoryId::from_str(&string(row, "memory_id"))?,
        repository_id: RepositoryId::from_str(&string(row, "repository_id"))?,
        snapshot_id: SnapshotId::from_str(&string(row, "snapshot_id"))?,
        memory_type: memory_type_from(&string(row, "memory_type")),
        title: string(row, "title"),
        summary: string(row, "summary"),
        scope: string(row, "scope"),
        created_by: string(row, "created_by"),
        created_at: string(row, "created_at"),
        last_accessed_at: string(row, "last_accessed_at"),
        status: memory_claim_status_from(&string(row, "status")),
        sharing_policy: string(row, "sharing_policy"),
        sensitivity: string(row, "sensitivity"),
        supersedes_memory_id: opt_string(row, "supersedes_memory_id")
            .and_then(|s| MemoryId::from_str(&s).ok()),
        stale_cause: opt_string(row, "stale_cause"),
        updated_at: string(row, "updated_at"),
    })
}

/// Tokens for the derived memory FTS index: lowercase alphanumeric runs of at
/// least two characters. The index is rebuildable and never the sole source
/// of provenance (TIS §5.2).
fn memory_terms(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| t.len() >= 2)
        .map(|t| t.to_ascii_lowercase())
        .collect()
}

/// Replaces the FTS rows of one memory inside the caller's transaction.
async fn replace_memory_fts(tx: &mut Tx<'_>, record: &MemoryRecord) -> StoreResult<()> {
    sqlx::query("DELETE FROM memory_fts WHERE memory_id = ?")
        .bind(record.memory_id.to_string())
        .execute(&mut **tx)
        .await
        .map_err(db_err)?;
    let mut terms: Vec<String> = Vec::new();
    for text in [&record.title, &record.summary] {
        for token in memory_terms(text) {
            if !terms.contains(&token) {
                terms.push(token);
            }
        }
    }
    for term in terms {
        sqlx::query("INSERT INTO memory_fts (term, memory_id, repository_id) VALUES (?, ?, ?)")
            .bind(term)
            .bind(record.memory_id.to_string())
            .bind(record.repository_id.to_string())
            .execute(&mut **tx)
            .await
            .map_err(db_err)?;
    }
    Ok(())
}

#[async_trait::async_trait]
impl MemoryStore for SqliteStore {
    async fn insert_memory(&self, record: &MemoryRecord, claim_ids: &[ClaimId]) -> StoreResult<()> {
        let mut tx = self.writer.begin().await.map_err(db_err)?;
        sqlx::query(&format!(
            "INSERT INTO memory_records ({MEMORY_COLS}) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        ))
        .bind(record.memory_id.to_string())
        .bind(record.repository_id.to_string())
        .bind(record.snapshot_id.to_string())
        .bind(record.memory_type.as_str())
        .bind(&record.title)
        .bind(&record.summary)
        .bind(&record.scope)
        .bind(&record.created_by)
        .bind(&record.created_at)
        .bind(&record.last_accessed_at)
        .bind(
            serde_json::to_string(&record.status)
                .unwrap_or_default()
                .trim_matches('"')
                .to_string(),
        )
        .bind(&record.sharing_policy)
        .bind(&record.sensitivity)
        .bind(record.supersedes_memory_id.map(|id| id.to_string()))
        .bind(record.stale_cause.clone())
        .bind(&record.updated_at)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        for claim_id in claim_ids {
            sqlx::query("INSERT OR IGNORE INTO memory_claims (memory_id, claim_id) VALUES (?, ?)")
                .bind(record.memory_id.to_string())
                .bind(claim_id.to_string())
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
        }
        replace_memory_fts(&mut tx, record).await?;
        tx.commit().await.map_err(db_err)
    }

    async fn get_memory(&self, id: MemoryId) -> StoreResult<Option<MemoryRecord>> {
        let row = sqlx::query("SELECT * FROM memory_records WHERE memory_id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.reader)
            .await
            .map_err(db_err)?;
        row.map(|r| row_to_memory(&r)).transpose()
    }

    async fn list_memories(&self, repository_id: RepositoryId) -> StoreResult<Vec<MemoryRecord>> {
        let rows =
            sqlx::query("SELECT * FROM memory_records WHERE repository_id = ? ORDER BY created_at")
                .bind(repository_id.to_string())
                .fetch_all(&self.reader)
                .await
                .map_err(db_err)?;
        rows.iter().map(row_to_memory).collect()
    }

    async fn list_memories_by_status(
        &self,
        repository_id: RepositoryId,
        statuses: &[ClaimStatus],
    ) -> StoreResult<Vec<MemoryRecord>> {
        if statuses.is_empty() {
            return self.list_memories(repository_id).await;
        }
        let rendered: Vec<String> = statuses
            .iter()
            .map(|s| {
                serde_json::to_string(s)
                    .unwrap_or_default()
                    .trim_matches('"')
                    .to_string()
            })
            .collect();
        let placeholders = vec!["?"; rendered.len()].join(", ");
        let sql = format!(
            "SELECT * FROM memory_records WHERE repository_id = ? AND status IN ({placeholders}) \
             ORDER BY created_at"
        );
        let mut query = sqlx::query(&sql).bind(repository_id.to_string());
        for status in &rendered {
            query = query.bind(status);
        }
        let rows = query.fetch_all(&self.reader).await.map_err(db_err)?;
        rows.iter().map(row_to_memory).collect()
    }

    async fn search_memories(
        &self,
        repository_id: RepositoryId,
        query: &str,
        status: Option<ClaimStatus>,
        memory_type: Option<MemoryType>,
        limit: u32,
    ) -> StoreResult<Vec<MemoryRecord>> {
        // Sanitize: each whitespace-separated term becomes a quoted FTS5
        // token, OR-combined (same discipline as the lexical channel).
        let match_query = quote_fts(query);
        if match_query.is_empty() {
            return Ok(Vec::new());
        }
        // FR-036: retraction removes a record from active retrieval — it
        // stays queryable only when explicitly requested by status.
        let mut clauses: Vec<String> = vec!["m.repository_id = ?".into()];
        match status {
            Some(_) => clauses.push("m.status = ?".into()),
            None => clauses.push("m.status != 'retracted'".into()),
        }
        if memory_type.is_some() {
            clauses.push("m.memory_type = ?".into());
        }
        let sql = format!(
            "SELECT m.* FROM memory_records m \
             JOIN (SELECT DISTINCT memory_id FROM memory_fts WHERE memory_fts MATCH ?) f \
             ON f.memory_id = m.memory_id \
             WHERE {} ORDER BY m.created_at DESC LIMIT ?",
            clauses.join(" AND ")
        );
        let mut q = sqlx::query(&sql).bind(&match_query);
        q = q.bind(repository_id.to_string());
        if let Some(status) = status {
            q = q.bind(
                serde_json::to_string(&status)
                    .unwrap_or_default()
                    .trim_matches('"')
                    .to_string(),
            );
        }
        if let Some(memory_type) = memory_type {
            q = q.bind(memory_type.as_str());
        }
        q = q.bind(limit as i64);
        let rows = q.fetch_all(&self.reader).await.map_err(db_err)?;
        rows.iter().map(row_to_memory).collect()
    }

    async fn claim_ids_for_memory(&self, memory_id: MemoryId) -> StoreResult<Vec<ClaimId>> {
        let rows =
            sqlx::query("SELECT claim_id FROM memory_claims WHERE memory_id = ? ORDER BY claim_id")
                .bind(memory_id.to_string())
                .fetch_all(&self.reader)
                .await
                .map_err(db_err)?;
        Ok(rows
            .iter()
            .filter_map(|r| ClaimId::from_str(&string(r, "claim_id")).ok())
            .collect())
    }

    async fn memories_for_claims(&self, claim_ids: &[ClaimId]) -> StoreResult<Vec<MemoryRecord>> {
        if claim_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; claim_ids.len()].join(", ");
        let sql = format!(
            "SELECT DISTINCT m.* FROM memory_records m \
             JOIN memory_claims mc ON mc.memory_id = m.memory_id \
             WHERE mc.claim_id IN ({placeholders}) ORDER BY m.created_at"
        );
        let mut query = sqlx::query(&sql);
        for claim_id in claim_ids {
            query = query.bind(claim_id.to_string());
        }
        let rows = query.fetch_all(&self.reader).await.map_err(db_err)?;
        rows.iter().map(row_to_memory).collect()
    }

    async fn update_memory_status(
        &self,
        memory_id: MemoryId,
        status: ClaimStatus,
        actor: &str,
        reason: Option<&str>,
        updated_at: &str,
    ) -> StoreResult<bool> {
        let mut tx = self.writer.begin().await.map_err(db_err)?;
        let status_str = serde_json::to_string(&status)
            .unwrap_or_default()
            .trim_matches('"')
            .to_string();
        // The invalidation cause is recorded on the record only for
        // freshness-bearing statuses (FR-043); leaving them clears it.
        let stale_cause = match status {
            ClaimStatus::Stale | ClaimStatus::Conflicted => reason.map(str::to_string),
            _ => None,
        };
        let result = sqlx::query(
            "UPDATE memory_records SET status = ?, stale_cause = ?, updated_at = ? \
             WHERE memory_id = ? AND status NOT IN ('superseded', 'retracted')",
        )
        .bind(&status_str)
        .bind(stale_cause)
        .bind(updated_at)
        .bind(memory_id.to_string())
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        if result.rows_affected() == 0 {
            return Ok(false);
        }
        sqlx::query(
            "INSERT INTO memory_status_events (event_id, memory_id, status, actor, reason, created_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(MemoryEventId::generate().to_string())
        .bind(memory_id.to_string())
        .bind(&status_str)
        .bind(actor)
        .bind(reason)
        .bind(updated_at)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(true)
    }

    async fn list_memory_status_events(
        &self,
        memory_id: MemoryId,
    ) -> StoreResult<Vec<MemoryStatusEvent>> {
        let rows = sqlx::query(
            "SELECT * FROM memory_status_events WHERE memory_id = ? ORDER BY created_at",
        )
        .bind(memory_id.to_string())
        .fetch_all(&self.reader)
        .await
        .map_err(db_err)?;
        Ok(rows.iter().map(row_to_memory_event).collect())
    }

    async fn touch_memory(&self, memory_id: MemoryId, accessed_at: &str) -> StoreResult<()> {
        sqlx::query("UPDATE memory_records SET last_accessed_at = ? WHERE memory_id = ?")
            .bind(accessed_at)
            .bind(memory_id.to_string())
            .execute(&self.writer)
            .await
            .map_err(db_err)?;
        Ok(())
    }
}

fn row_to_memory_event(row: &SqliteRow) -> MemoryStatusEvent {
    MemoryStatusEvent {
        event_id: MemoryEventId::from_str(&string(row, "event_id"))
            .unwrap_or_else(|_| MemoryEventId::generate()),
        memory_id: MemoryId::from_str(&string(row, "memory_id"))
            .unwrap_or_else(|_| MemoryId::generate()),
        status: memory_claim_status_from(&string(row, "status")),
        actor: string(row, "actor"),
        reason: opt_string(row, "reason"),
        created_at: string(row, "created_at"),
    }
}

#[async_trait::async_trait]
impl JobStore for SqliteStore {
    async fn insert_job_idempotent(&self, record: &JobRecord) -> StoreResult<JobRecord> {
        // Idempotency (TIS §13.3): same (repository, key) returns the original.
        let existing =
            sqlx::query("SELECT * FROM jobs WHERE repository_id = ? AND idempotency_key = ?")
                .bind(record.repository_id.to_string())
                .bind(&record.idempotency_key)
                .fetch_optional(&self.writer)
                .await
                .map_err(db_err)?;
        if let Some(row) = existing {
            return row_to_job(&row);
        }
        sqlx::query(
            "INSERT INTO jobs (job_id, repository_id, job_type, state, idempotency_key, payload_json, retry_count, \
             created_at, updated_at, finished_at, error_code, error_message) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(record.job_id.to_string())
        .bind(record.repository_id.to_string())
        .bind(&record.job_type)
        .bind(serde_json::to_string(&record.state).unwrap_or_default().trim_matches('"').to_string())
        .bind(&record.idempotency_key)
        .bind(record.payload_json.clone())
        .bind(record.retry_count as i64)
        .bind(&record.created_at)
        .bind(&record.updated_at)
        .bind(record.finished_at.clone())
        .bind(record.error_code.clone())
        .bind(record.error_message.clone())
        .execute(&self.writer)
        .await
        .map_err(db_err)?;
        Ok(record.clone())
    }

    async fn get_job(&self, id: JobId) -> StoreResult<Option<JobRecord>> {
        let row = sqlx::query("SELECT * FROM jobs WHERE job_id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.reader)
            .await
            .map_err(db_err)?;
        row.map(|r| row_to_job(&r)).transpose()
    }

    async fn update_job_state(
        &self,
        id: JobId,
        state: JobState,
        updated_at: &str,
        finished_at: Option<&str>,
        error_code: Option<&str>,
        error_message: Option<&str>,
    ) -> StoreResult<()> {
        sqlx::query(
            "UPDATE jobs SET state = ?, updated_at = ?, finished_at = ?, error_code = ?, error_message = ? WHERE job_id = ?",
        )
        .bind(serde_json::to_string(&state).unwrap_or_default().trim_matches('"').to_string())
        .bind(updated_at)
        .bind(finished_at)
        .bind(error_code)
        .bind(error_message)
        .bind(id.to_string())
        .execute(&self.writer)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn list_jobs(&self, repository_id: RepositoryId) -> StoreResult<Vec<JobRecord>> {
        let rows = sqlx::query("SELECT * FROM jobs WHERE repository_id = ? ORDER BY created_at")
            .bind(repository_id.to_string())
            .fetch_all(&self.reader)
            .await
            .map_err(db_err)?;
        rows.iter().map(row_to_job).collect()
    }
}

fn row_to_job(row: &SqliteRow) -> StoreResult<JobRecord> {
    Ok(JobRecord {
        job_id: JobId::from_str(&string(row, "job_id"))?,
        repository_id: RepositoryId::from_str(&string(row, "repository_id"))?,
        job_type: string(row, "job_type"),
        state: job_state_from(&string(row, "state")),
        idempotency_key: string(row, "idempotency_key"),
        payload_json: opt_string(row, "payload_json"),
        retry_count: i64_or(row, "retry_count", 0) as u32,
        created_at: string(row, "created_at"),
        updated_at: string(row, "updated_at"),
        finished_at: opt_string(row, "finished_at"),
        error_code: opt_string(row, "error_code"),
        error_message: opt_string(row, "error_message"),
    })
}

#[async_trait::async_trait]
impl BundleStore for SqliteStore {
    async fn insert_bundle(&self, record: &BundleRecord, items_json: &str) -> StoreResult<()> {
        let mut tx = self.writer.begin().await.map_err(db_err)?;
        sqlx::query(
            "INSERT INTO context_bundles (bundle_id, repository_id, snapshot_id, generation_id, schema_version, task_text, \
             normalized_json, retrieval_policy_version, budget_tokens, estimated_tokens, token_estimate_basis, \
             stopping_reason, bundle_hash, artifact_hash, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(record.bundle_id.to_string())
        .bind(record.repository_id.to_string())
        .bind(record.snapshot_id.to_string())
        .bind(record.generation_id.to_string())
        .bind(&record.schema_version)
        .bind(&record.task_text)
        .bind(&record.normalized_json)
        .bind(&record.retrieval_policy_version)
        .bind(record.budget_tokens as i64)
        .bind(record.estimated_tokens as i64)
        .bind(&record.token_estimate_basis)
        .bind(&record.stopping_reason)
        .bind(&record.bundle_hash)
        .bind(&record.artifact_hash)
        .bind(&record.created_at)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        let items: Vec<serde_json::Value> = serde_json::from_str(items_json).unwrap_or_default();
        for (position, item) in items.iter().enumerate() {
            sqlx::query(
                "INSERT INTO bundle_items (bundle_id, position, item_json) VALUES (?, ?, ?)",
            )
            .bind(record.bundle_id.to_string())
            .bind(position as i64)
            .bind(serde_json::to_string(item).unwrap_or_default())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)
    }

    async fn get_bundle(&self, id: BundleId) -> StoreResult<Option<BundleRecord>> {
        let row = sqlx::query("SELECT * FROM context_bundles WHERE bundle_id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.reader)
            .await
            .map_err(db_err)?;
        row.map(|r| row_to_bundle(&r)).transpose()
    }

    async fn get_bundle_items(&self, id: BundleId) -> StoreResult<String> {
        let rows =
            sqlx::query("SELECT item_json FROM bundle_items WHERE bundle_id = ? ORDER BY position")
                .bind(id.to_string())
                .fetch_all(&self.reader)
                .await
                .map_err(db_err)?;
        let items: Vec<serde_json::Value> = rows
            .iter()
            .filter_map(|r| serde_json::from_str(&string(r, "item_json")).ok())
            .collect();
        serde_json::to_string(&items).map_err(|e| CccError::internal_error(format!("items: {e}")))
    }

    async fn list_bundles(&self, repository_id: RepositoryId) -> StoreResult<Vec<BundleRecord>> {
        let rows = sqlx::query(
            "SELECT * FROM context_bundles WHERE repository_id = ? ORDER BY created_at",
        )
        .bind(repository_id.to_string())
        .fetch_all(&self.reader)
        .await
        .map_err(db_err)?;
        rows.iter().map(row_to_bundle).collect()
    }
}

fn row_to_bundle(row: &SqliteRow) -> StoreResult<BundleRecord> {
    Ok(BundleRecord {
        bundle_id: BundleId::from_str(&string(row, "bundle_id"))?,
        repository_id: RepositoryId::from_str(&string(row, "repository_id"))?,
        snapshot_id: SnapshotId::from_str(&string(row, "snapshot_id"))?,
        generation_id: GenerationId::from_str(&string(row, "generation_id"))?,
        schema_version: string(row, "schema_version"),
        task_text: string(row, "task_text"),
        normalized_json: string(row, "normalized_json"),
        retrieval_policy_version: string(row, "retrieval_policy_version"),
        budget_tokens: i64_or(row, "budget_tokens", 0) as u64,
        estimated_tokens: i64_or(row, "estimated_tokens", 0) as u64,
        token_estimate_basis: string(row, "token_estimate_basis"),
        stopping_reason: string(row, "stopping_reason"),
        bundle_hash: string(row, "bundle_hash"),
        artifact_hash: string(row, "artifact_hash"),
        created_at: string(row, "created_at"),
    })
}

#[async_trait::async_trait]
impl ImpactStore for SqliteStore {
    async fn insert_impact(&self, record: &ImpactRecord) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO impact_reports (impact_id, repository_id, snapshot_id, base, head, schema_version, report_hash, artifact_hash, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(record.impact_id.to_string())
        .bind(record.repository_id.to_string())
        .bind(record.snapshot_id.to_string())
        .bind(&record.base)
        .bind(&record.head)
        .bind(&record.schema_version)
        .bind(&record.report_hash)
        .bind(&record.artifact_hash)
        .bind(&record.created_at)
        .execute(&self.writer)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn get_impact(&self, id: ImpactId) -> StoreResult<Option<ImpactRecord>> {
        let row = sqlx::query("SELECT * FROM impact_reports WHERE impact_id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.reader)
            .await
            .map_err(db_err)?;
        row.map(|r| row_to_impact(&r)).transpose()
    }

    async fn list_impacts(&self, repository_id: RepositoryId) -> StoreResult<Vec<ImpactRecord>> {
        let rows =
            sqlx::query("SELECT * FROM impact_reports WHERE repository_id = ? ORDER BY created_at")
                .bind(repository_id.to_string())
                .fetch_all(&self.reader)
                .await
                .map_err(db_err)?;
        rows.iter().map(row_to_impact).collect()
    }
}

fn row_to_impact(row: &SqliteRow) -> StoreResult<ImpactRecord> {
    Ok(ImpactRecord {
        impact_id: ImpactId::from_str(&string(row, "impact_id"))?,
        repository_id: RepositoryId::from_str(&string(row, "repository_id"))?,
        snapshot_id: SnapshotId::from_str(&string(row, "snapshot_id"))?,
        base: string(row, "base"),
        head: string(row, "head"),
        schema_version: string(row, "schema_version"),
        report_hash: string(row, "report_hash"),
        artifact_hash: string(row, "artifact_hash"),
        created_at: string(row, "created_at"),
    })
}

fn verification_mode_from(s: &str) -> VerificationMode {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .unwrap_or(VerificationMode::Analysis)
}

fn check_status_from(s: &str) -> VerificationStatus {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .unwrap_or(VerificationStatus::NotRun)
}

#[async_trait::async_trait]
impl VerificationStore for SqliteStore {
    async fn insert_verification(
        &self,
        record: &VerificationRecord,
        checks: &[VerificationCheckRecord],
    ) -> StoreResult<()> {
        // One transaction: a record never exists without its check rows
        // (TIS §11.1 step 9 — the published record is the unit of provenance).
        let mut tx: Tx = self.writer.begin().await.map_err(db_err)?;
        sqlx::query(
            "INSERT INTO verification_records (verification_id, repository_id, snapshot_id, base, head, \
             policy_name, policy_hash, mode, statuses_json, artifact_hash, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(record.verification_id.to_string())
        .bind(record.repository_id.to_string())
        .bind(record.snapshot_id.to_string())
        .bind(&record.base)
        .bind(&record.head)
        .bind(&record.policy_name)
        .bind(&record.policy_hash)
        .bind(
            serde_json::to_string(&record.mode)
                .unwrap_or_default()
                .trim_matches('"')
                .to_string(),
        )
        .bind(&record.statuses_json)
        .bind(&record.artifact_hash)
        .bind(&record.created_at)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        for c in checks {
            sqlx::query(
                "INSERT INTO verification_checks (check_id, verification_id, position, suggested_command, relation, \
                 status, exit_code, duration_ms, stdout_ref, stderr_ref, truncated, env_summary_json) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(c.check_id.to_string())
            .bind(c.verification_id.to_string())
            .bind(c.position as i64)
            .bind(&c.suggested_command)
            .bind(&c.relation)
            .bind(c.status.as_str())
            .bind(c.exit_code)
            .bind(c.duration_ms.map(|d| d as i64))
            .bind(c.stdout_ref.clone())
            .bind(c.stderr_ref.clone())
            .bind(c.truncated as i64)
            .bind(&c.env_summary_json)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)
    }

    async fn get_verification(
        &self,
        id: VerificationId,
    ) -> StoreResult<Option<VerificationRecord>> {
        let row = sqlx::query("SELECT * FROM verification_records WHERE verification_id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.reader)
            .await
            .map_err(db_err)?;
        row.map(|r| row_to_verification(&r)).transpose()
    }

    async fn list_verifications(
        &self,
        repository_id: RepositoryId,
    ) -> StoreResult<Vec<VerificationRecord>> {
        let rows = sqlx::query(
            "SELECT * FROM verification_records WHERE repository_id = ? ORDER BY created_at",
        )
        .bind(repository_id.to_string())
        .fetch_all(&self.reader)
        .await
        .map_err(db_err)?;
        rows.iter().map(row_to_verification).collect()
    }

    async fn list_verification_checks(
        &self,
        verification_id: VerificationId,
    ) -> StoreResult<Vec<VerificationCheckRecord>> {
        let rows = sqlx::query(
            "SELECT * FROM verification_checks WHERE verification_id = ? ORDER BY position",
        )
        .bind(verification_id.to_string())
        .fetch_all(&self.reader)
        .await
        .map_err(db_err)?;
        Ok(rows.iter().map(row_to_check).collect())
    }
}

fn row_to_verification(row: &SqliteRow) -> StoreResult<VerificationRecord> {
    Ok(VerificationRecord {
        verification_id: VerificationId::from_str(&string(row, "verification_id"))?,
        repository_id: RepositoryId::from_str(&string(row, "repository_id"))?,
        snapshot_id: SnapshotId::from_str(&string(row, "snapshot_id"))?,
        base: string(row, "base"),
        head: string(row, "head"),
        policy_name: string(row, "policy_name"),
        policy_hash: string(row, "policy_hash"),
        mode: verification_mode_from(&string(row, "mode")),
        statuses_json: string(row, "statuses_json"),
        artifact_hash: string(row, "artifact_hash"),
        created_at: string(row, "created_at"),
    })
}

fn row_to_check(row: &SqliteRow) -> VerificationCheckRecord {
    VerificationCheckRecord {
        check_id: VerificationId::from_str(&string(row, "check_id"))
            .unwrap_or_else(|_| VerificationId::generate()),
        verification_id: VerificationId::from_str(&string(row, "verification_id"))
            .unwrap_or_else(|_| VerificationId::generate()),
        position: i64_or(row, "position", 0) as u32,
        suggested_command: string(row, "suggested_command"),
        relation: string(row, "relation"),
        status: check_status_from(&string(row, "status")),
        exit_code: row.try_get::<Option<i64>, _>("exit_code").unwrap_or(None),
        duration_ms: row
            .try_get::<Option<i64>, _>("duration_ms")
            .ok()
            .flatten()
            .map(|d| d.max(0) as u64),
        stdout_ref: opt_string(row, "stdout_ref"),
        stderr_ref: opt_string(row, "stderr_ref"),
        truncated: i64_or(row, "truncated", 0) != 0,
        env_summary_json: string(row, "env_summary_json"),
    }
}

#[async_trait::async_trait]
impl EvaluationStore for SqliteStore {
    async fn insert_suite(&self, record: &EvaluationSuiteRecord) -> StoreResult<()> {
        // Immutable registry: an identical hash is a no-op (Phase G).
        sqlx::query(
            "INSERT OR IGNORE INTO evaluation_suites (suite_hash, name, case_count, suite_json, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&record.suite_hash)
        .bind(&record.name)
        .bind(record.case_count as i64)
        .bind(&record.suite_json)
        .bind(&record.created_at)
        .execute(&self.writer)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn get_suite(&self, suite_hash: &str) -> StoreResult<Option<EvaluationSuiteRecord>> {
        let row = sqlx::query("SELECT * FROM evaluation_suites WHERE suite_hash = ?")
            .bind(suite_hash)
            .fetch_optional(&self.reader)
            .await
            .map_err(db_err)?;
        row.map(|r| {
            Ok(EvaluationSuiteRecord {
                suite_hash: string(&r, "suite_hash"),
                name: string(&r, "name"),
                case_count: i64_or(&r, "case_count", 0) as u32,
                suite_json: string(&r, "suite_json"),
                created_at: string(&r, "created_at"),
            })
        })
        .transpose()
    }

    async fn insert_run(&self, record: &EvaluationRunRecord) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO evaluation_runs (run_id, repository_id, suite_name, suite_hash, policy_version, \
             policy_hash, config_hash, metrics_json, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(record.run_id.to_string())
        .bind(record.repository_id.to_string())
        .bind(&record.suite_name)
        .bind(&record.suite_hash)
        .bind(&record.policy_version)
        .bind(&record.policy_hash)
        .bind(&record.config_hash)
        .bind(&record.metrics_json)
        .bind(&record.created_at)
        .execute(&self.writer)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn get_run(&self, id: EvaluationRunId) -> StoreResult<Option<EvaluationRunRecord>> {
        let row = sqlx::query("SELECT * FROM evaluation_runs WHERE run_id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.reader)
            .await
            .map_err(db_err)?;
        row.map(|r| row_to_run(&r)).transpose()
    }

    async fn list_runs(
        &self,
        repository_id: RepositoryId,
    ) -> StoreResult<Vec<EvaluationRunRecord>> {
        let rows = sqlx::query(
            "SELECT * FROM evaluation_runs WHERE repository_id = ? ORDER BY created_at",
        )
        .bind(repository_id.to_string())
        .fetch_all(&self.reader)
        .await
        .map_err(db_err)?;
        rows.iter().map(row_to_run).collect()
    }
}

fn row_to_run(row: &SqliteRow) -> StoreResult<EvaluationRunRecord> {
    Ok(EvaluationRunRecord {
        run_id: EvaluationRunId::from_str(&string(row, "run_id"))?,
        repository_id: RepositoryId::from_str(&string(row, "repository_id"))?,
        suite_name: string(row, "suite_name"),
        suite_hash: string(row, "suite_hash"),
        policy_version: string(row, "policy_version"),
        policy_hash: string(row, "policy_hash"),
        config_hash: string(row, "config_hash"),
        metrics_json: string(row, "metrics_json"),
        created_at: string(row, "created_at"),
    })
}

#[async_trait::async_trait]
impl ArtifactStore for SqliteStore {
    async fn put(&self, _bytes: &[u8], _media_type: &str) -> StoreResult<String> {
        Err(CccError::internal_error(
            "SqliteStore stores metadata only; use FsArtifactStore for blobs",
        ))
    }

    async fn get(&self, _content_hash: &str) -> StoreResult<Option<Vec<u8>>> {
        Err(CccError::internal_error(
            "SqliteStore stores metadata only; use FsArtifactStore for blobs",
        ))
    }

    async fn exists(&self, content_hash: &str) -> StoreResult<bool> {
        let row = sqlx::query("SELECT content_hash FROM artifacts WHERE content_hash = ?")
            .bind(content_hash)
            .fetch_optional(&self.reader)
            .await
            .map_err(db_err)?;
        Ok(row.is_some())
    }

    async fn get_meta(&self, content_hash: &str) -> StoreResult<Option<ArtifactMeta>> {
        let row = sqlx::query("SELECT * FROM artifacts WHERE content_hash = ?")
            .bind(content_hash)
            .fetch_optional(&self.reader)
            .await
            .map_err(db_err)?;
        Ok(row.map(|r| ArtifactMeta {
            content_hash: string(&r, "content_hash"),
            byte_length: i64_or(&r, "byte_length", 0) as u64,
            media_type: string(&r, "media_type"),
            retention_state: string(&r, "retention_state"),
        }))
    }
}

impl SqliteStore {
    /// Records artifact metadata (hash, length, media type, retention state —
    /// TIS §5.4). Used by the filesystem artifact store after blob writes.
    pub async fn record_artifact_meta(
        &self,
        meta: &ArtifactMeta,
        created_at: &str,
    ) -> StoreResult<()> {
        sqlx::query(
            "INSERT OR IGNORE INTO artifacts (content_hash, byte_length, media_type, retention_state, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&meta.content_hash)
        .bind(meta.byte_length as i64)
        .bind(&meta.media_type)
        .bind(&meta.retention_state)
        .bind(created_at)
        .execute(&self.writer)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// Exposes the reader pool for query helpers kept outside the traits.
    pub fn reader_pool(&self) -> &Pool<Sqlite> {
        &self.reader
    }

    /// Exposes the single-connection writer pool for migration helpers.
    pub fn writer_pool(&self) -> &Pool<Sqlite> {
        &self.writer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::FsArtifactStore;
    use std::sync::Arc;

    async fn temp_store() -> (tempfile::TempDir, Arc<SqliteStore>) {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::connect(&dir.path().join("ccc.db"))
            .await
            .expect("connect");
        (dir, Arc::new(store))
    }

    fn repo_record() -> RepositoryRecord {
        let now = now_rfc3339();
        RepositoryRecord {
            repository_id: RepositoryId::generate(),
            root_path: "C:/tmp/repo".into(),
            canonical_uri: None,
            default_branch: Some("main".into()),
            configuration_hash: "cfg-hash".into(),
            limitations: vec![],
            created_at: now.clone(),
            updated_at: now,
        }
    }

    fn snapshot_record(repo: &RepositoryRecord) -> SnapshotRecord {
        SnapshotRecord {
            snapshot_id: SnapshotId::generate(),
            repository_id: repo.repository_id,
            commit_hash: Some("0123456789abcdef0123456789abcdef01234567".into()),
            branch: Some("main".into()),
            kind: SnapshotKind::WorkingTree,
            is_dirty: false,
            manifest_hash: "manifest-hash".into(),
            indexer_version: "test".into(),
            configuration_hash: "cfg-hash".into(),
            status: SnapshotStatus::Building,
            created_at: now_rfc3339(),
        }
    }

    fn file_record(snapshot: &SnapshotRecord, path: &str) -> FileRecord {
        FileRecord {
            file_id: FileId::generate(),
            snapshot_id: snapshot.snapshot_id,
            repository_id: snapshot.repository_id,
            path: path.into(),
            content_hash: format!("hash-{path}"),
            size_bytes: 10,
            language: Some("rust".into()),
            binary: false,
            inclusion: FileInclusion::Included,
            exclusion_reason: None,
        }
    }

    fn symbol_record(file: &FileRecord, qualified: &str) -> SymbolRecord {
        SymbolRecord {
            symbol_id: SymbolId::generate(),
            repository_id: file.repository_id,
            snapshot_id: file.snapshot_id,
            file_id: file.file_id,
            symbol_key: format!("rust::{qualified}"),
            qualified_name: qualified.into(),
            kind: SymbolKind::Function,
            language: "rust".into(),
            start_line: 1,
            start_column: 0,
            end_line: 2,
            end_column: 1,
            signature_hash: "sig".into(),
            resolver: ResolutionMethod::TreeSitter,
            resolution_status: ResolutionStatus::Resolved,
        }
    }

    fn generation_record(repo: &RepositoryRecord, snapshot: &SnapshotRecord) -> GenerationRecord {
        GenerationRecord {
            generation_id: GenerationId::generate(),
            repository_id: repo.repository_id,
            snapshot_id: snapshot.snapshot_id,
            status: GenerationStatus::Building,
            counters_json: "{}".into(),
            created_at: now_rfc3339(),
            published_at: None,
        }
    }

    #[tokio::test]
    async fn migrations_are_idempotent_and_reversible() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::connect(&dir.path().join("ccc.db"))
            .await
            .unwrap();
        // Running migrations again must be a no-op (connect twice).
        drop(store);
        let store2 = SqliteStore::connect(&dir.path().join("ccc.db"))
            .await
            .unwrap();
        let _ = store2;

        // Down then up: apply the reverse SQL manually (sqlx 0.8 applies
        // migrations but does not implement revert), then re-apply.
        let migrator = sqlx::migrate!("../../migrations/sqlite");
        let pool = store2.writer_pool().clone();
        for down_file in [
            include_str!("../../../migrations/sqlite/manual-down/0003_memory_down.sql"),
            include_str!("../../../migrations/sqlite/manual-down/0002_verification_eval_down.sql"),
            include_str!("../../../migrations/sqlite/manual-down/0001_init_down.sql"),
        ] {
            for statement in down_file.split(';') {
                let stmt = statement.trim();
                if stmt.is_empty() || stmt.starts_with("--") {
                    continue;
                }
                sqlx::query(stmt).execute(&pool).await.unwrap();
            }
        }
        migrator.run(&pool).await.unwrap();
    }

    #[tokio::test]
    async fn verification_records_are_fk_enforced_and_checks_queryable() {
        let (_dir, store) = temp_store().await;
        let repo = repo_record();
        store.insert_repository(&repo).await.unwrap();
        let snapshot = snapshot_record(&repo);
        store.insert_snapshot(&snapshot).await.unwrap();

        let verification_id = VerificationId::generate();
        let record = VerificationRecord {
            verification_id,
            repository_id: repo.repository_id,
            snapshot_id: snapshot.snapshot_id,
            base: "HEAD~1".into(),
            head: "HEAD".into(),
            policy_name: "analysis".into(),
            policy_hash: "policy-hash".into(),
            mode: VerificationMode::Analysis,
            statuses_json: r#"{"not_run":2}"#.into(),
            artifact_hash: "artifact".into(),
            created_at: now_rfc3339(),
        };
        let mk_check = |position: u32, status: VerificationStatus| VerificationCheckRecord {
            check_id: VerificationId::generate(),
            verification_id,
            position,
            suggested_command: "cargo test".into(),
            relation: "directly".into(),
            status,
            exit_code: None,
            duration_ms: None,
            stdout_ref: None,
            stderr_ref: None,
            truncated: false,
            env_summary_json: "{}".into(),
        };
        let checks = vec![
            mk_check(0, VerificationStatus::NotRun),
            mk_check(1, VerificationStatus::Blocked),
        ];
        store.insert_verification(&record, &checks).await.unwrap();

        let fetched = store
            .get_verification(verification_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.policy_name, "analysis");
        assert_eq!(fetched.mode, VerificationMode::Analysis);
        let rows = store
            .list_verification_checks(verification_id)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].position, 0);
        assert_eq!(rows[0].status, VerificationStatus::NotRun);
        assert_eq!(rows[1].status, VerificationStatus::Blocked);

        // Unknown check statuses never pass silently (TIS §11.3).
        assert_ne!(rows[0].status, VerificationStatus::Passed);

        // FK: a record referencing an unknown snapshot must be rejected.
        let bad = VerificationRecord {
            verification_id: VerificationId::generate(),
            snapshot_id: SnapshotId::generate(),
            ..record.clone()
        };
        assert!(store.insert_verification(&bad, &[]).await.is_err());

        // A record referencing an unknown repository must be rejected too.
        let bad_repo = VerificationRecord {
            verification_id: VerificationId::generate(),
            repository_id: RepositoryId::generate(),
            ..record
        };
        assert!(store.insert_verification(&bad_repo, &[]).await.is_err());
    }

    #[tokio::test]
    async fn evaluation_suites_are_immutable_and_runs_queryable() {
        let (_dir, store) = temp_store().await;
        let repo = repo_record();
        store.insert_repository(&repo).await.unwrap();

        let suite = EvaluationSuiteRecord {
            suite_hash: "suite-hash-1".into(),
            name: "fixture".into(),
            case_count: 2,
            suite_json: "{\"name\":\"fixture\"}".into(),
            created_at: now_rfc3339(),
        };
        store.insert_suite(&suite).await.unwrap();
        // Re-recording an identical hash is a no-op (immutable registry).
        let mut renamed = suite.clone();
        renamed.case_count = 99;
        store.insert_suite(&renamed).await.unwrap();
        let stored = store.get_suite("suite-hash-1").await.unwrap().unwrap();
        assert_eq!(stored.case_count, 2);

        let run = EvaluationRunRecord {
            run_id: EvaluationRunId::generate(),
            repository_id: repo.repository_id,
            suite_name: "fixture".into(),
            suite_hash: "suite-hash-1".into(),
            policy_version: "policy-v1".into(),
            policy_hash: "policy-hash".into(),
            config_hash: "config-hash".into(),
            metrics_json: "{\"recall_millis\":1000}".into(),
            created_at: now_rfc3339(),
        };
        store.insert_run(&run).await.unwrap();
        let runs = store.list_runs(repo.repository_id).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run_id, run.run_id);
        assert_eq!(runs[0].policy_version, "policy-v1");
        let fetched = store.get_run(run.run_id).await.unwrap().unwrap();
        assert_eq!(fetched.metrics_json, run.metrics_json);

        // FK: runs must reference a recorded suite hash.
        let bad = EvaluationRunRecord {
            suite_hash: "unknown-suite".into(),
            ..run.clone()
        };
        assert!(store.insert_run(&bad).await.is_err());
    }

    #[tokio::test]
    async fn claim_validation_updates_respect_terminal_states() {
        let (_dir, store) = temp_store().await;
        let repo = repo_record();
        store.insert_repository(&repo).await.unwrap();
        let snapshot = snapshot_record(&repo);
        store.insert_snapshot(&snapshot).await.unwrap();
        let claim = ClaimRecord {
            claim_id: ClaimId::generate(),
            repository_id: repo.repository_id,
            snapshot_id: snapshot.snapshot_id,
            claim_type: "symbol_existence".into(),
            text: "claim".into(),
            status: ClaimStatus::Stale,
            source_kind: "extracted".into(),
            confidence_category: ConfidenceCategory::Unverified,
            created_at: now_rfc3339(),
            validated_at: None,
            updated_at: now_rfc3339(),
            supersedes_claim_id: None,
        };
        store
            .insert_claims(std::slice::from_ref(&claim))
            .await
            .unwrap();

        let validated = now_rfc3339();
        let ok = store
            .update_claim_validation(
                claim.claim_id,
                ClaimStatus::Supported,
                Some(&validated),
                &validated,
            )
            .await
            .unwrap();
        assert!(ok);
        let updated = store.get_claim(claim.claim_id).await.unwrap().unwrap();
        assert_eq!(updated.status, ClaimStatus::Supported);
        assert_eq!(updated.validated_at.as_deref(), Some(validated.as_str()));

        // Retracted claims are terminal and never re-touched.
        store
            .update_claim_validation(claim.claim_id, ClaimStatus::Retracted, None, &validated)
            .await
            .unwrap();
        let untouched = store
            .update_claim_validation(
                claim.claim_id,
                ClaimStatus::Supported,
                Some(&validated),
                &validated,
            )
            .await
            .unwrap();
        assert!(!untouched);
        let final_claim = store.get_claim(claim.claim_id).await.unwrap().unwrap();
        assert_eq!(final_claim.status, ClaimStatus::Retracted);
    }

    #[tokio::test]
    async fn memory_records_round_trip_search_and_transitions() {
        let (_dir, store) = temp_store().await;
        let repo = repo_record();
        store.insert_repository(&repo).await.unwrap();
        let snapshot = snapshot_record(&repo);
        store.insert_snapshot(&snapshot).await.unwrap();
        let claim = ClaimRecord {
            claim_id: ClaimId::generate(),
            repository_id: repo.repository_id,
            snapshot_id: snapshot.snapshot_id,
            claim_type: "symbol_existence".into(),
            text: "claim".into(),
            status: ClaimStatus::Candidate,
            source_kind: "extracted".into(),
            confidence_category: ConfidenceCategory::Unverified,
            created_at: now_rfc3339(),
            validated_at: None,
            updated_at: now_rfc3339(),
            supersedes_claim_id: None,
        };
        store
            .insert_claims(std::slice::from_ref(&claim))
            .await
            .unwrap();

        let memory = MemoryRecord {
            memory_id: MemoryId::generate(),
            repository_id: repo.repository_id,
            snapshot_id: snapshot.snapshot_id,
            memory_type: MemoryType::FailedApproach,
            title: "Do not cache manifest hashes".into(),
            summary: "Caching manifest hashes caused stale bundles in snapshot identity.".into(),
            scope: "crates/ccc-snapshot".into(),
            created_by: "human:test".into(),
            created_at: now_rfc3339(),
            last_accessed_at: now_rfc3339(),
            status: ClaimStatus::Candidate,
            sharing_policy: "private".into(),
            sensitivity: "none".into(),
            supersedes_memory_id: None,
            stale_cause: None,
            updated_at: now_rfc3339(),
        };
        store
            .insert_memory(&memory, &[claim.claim_id])
            .await
            .unwrap();

        let fetched = store.get_memory(memory.memory_id).await.unwrap().unwrap();
        assert_eq!(fetched, memory);
        assert_eq!(
            store.claim_ids_for_memory(memory.memory_id).await.unwrap(),
            vec![claim.claim_id]
        );
        let linked = store.memories_for_claims(&[claim.claim_id]).await.unwrap();
        assert_eq!(linked.len(), 1);

        // FTS search hits on title tokens; filters narrow it.
        let hits = store
            .search_memories(repo.repository_id, "manifest caching", None, None, 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "title+summary terms match: {hits:?}");
        let wrong_type = store
            .search_memories(
                repo.repository_id,
                "manifest",
                None,
                Some(MemoryType::Incident),
                10,
            )
            .await
            .unwrap();
        assert!(wrong_type.is_empty());
        let wrong_status = store
            .search_memories(
                repo.repository_id,
                "manifest",
                Some(ClaimStatus::Verified),
                None,
                10,
            )
            .await
            .unwrap();
        assert!(wrong_status.is_empty());

        // Transition appends an event and records the cause; terminal states
        // are never touched.
        let now = now_rfc3339();
        let flipped = store
            .update_memory_status(
                memory.memory_id,
                ClaimStatus::Stale,
                "ccc-freshness",
                Some("claim went stale after file change"),
                &now,
            )
            .await
            .unwrap();
        assert!(flipped);
        let stale = store.get_memory(memory.memory_id).await.unwrap().unwrap();
        assert_eq!(stale.status, ClaimStatus::Stale);
        assert_eq!(
            stale.stale_cause.as_deref(),
            Some("claim went stale after file change")
        );
        let by_status = store
            .list_memories_by_status(repo.repository_id, &[ClaimStatus::Stale])
            .await
            .unwrap();
        assert_eq!(by_status.len(), 1);
        let events = store
            .list_memory_status_events(memory.memory_id)
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].actor, "ccc-freshness");

        store
            .update_memory_status(
                memory.memory_id,
                ClaimStatus::Retracted,
                "human:test",
                None,
                &now,
            )
            .await
            .unwrap();
        let untouched = store
            .update_memory_status(
                memory.memory_id,
                ClaimStatus::Candidate,
                "ccc-test",
                None,
                &now,
            )
            .await
            .unwrap();
        assert!(!untouched, "retracted memories are terminal");
        assert_eq!(
            store
                .list_memory_status_events(memory.memory_id)
                .await
                .unwrap()
                .len(),
            2
        );

        store
            .touch_memory(memory.memory_id, "2026-10-17T00:00:00Z")
            .await
            .unwrap();
        let touched = store.get_memory(memory.memory_id).await.unwrap().unwrap();
        assert_eq!(touched.last_accessed_at, "2026-10-17T00:00:00Z");

        // FR-036: retraction removes the record from active retrieval.
        let after_retract = store
            .search_memories(repo.repository_id, "manifest", None, None, 10)
            .await
            .unwrap();
        assert!(
            after_retract.is_empty(),
            "retracted records are not retrieved"
        );
        let explicit = store
            .search_memories(
                repo.repository_id,
                "manifest",
                Some(ClaimStatus::Retracted),
                None,
                10,
            )
            .await
            .unwrap();
        assert_eq!(explicit.len(), 1, "explicit status query still finds it");
    }

    #[tokio::test]
    async fn mark_claims_superseded_respects_terminal_states_and_self() {
        let (_dir, store) = temp_store().await;
        let repo = repo_record();
        store.insert_repository(&repo).await.unwrap();
        let snapshot = snapshot_record(&repo);
        store.insert_snapshot(&snapshot).await.unwrap();
        let mk = |text: &str| ClaimRecord {
            claim_id: ClaimId::generate(),
            repository_id: repo.repository_id,
            snapshot_id: snapshot.snapshot_id,
            claim_type: "finding".into(),
            text: text.into(),
            status: ClaimStatus::Candidate,
            source_kind: "human".into(),
            confidence_category: ConfidenceCategory::Unverified,
            created_at: now_rfc3339(),
            validated_at: None,
            updated_at: now_rfc3339(),
            supersedes_claim_id: None,
        };
        let active = mk("active");
        let terminal = ClaimRecord {
            status: ClaimStatus::Retracted,
            ..mk("terminal")
        };
        let flippable = ClaimRecord {
            status: ClaimStatus::Supported,
            ..mk("flippable")
        };
        store
            .insert_claims(&[active.clone(), terminal.clone(), flippable.clone()])
            .await
            .unwrap();

        // Self-supersession is filtered out; retracted claims stay terminal;
        // the non-terminal claim flips.
        let flipped = store
            .mark_claims_superseded(
                &[active.claim_id, terminal.claim_id, flippable.claim_id],
                active.claim_id,
                &now_rfc3339(),
            )
            .await
            .unwrap();
        assert_eq!(flipped, vec![flippable.claim_id]);
        let old = store.get_claim(terminal.claim_id).await.unwrap().unwrap();
        assert_eq!(old.status, ClaimStatus::Retracted);
        let superseded = store.get_claim(flippable.claim_id).await.unwrap().unwrap();
        assert_eq!(superseded.status, ClaimStatus::Superseded);
    }

    #[tokio::test]
    async fn foreign_key_violations_are_rejected() {
        let (_dir, store) = temp_store().await;
        let phantom_snapshot = SnapshotRecord {
            snapshot_id: SnapshotId::generate(),
            repository_id: RepositoryId::generate(),
            commit_hash: None,
            branch: None,
            kind: SnapshotKind::WorkingTree,
            is_dirty: false,
            manifest_hash: "x".into(),
            indexer_version: "test".into(),
            configuration_hash: "cfg".into(),
            status: SnapshotStatus::Building,
            created_at: now_rfc3339(),
        };
        let file = file_record(&phantom_snapshot, "src/lib.rs");
        let result = store.insert_files(&[file]).await;
        assert!(
            result.is_err(),
            "file insert without snapshot row must fail FK check"
        );
    }

    #[tokio::test]
    async fn publish_generation_flips_active_pointer_atomically() {
        let (_dir, store) = temp_store().await;
        let repo = repo_record();
        store.insert_repository(&repo).await.unwrap();
        let snapshot = snapshot_record(&repo);
        store.insert_snapshot(&snapshot).await.unwrap();
        let file = file_record(&snapshot, "src/lib.rs");
        store
            .insert_files(std::slice::from_ref(&file))
            .await
            .unwrap();
        let symbol = symbol_record(&file, "lib::run");
        store.insert_symbols(&[symbol]).await.unwrap();
        let generation = generation_record(&repo, &snapshot);
        store.create_generation(&generation).await.unwrap();

        store
            .publish_generation(generation.generation_id)
            .await
            .unwrap();

        let active = store
            .get_active_generation(repo.repository_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(active.generation_id, generation.generation_id);
        let gen = store
            .get_generation(generation.generation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(gen.status, GenerationStatus::Published);
        let snap = store
            .get_snapshot(snapshot.snapshot_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snap.status, SnapshotStatus::Published);
    }

    #[tokio::test]
    async fn publish_rejects_cross_snapshot_data() {
        let (_dir, store) = temp_store().await;
        let repo = repo_record();
        store.insert_repository(&repo).await.unwrap();
        // Two distinct snapshots, both building: a symbol of snapshot A that
        // references a file of snapshot B is legal per-FK but mixes revisions;
        // the referential gate must block publication of A (SDD §7.5).
        let snapshot_a = snapshot_record(&repo);
        let snapshot_b = SnapshotRecord {
            snapshot_id: SnapshotId::generate(),
            manifest_hash: "other-manifest".into(),
            ..snapshot_record(&repo)
        };
        store.insert_snapshot(&snapshot_a).await.unwrap();
        store.insert_snapshot(&snapshot_b).await.unwrap();

        let file_b = file_record(&snapshot_b, "foreign.rs");
        store
            .insert_files(std::slice::from_ref(&file_b))
            .await
            .unwrap();
        let bad_symbol = SymbolRecord {
            file_id: file_b.file_id,
            ..symbol_record(&file_record(&snapshot_a, "placeholder.rs"), "x::y")
        };
        store.insert_symbols(&[bad_symbol]).await.unwrap();
        let generation = generation_record(&repo, &snapshot_a);
        store.create_generation(&generation).await.unwrap();

        let result = store.publish_generation(generation.generation_id).await;
        assert!(
            result.is_err(),
            "mixed-snapshot generation must not publish"
        );
        // No active generation was set.
        assert!(store
            .get_active_generation(repo.repository_id)
            .await
            .unwrap()
            .is_none());
        // Snapshot A stays building — publication never happened.
        let snap_a = store
            .get_snapshot(snapshot_a.snapshot_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snap_a.status, SnapshotStatus::Building);
    }

    #[tokio::test]
    async fn fts5_search_ranks_and_filters() {
        let (_dir, store) = temp_store().await;
        let repo = repo_record();
        store.insert_repository(&repo).await.unwrap();
        let snapshot = snapshot_record(&repo);
        store.insert_snapshot(&snapshot).await.unwrap();

        let entries = vec![
            SearchEntry {
                term: "snapshot identity".into(),
                path: "docs/identity.md".into(),
                kind: "file".into(),
                entity_id: "file-1".into(),
                title: None,
            },
            SearchEntry {
                term: "compute_snapshot_identity".into(),
                path: "src/identity.rs".into(),
                kind: "symbol".into(),
                entity_id: "symbol-1".into(),
                title: Some("identity::compute_snapshot_identity".into()),
            },
        ];
        store
            .replace_snapshot_entries(snapshot.snapshot_id, &entries)
            .await
            .unwrap();

        let hits = store
            .search(snapshot.snapshot_id, "identity", 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 2);

        let exact = store
            .lookup_exact(snapshot.snapshot_id, "compute_snapshot_identity", 10)
            .await
            .unwrap();
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].entity_id, "symbol-1");

        store
            .delete_snapshot_entries(snapshot.snapshot_id)
            .await
            .unwrap();
        let hits = store
            .search(snapshot.snapshot_id, "identity", 10)
            .await
            .unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn claims_depending_on_and_staleness() {
        let (_dir, store) = temp_store().await;
        let repo = repo_record();
        store.insert_repository(&repo).await.unwrap();
        let snapshot = snapshot_record(&repo);
        store.insert_snapshot(&snapshot).await.unwrap();
        let file_a = file_record(&snapshot, "src/a.rs");
        let file_b = file_record(&snapshot, "src/b.rs");
        store
            .insert_files(&[file_a.clone(), file_b.clone()])
            .await
            .unwrap();
        let symbol_a = symbol_record(&file_a, "a::func");
        store
            .insert_symbols(std::slice::from_ref(&symbol_a))
            .await
            .unwrap();

        let now = now_rfc3339();
        let claim = ClaimRecord {
            claim_id: ClaimId::generate(),
            repository_id: repo.repository_id,
            snapshot_id: snapshot.snapshot_id,
            claim_type: "symbol_existence".into(),
            text: "Symbol a::func exists".into(),
            status: ClaimStatus::Candidate,
            source_kind: "extracted".into(),
            confidence_category: ConfidenceCategory::Unverified,
            created_at: now.clone(),
            validated_at: None,
            updated_at: now.clone(),
            supersedes_claim_id: None,
        };
        store
            .insert_claims(std::slice::from_ref(&claim))
            .await
            .unwrap();
        store
            .insert_claim_dependencies(&[
                ClaimDependencyRecord {
                    claim_id: claim.claim_id,
                    depends_on: symbol_a.symbol_id.to_string(),
                    depends_on_kind: "symbol".into(),
                },
                ClaimDependencyRecord {
                    claim_id: claim.claim_id,
                    depends_on: file_a.file_id.to_string(),
                    depends_on_kind: "file".into(),
                },
            ])
            .await
            .unwrap();

        // Selective: depending on file_b only finds nothing (FR-040).
        let none = store
            .claims_depending_on(repo.repository_id, &[file_b.file_id.to_string()], &[])
            .await
            .unwrap();
        assert!(none.is_empty());

        let found = store
            .claims_depending_on(
                repo.repository_id,
                &[file_a.file_id.to_string()],
                &[symbol_a.symbol_id.to_string()],
            )
            .await
            .unwrap();
        assert_eq!(found.len(), 1);

        let flipped = store
            .mark_claims_stale(&[claim.claim_id], &now)
            .await
            .unwrap();
        assert_eq!(flipped, vec![claim.claim_id]);
        let updated = store.get_claim(claim.claim_id).await.unwrap().unwrap();
        assert_eq!(updated.status, ClaimStatus::Stale);
        // Already-stale claims are not re-flipped.
        let again = store
            .mark_claims_stale(&[claim.claim_id], &now)
            .await
            .unwrap();
        assert!(again.is_empty());
    }

    #[tokio::test]
    async fn jobs_are_idempotent_per_key() {
        let (_dir, store) = temp_store().await;
        let repo = repo_record();
        store.insert_repository(&repo).await.unwrap();
        let now = now_rfc3339();
        let job = JobRecord {
            job_id: JobId::generate(),
            repository_id: repo.repository_id,
            job_type: "repo_add".into(),
            state: JobState::Queued,
            idempotency_key: "key-1".into(),
            payload_json: None,
            retry_count: 0,
            created_at: now.clone(),
            updated_at: now,
            finished_at: None,
            error_code: None,
            error_message: None,
        };
        let first = store.insert_job_idempotent(&job).await.unwrap();
        let second = store.insert_job_idempotent(&job).await.unwrap();
        assert_eq!(
            first.job_id, second.job_id,
            "same key must reuse the job record"
        );
        assert_eq!(store.list_jobs(repo.repository_id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn blob_writes_are_atomic_and_deduplicated() {
        let (_dir, store) = temp_store().await;
        let blobs_dir = _dir.path().join("blobs");
        let artifacts = FsArtifactStore::new(blobs_dir.clone(), store.clone());

        let hash = artifacts
            .put(b"artifact payload", "application/json")
            .await
            .unwrap();
        assert_eq!(hash.len(), 64);
        // Second put with identical content: deduplicated, no error.
        let hash2 = artifacts
            .put(b"artifact payload", "application/json")
            .await
            .unwrap();
        assert_eq!(hash, hash2);

        let read_back = artifacts.get(&hash).await.unwrap().unwrap();
        assert_eq!(read_back, b"artifact payload");
        let meta = artifacts.get_meta(&hash).await.unwrap().unwrap();
        assert_eq!(meta.byte_length, 16);
        assert_eq!(meta.media_type, "application/json");
        // No temp files left behind.
        let prefix_dir = blobs_dir.join(&hash[..2]);
        let stray: Vec<_> = std::fs::read_dir(&prefix_dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.path()
                    .extension()
                    .is_some_and(|x| x.to_string_lossy().starts_with("tmp"))
            })
            .collect();
        assert!(stray.is_empty(), "temp files must not survive put()");

        assert!(!artifacts.exists("ffffffff").await.unwrap());
    }
}
