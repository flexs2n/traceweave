//! MCP adapter tests (Step 4 T6): a REAL initialize handshake over an
//! in-memory duplex pair (no binary spawn — D-3), tools/list, every
//! tools/call happy path, unknown-repo and unknown-tool error paths, cap
//! behavior, and a pinned test that the tool surface is exactly the eight
//! read/analysis tools (no execution, no memory mutation).

use ccc_mcp::{CccMcp, TOOL_NAMES};
use ccc_service::AppContext;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::serve_client;
use rmcp::{service::serve_server, RoleClient, RoleServer};
use serde_json::{json, Value};
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

async fn setup_ctx(temp: &Path) -> (AppContext, PathBuf, String) {
    let data_dir = temp.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let _ = ccc_config::write_default_config(&data_dir).unwrap();
    let ctx = AppContext::open(&data_dir).await.unwrap();
    let repo_dir = seed_repo(temp);
    let out = ccc_service::repo_add(&ctx, &repo_dir, None).await.unwrap();
    let envelope: Value = serde_json::to_value(out.envelope()).unwrap();
    let repo_id = envelope["data"]["repository"]["repository_id"]
        .as_str()
        .unwrap()
        .to_string();
    ccc_service::index_repo_record(
        &ctx,
        &ccc_service::resolve_repo_ref(&ctx.store, &repo_id)
            .await
            .unwrap(),
        false,
        None,
        None,
    )
    .await
    .unwrap();
    (ctx, repo_dir, repo_id)
}

type Client = rmcp::service::RunningService<RoleClient, ()>;
type Server = rmcp::service::RunningService<RoleServer, CccMcp>;

/// Real initialize handshake over in-memory duplex transports. BOTH handles
/// are returned: dropping the server's handle would close the transport.
async fn connect(handler: CccMcp) -> (Client, Server) {
    let (s2c_read, s2c_write) = tokio::io::duplex(1 << 20);
    let (c2s_read, c2s_write) = tokio::io::duplex(1 << 20);

    // Cross-wired: the server reads what the client writes and vice versa.
    let server = serve_server(handler, (c2s_read, s2c_write));
    let client = serve_client((), (s2c_read, c2s_write));
    let (server, client) = tokio::join!(server, client);
    (client.unwrap(), server.unwrap())
}

async fn call(client: &Client, name: &str, args: Value) -> CallToolResult {
    let arguments = match args {
        Value::Object(map) => Some(map),
        _ => None,
    };
    let mut params = CallToolRequestParams::new(name.to_string());
    if let Some(arguments) = arguments {
        params = params.with_arguments(arguments);
    }
    client.peer().call_tool(params).await.unwrap()
}

fn structured(result: &CallToolResult) -> Value {
    result
        .structured_content
        .clone()
        .expect("structured content")
}

#[tokio::test]
async fn handshake_negotiates_and_lists_exactly_the_pinned_tools() {
    let temp = tempfile::tempdir().unwrap();
    let (ctx, _repo, _id) = setup_ctx(temp.path()).await;
    let (client, _server) = connect(CccMcp::new(ctx, None)).await;

    // The handshake happened during serve_client (initialize + initialized);
    // a working list_tools round-trip proves the session is live.
    let tools = client
        .peer()
        .list_tools(Default::default())
        .await
        .unwrap()
        .tools;
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort_unstable();
    let mut expected = TOOL_NAMES.to_vec();
    expected.sort_unstable();
    assert_eq!(names, expected, "tool surface is exactly the pinned set");

    // Pin: every exposed tool is annotated read-only (TIS §13.5 — no
    // execution-capable or mutating tool is exposed; the pinned NAME list
    // above already excludes memory mutation, indexing, and verification
    // execution).
    for tool in tools.iter() {
        let annotations = tool.annotations.as_ref().expect("annotations present");
        assert_eq!(
            annotations.read_only_hint,
            Some(true),
            "tool {} must be read-only",
            tool.name
        );
    }
}

