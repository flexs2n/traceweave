//! `ccc-api` — the REST integration layer (FR-093, TIS §13.1, SDD §16).
//!
//! One Axum router over `ccc-service` operations. Responses are the same
//! versioned `Envelope` documents the CLI prints (`ccc.schema/1`); errors
//! are the same `ErrorEnvelope` contracts with stable TIS §13.2 codes mapped
//! to HTTP statuses. No evidence/freshness semantics live here (SDD §16.1).
//!
//! Security posture (NFR-020): bind loopback by default; optional bearer
//! token (`[api] token` / `CCC_API_TOKEN`); a non-loopback bind without a
//! token refuses to start.

use axum::{
    extract::{Path, Query, Request, State},
    http::{HeaderMap, HeaderName, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use ccc_domain::{CccError, ErrorCode, JobId};
use ccc_schema::{Envelope, EnvelopeMeta, ErrorEnvelope, SCHEMA_VERSION};
use serde::Deserialize;
use serde_json::value::RawValue;
use std::net::SocketAddr;
use std::sync::Arc;
use utoipa::{OpenApi as _, ToSchema};

/// The success body shape of every endpoint: the CLI's `--json` envelope.
pub type ApiEnvelope = Envelope<Box<RawValue>>;

/// Shared router state.
#[derive(Clone)]
pub struct AppState {
    pub ctx: Arc<ccc_service::AppContext>,
    /// When set, every request must present `Authorization: Bearer <token>`.
    pub token: Option<String>,
    /// Bounded executor for heavy jobs (index/verify/eval).
    pub jobs: Arc<JobRunner>,
}

impl AppState {
    /// Convenience constructor wiring the shared context into the runner.
    pub fn new(ctx: ccc_service::AppContext, token: Option<String>) -> Self {
        let ctx = Arc::new(ctx);
        Self {
            ctx: ctx.clone(),
            token,
            jobs: Arc::new(JobRunner::new(ctx)),
        }
    }
}

/// Bounded async job execution (T4): heavy operations run in spawned tasks
/// gated by a semaphore (≤ 2 at once; SQLite's single serialized writer sits
/// underneath), results land in the same job ledger the CLI records, and
/// clients poll `GET /v1/jobs/{id}`. There is no cancel endpoint (documented
/// limitation, consistent with the CLI).
#[derive(Clone)]
pub struct JobRunner {
    ctx: Arc<ccc_service::AppContext>,
    sem: Arc<tokio::sync::Semaphore>,
    /// Job ids currently executing in this process — a duplicate submit of a
    /// non-terminal job never double-executes (TIS §13.3).
    inflight: Arc<tokio::sync::Mutex<std::collections::HashSet<String>>>,
    /// Per-repository execution locks: two index/verify/eval runs on the same
    /// repository serialize (the CLI never runs them concurrently); jobs on
    /// different repositories may overlap within the semaphore bound.
    repo_locks:
        Arc<tokio::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl JobRunner {
    pub fn new(ctx: Arc<ccc_service::AppContext>) -> Self {
        Self {
            ctx,
            sem: Arc::new(tokio::sync::Semaphore::new(2)),
            inflight: Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new())),
            repo_locks: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Free heavy-job slots (exposed for tests/monitoring).
    pub fn available_heavy_slots(&self) -> usize {
        self.sem.available_permits()
    }

    async fn repo_lock(
        &self,
        repository_id: &ccc_domain::RepositoryId,
    ) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.repo_locks.lock().await;
        locks.entry(repository_id.to_string()).or_default().clone()
    }

    async fn mark(&self, job_id: JobId) -> bool {
        self.inflight.lock().await.insert(job_id.to_string())
    }

    fn spawn<F>(&self, job_id: JobId, repo_lock: Arc<tokio::sync::Mutex<()>>, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let sem = self.sem.clone();
        let inflight = self.inflight.clone();
        tokio::spawn(async move {
            // Repository work serializes first; the semaphore only bounds how
            // many repos are actively processed at once (T4 ≤ 2).
            let _repo = repo_lock.lock().await;
            let _permit = sem.acquire().await;
            fut.await;
            inflight.lock().await.remove(&job_id.to_string());
        });
    }

    pub async fn submit_index(
        &self,
        job_id: JobId,
        repo: ccc_domain::RepositoryRecord,
        full: bool,
        snapshot: Option<String>,
        key: String,
    ) {
        if !self.mark(job_id).await {
            return;
        }
        let ctx = self.ctx.clone();
        let lock = self.repo_lock(&repo.repository_id).await;
        self.spawn(job_id, lock, async move {
            if let Err(e) =
                ccc_service::index_repo_record(&ctx, &repo, full, snapshot.as_deref(), Some(&key))
                    .await
            {
                let _ = ccc_service::fail_job(&ctx.store, job_id, &e).await;
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn submit_verify(
        &self,
        job_id: JobId,
        repo: ccc_domain::RepositoryRecord,
        base: String,
        head: Option<String>,
        policy: String,
        format: String,
        key: String,
    ) {
        if !self.mark(job_id).await {
            return;
        }
        let ctx = self.ctx.clone();
        let lock = self.repo_lock(&repo.repository_id).await;
        self.spawn(job_id, lock, async move {
            if let Err(e) = ccc_service::verify_record(
                &ctx,
                &repo,
                &base,
                head.as_deref(),
                &policy,
                &format,
                Some(&key),
            )
            .await
            {
                let _ = ccc_service::fail_job(&ctx.store, job_id, &e).await;
            }
        });
    }

    pub async fn submit_eval(
        &self,
        job_id: JobId,
        repo: ccc_domain::RepositoryRecord,
        suite: String,
        policy: Option<String>,
        key: String,
    ) {
        if !self.mark(job_id).await {
            return;
        }
        let ctx = self.ctx.clone();
        let lock = self.repo_lock(&repo.repository_id).await;
        self.spawn(job_id, lock, async move {
            if let Err(e) =
                ccc_service::eval_run_record(&ctx, &repo, &suite, policy.as_deref(), Some(&key))
                    .await
            {
                let _ = ccc_service::fail_job(&ctx.store, job_id, &e).await;
            }
        });
    }
}

/// Serve-time configuration (T5 wires config file + env precedence).
#[derive(Debug, Clone)]
pub struct ServeConfig {
    pub bind: SocketAddr,
    pub token: Option<String>,
}

// ---------------------------------------------------------------------------
// error mapping (TIS §13.2 → HTTP)
// ---------------------------------------------------------------------------

/// HTTP status for a stable error code. Auth failures are 401; the generic
/// `PERMISSION_DENIED` code maps to 403 (plan T3).
pub fn status_for(code: ErrorCode) -> StatusCode {
    match code {
        ErrorCode::InvalidRequest => StatusCode::BAD_REQUEST,
        ErrorCode::RepositoryNotFound | ErrorCode::SnapshotNotFound => StatusCode::NOT_FOUND,
        ErrorCode::PermissionDenied => StatusCode::FORBIDDEN,
        ErrorCode::IndexStale => StatusCode::CONFLICT,
        ErrorCode::UnsupportedLanguage => StatusCode::UNPROCESSABLE_ENTITY,
        ErrorCode::BudgetExceeded => StatusCode::UNPROCESSABLE_ENTITY,
        ErrorCode::ResourceLimitExceeded => StatusCode::PAYLOAD_TOO_LARGE,
        ErrorCode::ExecutionNotAuthorized => StatusCode::FORBIDDEN,
        ErrorCode::JobConflict => StatusCode::CONFLICT,
        ErrorCode::DependencyUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::InternalError => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// An operation error crossing the HTTP boundary as an `ErrorEnvelope`.
#[derive(Debug)]
pub struct ApiError(pub CccError);

impl From<CccError> for ApiError {
    fn from(err: CccError) -> Self {
        ApiError(err)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        error_response(&self.0, status_for(self.0.code)).into_response()
    }
}

fn error_response(err: &CccError, status: StatusCode) -> (StatusCode, Json<ErrorEnvelope>) {
    (status, Json(ErrorEnvelope::from_ccc(err)))
}

fn json_envelope<T: serde::Serialize>(data: &T, meta: EnvelopeMeta) -> ApiEnvelope {
    let data = serde_json::value::to_raw_value(data)
        .unwrap_or_else(|_| RawValue::from_string("null".into()).expect("null literal"));
    Envelope {
        schema_version: SCHEMA_VERSION.into(),
        data,
        meta: Some(meta),
    }
}

/// `202 Accepted` + `Location: /v1/jobs/{id}` + the job record as the body
/// (TIS §13.1: clients poll the job resource, requests never stay open).
fn accepted(job_id: &str, envelope: ApiEnvelope) -> Response {
    (
        StatusCode::ACCEPTED,
        [("Location", format!("/v1/jobs/{job_id}"))],
        Json(envelope),
    )
        .into_response()
}

/// `Idempotency-Key` header wins over an optional body field (TIS §13.3).
fn idempotency_key(headers: &HeaderMap, body_key: Option<&str>) -> Option<String> {
    headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| body_key.map(str::to_string))
}

// ---------------------------------------------------------------------------
// auth (NFR-020)
// ---------------------------------------------------------------------------

/// A non-loopback bind without a token is refused at startup (NFR-020).
pub fn validate_bind(addr: SocketAddr, token: Option<&str>) -> Result<(), CccError> {
    if !addr.ip().is_loopback() && token.is_none() {
        return Err(CccError::permission_denied(format!(
            "refusing to bind non-loopback address {addr} without an API token \
             (set [api] token in config.toml or CCC_API_TOKEN)"
        )));
    }
    Ok(())
}

async fn auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if let Some(token) = &state.token {
        let expected = format!("Bearer {token}");
        let provided = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        if provided != Some(expected.as_str()) {
            let err = CccError::permission_denied("missing or invalid bearer token");
            return error_response(&err, StatusCode::UNAUTHORIZED).into_response();
        }
    }
    next.run(req).await
}

// ---------------------------------------------------------------------------
// request DTOs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
pub struct RepoAddRequest {
    /// Filesystem path of the git repository to register.
    pub path: String,
    /// Optional idempotency key (TIS §13.3); identical requests replay one
    /// logical job.
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct IndexJobRequest {
    #[serde(default)]
    pub full: bool,
    /// Re-index an existing snapshot id instead of capturing a new state.
    pub snapshot: Option<String>,
    /// Optional explicit idempotency key (TIS §13.3).
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ContextCompileRequest {
    /// Repository id or path; omitted resolves like the CLI (cwd / single repo).
    pub repo: Option<String>,
    pub task: String,
    pub budget: u64,
    /// `json` (default) or `markdown` — affects only a `text` response.
    pub format: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ImpactAnalyzeRequest {
    pub repo: Option<String>,
    pub base: String,
    pub head: Option<String>,
    pub format: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct VerificationRequest {
    pub repo: Option<String>,
    pub base: String,
    pub head: Option<String>,
    /// Policy name from the data dir (default: built-in `analysis`).
    pub policy: Option<String>,
    pub format: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct EvalRunRequest {
    pub repo: Option<String>,
    /// Suite name (resolved from `./eval/suites/` or the data dir) or a
    /// direct path.
    pub suite: String,
    /// Optional retrieval-policy override TOML path (`[weights]` table).
    pub policy: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SearchQuery {
    /// Repository id or path.
    pub repo: Option<String>,
    pub q: String,
    pub limit: Option<u32>,
}

// documentation-only shapes (the wire format is a `RawValue` payload)

#[derive(Debug, ToSchema)]
pub struct EnvelopeMetaDoc {
    pub repository_id: Option<String>,
    pub snapshot_id: Option<String>,
    pub generation_id: Option<String>,
    pub request_id: Option<String>,
    pub index_current: Option<bool>,
}

#[derive(Debug, ToSchema)]
pub struct EnvelopeDoc {
    pub schema_version: String,
    #[schema(value_type = Object)]
    pub data: serde_json::Value,
    pub meta: Option<EnvelopeMetaDoc>,
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

#[utoipa::path(get, path = "/v1/health", responses((status = 200, body = EnvelopeDoc)))]
async fn health() -> Json<ApiEnvelope> {
    let data = serde_json::json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "schema_version": SCHEMA_VERSION,
    });
    Json(json_envelope(&data, EnvelopeMeta::default()))
}

#[utoipa::path(
    post,
    path = "/v1/repositories",
    request_body = RepoAddRequest,
    responses(
        (status = 201, description = "Registered (idempotent per root path)", body = EnvelopeDoc),
        (status = 400, body = EnvelopeDoc)
    )
)]
async fn repositories_post(
    State(state): State<AppState>,
    Json(req): Json<RepoAddRequest>,
) -> Result<(StatusCode, Json<ApiEnvelope>), ApiError> {
    let out = ccc_service::repo_add(
        &state.ctx,
        std::path::Path::new(&req.path),
        req.idempotency_key.as_deref(),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(out.envelope())))
}

#[utoipa::path(
    get,
    path = "/v1/repositories",
    responses((status = 200, body = EnvelopeDoc))
)]
async fn repositories_get(State(state): State<AppState>) -> Result<Json<ApiEnvelope>, ApiError> {
    let out = ccc_service::repo_status(&state.ctx, None).await?;
    Ok(Json(out.envelope()))
}

#[utoipa::path(
    get,
    path = "/v1/repositories/{id}",
    params(("id" = String, Path, description = "Repository id or path")),
    responses(
        (status = 200, body = EnvelopeDoc),
        (status = 404, body = EnvelopeDoc)
    )
)]
async fn repository_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ApiEnvelope>, ApiError> {
    let out = ccc_service::repo_metadata(&state.ctx, &id).await?;
    Ok(Json(out.envelope()))
}

#[utoipa::path(
    post,
    path = "/v1/repositories/{id}/index",
    request_body = IndexJobRequest,
    params(("id" = String, Path, description = "Repository id or path")),
    responses(
        (status = 202, description = "Job accepted; poll /v1/jobs/{{id}}", body = EnvelopeDoc),
        (status = 404, body = EnvelopeDoc)
    )
)]
async fn repository_index(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<IndexJobRequest>,
) -> Result<Response, ApiError> {
    let repo = ccc_service::resolve_repo_ref(&state.ctx.store, &id).await?;
    let key = idempotency_key(&headers, req.idempotency_key.as_deref());
    let job = match &key {
        Some(k) => ccc_service::begin_job(&state.ctx.store, repo.repository_id, "index", k).await?,
        None => {
            let k = ccc_service::index_job_key(&repo, req.full, req.snapshot.as_deref()).await?;
            ccc_service::begin_job(&state.ctx.store, repo.repository_id, "index", &k).await?
        }
    };
    if !job.state.is_terminal() {
        state
            .jobs
            .submit_index(
                job.job_id,
                repo,
                req.full,
                req.snapshot.clone(),
                job.idempotency_key,
            )
            .await;
    }
    let out = ccc_service::job_get(&state.ctx, &job.job_id.to_string()).await?;
    Ok(accepted(&job.job_id.to_string(), out.envelope()))
}

#[utoipa::path(
    get,
    path = "/v1/repositories/{id}/snapshots",
    params(("id" = String, Path, description = "Repository id or path")),
    responses(
        (status = 200, body = EnvelopeDoc),
        (status = 404, body = EnvelopeDoc)
    )
)]
async fn repository_snapshots(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ApiEnvelope>, ApiError> {
    let out = ccc_service::repo_snapshots(&state.ctx, &id).await?;
    Ok(Json(out.envelope()))
}

#[utoipa::path(
    post,
    path = "/v1/context/compile",
    request_body = ContextCompileRequest,
    responses(
        (status = 200, body = EnvelopeDoc),
        (status = 400, body = EnvelopeDoc),
        (status = 404, body = EnvelopeDoc)
    )
)]
async fn context_compile(
    State(state): State<AppState>,
    Json(req): Json<ContextCompileRequest>,
) -> Result<Json<ApiEnvelope>, ApiError> {
    let repo = match &req.repo {
        Some(r) => ccc_service::resolve_repo_ref(&state.ctx.store, r).await?,
        None => ccc_service::resolve_repo(&state.ctx.store, None).await?,
    };
    let mut out = ccc_service::context_compile_record(
        &state.ctx,
        &repo,
        &req.task,
        req.budget,
        req.format.as_deref().unwrap_or("json"),
    )
    .await?;
    // FR-010 drift projection rides in response meta only; bundle DTO bytes
    // are untouched (SDD §14.5 pattern).
    out.meta.index_current = Some(ccc_service::index_current(&state.ctx.store, &repo).await?);
    Ok(Json(out.envelope()))
}

