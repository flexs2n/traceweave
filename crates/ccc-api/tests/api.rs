//! REST /v1 integration tests (Step 4 T3): happy paths, error-envelope
//! shapes, token on/off, bind validation. The router is exercised
//! in-process (no binary spawn — D-3 locked-executable note); every response
//! body is the same `Envelope` document the CLI prints.

use axum::body::Body;
use ccc_api::{router, AppState, ServeConfig};
use ccc_service::AppContext;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tower::ServiceExt;

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

/// Initialized context + registered + indexed fixture repo. Returns (ctx,
/// repo_path, repository_id).
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

fn state(ctx: &AppContext, token: Option<&str>) -> AppState {
    let ctx = AppContext {
        data_dir: ctx.data_dir.clone(),
        store: ctx.store.clone(),
        artifacts: ctx.artifacts.clone(),
    };
    AppState::new(ctx, token.map(str::to_string))
}

/// Polls `GET /v1/jobs/{id}` until a terminal state (TIS §15.1).
async fn wait_terminal(app: &axum::Router, job_id: &str) -> Value {
    for _ in 0..300 {
        let (status, body) = send(
            app,
            request("GET", &format!("/v1/jobs/{job_id}"), None, None),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        let state = body["data"]["state"].as_str().unwrap_or("");
        if matches!(state, "succeeded" | "failed" | "cancelled") {
            return body;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("job {job_id} never reached a terminal state");
}

fn request(
    method: &str,
    uri: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> axum::http::Request<Body> {
    let mut builder = axum::http::Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    let payload = body.unwrap_or(json!({}));
    builder
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&payload).unwrap()))
        .unwrap()
}

async fn send(
    app: &axum::Router,
    req: axum::http::Request<Body>,
) -> (axum::http::StatusCode, Value) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

#[tokio::test]
async fn health_and_openapi_are_served() {
    let temp = tempfile::tempdir().unwrap();
    let (ctx, _repo, _id) = setup_ctx(temp.path()).await;
    let app = router(state(&ctx, None));

    let (status, body) = send(&app, request("GET", "/v1/health", None, None)).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["data"]["ok"], true);
    assert_eq!(body["schema_version"], "ccc.schema/1");

    let (status, body) = send(&app, request("GET", "/v1/openapi.json", None, None)).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(
        body["paths"]["/v1/repositories"].is_object(),
        "openapi paths present"
    );
    assert!(body["paths"]["/v1/context/compile"].is_object());
    assert!(body["paths"]["/v1/jobs/{id}"].is_object());
}