#[tokio::test]
async fn repository_status_search_and_lookup() {
    let temp = tempfile::tempdir().unwrap();
    let (ctx, _repo, repo_id) = setup_ctx(temp.path()).await;
    let (client, _server) = connect(CccMcp::new(ctx, None)).await;

    let result = call(&client, "repository_status", json!({ "repo": repo_id })).await;
    assert_ne!(result.is_error, Some(true), "status ok: {result:?}");
    let value = structured(&result);
    assert_eq!(value["schema_version"], "ccc.schema/1");
    assert_eq!(value["data"]["indexed"], true);
    assert_eq!(value["meta"]["index_current"], true);
    assert_eq!(value["truncated"], false);

    let result = call(
        &client,
        "search_code",
        json!({ "repo": repo_id, "query": "read_config" }),
    )
    .await;
    let value = structured(&result);
    assert!(!value["data"]["hits"].as_array().unwrap().is_empty());

    let result = call(
        &client,
        "lookup_symbol",
        json!({ "repo": repo_id, "symbol": "read_config" }),
    )
    .await;
    let value = structured(&result);
    let hits = value["data"]["hits"].as_array().unwrap();
    assert!(!hits.is_empty(), "exact hit found");
    assert!(hits.iter().all(|h| h["match_class"] == "exact"));
}