#[utoipa::path(
    get,
    path = "/v1/context/{id}",
    params(("id" = String, Path, description = "Bundle id")),
    responses(
        (status = 200, body = EnvelopeDoc),
        (status = 404, body = EnvelopeDoc)
    )
)]
async fn context_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ApiEnvelope>, ApiError> {
    let out = ccc_service::context_show(&state.ctx, &id).await?;
    Ok(Json(out.envelope()))
}

#[utoipa::path(
    get,
    path = "/v1/search",
    params(("q" = String, Query, description = "Query text"), ("repo" = Option<String>, Query), ("limit" = Option<u32>, Query)),
    responses((status = 200, body = EnvelopeDoc), (status = 400, body = EnvelopeDoc))
)]
async fn search(
    State(state): State<AppState>,
    Query(req): Query<SearchQuery>,
) -> Result<Json<ApiEnvelope>, ApiError> {
    let repo = match &req.repo {
        Some(r) => ccc_service::resolve_repo_ref(&state.ctx.store, r).await?,
        None => ccc_service::resolve_repo(&state.ctx.store, None).await?,
    };
    let out =
        ccc_service::search_record(&state.ctx, &repo, &req.q, req.limit.unwrap_or(20)).await?;
    Ok(Json(out.envelope()))
}

#[utoipa::path(
    post,
    path = "/v1/impact/analyze",
    request_body = ImpactAnalyzeRequest,
    responses(
        (status = 200, body = EnvelopeDoc),
        (status = 400, body = EnvelopeDoc),
        (status = 404, body = EnvelopeDoc)
    )
)]
async fn impact_analyze(
    State(state): State<AppState>,
    Json(req): Json<ImpactAnalyzeRequest>,
) -> Result<Json<ApiEnvelope>, ApiError> {
    let repo = match &req.repo {
        Some(r) => ccc_service::resolve_repo_ref(&state.ctx.store, r).await?,
        None => ccc_service::resolve_repo(&state.ctx.store, None).await?,
    };
    let out = ccc_service::impact_analyze_record(
        &state.ctx,
        &repo,
        &req.base,
        req.head.as_deref(),
        req.format.as_deref().unwrap_or("json"),
    )
    .await?;
    Ok(Json(out.envelope()))
}

