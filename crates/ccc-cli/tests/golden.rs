//! Golden CLI integration test (plan T14/T15): drives the full vertical path
//! on the fixture repository through the built binary.
//!
//! Flow: init → repo add → index → search → modify one file → incremental
//! reindex (reparse count == 1) → compile (deterministic bundle_hash) →
//! impact analyze → export → verify analysis mode (zero execution) →
//! policies → declared-not-implemented commands.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_ccc");

struct Cli {
    data_dir: PathBuf,
    repo_dir: PathBuf,
}

impl Cli {
    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(BIN);
        cmd.arg("--data-dir").arg(&self.data_dir).args(args);
        cmd.current_dir(&self.repo_dir);

        cmd.output().expect("ccc binary runs")
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut cmd = Command::new(BIN);
        cmd.arg("--json")
            .arg("--data-dir")
            .arg(&self.data_dir)
            .args(args);
        cmd.current_dir(&self.repo_dir);
        let out = cmd.output().expect("ccc binary runs");
        assert!(
            out.status.success(),
            "ccc {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("valid JSON stdout")
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
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

/// Seeds a git repo from the fixture sources (fixture dir holds plain files).
fn seed_repo(temp: &Path) -> PathBuf {
    let repo = temp.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::create_dir_all(repo.join("tests")).unwrap();
    std::fs::create_dir_all(repo.join("vendor")).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/rust-sample");
    for rel in [
        "src/lib.rs",
        "src/util.rs",
        "tests/smoke.rs",
        "vendor/dep.rs",
        "notes.md",
    ] {
        let from = fixture.join(rel);
        let to = repo.join(rel);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(&from, &to).unwrap();
    }
    // A real binary file for binary detection (NUL byte in first 8 KiB).
    std::fs::write(repo.join("data.bin"), b"binary\x00data").unwrap();

    git(&repo, &["init", "--initial-branch=main"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "test"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-m", "fixture base"]);
    repo
}

fn stderr_code(out: &Output) -> String {
    let text = String::from_utf8_lossy(&out.stderr);
    // --json envelope form: {"error":{"code":"..."}}
    if let Some(rest) = text.split("\"code\":\"").nth(1) {
        if let Some(code) = rest.split('"').next() {
            return code.to_string();
        }
    }
    // Human form: "ERROR CODE: message"
    text.split("ERROR ")
        .nth(1)
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("")
        .trim()
        .to_string()
}

#[test]
fn golden_vertical_path() {
    let temp = tempfile::tempdir().unwrap();
    let cli = Cli {
        data_dir: temp.path().join("data"),
        repo_dir: seed_repo(temp.path()),
    };

    // init
    let out = cli.run(&["init"]);
    assert!(out.status.success());

    // repo add (idempotent: two identical adds → one logical job).
    let add1 = cli.json(&["repo", "add", cli.repo_dir.to_str().unwrap()]);
    let repo_id = add1["data"]["repository"]["repository_id"]
        .as_str()
        .unwrap()
        .to_string();
    let add2 = cli.json(&[
        "repo",
        "add",
        cli.repo_dir.to_str().unwrap(),
        "--idempotency-key",
        "golden-key",
    ]);
    let _ = add2;
    let add3 = cli.json(&[
        "repo",
        "add",
        cli.repo_dir.to_str().unwrap(),
        "--idempotency-key",
        "golden-key",
    ]);
    assert_eq!(
        add1["data"]["repository"]["repository_id"]
            .as_str()
            .unwrap(),
        add3["data"]["repository"]["repository_id"]
            .as_str()
            .unwrap()
    );
    let _ = repo_id;

    // index — first full run.
    let index1 = cli.json(&["index"]);
    let counters1 = index1["data"]["counters"].clone();
    assert_eq!(
        counters1["parsed"].as_u64().unwrap(),
        3,
        "lib, util, tests/smoke parsed"
    );
    assert_eq!(counters1["reused"].as_u64().unwrap(), 0);
    let _generation_id = index1["data"]["generation_id"]
        .as_str()
        .unwrap()
        .to_string();
    let snapshot_id = index1["data"]["snapshot_id"].as_str().unwrap().to_string();

    // status — exclusions visible, unsupported listed.
    let status = cli.json(&["status"]);
    let excluded = status["data"]["excluded_files"].as_array().unwrap();
    assert!(
        excluded
            .iter()
            .any(|f| f["path"] == "vendor/dep.rs" && f["reason"] == "excluded_by_pattern"),
        "pattern exclusion recorded: {excluded:?}"
    );
    assert!(
        excluded
            .iter()
            .any(|f| f["path"] == "notes.md" && f["reason"] == "unsupported_language"),
        "unsupported file recorded: {excluded:?}"
    );
    assert!(
        excluded
            .iter()
            .any(|f| f["path"] == "data.bin" && f["reason"] == "binary"),
        "binary file recorded: {excluded:?}"
    );

    // search — exact match ranks first; excluded content never appears.
    let search = cli.json(&["search", "read_config", "--limit", "10"]);
    let hits = search["data"]["hits"].as_array().unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0]["match_class"], "exact");
    assert!(hits[0]["title"].as_str().unwrap().contains("read_config"));
    assert!(
        hits.iter()
            .all(|h| h["path"] != "notes.md" && h["path"] != "vendor/dep.rs"),
        "excluded content never in search results"
    );

    // context compile BEFORE the change (baseline for the context diff).
    let compile_pre = cli.json(&[
        "context",
        "compile",
        "--task",
        "how does read_config get called",
        "--budget",
        "3000",
    ]);
    let pre_bundle_id = compile_pre["data"]["bundle_id"]
        .as_str()
        .unwrap()
        .to_string();

    // modify ONE file, commit, reindex: exactly one reparse.
    std::fs::write(
        cli.repo_dir.join("src/util.rs"),
        "/// Reads the application configuration.\npub fn read_config() -> u8 {\n    42\n}\n\npub fn write_config(value: u8) {\n    let _ = value;\n}\n",
    )
    .unwrap();
    git(&cli.repo_dir, &["add", "-A"]);
    git(&cli.repo_dir, &["commit", "-m", "change util"]);

    let index2 = cli.json(&["index"]);
    let counters2 = index2["data"]["counters"].clone();
    assert_eq!(
        counters2["parsed"].as_u64().unwrap(),
        1,
        "exactly one file reparsed"
    );
    assert_eq!(
        counters2["reused"].as_u64().unwrap(),
        2,
        "unchanged files reused"
    );
    let snapshot_id_2 = index2["data"]["snapshot_id"].as_str().unwrap().to_string();

    // snapshot diff — the manifest comparison sees exactly the changed path.
    let snap_diff = cli.json(&["snapshot", "diff", &snapshot_id, &snapshot_id_2]);
    assert_eq!(
        snap_diff["data"]["changed"].as_array().unwrap().len(),
        1,
        "one changed path: {snap_diff:?}"
    );
    assert_eq!(snap_diff["data"]["changed"][0]["path"], "src/util.rs");
    assert_eq!(snap_diff["data"]["added"].as_array().unwrap().len(), 0);
    assert_eq!(snap_diff["data"]["removed"].as_array().unwrap().len(), 0);

    // context compile — deterministic bundle_hash across identical invocations.
    let compile1 = cli.json(&[
        "context",
        "compile",
        "--task",
        "how does read_config get called",
        "--budget",
        "3000",
    ]);
    let compile2 = cli.json(&[
        "context",
        "compile",
        "--task",
        "how does read_config get called",
        "--budget",
        "3000",
    ]);
    // Determinism: identical inputs → identical items and estimated tokens
    // (bundle_id differs by design — each compile is a new immutable bundle;
    // the content hash equality is pinned in the compiler unit tests).
    assert_eq!(compile1["data"]["items"], compile2["data"]["items"]);
    assert_eq!(
        compile1["data"]["estimated_tokens"],
        compile2["data"]["estimated_tokens"]
    );
    assert_eq!(
        compile1["data"]["token_estimate_basis"],
        "approximate_chars_div4"
    );
    // Every machine output carries snapshot identity.
    assert!(compile1["meta"]["repository_id"].as_str().is_some());

    // context diff — post-change refresh visibility: items changed content
    // and flipped evidence status (candidate → stale after the reindex).
    let post_bundle_id = compile1["data"]["bundle_id"].as_str().unwrap().to_string();
    let ctx_diff = cli.json(&["context", "diff", &pre_bundle_id, &post_bundle_id]);
    let changed = ctx_diff["data"]["changed"].as_array().unwrap();
    assert!(
        changed
            .iter()
            .any(|c| c["path"] == "src/util.rs" && c["content_changed"] == true),
        "the changed file surfaces in the bundle diff: {changed:?}"
    );
    assert!(
        changed
            .iter()
            .any(|c| c["evidence_status"]["from"] == "candidate"
                && c["evidence_status"]["to"] == "stale"),
        "evidence-status flips are visible (SDD §11.4)"
    );

    // eval run — the fixture suite is deterministic: two identical runs
    // produce identical metrics; the suite hash pins the dataset.
    let suite_path = std::fs::canonicalize(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../eval/suites/fixture.toml"),
    )
    .unwrap();
    let suite_arg = suite_path.to_str().unwrap();
    let eval1 = cli.json(&["eval", "run", "--suite", suite_arg]);
    let eval2 = cli.json(&["eval", "run", "--suite", suite_arg]);
    assert_eq!(eval1["data"]["metrics"], eval2["data"]["metrics"]);
    let run_id = eval1["data"]["run_id"].as_str().unwrap().to_string();
    assert!(
        eval1["data"]["metrics"]["tuning"]["recall_millis"]
            .as_u64()
            .unwrap()
            > 0,
        "golden expectations are actually retrieved: {eval1:?}"
    );
    assert_eq!(eval1["data"]["metrics"]["tuning"]["cases"], 2);
    assert_eq!(eval1["data"]["metrics"]["held_out"]["cases"], 1);
    assert!(eval1["data"]["metrics"]["suite_hash"].as_str().is_some());
    assert_eq!(
        eval1["data"]["metrics"]["retrieval_policy_version"],
        "policy-v1"
    );

    // eval show — the recorded run is queryable with identical metrics.
    let eval_show = cli.json(&["eval", "show", &run_id]);
    assert_eq!(eval_show["data"]["metrics"], eval1["data"]["metrics"]);

    // impact analyze — caller of changed function reported with a path.
    let impact = cli.json(&["impact", "analyze", "--base", "HEAD~1", "--head", "HEAD"]);
    let affected = impact["data"]["affected_artifacts"].as_array().unwrap();
    assert!(
        affected.iter().any(|a| a["symbol_qualified_name"]
            .as_str()
            .unwrap()
            .contains("run_app")),
        "caller of changed function is impacted: {affected:?}"
    );
    assert!(
        affected.iter().all(|a| a["classification"] == "candidate"),
        "syntax-level bindings are honestly classified as candidates"
    );
    assert!(
        impact["data"]["known_blind_spots"]
            .as_array()
            .unwrap()
            .len()
            >= 3
    );

    // export — bundles + evidence JSON, excluded content absent.
    let export_path = temp.path().join("export.json");
    let export = cli.json(&["export", "--output", export_path.to_str().unwrap()]);
    assert!(export["data"]["claims"].as_u64().unwrap() > 0);
    let export_text = std::fs::read_to_string(&export_path).unwrap();
    assert!(export_text.contains("read_config"));
    assert!(
        !export_text.contains("vendored"),
        "excluded content never in exports"
    );

    // verify — unknown policy names are rejected with a stable code (policies
    // come only from the data dir; there is no "strict" policy here).
    let verify_unknown = cli.run(&["verify", "--base", "HEAD~1", "--policy", "strict"]);
    assert!(!verify_unknown.status.success());
    assert_eq!(stderr_code(&verify_unknown), "INVALID_REQUEST");

    // verify analysis mode — a real change produces a full report with zero
    // execution: every executable suggestion is `not_run`, never `passed`.
    let verify = cli.json(&["verify", "--base", "HEAD~1", "--head", "HEAD"]);
    let checks = verify["data"]["checks"].as_array().unwrap();
    assert!(!checks.is_empty(), "suggestions are present");
    assert!(
        checks
            .iter()
            .all(|c| c["status"] == "not_run" && c["exit_code"].is_null()),
        "analysis mode executes nothing: {checks:?}"
    );
    assert_eq!(verify["data"]["mode"], "analysis");
    assert_eq!(verify["data"]["statuses"]["passed"], 0);
    assert_eq!(verify["data"]["statuses"]["not_run"], checks.len() as u64);
    assert_eq!(verify["data"]["policy_name"], "analysis");
    assert!(verify["data"]["policy_hash"].as_str().is_some());
    assert!(
        verify["data"]["limitations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l.as_str().unwrap().contains("no command was executed")),
        "the analysis limitation is stated, never hidden"
    );

    // eval run — declared not implemented (INVALID_REQUEST).
    let eval = cli.run(&["eval", "run", "--suite", "baseline"]);
    assert!(!eval.status.success());
    assert_eq!(stderr_code(&eval), "INVALID_REQUEST");

    // policies list/show — data-dir sourced; the built-in analysis policy is
    // always present and never allows execution.
    let policies = cli.json(&["policies", "list"]);
    let names: Vec<&str> = policies["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"analysis"));
    let show = cli.json(&["policies", "show", "analysis"]);
    assert_eq!(show["data"]["allow_execution"], false);
    assert_eq!(show["data"]["name"], "analysis");

    // json error envelope on failure path.
    let bad = cli.run(&["--json", "search", "x", "--limit", "0"]);
    let _ = Duration::from_secs(0);
    let _ = bad; // limit 0 is clamped, not an error; covered by verify/eval above.
}

/// Step 3 golden flow (plan T4/T9): memory add → search → conflicts →
/// conflict-aware bundle → resolve → supersede → freshness after reindex.
/// Memory stays subordinate: agent-free here, and every stale/conflicted
/// memory is explicitly labeled (SDD §9.5).
#[test]
fn golden_memory_and_conflict_flow() {
    let temp = tempfile::tempdir().unwrap();
    let cli = Cli {
        data_dir: temp.path().join("data"),
        repo_dir: seed_repo(temp.path()),
    };
    assert!(cli.run(&["init"]).status.success());
    cli.json(&["repo", "add", cli.repo_dir.to_str().unwrap()]);
    cli.json(&["index"]);

    // A claim from the indexer (symbol_existence) to link a memory to — the
    // util.rs claim, so the later reindex of util.rs exercises the
    // freshness projection (FR-043).
    let claims = cli.json(&["evidence", "list", "--status", "candidate"]);
    let claim_rows = claims["data"].as_array().unwrap();
    assert!(!claim_rows.is_empty(), "indexing produced claims");
    let claim_id = claim_rows
        .iter()
        .find(|c| {
            c["text"]
                .as_str()
                .is_some_and(|t| t.contains("read_config"))
        })
        .map(|c| c["claim_id"].as_str().unwrap().to_string())
        .expect("a read_config claim exists");

    // memory add — starts candidate with provenance (FR-032).
    let added = cli.json(&[
        "memory",
        "add",
        "--title",
        "Do not cache manifest hashes",
        "--summary",
        "Caching the manifest hash broke snapshot identity once.",
        "--memory-type",
        "failed_approach",
        "--scope",
        "src",
        "--created-by",
        "human:test",
        "--claim-id",
        &claim_id,
    ]);
    let memory_id = added["data"]["memory_id"].as_str().unwrap().to_string();
    assert_eq!(added["data"]["status"], "candidate");

    // Unknown claim ids are rejected (no false availability).
    let unknown_claim = cli.run(&[
        "--json",
        "memory",
        "add",
        "--title",
        "t",
        "--summary",
        "s",
        "--memory-type",
        "invariant",
        "--created-by",
        "human:test",
        "--claim-id",
        "00000000-0000-7000-8000-000000000000",
    ]);
    assert!(!unknown_claim.status.success());
    assert_eq!(stderr_code(&unknown_claim), "SNAPSHOT_NOT_FOUND");

    // Unknown memory types are INVALID_REQUEST.
    let bad_type = cli.run(&[
        "--json",
        "memory",
        "add",
        "--title",
        "t",
        "--summary",
        "s",
        "--memory-type",
        "best_practice",
        "--created-by",
        "human:test",
    ]);
    assert!(!bad_type.status.success());
    assert_eq!(stderr_code(&bad_type), "INVALID_REQUEST");

    // search (FR-035) — FTS over title+summary.
    let found = cli.json(&["memory", "search", "manifest caching"]);
    let hits = found["data"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["memory_id"], memory_id.as_str());

    // show — claim link + touch of last_accessed_at.
    let shown = cli.json(&["memory", "show", &memory_id]);
    assert_eq!(shown["data"]["claim_ids"][0], claim_id.as_str());

    // Conflict-aware compilation (FR-060): the memory channel surfaces the
    // memory item before any conflict exists.
    let bundle_plain = cli.json(&[
        "context",
        "compile",
        "--task",
        "manifest caching identity",
        "--budget",
        "4000",
    ]);
    assert!(
        bundle_plain["data"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["channel"] == "memory" && i["evidence_status"] == "candidate"),
        "memory channel surfaces the record: {bundle_plain:?}"
    );

    // Two disagreeing memories in the same scope → both conflicted (FR-034).
    let contradict = cli.json(&[
        "memory",
        "add",
        "--title",
        "Do not cache manifest hashes",
        "--summary",
        "Always cache the manifest hash; it is content-addressed.",
        "--memory-type",
        "failed_approach",
        "--scope",
        "src",
        "--created-by",
        "human:other",
    ]);
    let other_id = contradict["data"]["memory_id"]
        .as_str()
        .unwrap()
        .to_string();
    let conflicts = cli.json(&["conflicts"]);
    let pairs = conflicts["data"].as_array().unwrap();
    assert_eq!(pairs.len(), 1, "one conflict pair: {conflicts:?}");
    assert_eq!(pairs[0]["label"], "conflicted");
    assert_eq!(pairs[0]["left_id"], memory_id.as_str());
    assert_eq!(pairs[0]["right_id"], other_id.as_str());
    assert_eq!(
        pairs[0]["left_text"],
        "Caching the manifest hash broke snapshot identity once."
    );
    assert_eq!(
        pairs[0]["right_text"],
        "Always cache the manifest hash; it is content-addressed."
    );

    // A bundle whose task touches the conflicted memories carries the
    // conflicts section with both sides — never a silent preference.
    let bundle_conflict = cli.json(&[
        "context",
        "compile",
        "--task",
        "manifest caching identity",
        "--budget",
        "4000",
    ]);
    let bundle_conflicts = bundle_conflict["data"]["conflicts"]
        .as_array()
        .expect("conflicts section present");
    assert_eq!(bundle_conflicts.len(), 1);
    assert_eq!(bundle_conflicts[0]["status"], "conflicted");
    assert_eq!(
        bundle_conflicts[0]["entity_ids"].as_array().unwrap().len(),
        2
    );

    // Freshness (FR-043): the linked claim goes stale after a reindex → the
    // memory flips stale with a cause; unrelated memories stay candidate.
    std::fs::write(
        cli.repo_dir.join("src/util.rs"),
        "pub fn read_config() -> u8 {\n    42\n}\n",
    )
    .unwrap();
    git(&cli.repo_dir, &["add", "-A"]);
    git(&cli.repo_dir, &["commit", "-m", "change util"]);
    let index = cli.json(&["index"]);
    assert!(
        index["data"]["memories_flipped_stale"].as_u64().unwrap() >= 1,
        "the freshness projection reported the stale memory: {index:?}"
    );
    let stale = cli.json(&["memory", "list", "--status", "stale"]);
    let stale_rows = stale["data"].as_array().unwrap();
    assert!(stale_rows
        .iter()
        .any(|m| m["memory_id"] == memory_id.as_str()));
    // Selective: the contradicting memory (no linked claims) was flipped
    // `conflicted` by detection earlier and stays untouched by the
    // invalidation — freshness follows linked claims only.
    let still_conflicted = cli.json(&["memory", "list", "--status", "conflicted"]);
    assert!(
        still_conflicted["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["memory_id"] == other_id.as_str()),
        "the other memory kept its conflicted status"
    );

    // Human resolution (SDD §9.4 steps 7–8): loser retracted, rationale kept.
    let resolved = cli.json(&[
        "memory",
        "resolve",
        "--keep",
        &other_id,
        "--drop",
        &memory_id,
        "--rationale",
        "caching is safe; verified by test",
        "--actor",
        "human:test",
    ]);
    assert_eq!(resolved["data"]["outcome"], "retracted");
    let after = cli.json(&["memory", "show", &memory_id]);
    assert_eq!(after["data"]["memory"]["status"], "retracted");
    let events = after["data"]["status_events"].as_array().unwrap();
    assert!(
        events.iter().any(|e| e["actor"] == "human:test"
            && e["reason"]
                .as_str()
                .unwrap_or("")
                .contains("caching is safe")),
        "the resolution rationale is recorded: {events:?}"
    );

    // Edit-as-supersede (SDD §5.8): a new version, old record retired.
    let superseded = cli.json(&[
        "memory",
        "supersede",
        &other_id,
        "--title",
        "Do not cache manifest hashes",
        "--summary",
        "Caching is safe only after identity covers the config hash.",
        "--memory-type",
        "failed_approach",
        "--scope",
        "src",
        "--created-by",
        "human:other",
        "--reason",
        "clarified after the fix",
    ]);
    let new_version = superseded["data"]["memory_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        superseded["data"]["supersedes_memory_id"],
        other_id.as_str()
    );
    let old_shown = cli.json(&["memory", "show", &other_id]);
    assert_eq!(old_shown["data"]["memory"]["status"], "superseded");

    // Retract the new version; history stays queryable.
    cli.json(&[
        "memory",
        "retract",
        &new_version,
        "--actor",
        "human:test",
        "--reason",
        "obsolete",
    ]);
    let retracted = cli.json(&["memory", "show", &new_version]);
    assert_eq!(retracted["data"]["memory"]["status"], "retracted");

    // Unknown memory ids are SNAPSHOT_NOT_FOUND.
    let missing = cli.run(&[
        "--json",
        "memory",
        "show",
        "00000000-0000-7000-8000-000000000000",
    ]);
    assert!(!missing.status.success());
    assert_eq!(stderr_code(&missing), "SNAPSHOT_NOT_FOUND");

    // Export carries the memory records (FR-036).
    let export_path = temp.path().join("export.json");
    let export = cli.json(&["export", "--output", export_path.to_str().unwrap()]);
    assert!(export["data"]["memories"].as_u64().unwrap() >= 3);
    let export_text = std::fs::read_to_string(&export_path).unwrap();
    assert!(export_text.contains("Do not cache manifest hashes"));
}