#[tokio::test]
async fn repositories_flow_with_error_envelope_shapes() {
    let temp = tempfile::tempdir().unwrap();
    let (ctx, repo_dir, _id) = setup_ctx(temp.path()).await;
    let app = router(state(&ctx, None));

    // POST /v1/repositories → 201 with repository + job.
    let (status, body) = send(
        &app,
        request(
            "POST",
            "/v1/repositories",
            Some(json!({"path": repo_dir.to_string_lossy()})),
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    assert!(body["data"]["repository"]["repository_id"].is_string());
    assert!(body["data"]["job_id"].is_string());
    let repo_id = body["data"]["repository"]["repository_id"]
        .as_str()
        .unwrap()
        .to_string();
    let job_id = body["data"]["job_id"].as_str().unwrap().to_string();

    // GET list + single.
    let (status, body) = send(&app, request("GET", "/v1/repositories", None, None)).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(body["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["repository_id"] == repo_id.as_str()));

    let (status, body) = send(
        &app,
        request("GET", &format!("/v1/repositories/{repo_id}"), None, None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(body["data"]["root_path"].is_string());
    // FR-010: drift projection in response meta.
    assert_eq!(body["meta"]["index_current"], true);

    // Unknown repo → 404 + stable error envelope (TIS §13.2 shape).
    let (status, body) = send(
        &app,
        request(
            "GET",
            "/v1/repositories/00000000-0000-7000-8000-000000000000",
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "REPOSITORY_NOT_FOUND");
    assert_eq!(body["schema_version"], "ccc.schema/1");

    // Job record from registration is queryable and terminal.
    let (status, body) = send(
        &app,
        request("GET", &format!("/v1/jobs/{job_id}"), None, None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["data"]["state"], "succeeded");
    let (status, body) = send(
        &app,
        request(
            "GET",
            "/v1/jobs/00000000-0000-7000-8000-000000000000",
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "SNAPSHOT_NOT_FOUND");
}

#[tokio::test]
async fn index_snapshots_search_compile_flow() {
    let temp = tempfile::tempdir().unwrap();
    let (ctx, _repo, repo_id) = setup_ctx(temp.path()).await;
    let app = router(state(&ctx, None));

    // POST index → 202 + Location; the job polls to succeeded.
    let (status, body) = send(
        &app,
        request(
            "POST",
            &format!("/v1/repositories/{repo_id}/index"),
            Some(json!({})),
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::ACCEPTED);
    let job_id = body["data"]["job_id"].as_str().unwrap().to_string();
    // The setup already indexed this exact tree, so this submit may replay
    // the setup's terminal job outright (TIS §13.3) — polling stays correct.
    let job = wait_terminal(&app, &job_id).await;
    assert_eq!(job["data"]["state"], "succeeded");
    assert!(
        job["data"]["payload_json"].is_string(),
        "the index job payload persists the outcome: {job}"
    );

    // Re-submit against the same tree: the derived key replays the same
    // logical job (TIS §13.3).
    let (status, body) = send(
        &app,
        request(
            "POST",
            &format!("/v1/repositories/{repo_id}/index"),
            Some(json!({})),
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::ACCEPTED);
    assert_eq!(
        body["data"]["job_id"].as_str().unwrap(),
        job_id.as_str(),
        "same tree → one logical job"
    );

    // Snapshots list.
    let (status, body) = send(
        &app,
        request(
            "GET",
            &format!("/v1/repositories/{repo_id}/snapshots"),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(!body["data"].as_array().unwrap().is_empty());

    // Search: exact match ranks first; envelope meta carries repo identity.
    let (status, body) = send(
        &app,
        request(
            "GET",
            &format!("/v1/search?repo={repo_id}&q=read_config&limit=10"),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let hits = body["data"]["hits"].as_array().unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0]["match_class"], "exact");
    assert_eq!(body["meta"]["repository_id"], repo_id.as_str());

    // Compile: bundle + drift projection in meta (clean tree → current).
    let (status, body) = send(
        &app,
        request(
            "POST",
            "/v1/context/compile",
            Some(
                json!({"repo": repo_id, "task": "how does read_config get called", "budget": 3000}),
            ),
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let bundle_id = body["data"]["bundle_id"].as_str().unwrap().to_string();
    assert!(!body["data"]["items"].as_array().unwrap().is_empty());
    assert_eq!(body["meta"]["index_current"], true);

    // Bundle is retrievable and immutable; unknown ids are 404.
    let (status, body) = send(
        &app,
        request("GET", &format!("/v1/context/{bundle_id}"), None, None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["data"]["bundle"]["bundle_id"], bundle_id.as_str());
    let (status, body) = send(&app, request("GET", "/v1/context/not-a-bundle", None, None)).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "INVALID_REQUEST");
}

#[tokio::test]
async fn impact_verify_and_eval_flow() {
    let temp = tempfile::tempdir().unwrap();
    let (ctx, repo_dir, repo_id) = setup_ctx(temp.path()).await;
    let app = router(state(&ctx, None));

    // A committed change so base..head is a real range (golden-trail parity).
    std::fs::write(
        repo_dir.join("src/util.rs"),
        "/// Reads the application configuration.\npub fn read_config() -> u8 {\n    42\n}\n",
    )
    .unwrap();
    git(&repo_dir, &["add", "-A"]);
    git(&repo_dir, &["commit", "-m", "change util"]);

    let (status, body) = send(
        &app,
        request(
            "POST",
            "/v1/impact/analyze",
            Some(json!({"repo": repo_id, "base": "HEAD~1", "head": "HEAD"})),
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(!body["data"]["affected_artifacts"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(body["data"]["known_blind_spots"].as_array().unwrap().len() >= 3);

    // Verification: job accepted, polls terminal; the job payload carries
    // the report identity (the report itself renders via CLI/markdown).
    let (status, body) = send(
        &app,
        request(
            "POST",
            "/v1/verification",
            Some(json!({"repo": repo_id, "base": "HEAD~1", "head": "HEAD"})),
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::ACCEPTED);
    let verify_job_id = body["data"]["job_id"].as_str().unwrap().to_string();
    let job = wait_terminal(&app, &verify_job_id).await;
    assert_eq!(job["data"]["state"], "succeeded");
    assert!(
        job["data"]["payload_json"].is_string(),
        "the verify job payload carries the report identity: {job}"
    );

    // Unknown policy → 400 INVALID_REQUEST.
    let (status, body) = send(
        &app,
        request(
            "POST",
            "/v1/verification",
            Some(json!({"repo": repo_id, "base": "HEAD~1", "policy": "strict"})),
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "INVALID_REQUEST");

    // Eval run: job accepted, polls terminal; the recorded run id comes
    // from the job payload; the run is then retrievable with metrics.
    let suite_path = std::fs::canonicalize(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../eval/suites/fixture.toml"),
    )
    .unwrap();
    let (status, body) = send(
        &app,
        request(
            "POST",
            "/v1/evaluation/runs",
            Some(json!({"repo": repo_id, "suite": suite_path.to_string_lossy()})),
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::ACCEPTED);
    let eval_job_id = body["data"]["job_id"].as_str().unwrap().to_string();
    let job = wait_terminal(&app, &eval_job_id).await;
    assert_eq!(job["data"]["state"], "succeeded");
    let run_id = job["data"]["payload_json"]
        .as_str()
        .unwrap()
        .trim_matches('"')
        .to_string();

    let (status, body) = send(
        &app,
        request("GET", &format!("/v1/evaluation/runs/{run_id}"), None, None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(
        body["data"]["metrics"]["retrieval_policy_version"],
        "policy-v1"
    );
    assert!(
        body["data"]["metrics"]["tuning"]["recall_millis"]
            .as_u64()
            .unwrap()
            > 0
    );
    let (status, _body) = send(
        &app,
        request(
            "GET",
            "/v1/evaluation/runs/00000000-0000-7000-8000-000000000000",
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);

    // Evidence record by id (taken from the ledger the index produced).
    let repo = ccc_service::resolve_repo_ref(&ctx.store, &repo_id)
        .await
        .unwrap();
    let evidence = ccc_storage::EvidenceStore::list_evidence(&*ctx.store, repo.repository_id)
        .await
        .unwrap();
    assert!(!evidence.is_empty(), "indexing produced evidence records");
    let evidence_id = evidence[0].evidence_id.to_string();
    let (status, body) = send(
        &app,
        request("GET", &format!("/v1/evidence/{evidence_id}"), None, None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["data"]["evidence_id"], evidence_id.as_str());
}

#[tokio::test]
async fn token_is_enforced_when_configured() {
    let temp = tempfile::tempdir().unwrap();
    let (ctx, _repo, _id) = setup_ctx(temp.path()).await;
    let app = router(state(&ctx, Some("sekrit")));

    let (status, body) = send(&app, request("GET", "/v1/health", None, None)).await;
    assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "PERMISSION_DENIED");

    let (status, _body) = send(&app, request("GET", "/v1/health", None, Some("wrong"))).await;
    assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);

    let (status, _body) = send(&app, request("GET", "/v1/health", None, Some("sekrit"))).await;
    assert_eq!(status, axum::http::StatusCode::OK);
}

#[test]
fn non_loopback_bind_requires_a_token() {
    let loopback: std::net::SocketAddr = "127.0.0.1:7700".parse().unwrap();
    let any_v4: std::net::SocketAddr = "0.0.0.0:7700".parse().unwrap();
    assert!(ccc_api::validate_bind(loopback, None).is_ok());
    assert!(ccc_api::validate_bind(any_v4, Some("t")).is_ok());
    let err = ccc_api::validate_bind(any_v4, None).unwrap_err();
    assert_eq!(err.code, ccc_domain::ErrorCode::PermissionDenied);
}

#[tokio::test]
async fn serve_binds_loopback_and_answers_health_over_tcp() {
    let temp = tempfile::tempdir().unwrap();
    let (ctx, _repo, _id) = setup_ctx(temp.path()).await;

    // Discover an OS-assigned port, then serve on that listener directly.
    let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listener = ccc_api::bind_listener(&ServeConfig {
        bind: addr,
        token: None,
    })
    .await
    .unwrap();
    let bound = listener.local_addr().unwrap();
    let ctx = AppContext {
        data_dir: ctx.data_dir.clone(),
        store: ctx.store.clone(),
        artifacts: ctx.artifacts.clone(),
    };
    let server = tokio::spawn(async move { ccc_api::serve_listener(listener, ctx, None).await });

    let mut stream = tokio::net::TcpStream::connect(bound).await.unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    stream
        .write_all(b"GET /v1/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "health answers over TCP: {response:.120}"
    );
    assert!(response.contains("\"ok\":true"), "health body present");

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn idempotency_header_replays_one_logical_job() {
    let temp = tempfile::tempdir().unwrap();
    let (ctx, _repo, repo_id) = setup_ctx(temp.path()).await;
    let app = router(state(&ctx, None));

    // Two submissions carrying the same Idempotency-Key header (the header
    // replaces the derived key, TIS §13.3): one logical job.
    let submit = |app: &axum::Router| {
        let mut r = request(
            "POST",
            &format!("/v1/repositories/{repo_id}/index"),
            Some(json!({})),
            None,
        );
        r.headers_mut().insert(
            "idempotency-key",
            axum::http::HeaderValue::from_static("flow-key-1"),
        );
        app.clone().oneshot(r)
    };

    let first = submit(&app).await.unwrap();
    assert_eq!(first.status(), axum::http::StatusCode::ACCEPTED);
    let location = first.headers()["location"].to_str().unwrap().to_string();
    let bytes = first.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let job_id = body["data"]["job_id"].as_str().unwrap().to_string();
    assert_eq!(
        location,
        format!("/v1/jobs/{job_id}"),
        "202 carries the poll Location (TIS §13.1)"
    );
    wait_terminal(&app, &job_id).await;

    let second = submit(&app).await.unwrap();
    let bytes = second.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        body["data"]["job_id"].as_str().unwrap(),
        job_id.as_str(),
        "same header key replays one logical job"
    );
}

#[tokio::test]
async fn failed_jobs_report_the_error_code() {
    let temp = tempfile::tempdir().unwrap();
    let (ctx, _repo, repo_id) = setup_ctx(temp.path()).await;
    let app = router(state(&ctx, None));

    // A bogus snapshot id fails the index job; the ledger records why.
    let (status, body) = send(
        &app,
        request(
            "POST",
            &format!("/v1/repositories/{repo_id}/index"),
            Some(json!({"snapshot": "nope-not-a-uuid"})),
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::ACCEPTED);
    let job_id = body["data"]["job_id"].as_str().unwrap().to_string();
    let job = wait_terminal(&app, &job_id).await;
    assert_eq!(job["data"]["state"], "failed");
    assert_eq!(job["data"]["error_code"], "INVALID_REQUEST");
}

#[tokio::test]
async fn heavy_jobs_share_two_slots_and_all_complete() {
    let temp = tempfile::tempdir().unwrap();
    let (ctx, repo_dir, repo_id) = setup_ctx(temp.path()).await;
    let app = router(state(&ctx, None));

    // A committed change so the verify job has a real range.
    std::fs::write(
        repo_dir.join("src/util.rs"),
        "/// Reads the application configuration.\npub fn read_config() -> u8 {\n    42\n}\n",
    )
    .unwrap();
    git(&repo_dir, &["add", "-A"]);
    git(&repo_dir, &["commit", "-m", "change util"]);

    // Four distinct heavy jobs race through the two slots (T4 bound: ≤2
    // concurrent heavy jobs; if the semaphore starved them, the polls below
    // would time out).
    let suite_path = std::fs::canonicalize(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../eval/suites/fixture.toml"),
    )
    .unwrap();
    let mut job_ids = Vec::new();
    for (uri, body) in [
        (format!("/v1/repositories/{repo_id}/index"), json!({})),
        (
            format!("/v1/repositories/{repo_id}/index"),
            json!({"full": true}),
        ),
        (
            "/v1/verification".to_string(),
            json!({"repo": repo_id, "base": "HEAD~1", "head": "HEAD"}),
        ),
        (
            "/v1/evaluation/runs".to_string(),
            json!({"repo": repo_id, "suite": suite_path.to_string_lossy()}),
        ),
    ] {
        let (status, resp) = send(&app, request("POST", &uri, Some(body), None)).await;
        assert_eq!(
            status,
            axum::http::StatusCode::ACCEPTED,
            "submit ok: {resp}"
        );
        job_ids.push(resp["data"]["job_id"].as_str().unwrap().to_string());
    }
    let mut unique = job_ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 4, "four distinct logical jobs");
    for id in &job_ids {
        let job = wait_terminal(&app, id).await;
        assert_eq!(job["data"]["state"], "succeeded", "job {id}: {job}");
    }
}
