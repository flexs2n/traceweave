//! `ccc-mcp` — the MCP adapter (FR-094, TIS §13.5, SDD §16).
//!
//! A bounded set of **read/analysis-only** tools over `ccc-service`:
//! `repository_status`, `search_code`, `lookup_symbol`, `trace_dependencies`,
//! `compile_context`, `inspect_evidence`, `analyze_change_impact`,
//! `get_verification_results`. Mutating or execution-capable operations
//! (memory writes, indexing, verification runs) are deliberately absent —
//! they are separately permissioned work for a later step.
//!
//! Every invocation applies repository scoping (explicit argument or the
//! configured default — no cwd fallback), output caps with explicit
//! `truncated` flags, and embeds the same `Envelope` shapes as the REST API
//! so evidence semantics are shared, never re-implemented (SDD §16.1).
//! Logs never carry source content (TIS §18).

use ccc_domain::CccError;
use ccc_schema::{Envelope, SCHEMA_VERSION};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    ToolAnnotations,
};
use rmcp::{service::RequestContext, ErrorData as McpError, RoleServer, ServerHandler};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

/// Default item cap for list-shaped tool output (T4/T6 caps).
pub const MAX_HITS_DEFAULT: usize = 20;
/// Hard item cap: larger client limits are clamped to this (T6).
pub const MAX_HITS_HARD: usize = 50;
/// Default compile budget for `compile_context`.
pub const DEFAULT_BUDGET_TOKENS: u64 = 4_000;
/// Hard budget cap; a larger request is rejected before compiling.
pub const MAX_BUDGET_TOKENS: u64 = 16_000;
/// Serialized-result byte cap; larger results have list fields trimmed with
/// an explicit `truncated` flag, and are refused if they still do not fit.
pub const MAX_OUTPUT_BYTES: usize = 1_000_000;
/// Default trace depth for `trace_dependencies` (1..=5).
pub const DEFAULT_TRACE_DEPTH: u32 = 3;

/// The exact tool surface — pinned by a test so nothing execution-capable or
/// memory-mutating can quietly appear here (TIS §13.5).
pub const TOOL_NAMES: [&str; 8] = [
    "repository_status",
    "search_code",
    "lookup_symbol",
    "trace_dependencies",
    "compile_context",
    "inspect_evidence",
    "analyze_change_impact",
    "get_verification_results",
];

/// The stdio MCP server over one `ccc-service` context.
pub struct CccMcp {
    ctx: Arc<ccc_service::AppContext>,
    default_repo: Option<String>,
    output_cap: usize,
}

impl CccMcp {
    pub fn new(ctx: ccc_service::AppContext, default_repo: Option<String>) -> Self {
        Self {
            ctx: Arc::new(ctx),
            default_repo,
            output_cap: MAX_OUTPUT_BYTES,
        }
    }

    /// Overrides the serialized-result byte cap (tests, profiled deployments).
    pub fn with_output_cap(mut self, cap: usize) -> Self {
        self.output_cap = cap;
        self
    }

    fn app(&self) -> &ccc_service::AppContext {
        &self.ctx
    }

    /// Repository scoping for every tool: explicit argument or configured
    /// default; unknown ids/paths are tool-level errors (never a silent
    /// fallback to cwd).
    async fn resolve_repo(
        &self,
        arg: Option<&str>,
    ) -> Result<ccc_domain::RepositoryRecord, CallToolResult> {
        let repo_ref = arg
            .map(str::to_string)
            .or_else(|| self.default_repo.clone())
            .ok_or_else(|| {
                tool_error(&CccError::invalid_request(
                    "a repository (id or path) is required: pass `repo` or configure a default",
                ))
            })?;
        ccc_service::resolve_repo_ref(&self.ctx.store, &repo_ref)
            .await
            .map_err(|e| tool_error(&e))
    }
}

fn tool_error(err: &CccError) -> CallToolResult {
    let payload = serde_json::json!({
        "schema_version": SCHEMA_VERSION,
        "error": {
            "code": err.code.as_str(),
            "message": err.message,
        },
    });
    let mut result = CallToolResult::structured_error(payload);
    if result.content.is_empty() {
        result = CallToolResult::error(vec![ContentBlock::text(format!(
            "{}: {}",
            err.code.as_str(),
            err.message
        ))]);
    }
    result
}

fn method_not_found(name: &str) -> McpError {
    McpError::invalid_params(format!("unknown tool {name:?}"), None)
}

fn internal(msg: String) -> McpError {
    McpError::internal_error(msg, None)
}