#[utoipa::path(
    post,
    path = "/v1/verification",
    request_body = VerificationRequest,
    responses(
        (status = 202, description = "Job accepted; poll /v1/jobs/{{id}}", body = EnvelopeDoc),
        (status = 400, body = EnvelopeDoc),
        (status = 403, body = EnvelopeDoc),
        (status = 404, body = EnvelopeDoc)
    )
)]
async fn verification_request(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<VerificationRequest>,
) -> Result<Response, ApiError> {
    let repo = match &req.repo {
        Some(r) => ccc_service::resolve_repo_ref(&state.ctx.store, r).await?,
        None => ccc_service::resolve_repo(&state.ctx.store, None).await?,
    };
    let policy = req.policy.unwrap_or_else(|| "analysis".into());
    let key = idempotency_key(&headers, None);
    let job = match &key {
        Some(k) => {
            ccc_service::begin_job(&state.ctx.store, repo.repository_id, "verify", k).await?
        }
        None => {
            let k = ccc_service::verify_job_key(
                &state.ctx.data_dir,
                &repo,
                &req.base,
                req.head.as_deref(),
                &policy,
            )?;
            ccc_service::begin_job(&state.ctx.store, repo.repository_id, "verify", &k).await?
        }
    };
    if !job.state.is_terminal() {
        state
            .jobs
            .submit_verify(
                job.job_id,
                repo,
                req.base.clone(),
                req.head.clone(),
                policy,
                req.format.unwrap_or_else(|| "json".into()),
                job.idempotency_key,
            )
            .await;
    }
    let out = ccc_service::job_get(&state.ctx, &job.job_id.to_string()).await?;
    Ok(accepted(&job.job_id.to_string(), out.envelope()))
}

