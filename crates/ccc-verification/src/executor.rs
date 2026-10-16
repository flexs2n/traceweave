//! Authorized check execution (SDD §15.4, TIS §11.1 steps 6–7).
//!
//! Threat-model notes (TIS §17.2): the command text comes from ccc's own test
//! selection, never from repository content; policy comes only from the user
//! data dir. Every command is classified against the policy's exact-prefix
//! argv allowlist before any process is created. The executed environment is
//! minimal (allowlisted names only), cwd is pinned to the repository root,
//! output is size-capped and stored as artifacts, and a timeout kills the
//! whole process tree.
//!
//! **Honest limitation (recorded in every check's environment summary):**
//! there is no OS-level sandbox in this step — no filesystem or network
//! isolation is claimed. The controls here are allowlisting, timeout +
//! process-tree kill, output caps, a minimal environment, and cwd pinning.

use crate::policy::{PermitDecision, VerificationPolicy};
use crate::select::SelectedCheck;
use crate::VerificationCheckDto;
use ccc_domain::VerificationCheckRecord;
use ccc_domain::*;
use ccc_storage::{ArtifactStore, StoreResult};
use std::path::Path;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Runs one selected check under the policy and produces the check DTO plus
/// its persistence record (SDD §15.4 lifecycle, steps 3–7). Output blobs are
/// stored through the artifact store as capped, content-addressed artifacts.
pub(super) async fn execute_check(
    repo_root: &Path,
    policy: &VerificationPolicy,
    artifacts: &dyn ArtifactStore,
    candidate: &SelectedCheck,
    verification_id: &VerificationId,
    position: u32,
) -> StoreResult<(VerificationCheckDto, VerificationCheckRecord)> {
    let check_id = VerificationId::generate();
    let mut dto = VerificationCheckDto {
        check_id: check_id.to_string(),
        position,
        command: candidate.command.clone(),
        relation: candidate.relation.as_str().into(),
        status: VerificationStatus::NotRun.as_str().into(),
        exit_code: None,
        duration_ms: None,
        truncated: false,
        note: String::new(),
    };
    let mut record = VerificationCheckRecord {
        check_id,
        verification_id: *verification_id,
        position,
        suggested_command: candidate.command.clone(),
        relation: candidate.relation.as_str().into(),
        status: VerificationStatus::NotRun,
        exit_code: None,
        duration_ms: None,
        stdout_ref: None,
        stderr_ref: None,
        truncated: false,
        env_summary_json: "{}".into(),
    };

    // Policy gate (TIS §11.2): the disallowlist wins; the allowlist must
    // cover the command as an exact prefix. Blocked checks never spawn.
    if !policy.allow_execution {
        record.status = VerificationStatus::Blocked;
        dto.status = VerificationStatus::Blocked.as_str().into();
        dto.note = "execution is not authorized by policy".into();
        return Ok((dto, record));
    }
    if let PermitDecision::Blocked(reason) = policy.classify(&candidate.command) {
        record.status = VerificationStatus::Blocked;
        dto.status = VerificationStatus::Blocked.as_str().into();
        dto.note = reason;
        return Ok((dto, record));
    }

    // Spawn the approved command. Arguments are split on whitespace as the
    // policy prefix form implies; no shell is involved (literal argv only).
    let mut parts = candidate.command.split_whitespace();
    let program = parts.next().unwrap_or_default().to_string();
    let args: Vec<String> = parts.map(str::to_string).collect();

    let mut command = Command::new(&program);
    command
        .args(&args)
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Minimal environment: only allowlisted variable names pass through.
    for name in &policy.env_allowlist {
        if let Ok(value) = std::env::var(name) {
            command.env(name, value);
        }
    }
    // `SystemRoot` is required by many Windows processes; it is operational
    // plumbing rather than a policy decision, so it is added unconditionally.
    #[cfg(target_os = "windows")]
    if let Ok(value) = std::env::var("SystemRoot") {
        command.env("SystemRoot", value);
    }

    let started = std::time::Instant::now();
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            // Missing binary / PATH problems are infrastructure, not test
            // failures (SDD §15.4: distinguish the cause).
            record.status = VerificationStatus::Unavailable;
            dto.status = VerificationStatus::Unavailable.as_str().into();
            dto.note = format!("could not start {program:?}: {e}");
            return Ok((dto, record));
        }
    };

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let cap = policy.max_output_bytes;
    let read_stdout = read_capped(stdout_pipe.as_mut(), cap);
    let read_stderr = read_capped(stderr_pipe.as_mut(), cap);
    let wait = child.wait();
    let timeout = tokio::time::timeout(
        std::time::Duration::from_secs(policy.timeout_secs.max(1)),
        async { tokio::join!(read_stdout, read_stderr, wait) },
    )
    .await;

    let duration_ms = started.elapsed().as_millis() as u64;
    record.duration_ms = Some(duration_ms);
    dto.duration_ms = Some(duration_ms);

    let (stdout_bytes, stderr_bytes, status) = match timeout {
        Ok((out, err, status)) => {
            let (out, out_trunc) = out;
            let (err, err_trunc) = err;
            record.truncated = out_trunc || err_trunc;
            dto.truncated = record.truncated;
            if record.truncated {
                dto.note = format!(
                    "output capped at {} bytes; truncation is flagged, never silent",
                    policy.max_output_bytes
                );
            }
            (out, err, status)
        }
        Err(_elapsed) => {
            // Timeout: kill the process tree and report `timed_out` — never
            // `failed` (TIS §11.3, SDD §15.4).
            kill_process_tree(child.id());
            let _ = child.start_kill();
            record.status = VerificationStatus::TimedOut;
            dto.status = VerificationStatus::TimedOut.as_str().into();
            dto.note = format!(
                "killed after {}s (policy timeout); the process tree was terminated",
                policy.timeout_secs
            );
            return Ok((dto, record));
        }
    };

    // Output lives as capped blob artifacts (refs, never inline).
    if !stdout_bytes.is_empty() {
        let hash = artifacts
            .put(&stdout_bytes, "text/plain; charset=utf-8")
            .await?;
        record.stdout_ref = Some(hash);
    }
    if !stderr_bytes.is_empty() {
        let hash = artifacts
            .put(&stderr_bytes, "text/plain; charset=utf-8")
            .await?;
        record.stderr_ref = Some(hash);
    }

    match status {
        Ok(exit) => {
            let code = exit.code();
            record.exit_code = code.map(i64::from);
            dto.exit_code = record.exit_code;
            if exit.success() {
                record.status = VerificationStatus::Passed;
                dto.status = VerificationStatus::Passed.as_str().into();
            } else {
                record.status = VerificationStatus::Failed;
                dto.status = VerificationStatus::Failed.as_str().into();
                dto.note = format!("non-zero exit code {}", code.unwrap_or(-1));
            }
        }
        Err(e) => {
            // Wait failures are infrastructure errors (SDD §15.4).
            record.status = VerificationStatus::Unavailable;
            dto.status = VerificationStatus::Unavailable.as_str().into();
            dto.note = format!("process bookkeeping failed: {e}");
        }
    }

    let env_summary = serde_json::json!({
        "cwd": repo_root.to_string_lossy(),
        "env_names": policy.env_allowlist,
        "timeout_secs": policy.timeout_secs,
        "max_output_bytes": policy.max_output_bytes,
        "sandbox": "none (documented limitation; allowlist + timeout + caps only)"
    });
    record.env_summary_json = serde_json::to_string(&env_summary).unwrap_or_else(|_| "{}".into());

    Ok((dto, record))
}

/// Reads a pipe up to `cap + 1` bytes so an overrun is detectable; the excess
/// byte flags truncation.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    pipe: Option<&mut R>,
    cap: u64,
) -> (Vec<u8>, bool) {
    let Some(pipe) = pipe else {
        return (Vec::new(), false);
    };
    let limit = cap.saturating_add(1) as usize;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                let remaining = limit.saturating_sub(buf.len());
                let take = n.min(remaining);
                buf.extend_from_slice(&chunk[..take]);
                if buf.len() >= limit {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let truncated = buf.len() as u64 > cap;
    buf.truncate(cap as usize);
    (buf, truncated)
}

/// Kills the whole process tree. On Windows `taskkill /T /F` removes spawned
/// grandchildren; elsewhere the direct child is signalled (tokio's
/// `kill_on_drop` also covers the direct child on drop).
fn kill_process_tree(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .output();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .output();
    }
}