/// Args of one tool call as a typed struct (tool-level error on bad input).
fn parse_args<T: for<'de> Deserialize<'de>>(
    params: &CallToolRequestParams,
) -> Result<T, CallToolResult> {
    let args = params.arguments.clone().unwrap_or_default();
    let value = Value::Object(args);
    serde_json::from_value(value)
        .map_err(|e| tool_error(&CccError::invalid_request(format!("bad arguments: {e}"))))
}

fn clamp_limit(limit: Option<u32>) -> usize {
    limit
        .map(|l| l as usize)
        .unwrap_or(MAX_HITS_DEFAULT)
        .min(MAX_HITS_HARD)
}

/// Success response: the same envelope shape as REST (`ccc.schema/1`) with an
/// explicit `truncated` flag; list fields are trimmed (then flagged, never
/// silently byte-chopped) when the serialized result exceeds the cap.
fn finalize(envelope: serde_json::Value, output_cap: usize) -> Result<CallToolResponse, McpError> {
    let mut truncated = false;
    let mut value = envelope;
    for _ in 0..32 {
        let size = serde_json::to_string(&value)
            .map_err(|e| internal(format!("serialize: {e}")))?
            .len();
        if size <= output_cap {
            value["truncated"] = Value::Bool(truncated);
            return Ok(CallToolResult::structured(value).into());
        }
        if !trim_largest_list(&mut value) {
            return Err(internal(format!(
                "result exceeds the {output_cap}-byte output cap even after trimming; narrow the query"
            )));
        }
        truncated = true;
    }
    Err(internal(
        "result could not be trimmed under the output cap".into(),
    ))
}

/// Halves the largest known list field under `data`. Returns false when no
/// trimmable list remains.
fn trim_largest_list(value: &mut Value) -> bool {
    const LIST_KEYS: [&str; 12] = [
        "hits",
        "items",
        "symbols",
        "edges",
        "checks",
        "verifications",
        "affected_artifacts",
        "excluded_files",
        "unsupported_files",
        "added",
        "removed",
        "changed",
    ];
    let Some(data) = value.get_mut("data") else {
        return false;
    };
    let mut best: Option<(&str, usize)> = None;
    for key in LIST_KEYS {
        if let Some(arr) = data.get_mut(key).and_then(Value::as_array_mut) {
            if arr.len() > 1 {
                let len = arr.len();
                if best.is_none() || len > best.unwrap().1 {
                    best = Some((key, len));
                }
            }
        }
    }
    match best {
        Some((key, len)) => {
            let keep = (len / 2).max(1);
            if let Some(arr) = data.get_mut(key).and_then(Value::as_array_mut) {
                arr.truncate(keep);
            }
            true
        }
        None => false,
    }
}

fn envelope_value(out: &ccc_service::OpOutput) -> Result<serde_json::Value, CallToolResult> {
    serde_json::to_value(Envelope {
        schema_version: SCHEMA_VERSION.into(),
        data: out.data.clone(),
        meta: Some(out.meta.clone()),
    })
    .map_err(|e| {
        tool_error(&CccError::internal_error(format!(
            "serialize envelope: {e}"
        )))
    })
}

fn schema_object(properties: serde_json::Value, required: &[&str]) -> Arc<JsonObject> {
    let mut schema = serde_json::Map::new();
    schema.insert("type".into(), Value::String("object".into()));
    schema.insert("properties".into(), properties);
    if !required.is_empty() {
        schema.insert(
            "required".into(),
            Value::Array(
                required
                    .iter()
                    .map(|r| Value::String(r.to_string()))
                    .collect(),
            ),
        );
    }
    Arc::new(schema)
}

fn read_only_tool(name: &'static str, description: &'static str, schema: Arc<JsonObject>) -> Tool {
    Tool::new(name, description, schema).annotate(ToolAnnotations::new().read_only(true))
}