#[utoipa::path(
    post,
    path = "/v1/evaluation/runs",
    request_body = EvalRunRequest,
    responses(
        (status = 202, description = "Job accepted; poll /v1/jobs/{{id}}", body = EnvelopeDoc),
        (status = 400, body = EnvelopeDoc),
        (status = 404, body = EnvelopeDoc)
    )
)]
async fn eval_run_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<EvalRunRequest>,
) -> Result<Response, ApiError> {
    let repo = match &req.repo {
        Some(r) => ccc_service::resolve_repo_ref(&state.ctx.store, r).await?,
        None => ccc_service::resolve_repo(&state.ctx.store, None).await?,
    };
    let key = idempotency_key(&headers, None);
    let job = match &key {
        Some(k) => {
            ccc_service::begin_job(&state.ctx.store, repo.repository_id, "eval_run", k).await?
        }
        None => {
            let k = ccc_service::eval_run_job_key(
                &state.ctx.data_dir,
                &repo,
                &req.suite,
                req.policy.as_deref(),
            )?;
            ccc_service::begin_job(&state.ctx.store, repo.repository_id, "eval_run", &k).await?
        }
    };
    if !job.state.is_terminal() {
        state
            .jobs
            .submit_eval(
                job.job_id,
                repo,
                req.suite.clone(),
                req.policy.clone(),
                job.idempotency_key,
            )
            .await;
    }
    let out = ccc_service::job_get(&state.ctx, &job.job_id.to_string()).await?;
    Ok(accepted(&job.job_id.to_string(), out.envelope()))
}

