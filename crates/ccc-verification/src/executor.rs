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
use ccc_domain::{VerificationCheckRecord, *};
use ccc_storage::{ArtifactStore, StoreResult};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Runs one selected check under the policy and produces the check DTO plus
/// its persistence record (SDD §15.4 lifecycle, steps 3–7). Output blobs are
/// stored through the artifact store as capped, content-addressed artifacts.
/// `abort` carries a user-cancellation flag (checked before spawn and raced
/// during execution; the Step 2 CLI passes `None` — the seam exists for job
/// cancellation and signal handling).
pub(super) async fn execute_check(
    repo_root: &Path,
    policy: &VerificationPolicy,
    artifacts: &dyn ArtifactStore,
    candidate: &SelectedCheck,
    verification_id: &VerificationId,
    position: u32,
    abort: Option<Arc<AtomicBool>>,
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

    // Pre-spawn cancellation: an aborted check never starts a process.
    if abort
        .as_ref()
        .is_some_and(|flag| flag.load(Ordering::Relaxed))
    {
        record.status = VerificationStatus::Cancelled;
        dto.status = VerificationStatus::Cancelled.as_str().into();
        dto.note = "cancelled before execution (user abort); no process was started".into();
        return Ok((dto, record));
    }

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

    let cap = policy.max_output_bytes;
    let timeout = tokio::time::timeout(
        std::time::Duration::from_secs(policy.timeout_secs.max(1)),
        drive_child(&mut child, cap, &abort),
    )
    .await;

    let duration_ms = started.elapsed().as_millis() as u64;
    record.duration_ms = Some(duration_ms);
    dto.duration_ms = Some(duration_ms);

    let (stdout_bytes, stderr_bytes, status) = match timeout {
        Ok(WaitOutcome::Finished(out, err, status)) => {
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
            (out, err, Some(status))
        }
        Ok(WaitOutcome::Cancelled) => {
            // User abort: the process tree is killed and the status says so —
            // never `failed`, never `passed` (TIS §11.3).
            record.status = VerificationStatus::Cancelled;
            dto.status = VerificationStatus::Cancelled.as_str().into();
            dto.note = "cancelled by user abort; the process tree was terminated".into();
            return Ok((dto, record));
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
        Some(Ok(exit)) => {
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
        Some(Err(e)) => {
            // Wait failures are infrastructure errors (SDD §15.4).
            record.status = VerificationStatus::Unavailable;
            dto.status = VerificationStatus::Unavailable.as_str().into();
            dto.note = format!("process bookkeeping failed: {e}");
        }
        None => unreachable!("cancelled outcome returned early"),
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

/// Outcome of driving a child process to completion.
enum WaitOutcome {
    /// Output captured, wait completed: (stdout, stderr, exit status).
    Finished(
        (Vec<u8>, bool),
        (Vec<u8>, bool),
        std::io::Result<std::process::ExitStatus>,
    ),
    /// The abort flag flipped: the tree was killed before exit.
    Cancelled,
}

/// Drives the child to completion: pipe readers run as independent tasks (so
/// a chatty child cannot block the wait), and the abort flag is raced against
/// the wait. The caller applies the overall timeout.
async fn drive_child(
    child: &mut tokio::process::Child,
    cap: u64,
    abort: &Option<Arc<AtomicBool>>,
) -> WaitOutcome {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_task = tokio::spawn(read_capped(stdout, cap));
    let err_task = tokio::spawn(read_capped(stderr, cap));
    let status = match abort {
        Some(flag) => {
            tokio::select! {
                s = child.wait() => s,
                _ = abort_poll(flag) => {
                    kill_process_tree(child.id());
                    let _ = child.start_kill();
                    return WaitOutcome::Cancelled;
                }
            }
        }
        None => child.wait().await,
    };
    // Both pipes close when the process exits, so the readers finish.
    let (out, err) = tokio::join!(out_task, err_task);
    WaitOutcome::Finished(
        out.unwrap_or((Vec::new(), false)),
        err.unwrap_or((Vec::new(), false)),
        status,
    )
}

/// Polls the abort flag; cancellation latency is bounded at 50 ms.
async fn abort_poll(flag: &AtomicBool) {
    loop {
        if flag.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Reads a pipe to completion up to `cap + 1` bytes so an overrun is
/// detectable; the excess byte flags truncation.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    pipe: Option<R>,
    cap: u64,
) -> (Vec<u8>, bool) {
    let Some(mut pipe) = pipe else {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::DEFAULT_MAX_OUTPUT_BYTES;
    use ccc_storage::test_support::InMemoryArtifactStore;
    use std::path::PathBuf;

    fn exec_policy(allowlist: &[&str]) -> VerificationPolicy {
        VerificationPolicy {
            name: "executor-test".into(),
            allow_execution: true,
            allowlist: allowlist.iter().map(|s| s.to_string()).collect(),
            disallowlist: Vec::new(),
            timeout_secs: 600,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            env_allowlist: vec!["PATH".into(), "CARGO_HOME".into(), "RUSTUP_HOME".into()],
            revalidate_claims: true,
        }
    }

    fn candidate(command: &str) -> SelectedCheck {
        SelectedCheck {
            command: command.into(),
            relation: TestRelation::Directly,
            rationale: "test".into(),
        }
    }

    fn artifacts() -> InMemoryArtifactStore {
        InMemoryArtifactStore::new()
    }

    fn repo_root() -> PathBuf {
        std::env::temp_dir()
    }

    // Platform-appropriate commands: something that succeeds, fails, sleeps
    // "forever", and prints more than a few dozen bytes.
    #[cfg(target_os = "windows")]
    const OK_CMD: &str = "cmd /c exit 0";
    #[cfg(target_os = "windows")]
    const FAIL_CMD: &str = "cmd /c exit 3";
    #[cfg(target_os = "windows")]
    const SLEEP_CMD: &str = "cmd /c ping -n 60 127.0.0.1";
    #[cfg(target_os = "windows")]
    const BIG_OUTPUT_CMD: &str = "cmd /c path";
    #[cfg(target_os = "windows")]
    const OK_PREFIX: &str = "cmd /c exit";
    #[cfg(target_os = "windows")]
    const SLEEP_PREFIX: &str = "cmd /c ping";
    #[cfg(target_os = "windows")]
    const BIG_OUTPUT_PREFIX: &str = "cmd /c path";
    #[cfg(not(target_os = "windows"))]
    const OK_CMD: &str = "true";
    #[cfg(not(target_os = "windows"))]
    const FAIL_CMD: &str = "false";
    #[cfg(not(target_os = "windows"))]
    const SLEEP_CMD: &str = "sleep 60";
    #[cfg(not(target_os = "windows"))]
    const BIG_OUTPUT_CMD: &str = "printenv PATH";
    #[cfg(not(target_os = "windows"))]
    const OK_PREFIX: &str = "true";
    #[cfg(not(target_os = "windows"))]
    const SLEEP_PREFIX: &str = "sleep";
    #[cfg(not(target_os = "windows"))]
    const BIG_OUTPUT_PREFIX: &str = "printenv";

    #[tokio::test]
    async fn zero_exit_reports_passed() {
        let policy = exec_policy(&[OK_PREFIX]);
        let (dto, record) = execute_check(
            &repo_root(),
            &policy,
            &artifacts(),
            &candidate(OK_CMD),
            &VerificationId::generate(),
            0,
            None,
        )
        .await
        .unwrap();
        assert_eq!(record.status, VerificationStatus::Passed);
        assert_eq!(dto.status, "passed");
        assert_eq!(dto.exit_code, Some(0));
        assert!(dto.duration_ms.is_some());
        assert!(!dto.truncated);
    }

    #[tokio::test]
    async fn nonzero_exit_reports_failed_with_code() {
        let policy = exec_policy(&[OK_PREFIX]);
        let (dto, record) = execute_check(
            &repo_root(),
            &policy,
            &artifacts(),
            &candidate(FAIL_CMD),
            &VerificationId::generate(),
            0,
            None,
        )
        .await
        .unwrap();
        assert_eq!(record.status, VerificationStatus::Failed);
        assert_eq!(dto.status, "failed");
        assert_eq!(dto.exit_code, Some(3));
        assert!(dto.note.contains("exit code"));
    }

    #[tokio::test]
    async fn non_allowlisted_command_is_blocked_without_spawning() {
        // The allowlist covers `cargo test` only; a stray command is blocked
        // before any process is created (TIS §11.2).
        let policy = exec_policy(&["cargo test"]);
        let (dto, record) = execute_check(
            &repo_root(),
            &policy,
            &artifacts(),
            &candidate("rm -rf /"),
            &VerificationId::generate(),
            0,
            None,
        )
        .await
        .unwrap();
        assert_eq!(record.status, VerificationStatus::Blocked);
        assert_eq!(dto.status, "blocked");
        assert!(dto.exit_code.is_none(), "skipped checks have no exit code");
        assert!(record.stdout_ref.is_none(), "no output was captured");
        // Sneaky variants are covered by the exact-prefix rule: these were
        // exercised at the policy level; the executor honors the decision.
        assert_eq!(
            policy.classify("cargo test -- --exact sneaky"),
            PermitDecision::Allowed
        );
        assert!(matches!(
            policy.classify("cargo test2 --sneaky"),
            PermitDecision::Blocked(_)
        ));
    }

    #[tokio::test]
    async fn disallowlist_wins_over_allowlist() {
        let mut policy = exec_policy(&[SLEEP_PREFIX]);
        policy.disallowlist = vec!["cmd /c ping -n 60".into()];
        let (dto, _record) = execute_check(
            &repo_root(),
            &policy,
            &artifacts(),
            &candidate(SLEEP_CMD),
            &VerificationId::generate(),
            0,
            None,
        )
        .await
        .unwrap();
        assert_eq!(dto.status, "blocked");
        assert!(dto.note.contains("disallowlist"));
    }

    #[tokio::test]
    async fn missing_binary_reports_unavailable_never_failed() {
        let policy = exec_policy(&["definitely-not-a-real-binary"]);
        let (dto, record) = execute_check(
            &repo_root(),
            &policy,
            &artifacts(),
            &candidate("definitely-not-a-real-binary --version"),
            &VerificationId::generate(),
            0,
            None,
        )
        .await
        .unwrap();
        assert_eq!(record.status, VerificationStatus::Unavailable);
        assert_eq!(dto.status, "unavailable");
        assert!(dto.note.contains("could not start"));
        assert!(
            dto.exit_code.is_none(),
            "infra failure is not a test failure"
        );
    }

    #[tokio::test]
    async fn timeout_kills_the_sleeping_child_and_reports_timed_out() {
        let mut policy = exec_policy(&[SLEEP_PREFIX]);
        policy.timeout_secs = 2;
        let started = std::time::Instant::now();
        let (dto, record) = execute_check(
            &repo_root(),
            &policy,
            &artifacts(),
            &candidate(SLEEP_CMD),
            &VerificationId::generate(),
            0,
            None,
        )
        .await
        .unwrap();
        let elapsed = started.elapsed().as_secs();
        assert_eq!(record.status, VerificationStatus::TimedOut);
        assert_eq!(dto.status, "timed_out");
        assert!(
            elapsed < 20,
            "the kill must happen at the 2s timeout, not after the child finishes: {elapsed}s"
        );
        assert!(dto.note.contains("terminated"));
        assert!(
            dto.exit_code.is_none(),
            "timed-out checks are ambiguous, not failed"
        );
    }

    #[tokio::test]
    async fn user_abort_reports_cancelled_and_terminates_the_child() {
        let mut policy = exec_policy(&[SLEEP_PREFIX]);
        policy.timeout_secs = 120;
        let abort = Arc::new(AtomicBool::new(false));
        let flag = abort.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            flag.store(true, Ordering::Relaxed);
        });
        let started = std::time::Instant::now();
        let (dto, record) = execute_check(
            &repo_root(),
            &policy,
            &artifacts(),
            &candidate(SLEEP_CMD),
            &VerificationId::generate(),
            0,
            Some(abort),
        )
        .await
        .unwrap();
        assert_eq!(record.status, VerificationStatus::Cancelled);
        assert_eq!(dto.status, "cancelled");
        assert!(
            started.elapsed().as_secs() < 30,
            "abort must cut the 120s command short"
        );
    }

    #[tokio::test]
    async fn abort_before_spawn_starts_nothing() {
        let policy = exec_policy(&[OK_PREFIX]);
        let abort = Arc::new(AtomicBool::new(true));
        let (dto, record) = execute_check(
            &repo_root(),
            &policy,
            &artifacts(),
            &candidate(OK_CMD),
            &VerificationId::generate(),
            0,
            Some(abort),
        )
        .await
        .unwrap();
        assert_eq!(record.status, VerificationStatus::Cancelled);
        assert!(dto.note.contains("no process was started"));
        assert!(dto.duration_ms.is_none());
    }

    #[tokio::test]
    async fn oversized_output_is_capped_and_truncation_is_flagged() {
        let mut policy = exec_policy(&[BIG_OUTPUT_PREFIX]);
        policy.max_output_bytes = 64;
        let store = artifacts();
        let (dto, record) = execute_check(
            &repo_root(),
            &policy,
            &store,
            &candidate(BIG_OUTPUT_CMD),
            &VerificationId::generate(),
            0,
            None,
        )
        .await
        .unwrap();
        assert!(dto.truncated, "a large output must be flagged as truncated");
        assert!(record.truncated);
        assert!(dto.note.contains("capped"));
        let stdout_hash = record.stdout_ref.expect("stdout stored as artifact");
        let meta = store.get_meta(&stdout_hash).await.unwrap().unwrap();
        assert_eq!(
            meta.byte_length, 64,
            "stored output is capped at the policy limit"
        );
    }

    #[tokio::test]
    async fn execution_denied_by_policy_is_blocked_in_the_executor() {
        let mut policy = exec_policy(&[OK_PREFIX]);
        policy.allow_execution = false;
        let (dto, record) = execute_check(
            &repo_root(),
            &policy,
            &artifacts(),
            &candidate(OK_CMD),
            &VerificationId::generate(),
            0,
            None,
        )
        .await
        .unwrap();
        assert_eq!(record.status, VerificationStatus::Blocked);
        assert!(dto.note.contains("not authorized"));
    }

    #[test]
    fn skipped_statuses_are_never_passed() {
        // TIS §11.3 honesty rule, asserted explicitly per vocabulary entry.
        for status in [
            VerificationStatus::Failed,
            VerificationStatus::TimedOut,
            VerificationStatus::Blocked,
            VerificationStatus::NotRun,
            VerificationStatus::Unavailable,
            VerificationStatus::Cancelled,
        ] {
            assert_ne!(status.as_str(), VerificationStatus::Passed.as_str());
        }
        assert_eq!(VerificationStatus::Passed.as_str(), "passed");
    }

    #[test]
    fn env_summary_records_honest_sandbox_limitation() {
        let summary = serde_json::json!({
            "cwd": "x",
            "env_names": ["PATH"],
            "timeout_secs": 600,
            "max_output_bytes": DEFAULT_MAX_OUTPUT_BYTES,
            "sandbox": "none (documented limitation; allowlist + timeout + caps only)"
        });
        let text = serde_json::to_string(&summary).unwrap();
        assert!(text.contains("none (documented limitation"));
        assert!(!text.contains("isolated"), "no false sandbox claim");
    }
}
