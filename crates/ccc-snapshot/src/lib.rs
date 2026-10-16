//! `ccc-snapshot` — snapshot identity (TIS §4.2).
//!
//! A snapshot captures the full repository state: commit, branch, dirty flag,
//! and a BLAKE3 manifest hash over the sorted `{path → content_hash}` mapping
//! of tracked (and configured untracked) files. A dirty worktree is never
//! identified by its base commit alone — the manifest hash makes dirty
//! snapshots distinct (TIS §4.2).

use ccc_domain::*;
use ccc_repository::GitProvider;
use ccc_storage::{SnapshotStore, StoreResult};
use std::collections::BTreeMap;
use std::path::Path;

/// The fully computed identity of a repository state.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotDraft {
    pub repository_id: RepositoryId,
    pub commit_hash: Option<String>,
    pub branch: Option<String>,
    pub kind: SnapshotKind,
    pub is_dirty: bool,
    /// Sorted `{path → BLAKE3 content hash}` of discovered files.
    pub manifest: BTreeMap<String, String>,
    pub manifest_hash: String,
    pub indexer_version: String,
    pub configuration_hash: String,
}

/// Current indexer version; participates in snapshot identity.
pub const INDEXER_VERSION: &str = concat!("ccc-indexer/", env!("CARGO_PKG_VERSION"));

/// Computes a snapshot draft for a repository working tree.
pub struct SnapshotBuilder<'a> {
    git: &'a dyn GitProvider,
}

impl<'a> SnapshotBuilder<'a> {
    pub fn new(git: &'a dyn GitProvider) -> Self {
        Self { git }
    }

    /// Builds the manifest by hashing every discovered file's raw bytes.
    ///
    /// CRLF note: raw bytes are hashed, so files that differ only by line
    /// ending (e.g. via autocrlf) produce distinct — and individually valid —
    /// content hashes (plan risk R2).
    pub fn build_working_tree(
        &self,
        repository_id: RepositoryId,
        root: &Path,
        configuration_hash: &str,
        include_untracked: bool,
    ) -> Result<SnapshotDraft, CccError> {
        let commit_hash = self.git.rev_parse_head(root)?;
        let branch = self.git.current_branch(root)?;
        let is_dirty = !self.git.status_porcelain(root)?.is_empty();

        let mut manifest = BTreeMap::new();
        for rel_path in self.git.ls_files(root, include_untracked)? {
            let abs = root.join(&rel_path);
            let bytes = std::fs::read(&abs).map_err(|e| {
                CccError::internal_error(format!("cannot read manifest file {rel_path}: {e}"))
            })?;
            manifest.insert(rel_path, blake3_hex(&bytes));
        }

        let manifest_hash = manifest_hash(&manifest);

        Ok(SnapshotDraft {
            repository_id,
            commit_hash,
            branch,
            kind: SnapshotKind::WorkingTree,
            is_dirty,
            manifest,
            manifest_hash,
            indexer_version: INDEXER_VERSION.to_string(),
            configuration_hash: configuration_hash.to_string(),
        })
    }
}

/// BLAKE3 over the canonical serialization of the sorted manifest: one
/// `path\nhash\n` pair per line, sorted by path.
pub fn manifest_hash(manifest: &BTreeMap<String, String>) -> String {
    let mut canonical = String::with_capacity(manifest.len() * 96);
    for (path, hash) in manifest {
        canonical.push_str(path);
        canonical.push('\n');
        canonical.push_str(hash);
        canonical.push('\n');
    }
    blake3_hex(canonical.as_bytes())
}

