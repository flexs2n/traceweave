//! Drift-guard tests (Step 4 T2, FR-010): `index_current` is a cheap
//! projection of the active generation's snapshot vs. the repository's
//! current state (HEAD commit + dirty flag) — clean → current, dirty worktree
//! → stale, advanced HEAD → stale, re-index → current again. The flag rides
//! in `EnvelopeMeta` only; bundle DTO bytes are untouched (pinned in
//! ccc-context-compiler).

use ccc_service::{index_current, AppContext};
use std::path::{Path, PathBuf};

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Seeds a git repo from the fixture sources (mirrors the golden CLI test).
fn seed_repo(temp: &Path) -> PathBuf {
    let repo = temp.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/rust-sample");
    for rel in ["src/lib.rs", "src/util.rs"] {
        let from = fixture.join(rel);
        let to = repo.join(rel);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(&from, &to).unwrap();
    }
    git(&repo, &["init", "--initial-branch=main"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "test"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-m", "fixture base"]);
    repo
}

async fn setup(temp: &Path) -> AppContext {
    let data_dir = temp.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let _ = ccc_config::write_default_config(&data_dir).unwrap();
    let ctx = AppContext::open(&data_dir).await.unwrap();
    let repo_dir = seed_repo(temp);
    ccc_service::repo_add(&ctx, &repo_dir, None).await.unwrap();
    ccc_service::index_repo(&ctx, Some(&repo_dir), false, None)
        .await
        .unwrap();
    ctx
}

#[tokio::test]
async fn clean_worktree_reports_current_index() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = setup(temp.path()).await;
    let repo = ccc_service::resolve_repo(&ctx.store, Some(&temp.path().join("repo")))
        .await
        .unwrap();
    assert!(
        index_current(&ctx.store, &repo).await.unwrap(),
        "a freshly indexed clean worktree is current"
    );
}

#[tokio::test]
async fn dirty_worktree_reports_stale_index() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = setup(temp.path()).await;
    std::fs::write(
        temp.path().join("repo/src/util.rs"),
        "pub fn changed() {}\n",
    )
    .unwrap();
    let repo = ccc_service::resolve_repo(&ctx.store, Some(&temp.path().join("repo")))
        .await
        .unwrap();
    assert!(
        !index_current(&ctx.store, &repo).await.unwrap(),
        "a dirty worktree must not report a current index (FR-010)"
    );
}

#[tokio::test]
async fn advanced_head_reports_stale_index_until_reindexed() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = setup(temp.path()).await;
    let repo_dir = temp.path().join("repo");
    std::fs::write(repo_dir.join("src/util.rs"), "pub fn advanced() {}\n").unwrap();
    git(&repo_dir, &["add", "-A"]);
    git(&repo_dir, &["commit", "-m", "advance"]);
    let repo = ccc_service::resolve_repo(&ctx.store, Some(&repo_dir))
        .await
        .unwrap();
    assert!(
        !index_current(&ctx.store, &repo).await.unwrap(),
        "a commit-advanced HEAD must not report a current index (FR-010)"
    );
    ccc_service::index_repo(&ctx, Some(&repo_dir), false, None)
        .await
        .unwrap();
    assert!(
        index_current(&ctx.store, &repo).await.unwrap(),
        "re-indexing the advanced HEAD restores currency"
    );
}

#[tokio::test]
async fn repository_resolves_by_explicit_path_with_windows_prefix_normalized() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = setup(temp.path()).await;
    // Registration stores the root path without the Windows `//?/` prefix;
    // explicit-path resolution must apply the same normalization.
    let repo = ccc_service::resolve_repo(&ctx.store, Some(&temp.path().join("repo")))
        .await
        .unwrap();
    assert_eq!(
        repo.root_path
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_lowercase(),
        temp.path()
            .join("repo")
            .to_string_lossy()
            .replace('\\', "/")
            .trim_start_matches("//?/")
            .trim_end_matches('/')
            .to_lowercase(),
        "the resolved record is the registered repository"
    );
}

#[tokio::test]
async fn unindexed_repository_reports_not_current() {
    let temp = tempfile::tempdir().unwrap();
    let data_dir = temp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let _ = ccc_config::write_default_config(&data_dir).unwrap();
    let ctx = AppContext::open(&data_dir).await.unwrap();
    let repo_dir = seed_repo(temp.path());
    ccc_service::repo_add(&ctx, &repo_dir, None).await.unwrap();
    let repo = ccc_service::resolve_repo(&ctx.store, Some(&repo_dir))
        .await
        .unwrap();
    assert!(
        !index_current(&ctx.store, &repo).await.unwrap(),
        "no active generation → not current"
    );
}

#[test]
fn index_current_meta_is_additive_and_omitted_when_absent() {
    let meta = ccc_schema::EnvelopeMeta::default();
    let json = serde_json::to_string(&meta).unwrap();
    assert!(!json.contains("index_current"), "absent by default: {json}");
    let meta = ccc_schema::EnvelopeMeta {
        index_current: Some(true),
        ..Default::default()
    };
    let json = serde_json::to_string(&meta).unwrap();
    assert!(
        json.contains("\"index_current\":true"),
        "present when set: {json}"
    );
    let back: ccc_schema::EnvelopeMeta = serde_json::from_str(&json).unwrap();
    assert_eq!(back.index_current, Some(true));
}