#[utoipa::path(
    get,
    path = "/v1/jobs/{id}",
    params(("id" = String, Path, description = "Job id")),
    responses(
        (status = 200, body = EnvelopeDoc),
        (status = 404, body = EnvelopeDoc)
    )
)]
async fn job_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ApiEnvelope>, ApiError> {
    let out = ccc_service::job_get(&state.ctx, &id).await?;
    Ok(Json(out.envelope()))
}

#[utoipa::path(
    get,
    path = "/v1/evidence/{id}",
    params(("id" = String, Path, description = "Evidence id")),
    responses(
        (status = 200, body = EnvelopeDoc),
        (status = 404, body = EnvelopeDoc)
    )
)]
async fn evidence_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ApiEnvelope>, ApiError> {
    let out = ccc_service::evidence_show(&state.ctx, &id).await?;
    Ok(Json(out.envelope()))
}

#[utoipa::path(
    get,
    path = "/v1/evaluation/runs/{id}",
    params(("id" = String, Path, description = "Evaluation run id")),
    responses(
        (status = 200, body = EnvelopeDoc),
        (status = 404, body = EnvelopeDoc)
    )
)]
async fn eval_run_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ApiEnvelope>, ApiError> {
    let out = ccc_service::eval_show(&state.ctx, &id).await?;
    Ok(Json(out.envelope()))
}

