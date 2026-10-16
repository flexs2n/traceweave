//! `ccc-repository` — repository registration and the Git adapter.
//!
//! Git access goes through the [`GitProvider`] trait behind the [`GitCli`]
//! subprocess implementation. No shell is involved: every argument is passed
//! verbatim to the git executable, so repository content can never inject
//! commands (TIS §4 table, TIS §17.2).

use ccc_domain::{now_rfc3339, CccError, ChangeKind, RepositoryId, RepositoryRecord};
use ccc_storage::RepositoryStore;
use std::path::Path;
use std::process::Command;

/// A single entry of `git status --porcelain` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    pub index_status: char,
    pub worktree_status: char,
    /// Repository-relative, forward-slash path.
    pub path: String,
    /// Original path for renames.
    pub orig_path: Option<String>,
}

/// A single entry of a normalized change set (SDD §5.15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeEntry {
    pub kind: ChangeKind,
    pub path: String,
    pub old_path: Option<String>,
}

/// Git access boundary (TIS §14).
pub trait GitProvider: Send + Sync {
    fn is_git_repository(&self, root: &Path) -> bool;
    /// HEAD commit hash; `None` when the repository has no commits yet.
    fn rev_parse_head(&self, root: &Path) -> Result<Option<String>, CccError>;
    /// Current branch name; `None` in detached-HEAD state.
    fn current_branch(&self, root: &Path) -> Result<Option<String>, CccError>;
    fn status_porcelain(&self, root: &Path) -> Result<Vec<StatusEntry>, CccError>;
    /// Tracked files (plus untracked non-ignored files when requested).
    /// `.gitignore` is honored inherently (TIS §7.1).
    fn ls_files(&self, root: &Path, include_untracked: bool) -> Result<Vec<String>, CccError>;
    /// Rename-aware diff (`git diff --name-status -M`); `head = None` diffs
    /// the base revision against the working tree.
    fn diff_name_status(
        &self,
        root: &Path,
        base: &str,
        head: Option<&str>,
    ) -> Result<Vec<ChangeEntry>, CccError>;
    fn rev_exists(&self, root: &Path, rev: &str) -> Result<bool, CccError>;
}

/// Git CLI adapter. Commands run without a shell; arguments are passed as
/// literal argv entries.
pub struct GitCli;