#[derive(Debug, Deserialize)]
struct RepoArgs {
    repo: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchArgs {
    repo: String,
    query: String,
    limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct SymbolArgs {
    repo: String,
    symbol: String,
    limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct TraceArgs {
    repo: String,
    symbol: String,
    depth: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct CompileArgs {
    repo: String,
    task: String,
    budget: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct EvidenceArgs {
    evidence_id: String,
}

#[derive(Debug, Deserialize)]
struct ImpactArgs {
    repo: String,
    base: String,
    head: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VerificationArgs {
    repo: Option<String>,
    verification_id: Option<String>,
}

impl ServerHandler for CccMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("ccc-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Change-aware context compiler — read/analysis tools over verified, \
                 provenance-carrying context. All tools are read-only; they never mutate \
                 memory, execute commands, or run verifications.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let repo_prop = || {
            serde_json::json!({
                "repo": { "type": "string", "description": "repository id or path" }
            })
        };
        let mut tools = Vec::new();
        tools.push(read_only_tool(
            "repository_status",
            "Index status of one repository: active generation, counters, exclusions, and whether the index is current (FR-010 drift projection in meta).",
            schema_object(repo_prop(), &[]),
        ));
        tools.push(read_only_tool(
            "search_code",
            "Lexical + symbol search with exact matches ranked first.",
            schema_object(
                serde_json::json!({
                    "repo": { "type": "string", "description": "repository id or path" },
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_HITS_HARD as u64 }
                }),
                &["repo", "query"],
            ),
        ));
        tools.push(read_only_tool(
            "lookup_symbol",
            "Exact symbol lookup (filters search to exact name matches).",
            schema_object(
                serde_json::json!({
                    "repo": { "type": "string" },
                    "symbol": { "type": "string", "description": "qualified name" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_HITS_HARD as u64 }
                }),
                &["repo", "symbol"],
            ),
        ));
        tools.push(read_only_tool(
            "trace_dependencies",
            "Breadth-first trace of outgoing dependency edges from a symbol (bounded depth 1..=5).",
            schema_object(
                serde_json::json!({
                    "repo": { "type": "string" },
                    "symbol": { "type": "string", "description": "qualified name" },
                    "depth": { "type": "integer", "minimum": 1, "maximum": 5 }
                }),
                &["repo", "symbol"],
            ),
        ));
        tools.push(read_only_tool(
            "compile_context",
            "Compile a token-budgeted, provenance-carrying context bundle for a task.",
            schema_object(
                serde_json::json!({
                    "repo": { "type": "string" },
                    "task": { "type": "string" },
                    "budget": { "type": "integer", "minimum": 1, "maximum": MAX_BUDGET_TOKENS }
                }),
                &["repo", "task"],
            ),
        ));
        tools.push(read_only_tool(
            "inspect_evidence",
            "Show one evidence record with full provenance.",
            schema_object(
                serde_json::json!({
                    "evidence_id": { "type": "string" }
                }),
                &["evidence_id"],
            ),
        ));
        tools.push(read_only_tool(
            "analyze_change_impact",
            "Analyze base → head (head omitted: working tree) and report affected artifacts and blind spots.",
            schema_object(
                serde_json::json!({
                    "repo": { "type": "string" },
                    "base": { "type": "string" },
                    "head": { "type": "string" }
                }),
                &["repo", "base"],
            ),
        ));
        tools.push(read_only_tool(
            "get_verification_results",
            "Read recorded verification results: latest reports for a repository, or one report with its check rows by id. This never runs a verification.",
            schema_object(
                serde_json::json!({
                    "repo": { "type": "string" },
                    "verification_id": { "type": "string" }
                }),
                &[],
            ),
        ));
        Ok(ListToolsResult {
            next_cursor: None,
            tools,
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        params: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let name: &str = &params.name;
        let envelope = match name {
            "repository_status" => self.tool_repository_status(&params).await,
            "search_code" => self.tool_search_code(&params).await,
            "lookup_symbol" => self.tool_lookup_symbol(&params).await,
            "trace_dependencies" => self.tool_trace_dependencies(&params).await,
            "compile_context" => self.tool_compile_context(&params).await,
            "inspect_evidence" => self.tool_inspect_evidence(&params).await,
            "analyze_change_impact" => self.tool_analyze_change_impact(&params).await,
            "get_verification_results" => self.tool_verification_results(&params).await,
            other => return Err(method_not_found(other)),
        };
        match envelope {
            Ok(value) => finalize(value, self.output_cap),
            // Tool-level failure: the caller sees the stable error code.
            Err(tool_error) => Ok(tool_error.into()),
        }
    }
}

impl CccMcp {
    async fn tool_repository_status(
        &self,
        params: &CallToolRequestParams,
    ) -> Result<Value, CallToolResult> {
        let args: RepoArgs = parse_args(params)?;
        let repo = self.resolve_repo(args.repo.as_deref()).await?;
        let status = ccc_service::status_record(self.app(), &repo)
            .await
            .map_err(|e| tool_error(&e))?;
        envelope_value(&status)
    }

    async fn tool_search_code(
        &self,
        params: &CallToolRequestParams,
    ) -> Result<Value, CallToolResult> {
        let args: SearchArgs = parse_args(params)?;
        let repo = self.resolve_repo(Some(&args.repo)).await?;
        let search = ccc_service::search_record(
            self.app(),
            &repo,
            &args.query,
            clamp_limit(args.limit) as u32,
        )
        .await
        .map_err(|e| tool_error(&e))?;
        envelope_value(&search)
    }

    async fn tool_lookup_symbol(
        &self,
        params: &CallToolRequestParams,
    ) -> Result<Value, CallToolResult> {
        let args: SymbolArgs = parse_args(params)?;
        let repo = self.resolve_repo(Some(&args.repo)).await?;
        let search = ccc_service::search_record(
            self.app(),
            &repo,
            &args.symbol,
            clamp_limit(args.limit) as u32,
        )
        .await
        .map_err(|e| tool_error(&e))?;
        // Exact-name semantics: lexical hits are not symbol lookups.
        let mut value = envelope_value(&search)?;
        if let Some(hits) = value
            .pointer_mut("/data/hits")
            .and_then(Value::as_array_mut)
        {
            hits.retain(|h| h["match_class"] == "exact");
        }
        Ok(value)
    }

    async fn tool_trace_dependencies(
        &self,
        params: &CallToolRequestParams,
    ) -> Result<Value, CallToolResult> {
        let args: TraceArgs = parse_args(params)?;
        let depth = args.depth.unwrap_or(DEFAULT_TRACE_DEPTH);
        let trace = ccc_service::trace_dependencies(self.app(), &args.repo, &args.symbol, depth)
            .await
            .map_err(|e| tool_error(&e))?;
        envelope_value(&trace)
    }

    async fn tool_compile_context(
        &self,
        params: &CallToolRequestParams,
    ) -> Result<Value, CallToolResult> {
        let args: CompileArgs = parse_args(params)?;
        let budget = args.budget.unwrap_or(DEFAULT_BUDGET_TOKENS);
        if budget > MAX_BUDGET_TOKENS {
            // Refuse BEFORE compiling (T6 gate).
            return Err(tool_error(&CccError::invalid_request(format!(
                "budget {budget} exceeds the {MAX_BUDGET_TOKENS}-token cap"
            ))));
        }
        let repo = self.resolve_repo(Some(&args.repo)).await?;
        let compile =
            ccc_service::context_compile_record(self.app(), &repo, &args.task, budget, "json")
                .await
                .map_err(|e| tool_error(&e))?;
        let mut value = envelope_value(&compile)?;
        // Drift projection rides in meta only (FR-010) — bundle DTO bytes untouched.
        if let Ok(current) = ccc_service::index_current(&self.ctx.store, &repo).await {
            value["meta"]["index_current"] = Value::Bool(current);
        }
        Ok(value)
    }

    async fn tool_inspect_evidence(
        &self,
        params: &CallToolRequestParams,
    ) -> Result<Value, CallToolResult> {
        let args: EvidenceArgs = parse_args(params)?;
        let evidence = ccc_service::evidence_show(self.app(), &args.evidence_id)
            .await
            .map_err(|e| tool_error(&e))?;
        envelope_value(&evidence)
    }

    async fn tool_analyze_change_impact(
        &self,
        params: &CallToolRequestParams,
    ) -> Result<Value, CallToolResult> {
        let args: ImpactArgs = parse_args(params)?;
        let repo = self.resolve_repo(Some(&args.repo)).await?;
        let impact = ccc_service::impact_analyze_record(
            self.app(),
            &repo,
            &args.base,
            args.head.as_deref(),
            "json",
        )
        .await
        .map_err(|e| tool_error(&e))?;
        envelope_value(&impact)
    }

    async fn tool_verification_results(
        &self,
        params: &CallToolRequestParams,
    ) -> Result<Value, CallToolResult> {
        let args: VerificationArgs = parse_args(params)?;
        let results = ccc_service::verification_results(
            self.app(),
            args.repo.as_deref(),
            args.verification_id.as_deref(),
        )
        .await
        .map_err(|e| tool_error(&e))?;
        envelope_value(&results)
    }
}

/// Runs the stdio MCP server until the client disconnects (T7: wired to
/// `ccc mcp`).
pub async fn run_stdio(
    ctx: ccc_service::AppContext,
    default_repo: Option<String>,
) -> Result<(), CccError> {
    let server = rmcp::serve_server(CccMcp::new(ctx, default_repo), rmcp::transport::stdio())
        .await
        .map_err(|e| map_service_error(e, "mcp server startup"))?;
    server
        .waiting()
        .await
        .map_err(|e| map_service_error(e, "mcp server"))?;
    Ok(())
}

fn map_service_error(e: impl std::fmt::Display, context: &str) -> CccError {
    CccError::internal_error(format!("{context}: {e}"))
}