async fn openapi_json() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(
    info(
        title = "ccc API",
        description = "Change-aware context compiler — REST /v1 (TIS §13.1). \
                       Success bodies are the CLI's versioned envelopes (ccc.schema/1); \
                       errors are ErrorEnvelope documents with stable TIS §13.2 codes."
    ),
    paths(
        health,
        repositories_post,
        repositories_get,
        repository_get,
        repository_index,
        repository_snapshots,
        context_compile,
        context_get,
        search,
        impact_analyze,
        verification_request,
        eval_run_post,
        eval_run_get,
        job_get,
        evidence_get
    ),
    components(schemas(
        RepoAddRequest,
        IndexJobRequest,
        ContextCompileRequest,
        ImpactAnalyzeRequest,
        VerificationRequest,
        EvalRunRequest,
        SearchQuery,
        EnvelopeDoc,
        EnvelopeMetaDoc
    ))
)]
pub struct ApiDoc;

/// The complete `/v1` router with auth middleware applied.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/openapi.json", get(openapi_json))
        .route(
            "/v1/repositories",
            post(repositories_post).get(repositories_get),
        )
        .route("/v1/repositories/{id}", get(repository_get))
        .route("/v1/repositories/{id}/index", post(repository_index))
        .route("/v1/repositories/{id}/snapshots", get(repository_snapshots))
        .route("/v1/context/compile", post(context_compile))
        .route("/v1/context/{id}", get(context_get))
        .route("/v1/search", get(search))
        .route("/v1/impact/analyze", post(impact_analyze))
        .route("/v1/verification", post(verification_request))
        .route("/v1/evaluation/runs", post(eval_run_post))
        .route("/v1/evaluation/runs/{id}", get(eval_run_get))
        .route("/v1/jobs/{id}", get(job_get))
        .route("/v1/evidence/{id}", get(evidence_get))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state)
}

