//! Verification under execution policies + policy inspection (TIS §11, §13.4).

use crate::{meta_for, output, output_envelope_as_human, resolve_repo, AppContext, OpOutput};
use ccc_domain::CccError;
use ccc_jobs as jobs;
use ccc_repository::{GitCli, GitProvider};
use std::path::PathBuf;

/// The verify idempotency key: same repository + patch + policy replays one
/// logical job. Single source for the synchronous op and the async submit
/// path (T4).
pub fn verify_job_key(
    data_dir: &std::path::Path,
    repo: &ccc_domain::RepositoryRecord,
    base: &str,
    head: Option<&str>,
    policy_name: &str,
) -> Result<String, CccError> {
    let policy = ccc_verification::load_policy(data_dir, policy_name)?;
    let root = PathBuf::from(&repo.root_path);
    let head_commit = GitCli.rev_parse_head(&root)?.unwrap_or_default();
    let dirty = !GitCli.status_porcelain(&root)?.is_empty();
    Ok(jobs::idempotency_key(&[
        "verify",
        &repo.repository_id.to_string(),
        base,
        head.unwrap_or("working_tree"),
        policy_name,
        &policy.hash(),
        &head_commit,
        &format!("{dirty}"),
    ]))
}

#[allow(clippy::too_many_arguments)]
pub async fn verify(
    ctx: &AppContext,
    repo_path: Option<&std::path::Path>,
    base: &str,
    head: Option<&str>,
    policy_name: &str,
    format: &str,
) -> Result<OpOutput, CccError> {
    let store = &ctx.store;
    let repo = resolve_repo(store, repo_path).await?;
    verify_record(ctx, &repo, base, head, policy_name, format, None).await
}

/// The verify operation against an already-resolved repository record (the
/// T4 job runner resolves once, then executes). `external_key` (an
/// `Idempotency-Key` header) replaces the derived key when present.
#[allow(clippy::too_many_arguments)]
pub async fn verify_record(
    ctx: &AppContext,
    repo: &ccc_domain::RepositoryRecord,
    base: &str,
    head: Option<&str>,
    policy_name: &str,
    format: &str,
    external_key: Option<&str>,
) -> Result<OpOutput, CccError> {
    let store = &ctx.store;
    let artifacts = &*ctx.artifacts;
    let data_dir = &ctx.data_dir;
    let root = PathBuf::from(&repo.root_path);
    let cfg = ccc_config::load_config(data_dir)?;
    // Policies come only from the data dir; an unknown name is INVALID_REQUEST
    // (no silent fallback, no false availability).
    let policy = ccc_verification::load_policy(data_dir, policy_name)?;
    let key = match external_key {
        Some(k) => k.to_string(),
        None => verify_job_key(data_dir, repo, base, head, policy_name)?,
    };
    let job = jobs::begin_job(&**store, repo.repository_id, "verify", &key, None).await?;

    let engine = ccc_verification::VerificationEngine::new(
        &**store,
        &**store,
        &**store,
        &**store,
        &**store,
        artifacts,
        &GitCli,
        cfg.indexing().traversal_depth,
    );
    match engine.verify(repo, &root, base, head, &policy).await {
        Ok(report) => {
            let payload = serde_json::json!({
                "verification_id": report.verification_id,
                "statuses": report.statuses,
            });
            let _job = if job.state.is_terminal() {
                // Idempotent replay: the logical job already completed.
                job
            } else {
                jobs::finish_success(&**store, job, Some(&payload.to_string())).await?
            };
            let out = if format == "markdown" {
                let human = ccc_verification::render_verification_markdown(&report);
                output(&report, meta_for(repo, None, None), human)?
            } else {
                output_envelope_as_human(&report, meta_for(repo, None, None))?
            };
            Ok(out)
        }
        Err(e) => {
            jobs::finish_failure(&**store, job, &e).await?;
            Err(e)
        }
    }
}

pub async fn policies_list(data_dir: &std::path::Path) -> Result<OpOutput, CccError> {
    let mut policies = Vec::new();
    for name in ccc_verification::list_policy_names(data_dir) {
        // Every listed policy is loadable by construction; load it to surface
        // its hash and execution posture (no false availability).
        let policy = ccc_verification::load_policy(data_dir, &name)?;
        policies.push(serde_json::json!({
            "name": policy.name,
            "allow_execution": policy.allow_execution,
            "policy_hash": policy.hash(),
        }));
    }
    let mut human = String::new();
    if policies.len() == 1 {
        human.push_str("1 policy available (source: data dir; repository content can never define policies):\n");
    } else {
        human.push_str(&format!(
            "{} policies available (source: data dir; repository content can never define policies):\n",
            policies.len()
        ));
    }
    for p in &policies {
        human.push_str(&format!(
            "{} — execution: {}, hash: {}\n",
            p["name"].as_str().unwrap_or_default(),
            if p["allow_execution"].as_bool().unwrap_or(false) {
                "allowed (allowlisted commands only)"
            } else {
                "not allowed (analysis-only)"
            },
            p["policy_hash"].as_str().unwrap_or_default()
        ));
    }
    output(&policies, ccc_schema::EnvelopeMeta::default(), human)
}

pub async fn policies_show(data_dir: &std::path::Path, name: &str) -> Result<OpOutput, CccError> {
    let policy = ccc_verification::load_policy(data_dir, name)?;
    let doc = serde_json::json!({
        "name": policy.name,
        "allow_execution": policy.allow_execution,
        "allowlist": policy.allowlist,
        "disallowlist": policy.disallowlist,
        "timeout_secs": policy.timeout_secs,
        "max_output_bytes": policy.max_output_bytes,
        "env_allowlist": policy.env_allowlist,
        "revalidate_claims": policy.revalidate_claims,
        "policy_hash": policy.hash(),
        "source": data_dir.join("policies").join(format!("{name}.toml")).to_string_lossy(),
    });
    let human = format!(
        "Policy {} — execution: {}, hash: {}\n",
        policy.name,
        if policy.allow_execution {
            "allowed (allowlisted commands only)"
        } else {
            "not allowed (analysis-only)"
        },
        policy.hash()
    );
    let human = format!(
        "{human}  allowlist: {:?}\n  disallowlist: {:?}\n  timeout: {}s, output cap: {} bytes\n  env allowlist: {:?}\n  claim revalidation: {}\n",
        policy.allowlist,
        policy.disallowlist,
        policy.timeout_secs,
        policy.max_output_bytes,
        policy.env_allowlist,
        if policy.revalidate_claims {
            "enabled"
        } else {
            "disabled"
        }
    );
    output(&doc, ccc_schema::EnvelopeMeta::default(), human)
}
