//! Filesystem content-addressed artifact store (TIS §5.4).
//!
//! Blobs live under `<data>/blobs/<aa>/<hash>`. Writes are atomic: bytes land
//! in a temporary file first, then are renamed into place. Metadata (hash,
//! byte length, media type, retention state) is recorded in the database.

use crate::sqlite::SqliteStore;
use crate::traits::*;
use ccc_domain::*;
use std::path::{Path, PathBuf};

pub struct FsArtifactStore {
    blobs_dir: PathBuf,
    db: std::sync::Arc<SqliteStore>,
}

impl FsArtifactStore {
    pub fn new(blobs_dir: PathBuf, db: std::sync::Arc<SqliteStore>) -> Self {
        Self { blobs_dir, db }
    }
}

#[async_trait::async_trait]
impl ArtifactStore for FsArtifactStore {
    async fn put(&self, bytes: &[u8], media_type: &str) -> StoreResult<String> {
        let hash = blake3_hex(bytes);
        let dest = blob_path(&self.blobs_dir, &hash);
        if !dest.exists() {
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    CccError::internal_error(format!("cannot create blob dir: {e}"))
                })?;
            }
            // Atomic write: tmp file + rename on the same volume.
            let tmp = dest.with_extension(format!("tmp-{}", uuid::Uuid::now_v7().simple()));
            tokio::fs::write(&tmp, bytes)
                .await
                .map_err(|e| CccError::internal_error(format!("cannot write blob: {e}")))?;
            tokio::fs::rename(&tmp, &dest)
                .await
                .map_err(|e| CccError::internal_error(format!("cannot finalize blob: {e}")))?;
        }
        let meta = ArtifactMeta {
            content_hash: hash.clone(),
            byte_length: bytes.len() as u64,
            media_type: media_type.to_string(),
            retention_state: "retained".to_string(),
        };
        self.db.record_artifact_meta(&meta, &now_rfc3339()).await?;
        Ok(hash)
    }

    async fn get(&self, content_hash: &str) -> StoreResult<Option<Vec<u8>>> {
        let path = blob_path(&self.blobs_dir, content_hash);
        if !path.exists() {
            return Ok(None);
        }
        tokio::fs::read(&path)
            .await
            .map(Some)
            .map_err(|e| CccError::internal_error(format!("cannot read blob: {e}")))
    }

    async fn exists(&self, content_hash: &str) -> StoreResult<bool> {
        Ok(blob_path(&self.blobs_dir, content_hash).exists())
    }

    async fn get_meta(&self, content_hash: &str) -> StoreResult<Option<ArtifactMeta>> {
        self.db.get_meta(content_hash).await
    }
}

/// Validates a hash string before using it as a filename (defensive).
fn blob_path(blobs_dir: &Path, content_hash: &str) -> PathBuf {
    debug_assert!(!content_hash.is_empty());
    let safe: String = content_hash
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let prefix = &safe[..2.min(safe.len())];
    blobs_dir.join(prefix).join(safe)
}