impl GitCli {
    /// Runs git with verbatim arguments and returns stdout.
    fn run(&self, root: &Path, args: &[&str]) -> Result<String, CccError> {
        let output = Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .map_err(|e| {
                CccError::dependency_unavailable(format!(
                    "cannot execute git (is it on PATH?): {e}"
                ))
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let brief: String = stderr.lines().next().unwrap_or("").to_string();
            return Err(CccError::dependency_unavailable(format!(
                "git {} failed: {brief}",
                args.first().unwrap_or(&"")
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

impl Default for GitCli {
    fn default() -> Self {
        Self
    }
}

impl GitProvider for GitCli {
    fn is_git_repository(&self, root: &Path) -> bool {
        Command::new("git")
            .current_dir(root)
            .args(["rev-parse", "--is-inside-work-tree"])
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn rev_parse_head(&self, root: &Path) -> Result<Option<String>, CccError> {
        let out = self.run(root, &["rev-parse", "HEAD"])?;
        let trimmed = out.trim();
        if trimmed.is_empty() {
            Ok(None)
        } else {
            Ok(Some(trimmed.to_string()))
        }
    }

    fn current_branch(&self, root: &Path) -> Result<Option<String>, CccError> {
        // --show-current prints nothing on detached HEAD.
        let out = self.run(root, &["branch", "--show-current"])?;
        let trimmed = out.trim();
        if trimmed.is_empty() {
            Ok(None)
        } else {
            Ok(Some(trimmed.to_string()))
        }
    }

    fn status_porcelain(&self, root: &Path) -> Result<Vec<StatusEntry>, CccError> {
        let out = self.run(root, &["status", "--porcelain", "-M"])?;
        Ok(out
            .lines()
            .filter(|line| line.len() >= 3)
            .map(|line| {
                let index_status = line.as_bytes()[0] as char;
                let worktree_status = line.as_bytes()[1] as char;
                let rest = &line[3..];
                if rest.contains(" -> ") {
                    let (orig, new) = rest.split_once(" -> ").unwrap_or((rest, ""));
                    StatusEntry {
                        index_status,
                        worktree_status,
                        path: normalize(new),
                        orig_path: Some(normalize(orig)),
                    }
                } else {
                    StatusEntry {
                        index_status,
                        worktree_status,
                        path: normalize(rest),
                        orig_path: None,
                    }
                }
            })
            .collect())
    }

    fn ls_files(&self, root: &Path, include_untracked: bool) -> Result<Vec<String>, CccError> {
        let mut args = vec!["ls-files", "-z", "--cached"];
        if include_untracked {
            args.push("--others");
            args.push("--exclude-standard");
        }
        let out = self.run(root, &args)?;
        Ok(out
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(normalize)
            .collect())
    }

    fn diff_name_status(
        &self,
        root: &Path,
        base: &str,
        head: Option<&str>,
    ) -> Result<Vec<ChangeEntry>, CccError> {
        let mut args = vec!["diff", "--name-status", "-M", "--no-color", base];
        match head {
            Some(h) => args.push(h),
            None => args.push("--"),
        }
        let out = self.run(root, &args)?;
        parse_name_status(&out)
    }

    fn rev_exists(&self, root: &Path, rev: &str) -> Result<bool, CccError> {
        // rev passed as an argument to a git subcommand, never through a shell.
        let output = Command::new("git")
            .current_dir(root)
            .args(["rev-parse", "--verify", "--quiet", rev])
            .output()
            .map_err(|e| CccError::dependency_unavailable(format!("cannot execute git: {e}")))?;
        Ok(output.status.success())
    }
}

/// Normalizes a git-reported path to repo-relative forward-slash form.
fn normalize(path: &str) -> String {
    path.replace('\\', "/")
}

fn parse_name_status(out: &str) -> Result<Vec<ChangeEntry>, CccError> {
    let mut entries = Vec::new();
    for line in out.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, '\t');
        let code = parts.next().unwrap_or("").trim();
        let paths = parts.next().unwrap_or("");
        let (kind, path, old_path) = if code.starts_with('R') {
            let mut p = paths.splitn(2, '\t');
            let old = p.next().unwrap_or("");
            let new = p.next().unwrap_or("");
            (ChangeKind::Renamed, normalize(new), Some(normalize(old)))
        } else if code.starts_with('C') {
            // Copies are treated as additions of the destination.
            let mut p = paths.splitn(2, '\t');
            let _old = p.next().unwrap_or("");
            let new = p.next().unwrap_or("");
            (ChangeKind::Added, normalize(new), None)
        } else {
            match code.chars().next() {
                Some('A') => (ChangeKind::Added, normalize(paths), None),
                Some('D') => (ChangeKind::Deleted, normalize(paths), None),
                Some('M') => (ChangeKind::Modified, normalize(paths), None),
                Some('T') => (ChangeKind::Modified, normalize(paths), None),
                _ => {
                    return Err(CccError::internal_error(format!(
                        "unrecognized diff status code {code:?}"
                    )))
                }
            }
        };
        entries.push(ChangeEntry {
            kind,
            path,
            old_path,
        });
    }
    Ok(entries)
}

/// Registration service (TIS §4.1).
pub struct RepositoryRegistry<'a> {
    store: &'a dyn RepositoryStore,
    git: &'a dyn GitProvider,
}

impl<'a> RepositoryRegistry<'a> {
    pub fn new(store: &'a dyn RepositoryStore, git: &'a dyn GitProvider) -> Self {
        Self { store, git }
    }

    /// Registers (or re-resolves) a repository. Idempotent per root path.
    /// When the effective configuration changed, `configuration_hash` is
    /// refreshed. Submodules are recorded as a detected limitation, not
    /// indexed.
    pub async fn register(
        &self,
        root: &Path,
        configuration_hash: &str,
    ) -> Result<RepositoryRecord, CccError> {
        let canonical = std::fs::canonicalize(root).map_err(|e| {
            CccError::invalid_request(format!("cannot resolve repository path: {e}"))
        })?;
        // Strip the Windows extended-length prefix for readable records.
        let canonical_str = canonical.to_string_lossy().replace('\\', "/");
        let root_str = canonical_str
            .strip_prefix("//?/")
            .unwrap_or(&canonical_str)
            .to_string();

        if !self.git.is_git_repository(&canonical) {
            return Err(CccError::invalid_request(format!(
                "not a git repository: {root_str}"
            )));
        }

        let mut limitations = Vec::new();
        if canonical.join(".gitmodules").exists() {
            limitations.push("submodules detected; submodule contents are not indexed".to_string());
        }

        if let Some(existing) = self.store.find_repository_by_path(&root_str).await? {
            if existing.configuration_hash != configuration_hash
                || existing.limitations != limitations
            {
                self.store
                    .update_repository_configuration(
                        existing.repository_id,
                        configuration_hash,
                        &now_rfc3339(),
                    )
                    .await?;
            }
            let branch = self.git.current_branch(&canonical)?;
            let mut updated = existing;
            updated.limitations = limitations;
            updated.default_branch = branch.or(updated.default_branch);
            updated.configuration_hash = configuration_hash.to_string();
            updated.updated_at = now_rfc3339();
            return Ok(updated);
        }

        let now = now_rfc3339();
        let branch = self.git.current_branch(&canonical)?;
        let record = RepositoryRecord {
            repository_id: RepositoryId::generate(),
            root_path: root_str,
            canonical_uri: None,
            default_branch: branch,
            configuration_hash: configuration_hash.to_string(),
            limitations,
            created_at: now.clone(),
            updated_at: now,
        };
        self.store.insert_repository(&record).await?;
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccc_domain::ErrorCode;
    use ccc_storage::{RepositoryStore, StoreResult};
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct FakeStore(Mutex<HashMap<String, RepositoryRecord>>);

    #[async_trait::async_trait]
    impl RepositoryStore for FakeStore {
        async fn insert_repository(&self, record: &RepositoryRecord) -> StoreResult<()> {
            self.0
                .lock()
                .unwrap()
                .insert(record.root_path.clone(), record.clone());
            Ok(())
        }
        async fn find_repository_by_path(
            &self,
            root_path: &str,
        ) -> StoreResult<Option<RepositoryRecord>> {
            Ok(self.0.lock().unwrap().get(root_path).cloned())
        }
        async fn get_repository(&self, _id: RepositoryId) -> StoreResult<Option<RepositoryRecord>> {
            Ok(None)
        }
        async fn list_repositories(&self) -> StoreResult<Vec<RepositoryRecord>> {
            Ok(self.0.lock().unwrap().values().cloned().collect())
        }
        async fn update_repository_configuration(
            &self,
            id: RepositoryId,
            configuration_hash: &str,
            updated_at: &str,
        ) -> StoreResult<()> {
            let mut map = self.0.lock().unwrap();
            if let Some(record) = map.values_mut().find(|r| r.repository_id == id) {
                record.configuration_hash = configuration_hash.to_string();
                record.updated_at = updated_at.to_string();
            }
            Ok(())
        }
    }

    fn git_init_repo(dir: &Path) {
        let git = GitCli;
        assert!(!git.is_git_repository(dir));
        run(dir, &["init", "--initial-branch=main"]);
        run(dir, &["config", "user.email", "test@example.com"]);
        run(dir, &["config", "user.name", "test"]);
    }

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

    fn commit_all(dir: &Path, message: &str) {
        run(dir, &["add", "-A"]);
        run(dir, &["commit", "-m", message]);
    }

    #[test]
    fn rev_parse_head_and_branch_on_clean_repo() {
        let dir = tempfile::tempdir().unwrap();
        git_init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        commit_all(dir.path(), "init");
        let git = GitCli;
        let head = git
            .rev_parse_head(dir.path())
            .unwrap()
            .expect("head exists");
        assert_eq!(head.len(), 40);
        assert_eq!(
            git.current_branch(dir.path()).unwrap().as_deref(),
            Some("main")
        );
    }

    #[test]
    fn ls_files_honors_gitignore_and_reports_forward_slashes() {
        let dir = tempfile::tempdir().unwrap();
        git_init_repo(dir.path());
        std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.path().join("src/nested/util.rs"), "// util\n").unwrap();
        std::fs::write(dir.path().join("ignored.rs"), "// ignored\n").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
        commit_all(dir.path(), "init");
        let git = GitCli;
        let files = git.ls_files(dir.path(), true).unwrap();
        assert!(files.contains(&"src/lib.rs".to_string()));
        assert!(files.contains(&"src/nested/util.rs".to_string()));
        assert!(!files.iter().any(|f| f.contains("ignored.rs")));
    }

    #[test]
    fn rename_is_reported_as_rename_not_delete_add() {
        let dir = tempfile::tempdir().unwrap();
        git_init_repo(dir.path());
        std::fs::write(dir.path().join("old_name.rs"), "pub fn kept() {}\n").unwrap();
        commit_all(dir.path(), "init");
        // git mv keeps content identical so -M detects the rename
        run(dir.path(), &["mv", "old_name.rs", "new_name.rs"]);
        commit_all(dir.path(), "rename");
        let head = GitCli.rev_parse_head(dir.path()).unwrap().unwrap();
        let changes = GitCli
            .diff_name_status(dir.path(), "HEAD~1", Some(&head))
            .unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Renamed);
        assert_eq!(changes[0].path, "new_name.rs");
        assert_eq!(changes[0].old_path.as_deref(), Some("old_name.rs"));
    }

    #[test]
    fn diff_against_working_tree_detects_modification() {
        let dir = tempfile::tempdir().unwrap();
        git_init_repo(dir.path());
        std::fs::write(dir.path().join("file.rs"), "pub fn a() {}\n").unwrap();
        commit_all(dir.path(), "init");
        std::fs::write(dir.path().join("file.rs"), "pub fn b() {}\n").unwrap();
        let changes = GitCli.diff_name_status(dir.path(), "HEAD", None).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Modified);
        assert_eq!(changes[0].path, "file.rs");
    }

    #[test]
    fn status_porcelain_reports_dirty_state() {
        let dir = tempfile::tempdir().unwrap();
        git_init_repo(dir.path());
        std::fs::write(dir.path().join("file.rs"), "pub fn a() {}\n").unwrap();
        commit_all(dir.path(), "init");
        let git = GitCli;
        assert!(git.status_porcelain(dir.path()).unwrap().is_empty());
        std::fs::write(dir.path().join("file.rs"), "pub fn changed() {}\n").unwrap();
        let status = git.status_porcelain(dir.path()).unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].path, "file.rs");
    }

    #[tokio::test]
    async fn register_is_idempotent_and_records_limitations() {
        let dir = tempfile::tempdir().unwrap();
        git_init_repo(dir.path());
        std::fs::write(dir.path().join("lib.rs"), "pub fn x() {}\n").unwrap();
        std::fs::write(dir.path().join(".gitmodules"), "").unwrap();
        commit_all(dir.path(), "init");

        let store = FakeStore(Mutex::new(HashMap::new()));
        let registry = RepositoryRegistry::new(&store, &GitCli);
        let first = registry.register(dir.path(), "hash-1").await.unwrap();
        assert!(first.limitations.iter().any(|l| l.contains("submodules")));
        let second = registry.register(dir.path(), "hash-1").await.unwrap();
        assert_eq!(first.repository_id, second.repository_id);

        let third = registry.register(dir.path(), "hash-2").await.unwrap();
        assert_eq!(first.repository_id, third.repository_id);
        assert_eq!(third.configuration_hash, "hash-2");
    }

    #[tokio::test]
    async fn register_rejects_non_git_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = FakeStore(Mutex::new(HashMap::new()));
        let registry = RepositoryRegistry::new(&store, &GitCli);
        let err = registry.register(dir.path(), "hash").await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
    }
}