/// Initializes tracing with `RUST_LOG` (default `info`). Logs never carry
/// source content (TIS §18).
pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
}

/// Validates the bind target and binds the listener (separated so callers
/// like tests can discover the OS-assigned port of `127.0.0.1:0`).
pub async fn bind_listener(cfg: &ServeConfig) -> Result<tokio::net::TcpListener, CccError> {
    validate_bind(cfg.bind, cfg.token.as_deref())?;
    let listener = tokio::net::TcpListener::bind(cfg.bind)
        .await
        .map_err(|e| CccError::internal_error(format!("cannot bind {}: {e}", cfg.bind)))?;
    Ok(listener)
}

/// Binds and serves until the listener errors (T5: wired to the `ccc serve`
/// subcommand and its config precedence).
pub async fn serve(ctx: ccc_service::AppContext, cfg: ServeConfig) -> Result<(), CccError> {
    let listener = bind_listener(&cfg).await?;
    serve_listener(listener, ctx, cfg.token).await
}

/// Serves the `/v1` router on an already-bound listener.
pub async fn serve_listener(
    listener: tokio::net::TcpListener,
    ctx: ccc_service::AppContext,
    token: Option<String>,
) -> Result<(), CccError> {
    init_tracing();
    let ctx = Arc::new(ctx);
    let app = router(AppState {
        ctx: ctx.clone(),
        token,
        jobs: Arc::new(JobRunner::new(ctx)),
    });
    axum::serve(listener, app)
        .await
        .map_err(|e| CccError::internal_error(format!("server error: {e}")))?;
    Ok(())
}

/// Header names used by the API (T4 idempotency).
pub const IDEMPOTENCY_KEY_HEADER: HeaderName = HeaderName::from_static("idempotency-key");