/// Finds or creates the persisted snapshot for a draft.
///
/// Re-running against the same tree yields the same manifest hash and reuses
/// the existing snapshot record (stable identity for repeat runs). A record
/// left `building`/`failed` by an interrupted run is reset to `building`.
pub async fn find_or_create_snapshot(
    store: &dyn SnapshotStore,
    draft: SnapshotDraft,
) -> StoreResult<SnapshotRecord> {
    if let Some(existing) = store
        .find_identical_snapshot(
            draft.repository_id,
            draft.kind,
            draft.is_dirty,
            &draft.manifest_hash,
            &draft.configuration_hash,
            &draft.indexer_version,
        )
        .await?
    {
        match existing.status {
            SnapshotStatus::Published => return Ok(existing),
            SnapshotStatus::Building | SnapshotStatus::Failed => {
                store
                    .set_snapshot_status(existing.snapshot_id, SnapshotStatus::Building)
                    .await?;
                return Ok(SnapshotRecord {
                    status: SnapshotStatus::Building,
                    ..existing
                });
            }
            SnapshotStatus::Superseded => {
                // Identity is immutable; a superseded snapshot with identical
                // identity can be republished by creating fresh state — reset
                // to building for a new generation run.
                store
                    .set_snapshot_status(existing.snapshot_id, SnapshotStatus::Building)
                    .await?;
                return Ok(SnapshotRecord {
                    status: SnapshotStatus::Building,
                    ..existing
                });
            }
        }
    }

    let record = SnapshotRecord {
        snapshot_id: SnapshotId::generate(),
        repository_id: draft.repository_id,
        commit_hash: draft.commit_hash,
        branch: draft.branch,
        kind: draft.kind,
        is_dirty: draft.is_dirty,
        manifest_hash: draft.manifest_hash,
        indexer_version: draft.indexer_version,
        configuration_hash: draft.configuration_hash,
        status: SnapshotStatus::Building,
        created_at: now_rfc3339(),
    };
    store.insert_snapshot(&record).await?;
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccc_repository::GitCli;
    use std::process::Command;

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

    #[test]
    fn same_tree_twice_same_manifest_hash() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("lib.rs"), "pub fn a() {}\n").unwrap();
        run(dir.path(), &["add", "-A"]);
        run(dir.path(), &["commit", "-m", "init"]);

        let builder = SnapshotBuilder::new(&GitCli);
        let first = builder
            .build_working_tree(RepositoryId::generate(), dir.path(), "cfg", false)
            .unwrap();
        let second = builder
            .build_working_tree(RepositoryId::generate(), dir.path(), "cfg", false)
            .unwrap();
        assert_eq!(first.manifest_hash, second.manifest_hash);
        assert_eq!(first.manifest, second.manifest);
        assert!(!first.is_dirty);
    }

    #[test]
    fn dirty_snapshot_differs_from_clean() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("lib.rs"), "pub fn a() {}\n").unwrap();
        run(dir.path(), &["add", "-A"]);
        run(dir.path(), &["commit", "-m", "init"]);

        let builder = SnapshotBuilder::new(&GitCli);
        let clean = builder
            .build_working_tree(RepositoryId::generate(), dir.path(), "cfg", false)
            .unwrap();

        std::fs::write(dir.path().join("lib.rs"), "pub fn b() {}\n").unwrap();
        let dirty = builder
            .build_working_tree(RepositoryId::generate(), dir.path(), "cfg", false)
            .unwrap();

        assert!(dirty.is_dirty);
        assert_ne!(dirty.manifest_hash, clean.manifest_hash);
        // Same base commit for both — identity must still be distinct.
        assert_eq!(dirty.commit_hash, clean.commit_hash);
    }

    #[test]
    fn manifest_hash_is_order_independent() {
        let mut a = BTreeMap::new();
        a.insert("z.rs".to_string(), "h1".to_string());
        a.insert("a.rs".to_string(), "h2".to_string());
        let mut b = BTreeMap::new();
        b.insert("a.rs".to_string(), "h2".to_string());
        b.insert("z.rs".to_string(), "h1".to_string());
        assert_eq!(manifest_hash(&a), manifest_hash(&b));
    }

    #[test]
    fn untracked_files_optionally_included() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("tracked.rs"), "pub fn t() {}\n").unwrap();
        run(dir.path(), &["add", "-A"]);
        run(dir.path(), &["commit", "-m", "init"]);
        std::fs::write(dir.path().join("untracked.rs"), "pub fn u() {}\n").unwrap();

        let builder = SnapshotBuilder::new(&GitCli);
        let without = builder
            .build_working_tree(RepositoryId::generate(), dir.path(), "cfg", false)
            .unwrap();
        let with = builder
            .build_working_tree(RepositoryId::generate(), dir.path(), "cfg", true)
            .unwrap();
        assert!(!without.manifest.contains_key("untracked.rs"));
        assert!(with.manifest.contains_key("untracked.rs"));
        assert!(with.is_dirty);
    }

    #[tokio::test]
    async fn find_or_create_reuses_identical_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("lib.rs"), "pub fn a() {}\n").unwrap();
        run(dir.path(), &["add", "-A"]);
        run(dir.path(), &["commit", "-m", "init"]);

        let store = ccc_storage::test_support::InMemorySnapshotStore::default();
        let builder = SnapshotBuilder::new(&GitCli);
        let draft = builder
            .build_working_tree(RepositoryId::generate(), dir.path(), "cfg", false)
            .unwrap();

        let first = find_or_create_snapshot(&store, draft.clone())
            .await
            .unwrap();
        assert_eq!(first.status, SnapshotStatus::Building);
        let second = find_or_create_snapshot(&store, draft).await.unwrap();
        assert_eq!(first.snapshot_id, second.snapshot_id);
    }
}