#[tokio::test]
async fn compile_context_respects_budget_cap_and_drift_meta() {
    let temp = tempfile::tempdir().unwrap();
    let (ctx, _repo, repo_id) = setup_ctx(temp.path()).await;
    let (client, _server) = connect(CccMcp::new(ctx, None)).await;

    let result = call(
        &client,
        "compile_context",
        json!({ "repo": repo_id, "task": "how does read_config get called" }),
    )
    .await;
    let value = structured(&result);
    assert!(
        value["data"]["bundle_id"].is_string(),
        "compile result: {value}"
    );
    assert!(!value["data"]["items"].as_array().unwrap().is_empty());
    assert_eq!(value["meta"]["index_current"], true);
    assert_eq!(value["truncated"], false);

    // Over-cap budget → tool error with INVALID_REQUEST, nothing compiled.
    let result = call(
        &client,
        "compile_context",
        json!({ "repo": repo_id, "task": "x", "budget": 1_000_000 }),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let value = structured(&result);
    assert_eq!(value["error"]["code"], "INVALID_REQUEST");
}

#[tokio::test]
async fn trace_dependencies_walks_edges() {
    let temp = tempfile::tempdir().unwrap();
    let (ctx, _repo, repo_id) = setup_ctx(temp.path()).await;
    let (client, _server) = connect(CccMcp::new(ctx, None)).await;

    // Trace from a qualified symbol (the indexer's qualified-name form);
    // an honest walk may end at depth 0 when it has no resolved outgoing
    // edges — the error case is an unknown root.
    let result = call(
        &client,
        "trace_dependencies",
        json!({ "repo": repo_id, "symbol": "crate::run_app" }),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "trace ok: {result:?}");
    let value = structured(&result);
    assert!(!value["data"]["symbols"].as_array().unwrap().is_empty());

    let result = call(
        &client,
        "trace_dependencies",
        json!({ "repo": repo_id, "symbol": "no_such_symbol_anywhere" }),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let value = structured(&result);
    assert_eq!(value["error"]["code"], "SNAPSHOT_NOT_FOUND");
}

#[tokio::test]
async fn evidence_impact_and_verification_results() {
    let temp = tempfile::tempdir().unwrap();
    let (ctx, repo_dir, repo_id) = setup_ctx(temp.path()).await;
    let (client, _server) = connect(CccMcp::new(ctx.clone(), None)).await;

    // Committed change → a verification record exists for results lookup.
    std::fs::write(
        repo_dir.join("src/util.rs"),
        "/// Reads the application configuration.\npub fn read_config() -> u8 {\n    42\n}\n",
    )
    .unwrap();
    git(&repo_dir, &["add", "-A"]);
    git(&repo_dir, &["commit", "-m", "change util"]);
    let app_ctx = ccc_service::resolve_repo_ref(&ctx.store, &repo_id)
        .await
        .unwrap();
    ccc_service::verify_record(
        &ctx,
        &app_ctx,
        "HEAD~1",
        Some("HEAD"),
        "analysis",
        "json",
        None,
    )
    .await
    .unwrap();

    let result = call(
        &client,
        "analyze_change_impact",
        json!({ "repo": repo_id, "base": "HEAD~1", "head": "HEAD" }),
    )
    .await;
    let value = structured(&result);
    assert!(!value["data"]["affected_artifacts"]
        .as_array()
        .unwrap()
        .is_empty());

    let result = call(
        &client,
        "get_verification_results",
        json!({ "repo": repo_id }),
    )
    .await;
    let value = structured(&result);
    assert!(!value["data"].as_array().unwrap().is_empty());

    // Evidence by id (from the ledger the index produced).
    let evidence = ccc_storage::EvidenceStore::list_evidence(&*ctx.store, app_ctx.repository_id)
        .await
        .unwrap();
    assert!(!evidence.is_empty());
    let result = call(
        &client,
        "inspect_evidence",
        json!({ "evidence_id": evidence[0].evidence_id.to_string() }),
    )
    .await;
    let value = structured(&result);
    assert_eq!(
        value["data"]["evidence_id"],
        evidence[0].evidence_id.to_string()
    );
}

#[tokio::test]
async fn unknown_repo_is_a_tool_error_not_a_protocol_error() {
    let temp = tempfile::tempdir().unwrap();
    let (ctx, _repo, _id) = setup_ctx(temp.path()).await;
    let (client, _server) = connect(CccMcp::new(ctx, None)).await;

    let result = call(
        &client,
        "search_code",
        json!({ "repo": "C:/does/not/exist", "query": "x" }),
    )
    .await;
    assert_eq!(result.is_error, Some(true), "tool-level error, not Err");
    let value = structured(&result);
    assert_eq!(value["error"]["code"], "INVALID_REQUEST");
}

#[tokio::test]
async fn unknown_tool_is_a_protocol_error() {
    let temp = tempfile::tempdir().unwrap();
    let (ctx, _repo, _id) = setup_ctx(temp.path()).await;
    let (client, _server) = connect(CccMcp::new(ctx, None)).await;
    let err = client
        .peer()
        .call_tool(CallToolRequestParams::new("memory_add".to_string()))
        .await
        .unwrap_err();
    // Unknown tools are protocol-level METHOD equivalents (INVALID_PARAMS).
    match err {
        rmcp::ServiceError::McpError(mcp_err) => {
            assert_eq!(mcp_err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        }
        other => panic!("expected McpError, got {other:?}"),
    }
}

#[tokio::test]
async fn oversized_results_are_trimmed_with_explicit_flag_or_refused() {
    let temp = tempfile::tempdir().unwrap();
    let (ctx, _repo, repo_id) = setup_ctx(temp.path()).await;

    // First: measure the natural size of a multi-hit search result.
    let (client, _server) = connect(CccMcp::new(ctx.clone(), None)).await;
    let result = call(
        &client,
        "search_code",
        json!({ "repo": repo_id, "query": "config" }),
    )
    .await;
    let value = structured(&result);
    assert!(
        value["data"]["hits"].as_array().unwrap().len() >= 2,
        "the trim test needs multiple hits: {value}"
    );
    let natural = serde_json::to_string(&value).unwrap().len();

    // Cap below natural size → the hit list is trimmed and flagged.
    let tight = natural - 40;
    let (client2, _s2) = connect(CccMcp::new(ctx.clone(), None).with_output_cap(tight)).await;
    let result = call(
        &client2,
        "search_code",
        json!({ "repo": repo_id, "query": "config" }),
    )
    .await;
    let value = structured(&result);
    assert_eq!(
        value["truncated"], true,
        "trim applied and flagged: {value}"
    );
    assert_eq!(result.is_error, Some(false));

    // 1-byte cap: hits trim to one element and stay over → refusal (honest,
    // never silently byte-chopped). The refusal is a protocol-level error.
    let (client3, _s3) = connect(CccMcp::new(ctx.clone(), None).with_output_cap(1)).await;
    let refusal = client3
        .peer()
        .call_tool(
            CallToolRequestParams::new("search_code".to_string()).with_arguments(
                serde_json::from_value(json!({ "repo": repo_id, "query": "config" })).unwrap(),
            ),
        )
        .await;
    assert!(refusal.is_err(), "refusal at 1-byte cap: {refusal:?}");
}

#[tokio::test]
async fn default_repo_configuration_scopes_every_call() {
    let temp = tempfile::tempdir().unwrap();
    let (ctx, _repo, repo_id) = setup_ctx(temp.path()).await;
    let (client, _server) = connect(CccMcp::new(ctx, Some(repo_id.clone()))).await;

    // No explicit repo → configured default is used.
    let result = call(&client, "repository_status", json!({})).await;
    assert_ne!(
        result.is_error,
        Some(true),
        "default repo applied: {result:?}"
    );
    let value = structured(&result);
    assert_eq!(value["data"]["repository_id"], repo_id.as_str());
}
