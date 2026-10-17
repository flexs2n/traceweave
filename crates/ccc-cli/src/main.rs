//! `ccc` — the CLI user surface (TIS §13.4).
//!
//! Human-readable output is the default; `--json` produces full,
//! never-truncated machine envelopes. Errors exit non-zero with a stable
//! error-code JSON on stderr (ui.md §3.2: no false availability).

use ccc_context_compiler::{CompileRequest, ContextCompiler, HeuristicNormalizer};
use ccc_domain::*;
use ccc_impact::{render_impact_markdown, ImpactAnalyzer};
use ccc_jobs as jobs;
use ccc_repository::{GitCli, GitProvider, RepositoryRegistry};
use ccc_schema::{Envelope, EnvelopeMeta, ErrorEnvelope, SCHEMA_VERSION};
use ccc_search::{FtsSearchBackend, SearchBackend, SearchRequest};
use ccc_snapshot::{find_or_create_snapshot, SnapshotBuilder};
use ccc_storage::*;
use clap::{Parser as ClapParser, Subcommand};
use serde::Deserialize;
use std::path::PathBuf;
use std::str::FromStr as _;
use std::sync::Arc;

#[derive(Debug, ClapParser)]
#[command(name = "ccc", version, about = "Change-aware context compiler (CLI)")]
struct Cli {
    /// Data directory override (default: $CCC_HOME or ~/.ccc).
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Machine-readable JSON output (full, never truncated).
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Initialize the ccc data directory and database.
    Init,
    /// Register a git repository.
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
    /// Show index status: active generation, counters, exclusions.
    Status {
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Index the current state of a registered repository.
    Index {
        /// Full rebuild (recovery path, TIS §7.5).
        #[arg(long)]
        full: bool,
        /// Re-index an existing snapshot id instead of capturing a new state.
        #[arg(long)]
        snapshot: Option<String>,
        /// Repository path (defaults to the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Lexical + symbol search.
    Search {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    Context {
        #[command(subcommand)]
        command: ContextCommand,
    },
    Evidence {
        #[command(subcommand)]
        command: EvidenceCommand,
    },
    /// Engineering memory (FR-032/033/035): create, list, search, supersede,
    /// retract, resolve. Memory is subordinate to current source; agent
    /// findings stay candidates until revalidated or approved.
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },
    /// List current conflict pairs (both sides + scopes, UI-018 analog).
    /// Conflicts are never auto-resolved.
    Conflicts {
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Change-impact analysis.
    Impact {
        #[command(subcommand)]
        command: ImpactCommand,
    },
    /// Export bundles + evidence as JSON.
    Export {
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Environment diagnostics.
    Doctor,
    /// Verify a patch under an execution policy (analysis-only by default,
    /// TIS §11; suggestions are advisory until a policy authorizes execution).
    Verify {
        /// Base revision (required).
        #[arg(long)]
        base: String,
        /// Head revision (omitted: working tree).
        #[arg(long)]
        head: Option<String>,
        /// Policy name from the data dir (default: built-in `analysis`).
        #[arg(long, default_value = "analysis")]
        policy: String,
        /// Output format: json or markdown.
        #[arg(long, default_value = "json")]
        format: String,
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Inspect verification policies (data-dir sourced; repo content can
    /// never define or override a policy, TIS §11.2).
    Policies {
        #[command(subcommand)]
        command: PoliciesCommand,
    },
    /// Offline evaluation of the retrieval pipeline against a hashed suite
    /// (SDD §20.2; deterministic, recorded immutably).
    Eval {
        #[command(subcommand)]
        command: EvalCommand,
    },
    /// Snapshot manifest comparison (added/removed/changed/renamed paths).
    Snapshot {
        #[command(subcommand)]
        command: SnapshotCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ContextCommand {
    /// Compile a token-budgeted context bundle for a task.
    Compile {
        #[arg(long)]
        task: String,
        #[arg(long)]
        budget: u64,
        /// Output format: json or markdown.
        #[arg(long, default_value = "json")]
        format: String,
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Inspect a persisted bundle.
    Show { bundle_id: String },
    /// Compare two persisted bundles (post-change context refresh: item-level
    /// added/removed/changed + evidence-status flips, SDD §11.4).
    Diff {
        /// Base bundle id.
        bundle_a: String,
        /// Target bundle id.
        bundle_b: String,
    },
}

#[derive(Debug, Subcommand)]
enum RepoCommand {
    /// Register a git repository.
    Add {
        path: PathBuf,
        /// Idempotency key: identical requests yield one logical job (TIS §13.3).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// Repository registration status.
    Status {
        /// Optional repository path; omitted lists all repositories.
        path: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum EvidenceCommand {
    /// List versioned claims of the evidence ledger, optionally filtered by
    /// status (candidate, supported, verified, stale, conflicted, ...).
    List {
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Show one evidence record with provenance.
    Show { evidence_id: String },
}

#[derive(Debug, Subcommand)]
enum MemoryCommand {
    /// Create a memory record (starts `candidate`; links optional claims).
    Add {
        #[arg(long)]
        title: String,
        #[arg(long)]
        summary: String,
        /// Memory type (e.g. failed_approach, invariant, convention).
        #[arg(long)]
        memory_type: String,
        /// Repository-relative directory scope (empty = repository-wide).
        #[arg(long, default_value = "")]
        scope: String,
        /// Author or producing process (recorded provenance).
        #[arg(long)]
        created_by: String,
        #[arg(long, default_value = "private")]
        sharing_policy: String,
        #[arg(long, default_value = "none")]
        sensitivity: String,
        /// Supporting claims to link (the memory projects their status).
        #[arg(long)]
        claim_id: Vec<String>,
        /// Idempotency key: identical requests yield one logical job.
        #[arg(long)]
        idempotency_key: Option<String>,
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// List memory records, optionally filtered by status or type.
    List {
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        memory_type: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Show one memory record with claim links and its status history.
    Show { memory_id: String },
    /// Full-text search over memory titles and summaries (FR-035).
    Search {
        query: String,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        memory_type: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Edit-as-supersede: create a new version; the old record is retired as
    /// `superseded` (history immutable, SDD §5.8).
    Supersede {
        memory_id: String,
        #[arg(long)]
        title: String,
        #[arg(long)]
        summary: String,
        #[arg(long)]
        memory_type: String,
        #[arg(long, default_value = "")]
        scope: String,
        #[arg(long)]
        created_by: String,
        #[arg(long, default_value = "private")]
        sharing_policy: String,
        #[arg(long, default_value = "none")]
        sensitivity: String,
        #[arg(long)]
        claim_id: Vec<String>,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long)]
        idempotency_key: Option<String>,
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Retract a memory (logical removal from retrieval; history preserved).
    Retract {
        memory_id: String,
        #[arg(long)]
        actor: String,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long)]
        idempotency_key: Option<String>,
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Human conflict resolution (SDD §9.4 steps 7–8): the losing memory is
    /// retracted with the rationale recorded; the kept side returns to
    /// `candidate`. Conflicts are never resolved automatically.
    Resolve {
        #[arg(long)]
        keep: String,
        #[arg(long)]
        drop: String,
        #[arg(long)]
        rationale: String,
        #[arg(long)]
        actor: String,
        #[arg(long)]
        idempotency_key: Option<String>,
        #[arg(long)]
        repo: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum ImpactCommand {
    /// Analyze base → head (head omitted = working tree).
    Analyze {
        #[arg(long)]
        base: String,
        #[arg(long)]
        head: Option<String>,
        #[arg(long, default_value = "json")]
        format: String,
        #[arg(long)]
        repo: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum EvalCommand {
    /// Run an evaluation suite (name resolved from ./eval/suites/ or the data
    /// dir; a path is accepted directly).
    Run {
        #[arg(long)]
        suite: String,
        /// Optional retrieval-policy override: a TOML file with a
        /// `[weights]` table (ADR-008 experiments; runs stay recorded).
        #[arg(long)]
        policy: Option<String>,
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Show one recorded evaluation run.
    Show { run_id: String },
}

#[derive(Debug, Subcommand)]
enum SnapshotCommand {
    /// Compare two snapshot manifests by id.
    Diff {
        /// Base snapshot id.
        a: String,
        /// Target snapshot id.
        b: String,
    },
}

#[derive(Debug, Subcommand)]
enum PoliciesCommand {
    /// List available policy names (built-in default + data-dir policies).
    List,
    /// Show one policy's effective contents and hash.
    Show { name: String },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let output_json = cli.json;
    let result = dispatch(&cli).await;
    let code = match result {
        Ok(()) => 0,
        Err(err) => {
            if output_json {
                let envelope = ErrorEnvelope::from_ccc(&err);
                eprintln!(
                    "{}",
                    serde_json::to_string(&envelope).unwrap_or_else(|_| err.to_string())
                );
            } else {
                eprintln!("ERROR {}: {}", err.code.as_str(), err.message);
            }
            1
        }
    };
    std::process::exit(code);
}

async fn dispatch(cli: &Cli) -> Result<(), CccError> {
    let data_dir = ccc_config::resolve_data_dir(cli.data_dir.as_deref());
    match &cli.command {
        Command::Init => cmd_init(&data_dir, cli.json).await,
        Command::Doctor => cmd_doctor(&data_dir, cli.json).await,
        Command::Policies { command } => match command {
            PoliciesCommand::List => cmd_policies_list(&data_dir, cli.json).await,
            PoliciesCommand::Show { name } => cmd_policies_show(&data_dir, name, cli.json).await,
        },
        // Everything below requires an initialized store.
        _ => {
            let store = open_store(&data_dir).await?;
            let artifacts = FsArtifactStore::new(data_dir.join("blobs"), store.clone());
            match &cli.command {
                Command::Repo { command } => match command {
                    RepoCommand::Add {
                        path,
                        idempotency_key,
                    } => {
                        cmd_repo_add(
                            &store,
                            &artifacts,
                            path,
                            idempotency_key.as_deref(),
                            cli.json,
                        )
                        .await
                    }
                    RepoCommand::Status { path } => {
                        cmd_repo_status(&store, path.as_deref(), cli.json).await
                    }
                },
                Command::Index {
                    full,
                    snapshot,
                    repo,
                } => {
                    cmd_index(
                        &store,
                        &artifacts,
                        repo.as_deref(),
                        *full,
                        snapshot.as_deref(),
                        cli.json,
                    )
                    .await
                }
                Command::Status { repo } => cmd_status(&store, repo.as_deref(), cli.json).await,
                Command::Search { query, limit, repo } => {
                    cmd_search(&store, repo.as_deref(), query, *limit, cli.json).await
                }
                Command::Context { command } => match command {
                    ContextCommand::Compile {
                        task,
                        budget,
                        format,
                        repo,
                    } => {
                        cmd_context_compile(
                            &store,
                            &artifacts,
                            repo.as_deref(),
                            task,
                            *budget,
                            format,
                            cli.json,
                        )
                        .await
                    }
                    ContextCommand::Show { bundle_id } => {
                        cmd_context_show(&store, bundle_id, cli.json).await
                    }
                    ContextCommand::Diff { bundle_a, bundle_b } => {
                        cmd_context_diff(&store, bundle_a, bundle_b, cli.json).await
                    }
                },
                Command::Evidence { command } => match command {
                    EvidenceCommand::List { status, limit } => {
                        cmd_evidence_list(&store, status.as_deref(), *limit, cli.json).await
                    }
                    EvidenceCommand::Show { evidence_id } => {
                        cmd_evidence_show(&store, evidence_id, cli.json).await
                    }
                },
                Command::Memory { command } => match command {
                    MemoryCommand::Add {
                        title,
                        summary,
                        memory_type,
                        scope,
                        created_by,
                        sharing_policy,
                        sensitivity,
                        claim_id,
                        idempotency_key,
                        repo,
                    } => {
                        cmd_memory_add(
                            &store,
                            repo.as_deref(),
                            title,
                            summary,
                            memory_type,
                            scope,
                            created_by,
                            sharing_policy,
                            sensitivity,
                            claim_id,
                            idempotency_key.as_deref(),
                            cli.json,
                        )
                        .await
                    }
                    MemoryCommand::List {
                        status,
                        memory_type,
                        limit,
                        repo,
                    } => {
                        cmd_memory_list(
                            &store,
                            repo.as_deref(),
                            status.as_deref(),
                            memory_type.as_deref(),
                            *limit,
                            cli.json,
                        )
                        .await
                    }
                    MemoryCommand::Show { memory_id } => {
                        cmd_memory_show(&store, memory_id, cli.json).await
                    }
                    MemoryCommand::Search {
                        query,
                        status,
                        memory_type,
                        limit,
                        repo,
                    } => {
                        cmd_memory_search(
                            &store,
                            repo.as_deref(),
                            query,
                            status.as_deref(),
                            memory_type.as_deref(),
                            *limit,
                            cli.json,
                        )
                        .await
                    }
                    MemoryCommand::Supersede {
                        memory_id,
                        title,
                        summary,
                        memory_type,
                        scope,
                        created_by,
                        sharing_policy,
                        sensitivity,
                        claim_id,
                        reason,
                        idempotency_key,
                        repo,
                    } => {
                        cmd_memory_supersede(
                            &store,
                            repo.as_deref(),
                            memory_id,
                            title,
                            summary,
                            memory_type,
                            scope,
                            created_by,
                            sharing_policy,
                            sensitivity,
                            claim_id,
                            reason.as_deref(),
                            idempotency_key.as_deref(),
                            cli.json,
                        )
                        .await
                    }
                    MemoryCommand::Retract {
                        memory_id,
                        actor,
                        reason,
                        idempotency_key,
                        repo,
                    } => {
                        cmd_memory_retract(
                            &store,
                            repo.as_deref(),
                            memory_id,
                            actor,
                            reason.as_deref(),
                            idempotency_key.as_deref(),
                            cli.json,
                        )
                        .await
                    }
                    MemoryCommand::Resolve {
                        keep,
                        drop,
                        rationale,
                        actor,
                        idempotency_key,
                        repo,
                    } => {
                        cmd_memory_resolve(
                            &store,
                            repo.as_deref(),
                            keep,
                            drop,
                            rationale,
                            actor,
                            idempotency_key.as_deref(),
                            cli.json,
                        )
                        .await
                    }
                },
                Command::Conflicts { repo } => {
                    cmd_conflicts(&store, repo.as_deref(), cli.json).await
                }
                Command::Impact { command } => match command {
                    ImpactCommand::Analyze {
                        base,
                        head,
                        format,
                        repo,
                    } => {
                        cmd_impact_analyze(
                            &store,
                            &artifacts,
                            repo.as_deref(),
                            base,
                            head.as_deref(),
                            format,
                            cli.json,
                        )
                        .await
                    }
                },
                Command::Export { output, repo } => {
                    cmd_export(&store, repo.as_deref(), output, cli.json).await
                }
                Command::Eval { command } => match command {
                    EvalCommand::Run {
                        suite,
                        policy,
                        repo,
                    } => {
                        cmd_eval_run(
                            &store,
                            &artifacts,
                            &data_dir,
                            repo.as_deref(),
                            suite,
                            policy.as_deref(),
                            cli.json,
                        )
                        .await
                    }
                    EvalCommand::Show { run_id } => cmd_eval_show(&store, run_id, cli.json).await,
                },
                Command::Snapshot { command } => match command {
                    SnapshotCommand::Diff { a, b } => {
                        cmd_snapshot_diff(&store, a, b, cli.json).await
                    }
                },
                Command::Verify {
                    base,
                    head,
                    policy,
                    format,
                    repo,
                } => {
                    cmd_verify(
                        &store,
                        &artifacts,
                        &data_dir,
                        repo.as_deref(),
                        base,
                        head.as_deref(),
                        policy,
                        format,
                        cli.json,
                    )
                    .await
                }
                _ => unreachable!("handled above"),
            }
        }
    }
}

async fn open_store(data_dir: &std::path::Path) -> Result<Arc<SqliteStore>, CccError> {
    if !data_dir.exists() {
        return Err(CccError::invalid_request(format!(
            "ccc is not initialized in {}; run `ccc init` first",
            data_dir.display()
        )));
    }
    let store = SqliteStore::connect(&data_dir.join("ccc.db")).await?;
    Ok(Arc::new(store))
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<(), CccError> {
    let text = serde_json::to_string(value)
        .map_err(|e| CccError::internal_error(format!("json serialization: {e}")))?;
    println!("{text}");
    Ok(())
}

fn envelope<T: serde::Serialize>(data: T, meta: EnvelopeMeta) -> Result<Envelope<T>, CccError> {
    Ok(Envelope {
        schema_version: SCHEMA_VERSION.into(),
        data,
        meta: Some(meta),
    })
}

fn meta_for(
    repo: &RepositoryRecord,
    snapshot: Option<SnapshotId>,
    generation: Option<GenerationId>,
) -> EnvelopeMeta {
    EnvelopeMeta {
        repository_id: Some(repo.repository_id.to_string()),
        snapshot_id: snapshot.map(|s| s.to_string()),
        generation_id: generation.map(|g| g.to_string()),
        request_id: None,
    }
}

// ---------------------------------------------------------------------------
// init / doctor
// ---------------------------------------------------------------------------

async fn cmd_init(data_dir: &std::path::Path, json: bool) -> Result<(), CccError> {
    std::fs::create_dir_all(data_dir)
        .map_err(|e| CccError::internal_error(format!("cannot create data dir: {e}")))?;
    let config_path = ccc_config::write_default_config(data_dir)?;
    let _store = SqliteStore::connect(&data_dir.join("ccc.db")).await?;
    if json {
        print_json(&serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "data_dir": data_dir.to_string_lossy(),
            "config_path": config_path.to_string_lossy(),
            "initialized": true
        }))?;
    } else {
        println!("Initialized ccc at {}", data_dir.display());
        println!("Config: {}", config_path.display());
    }
    Ok(())
}

struct DoctorCheck {
    name: &'static str,
    ok: bool,
    detail: String,
}

async fn cmd_doctor(data_dir: &std::path::Path, json: bool) -> Result<(), CccError> {
    let mut checks: Vec<DoctorCheck> = Vec::new();

    // Data dir writable.
    let writable = std::fs::create_dir_all(data_dir).is_ok()
        && std::fs::write(data_dir.join(".doctor-probe"), b"ok").is_ok();
    let _ = std::fs::remove_file(data_dir.join(".doctor-probe"));
    checks.push(DoctorCheck {
        name: "data_dir_writable",
        ok: writable,
        detail: data_dir.to_string_lossy().into_owned(),
    });

    // Git on PATH (Windows-relevant; plan risk R2).
    let git_ok = std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    checks.push(DoctorCheck {
        name: "git_on_path",
        ok: git_ok,
        detail: if git_ok {
            "git available".into()
        } else {
            "git executable not found".into()
        },
    });

    // Database + migrations.
    match SqliteStore::connect(&data_dir.join("ccc.db")).await {
        Ok(_) => checks.push(DoctorCheck {
            name: "database",
            ok: true,
            detail: "opened and migrated".into(),
        }),
        Err(e) => checks.push(DoctorCheck {
            name: "database",
            ok: false,
            detail: e.message,
        }),
    }

    // Tree-sitter grammar.
    let mut extractor = ccc_tree_sitter::RustExtractor::new();
    let grammar_ok = !extractor.extract(b"fn ok() {}").parse_error;
    checks.push(DoctorCheck {
        name: "tree_sitter_rust",
        ok: grammar_ok,
        detail: if grammar_ok {
            "grammar parses".into()
        } else {
            "grammar check failed".into()
        },
    });

    let all_ok = checks.iter().all(|c| c.ok);
    if json {
        print_json(&serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "ok": all_ok,
            "checks": checks.iter().map(|c| serde_json::json!({
                "name": c.name, "ok": c.ok, "detail": c.detail
            })).collect::<Vec<_>>()
        }))?;
    } else {
        for c in &checks {
            println!(
                "{:<20} {} — {}",
                c.name,
                if c.ok { "OK  " } else { "FAIL" },
                c.detail
            );
        }
    }
    if !all_ok {
        return Err(CccError::dependency_unavailable(
            "one or more doctor checks failed",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// repo resolution helper
// ---------------------------------------------------------------------------

async fn resolve_repo(
    store: &SqliteStore,
    repo: Option<&std::path::Path>,
) -> Result<RepositoryRecord, CccError> {
    let repos = store.list_repositories().await?;
    if let Some(path) = repo {
        let canonical = std::fs::canonicalize(path)
            .map_err(|e| CccError::invalid_request(format!("cannot resolve path: {e}")))?;
        let normalized = canonical.to_string_lossy().replace('\\', "/");
        if let Some(found) = store.find_repository_by_path(&normalized).await? {
            return Ok(found);
        }
        // Also accept a raw id.
        if let Ok(id) = RepositoryId::from_str(&normalized) {
            if let Some(found) = store.get_repository(id).await? {
                return Ok(found);
            }
        }
        return Err(CccError::repository_not_found(format!(
            "no registered repository at {normalized}"
        )));
    }
    // Default: current directory match, else the single registered repo.
    let cwd = std::env::current_dir()
        .map_err(|e| CccError::internal_error(format!("cannot read cwd: {e}")))?;
    let cwd_norm = cwd.to_string_lossy().replace('\\', "/");
    if let Some(found) = store.find_repository_by_path(&cwd_norm).await? {
        return Ok(found);
    }
    if repos.len() == 1 {
        return Ok(repos[0].clone());
    }
    Err(CccError::repository_not_found(
        "no repository specified and none matches the current directory; use --repo <path>",
    ))
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

async fn cmd_repo_add(
    store: &Arc<SqliteStore>,
    artifacts: &dyn ArtifactStore,
    path: &std::path::Path,
    idempotency_key: Option<&str>,
    json: bool,
) -> Result<(), CccError> {
    let _ = artifacts;
    let cfg = ccc_config::load_config(&ccc_config::resolve_data_dir(None))?;
    let config_hash = cfg.indexing().configuration_hash()?;

    // Registration itself is idempotent per root path; the job ledger then
    // guarantees one logical job per idempotency key (TIS §13.3).
    let registry = RepositoryRegistry::new(&**store, &GitCli);
    let record = registry.register(path, &config_hash).await?;
    let key = idempotency_key
        .map(str::to_string)
        .unwrap_or_else(|| jobs::idempotency_key(&["repo_add", &record.root_path]));
    let job = jobs::begin_job(&**store, record.repository_id, "repo_add", &key, None).await?;
    let job = if job.state.is_terminal() {
        // Idempotent replay: the logical job already completed.
        job
    } else {
        jobs::finish_success(&**store, job, Some(&record.repository_id.to_string())).await?
    };

    if json {
        print_json(&envelope(
            &serde_json::json!({
                "repository": record,
                "job_id": job.job_id.to_string(),
                "idempotent_replay": job.created_at != job.updated_at,
            }),
            meta_for(&record, None, None),
        )?)?;
    } else {
        println!(
            "Registered repository {} at {} (job {})",
            record.repository_id, record.root_path, job.job_id
        );
    }
    Ok(())
}

async fn cmd_repo_status(
    store: &SqliteStore,
    path: Option<&std::path::Path>,
    json: bool,
) -> Result<(), CccError> {
    let repos = match path {
        Some(p) => vec![resolve_repo(store, Some(p)).await?],
        None => store.list_repositories().await?,
    };
    if json {
        print_json(&envelope(&repos, EnvelopeMeta::default())?)?;
    } else if repos.is_empty() {
        println!("No repositories registered. Use `ccc repo add <path>`.");
    } else {
        for r in &repos {
            let active = store.get_active_generation(r.repository_id).await?;
            println!(
                "{} — {} [{}]",
                r.repository_id,
                r.root_path,
                active
                    .map(|g| format!("generation {}", g.generation_id))
                    .unwrap_or_else(|| "not indexed".into())
            );
        }
    }
    Ok(())
}

async fn cmd_index(
    store: &Arc<SqliteStore>,
    artifacts: &FsArtifactStore,
    repo_path: Option<&std::path::Path>,
    full: bool,
    snapshot_id: Option<&str>,
    json: bool,
) -> Result<(), CccError> {
    let repo = resolve_repo(store, repo_path).await?;
    let cfg = ccc_config::load_config(&ccc_config::resolve_data_dir(None))?;
    // The idempotency key includes the current repository state (HEAD + dirty
    // flag): re-running against the same tree replays one logical job, while
    // a changed tree is a genuinely new request.
    let root = PathBuf::from(&repo.root_path);
    let head = GitCli.rev_parse_head(&root)?.unwrap_or_default();
    let dirty = !GitCli.status_porcelain(&root)?.is_empty();
    let key = jobs::idempotency_key(&[
        "index",
        &repo.repository_id.to_string(),
        &format!("{full}"),
        snapshot_id.unwrap_or("current"),
        &cfg.indexing().configuration_hash()?,
        &head,
        &format!("{dirty}"),
    ]);
    let job = jobs::begin_job(&**store, repo.repository_id, "index", &key, None).await?;

    let result = index_once(store, artifacts, &repo, &cfg, full, snapshot_id).await;
    match result {
        Ok(outcome) => {
            // Freshness projection (FR-043): memories linked to claims that
            // the reindex invalidated flip `stale` with a recorded cause;
            // unrelated memories stay untouched.
            let memory_freshness =
                ccc_memory::project_claim_freshness(&**store, &**store, repo.repository_id)
                    .await
                    .ok();
            let payload = serde_json::to_string(&outcome).unwrap_or_else(|_| "{}".into());
            let _job = if job.state.is_terminal() {
                // Idempotent replay: the logical job already completed.
                job
            } else {
                jobs::finish_success(&**store, job, Some(&payload)).await?
            };
            if json {
                print_json(&envelope(
                    &serde_json::json!({
                        "snapshot_id": outcome.snapshot_id.to_string(),
                        "generation_id": outcome.generation_id.to_string(),
                        "counters": outcome.counters,
                        "memories_flipped_stale": memory_freshness
                            .as_ref()
                            .map(|s| s.stale_memories.len())
                            .unwrap_or(0),
                    }),
                    meta_for(
                        &repo,
                        Some(outcome.snapshot_id),
                        Some(outcome.generation_id),
                    ),
                )?)?;
            } else {
                println!("Snapshot {}", outcome.snapshot_id);
                println!("Generation {} published", outcome.generation_id);
                println!(
                    "Parsed: {}, reused: {}, excluded: {} (unsupported: {}), failed: {}",
                    outcome.counters.parsed,
                    outcome.counters.reused,
                    outcome.counters.excluded,
                    outcome.counters.unsupported,
                    outcome.counters.failed
                );
                if let Some(freshness) = &memory_freshness {
                    if !freshness.stale_memories.is_empty()
                        || !freshness.conflicted_memories.is_empty()
                    {
                        println!(
                            "Memory freshness: {} stale, {} conflicted (linked claims \
                             invalidated; see `ccc memory list --status stale`)",
                            freshness.stale_memories.len(),
                            freshness.conflicted_memories.len()
                        );
                    }
                }
            }
            Ok(())
        }
        Err(e) => {
            jobs::finish_failure(&**store, job, &e).await?;
            Err(e)
        }
    }
}

async fn index_once(
    store: &Arc<SqliteStore>,
    artifacts: &FsArtifactStore,
    repo: &RepositoryRecord,
    cfg: &ccc_config::CccConfig,
    full: bool,
    snapshot_id: Option<&str>,
) -> Result<ccc_indexer::IndexOutcome, CccError> {
    let root = PathBuf::from(&repo.root_path);
    let discovery_started = std::time::Instant::now();
    let (snapshot, manifest) = match snapshot_id {
        Some(id) => {
            let sid = SnapshotId::from_str(id)
                .map_err(|_| CccError::invalid_request(format!("invalid snapshot id {id:?}")))?;
            let snapshot = store
                .get_snapshot(sid)
                .await?
                .ok_or_else(|| CccError::snapshot_not_found(format!("snapshot {id} not found")))?;
            // Rebuild the manifest from stored file hashes.
            let files = store.list_files(sid).await?;
            let mut manifest = std::collections::BTreeMap::new();
            for f in files {
                manifest.insert(f.path.clone(), f.content_hash.clone());
            }
            (snapshot, manifest)
        }
        None => {
            let builder = SnapshotBuilder::new(&GitCli);
            let draft = builder.build_working_tree(
                repo.repository_id,
                &root,
                &cfg.indexing().configuration_hash()?,
                cfg.indexing().include_untracked,
            )?;
            let manifest = draft.manifest.clone();
            let snapshot = find_or_create_snapshot(&**store, draft).await?;
            (snapshot, manifest)
        }
    };
    let discovery_ms = discovery_started.elapsed().as_millis() as u64;
    // Production evidence goes to the SQLite store.
    let outcome = ccc_indexer::run_index(
        &**store,
        &**store,
        &**store,
        artifacts,
        &**store,
        repo,
        &snapshot,
        &root,
        &manifest,
        cfg.indexing(),
        full,
    )
    .await?;
    // Discovery happens in the snapshot build above; record it alongside the
    // in-pipeline stage timings (TIS §18).
    let mut outcome = outcome;
    outcome.counters.timings_ms.discovery_ms = discovery_ms;
    Ok(outcome)
}

async fn cmd_status(
    store: &SqliteStore,
    repo_path: Option<&std::path::Path>,
    json: bool,
) -> Result<(), CccError> {
    let repo = resolve_repo(store, repo_path).await?;
    let active = store.get_active_generation(repo.repository_id).await?;
    let mut status = serde_json::json!({
        "repository_id": repo.repository_id.to_string(),
        "root_path": repo.root_path,
        "limitations": repo.limitations,
        "indexed": active.is_some(),
    });
    let mut human = String::new();
    if let Some(gen) = &active {
        let counters: serde_json::Value =
            serde_json::from_str(&gen.counters_json).unwrap_or(serde_json::json!({}));
        let files = store.list_files(gen.snapshot_id).await?;
        let excluded: Vec<&FileRecord> = files
            .iter()
            .filter(|f| f.inclusion == FileInclusion::Excluded)
            .collect();
        let unsupported: Vec<&FileRecord> = excluded
            .iter()
            .filter(|f| f.exclusion_reason == Some(ExclusionReason::UnsupportedLanguage))
            .copied()
            .collect();
        status["snapshot_id"] = serde_json::json!(gen.snapshot_id.to_string());
        status["generation_id"] = serde_json::json!(gen.generation_id.to_string());
        status["counters"] = counters.clone();
        status["excluded_files"] = serde_json::json!(excluded.iter().map(|f| serde_json::json!({
            "path": f.path,
            "reason": f.exclusion_reason.map(|r| serde_json::to_string(&r).unwrap_or_default().trim_matches('"').to_string()),
        })).collect::<Vec<_>>());
        status["unsupported_files"] = serde_json::json!(unsupported
            .iter()
            .map(|f| f.path.clone())
            .collect::<Vec<_>>());
        human.push_str(&format!(
            "Repository {} at {}\nActive generation {} (snapshot {})\nCounters: {counters}\n",
            repo.repository_id, repo.root_path, gen.generation_id, gen.snapshot_id
        ));
        human.push_str(&format!(
            "Excluded: {} file(s); unsupported: {}\n",
            excluded.len(),
            unsupported
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    } else {
        human.push_str(&format!(
            "Repository {} at {} — not indexed yet\n",
            repo.repository_id, repo.root_path
        ));
    }
    if json {
        print_json(&envelope(&status, meta_for(&repo, None, None))?)?;
    } else {
        print!("{human}");
    }
    Ok(())
}

async fn cmd_search(
    store: &Arc<SqliteStore>,
    repo_path: Option<&std::path::Path>,
    query: &str,
    limit: u32,
    json: bool,
) -> Result<(), CccError> {
    let repo = resolve_repo(store, repo_path).await?;
    let backend = FtsSearchBackend::new(&**store, &**store, &**store);
    let response = backend
        .search(SearchRequest {
            repository_id: repo.repository_id,
            query: query.to_string(),
            snapshot_id: None,
            limit,
        })
        .await?;
    if json {
        print_json(&envelope(
            &response,
            meta_for(
                &repo,
                Some(response.snapshot_id),
                Some(response.generation_id),
            ),
        )?)?;
    } else {
        if response.hits.is_empty() {
            println!("No results for {query:?}.");
        }
        for hit in &response.hits {
            println!(
                "[{:?}] {:<8} {:?} ({})",
                hit.match_class,
                hit.kind,
                hit.title.as_deref().unwrap_or(&hit.path),
                hit.path
            );
        }
    }
    Ok(())
}

async fn cmd_context_compile(
    store: &Arc<SqliteStore>,
    artifacts: &FsArtifactStore,
    repo_path: Option<&std::path::Path>,
    task: &str,
    budget: u64,
    format: &str,
    json: bool,
) -> Result<(), CccError> {
    let repo = resolve_repo(store, repo_path).await?;
    let key = jobs::idempotency_key(&[
        "context_compile",
        &repo.repository_id.to_string(),
        task,
        &budget.to_string(),
    ]);
    let job = jobs::begin_job(&**store, repo.repository_id, "context_compile", &key, None).await?;

    let search_backend = FtsSearchBackend::new(&**store, &**store, &**store);
    let compiler = ContextCompiler::new(
        &**store,
        store.as_ref(),
        store.as_ref(),
        &**store,
        artifacts,
        &search_backend,
        &HeuristicNormalizer,
    );
    let compile_started = std::time::Instant::now();
    // History recency (H feature): optional data; with w_h = 0 bundles are
    // byte-identical with or without it (T9 constraint).
    let root = PathBuf::from(&repo.root_path);
    let history_scores = GitCli
        .log_name_status(&root, 200)
        .map(|log| ccc_repository::history_recency_millis(&log))
        .unwrap_or_default();
    match compiler
        .compile(&CompileRequest {
            repository_id: repo.repository_id,
            task_text: task.to_string(),
            budget_tokens: budget,
            weights_override: None,
            history_scores: Some(history_scores),
        })
        .await
    {
        Ok(bundle) => {
            let compile_latency_ms = compile_started.elapsed().as_millis() as u64;
            let payload = serde_json::json!({
                "bundle_id": bundle.bundle_id,
                "latency_ms": compile_latency_ms,
            });
            let _job = if job.state.is_terminal() {
                // Idempotent replay: the logical job already completed.
                job
            } else {
                jobs::finish_success(&**store, job, Some(&payload.to_string())).await?
            };
            match (format, json) {
                (_, true) => print_json(&envelope(&bundle, meta_for(&repo, None, None))?)?,
                ("markdown", false) => print!("{}", ccc_context_compiler::bundle_markdown(&bundle)),
                _ => print_json(&envelope(&bundle, meta_for(&repo, None, None))?)?,
            }
            Ok(())
        }
        Err(e) => {
            jobs::finish_failure(&**store, job, &e).await?;
            Err(e)
        }
    }
}

async fn cmd_context_show(
    store: &SqliteStore,
    bundle_id: &str,
    json: bool,
) -> Result<(), CccError> {
    let id = BundleId::from_str(bundle_id)
        .map_err(|_| CccError::invalid_request(format!("invalid bundle id {bundle_id:?}")))?;
    let record = store
        .get_bundle(id)
        .await?
        .ok_or_else(|| CccError::snapshot_not_found(format!("bundle {bundle_id} not found")))?;
    let items_json = store.get_bundle_items(id).await?;
    let items: Vec<ccc_schema::BundleItem> = serde_json::from_str(&items_json).unwrap_or_default();
    if json {
        print_json(&envelope(
            &serde_json::json!({
                "bundle": record,
                "items": items,
            }),
            EnvelopeMeta::default(),
        )?)?;
    } else {
        println!(
            "Bundle {} — task: {:?} — {} items, ~{} tokens ({}), stopping: {}",
            record.bundle_id,
            record.task_text,
            items.len(),
            record.estimated_tokens,
            record.token_estimate_basis,
            record.stopping_reason
        );
        for item in &items {
            println!(
                "{}. [{}] {} — {}:{} — evidence: {} — why: {}",
                item.position + 1,
                item.channel,
                item.title,
                item.path,
                item.span.map(|s| s.start_line).unwrap_or(0),
                item.evidence_status,
                item.selection_reason
            );
        }
    }
    Ok(())
}

async fn cmd_evidence_show(
    store: &SqliteStore,
    evidence_id: &str,
    json: bool,
) -> Result<(), CccError> {
    let id = EvidenceId::from_str(evidence_id)
        .map_err(|_| CccError::invalid_request(format!("invalid evidence id {evidence_id:?}")))?;
    let record = store
        .get_evidence(id)
        .await?
        .ok_or_else(|| CccError::snapshot_not_found(format!("evidence {evidence_id} not found")))?;
    if json {
        print_json(&envelope(&record, EnvelopeMeta::default())?)?;
    } else {
        println!("Evidence {}", record.evidence_id);
        println!("  type: {:?}", record.evidence_type);
        println!("  source: {}", record.source_ref);
        println!(
            "  extraction: {} ({}), actor: {}",
            serde_json::to_string(&record.extraction_method).unwrap_or_default(),
            record.extraction_version,
            record.actor
        );
        println!("  snapshot: {}", record.snapshot_id);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// evidence list + engineering memory (FR-032/034/035, ui.md UI-016/018)
// ---------------------------------------------------------------------------

fn parse_claim_status(s: &str) -> Result<ClaimStatus, CccError> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).map_err(|_| {
        CccError::invalid_request(format!(
            "unknown status {s:?}; known: candidate, supported, verified, stale, conflicted, \
             superseded, retracted"
        ))
    })
}

fn memory_draft_from(
    title: &str,
    summary: &str,
    memory_type: &str,
    scope: &str,
    created_by: &str,
    sharing_policy: &str,
    sensitivity: &str,
) -> Result<ccc_memory::MemoryDraft, CccError> {
    Ok(ccc_memory::MemoryDraft {
        title: title.to_string(),
        summary: summary.to_string(),
        memory_type: MemoryType::parse(memory_type)?,
        scope: scope.to_string(),
        created_by: created_by.to_string(),
        sharing_policy: sharing_policy.to_string(),
        sensitivity: sensitivity.to_string(),
    })
}

fn parse_claim_ids(raw: &[String]) -> Result<Vec<ClaimId>, CccError> {
    raw.iter()
        .map(|s| {
            ClaimId::from_str(s)
                .map_err(|_| CccError::invalid_request(format!("invalid claim id {s:?}")))
        })
        .collect()
}

/// Snapshot a memory is authored against: the active generation's snapshot
/// (memories without an indexed repository cannot be scoped — no false
/// availability).
async fn active_snapshot_id(
    store: &SqliteStore,
    repo: &RepositoryRecord,
) -> Result<SnapshotId, CccError> {
    let generation = store
        .get_active_generation(repo.repository_id)
        .await?
        .ok_or_else(|| {
            CccError::snapshot_not_found("no published index generation; run `ccc index` first")
        })?;
    Ok(generation.snapshot_id)
}

async fn cmd_evidence_list(
    store: &SqliteStore,
    status: Option<&str>,
    limit: usize,
    json: bool,
) -> Result<(), CccError> {
    let status_filter = status.map(parse_claim_status).transpose()?;
    let repos = store.list_repositories().await?;
    let mut claims = Vec::new();
    for repo in &repos {
        for claim in store.list_claims(repo.repository_id).await? {
            if let Some(want) = status_filter {
                if claim.status != want {
                    continue;
                }
            }
            claims.push(claim);
        }
    }
    claims.truncate(limit);
    if json {
        print_json(&envelope(&claims, EnvelopeMeta::default())?)?;
    } else {
        if claims.is_empty() {
            println!("No claims recorded.");
        }
        for c in &claims {
            println!(
                "{} [{}] ({}) {} — {}",
                c.claim_id,
                serde_json::to_string(&c.status)
                    .unwrap_or_default()
                    .trim_matches('"'),
                c.claim_type,
                c.text,
                c.snapshot_id
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_memory_add(
    store: &Arc<SqliteStore>,
    repo_path: Option<&std::path::Path>,
    title: &str,
    summary: &str,
    memory_type: &str,
    scope: &str,
    created_by: &str,
    sharing_policy: &str,
    sensitivity: &str,
    claim_ids: &[String],
    idempotency_key: Option<&str>,
    json: bool,
) -> Result<(), CccError> {
    let repo = resolve_repo(store, repo_path).await?;
    let snapshot_id = active_snapshot_id(store, &repo).await?;
    let claim_ids = parse_claim_ids(claim_ids)?;
    let draft = memory_draft_from(
        title,
        summary,
        memory_type,
        scope,
        created_by,
        sharing_policy,
        sensitivity,
    )?;
    let mut key_parts: Vec<String> = vec![
        "memory_add".into(),
        repo.repository_id.to_string(),
        draft.title.clone(),
        draft.summary.clone(),
        draft.memory_type.as_str().into(),
        draft.scope.clone(),
        draft.created_by.clone(),
        claim_ids
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(","),
    ];
    key_parts.sort();
    let key = idempotency_key.map(str::to_string).unwrap_or_else(|| {
        jobs::idempotency_key(&key_parts.iter().map(String::as_str).collect::<Vec<_>>())
    });
    let job = jobs::begin_job(&**store, repo.repository_id, "memory_add", &key, None).await?;

    match ccc_memory::create_memory_with_detection(
        store.as_ref(),
        store.as_ref(),
        repo.repository_id,
        snapshot_id,
        &claim_ids,
        draft,
    )
    .await
    {
        Ok(mutation) => {
            let _job = if job.state.is_terminal() {
                job
            } else {
                jobs::finish_success(&**store, job, Some(&mutation.record.memory_id.to_string()))
                    .await?
            };
            emit_memory_mutation(&mutation, &repo, Some(snapshot_id), json)?;
            Ok(())
        }
        Err(e) => {
            jobs::finish_failure(&**store, job, &e).await?;
            Err(e)
        }
    }
}

/// Emits a memory mutation; the detected-conflict count is part of the
/// output — new disagreements surface immediately, never silently (FR-034).
fn emit_memory_mutation(
    mutation: &ccc_memory::MemoryMutation,
    repo: &RepositoryRecord,
    snapshot_id: Option<SnapshotId>,
    json: bool,
) -> Result<(), CccError> {
    let detected = mutation.conflicts.conflicted.len();
    if json {
        let value = serde_json::to_value(&mutation.record)
            .map_err(|e| CccError::internal_error(format!("serialize memory: {e}")))?;
        let mut object = match value {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        object.insert("detected_conflicts".into(), serde_json::json!(detected));
        print_json(&envelope(
            &serde_json::Value::Object(object),
            meta_for(repo, snapshot_id, None),
        )?)?;
    } else {
        emit_memory(&mutation.record, repo, snapshot_id, false)?;
        if detected > 0 {
            println!(
                "Detected {detected} conflict pair(s) — inspect with `ccc conflicts`; \
                 conflicts are never auto-resolved."
            );
        }
    }
    Ok(())
}

fn emit_memory(
    record: &MemoryRecord,
    repo: &RepositoryRecord,
    snapshot_id: Option<SnapshotId>,
    json: bool,
) -> Result<(), CccError> {
    if json {
        print_json(&envelope(record, meta_for(repo, snapshot_id, None))?)?;
    } else {
        println!("Memory {}", record.memory_id);
        println!("  title: {}", record.title);
        println!("  type: {}", record.memory_type.as_str());
        println!(
            "  status: {}",
            serde_json::to_string(&record.status)
                .unwrap_or_default()
                .trim_matches('"')
        );
        println!(
            "  scope: {}",
            if record.scope.is_empty() {
                "(repository-wide)"
            } else {
                &record.scope
            }
        );
        println!("  created_by: {}", record.created_by);
        println!("  summary: {}", record.summary);
    }
    Ok(())
}

async fn cmd_memory_list(
    store: &SqliteStore,
    repo_path: Option<&std::path::Path>,
    status: Option<&str>,
    memory_type: Option<&str>,
    limit: usize,
    json: bool,
) -> Result<(), CccError> {
    let repo = resolve_repo(store, repo_path).await?;
    let status_filter = status.map(parse_claim_status).transpose()?;
    let type_filter = memory_type.map(MemoryType::parse).transpose()?;
    let mut records = match status_filter {
        Some(s) => {
            store
                .list_memories_by_status(repo.repository_id, &[s])
                .await?
        }
        None => store.list_memories(repo.repository_id).await?,
    };
    if let Some(t) = type_filter {
        records.retain(|m| m.memory_type == t);
    }
    records.truncate(limit);
    if json {
        print_json(&envelope(&records, meta_for(&repo, None, None))?)?;
    } else {
        if records.is_empty() {
            println!("No memory records.");
        }
        for m in &records {
            println!(
                "{} [{}] {} ({}) — {}",
                m.memory_id,
                serde_json::to_string(&m.status)
                    .unwrap_or_default()
                    .trim_matches('"'),
                m.title,
                m.memory_type.as_str(),
                m.summary
            );
        }
    }
    Ok(())
}

async fn cmd_memory_show(store: &SqliteStore, memory_id: &str, json: bool) -> Result<(), CccError> {
    let id = MemoryId::from_str(memory_id)
        .map_err(|_| CccError::invalid_request(format!("invalid memory id {memory_id:?}")))?;
    let record = store
        .get_memory(id)
        .await?
        .ok_or_else(|| CccError::snapshot_not_found(format!("memory {memory_id} not found")))?;
    let claim_ids = store.claim_ids_for_memory(id).await?;
    let events = store.list_memory_status_events(id).await?;
    // last_accessed_at records retrieval (SDD §5.11).
    store.touch_memory(id, &now_rfc3339()).await?;
    if json {
        print_json(&envelope(
            &serde_json::json!({
                "memory": record,
                "claim_ids": claim_ids.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
                "status_events": events,
            }),
            EnvelopeMeta::default(),
        )?)?;
    } else {
        println!("Memory {}", record.memory_id);
        println!("  title: {}", record.title);
        println!(
            "  status: {}",
            serde_json::to_string(&record.status)
                .unwrap_or_default()
                .trim_matches('"')
        );
        if let Some(cause) = &record.stale_cause {
            println!("  cause: {cause}");
        }
        println!("  type: {}", record.memory_type.as_str());
        println!(
            "  scope: {}",
            if record.scope.is_empty() {
                "(repository-wide)"
            } else {
                &record.scope
            }
        );
        println!(
            "  created_by: {} at {}",
            record.created_by, record.created_at
        );
        println!("  last_accessed_at: {}", record.last_accessed_at);
        println!("  summary: {}", record.summary);
        if let Some(superseded_id) = record.supersedes_memory_id {
            println!("  supersedes: {superseded_id}");
        }
        println!(
            "  claims: {}",
            if claim_ids.is_empty() {
                "(none — unverified note)"
            } else {
                ""
            }
        );
        for c in &claim_ids {
            println!("    - {c}");
        }
        for e in &events {
            println!(
                "  event: {} → {} by {}{}",
                e.created_at,
                serde_json::to_string(&e.status)
                    .unwrap_or_default()
                    .trim_matches('"'),
                e.actor,
                e.reason
                    .as_deref()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default()
            );
        }
    }
    Ok(())
}

async fn cmd_memory_search(
    store: &SqliteStore,
    repo_path: Option<&std::path::Path>,
    query: &str,
    status: Option<&str>,
    memory_type: Option<&str>,
    limit: u32,
    json: bool,
) -> Result<(), CccError> {
    let repo = resolve_repo(store, repo_path).await?;
    let status_filter = status.map(parse_claim_status).transpose()?;
    let type_filter = memory_type.map(MemoryType::parse).transpose()?;
    let records = store
        .search_memories(repo.repository_id, query, status_filter, type_filter, limit)
        .await?;
    let now = now_rfc3339();
    for m in &records {
        store.touch_memory(m.memory_id, &now).await?;
    }
    if json {
        print_json(&envelope(&records, meta_for(&repo, None, None))?)?;
    } else {
        if records.is_empty() {
            println!("No memory hits for {query:?}.");
        }
        for m in &records {
            println!(
                "{} [{}] {} — {}",
                m.memory_id,
                serde_json::to_string(&m.status)
                    .unwrap_or_default()
                    .trim_matches('"'),
                m.title,
                m.summary
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_memory_supersede(
    store: &Arc<SqliteStore>,
    repo_path: Option<&std::path::Path>,
    memory_id: &str,
    title: &str,
    summary: &str,
    memory_type: &str,
    scope: &str,
    created_by: &str,
    sharing_policy: &str,
    sensitivity: &str,
    claim_ids: &[String],
    reason: Option<&str>,
    idempotency_key: Option<&str>,
    json: bool,
) -> Result<(), CccError> {
    let repo = resolve_repo(store, repo_path).await?;
    let snapshot_id = active_snapshot_id(store, &repo).await?;
    let old_id = MemoryId::from_str(memory_id)
        .map_err(|_| CccError::invalid_request(format!("invalid memory id {memory_id:?}")))?;
    let claim_ids = parse_claim_ids(claim_ids)?;
    let draft = memory_draft_from(
        title,
        summary,
        memory_type,
        scope,
        created_by,
        sharing_policy,
        sensitivity,
    )?;
    let key = idempotency_key.map(str::to_string).unwrap_or_else(|| {
        jobs::idempotency_key(&[
            "memory_supersede",
            &repo.repository_id.to_string(),
            &old_id.to_string(),
            &draft.title,
            &draft.summary,
            draft.memory_type.as_str(),
            &draft.scope,
            &draft.created_by,
            reason.unwrap_or(""),
        ])
    });
    let job = jobs::begin_job(&**store, repo.repository_id, "memory_supersede", &key, None).await?;

    match ccc_memory::supersede_memory(
        store.as_ref(),
        store.as_ref(),
        repo.repository_id,
        snapshot_id,
        old_id,
        &claim_ids,
        ccc_memory::MemoryEdit {
            draft,
            reason: reason.map(str::to_string),
        },
    )
    .await
    {
        Ok(mutation) => {
            let _job = if job.state.is_terminal() {
                job
            } else {
                jobs::finish_success(&**store, job, Some(&mutation.record.memory_id.to_string()))
                    .await?
            };
            emit_memory_mutation(&mutation, &repo, Some(snapshot_id), json)?;
            Ok(())
        }
        Err(e) => {
            jobs::finish_failure(&**store, job, &e).await?;
            Err(e)
        }
    }
}

async fn cmd_memory_retract(
    store: &Arc<SqliteStore>,
    repo_path: Option<&std::path::Path>,
    memory_id: &str,
    actor: &str,
    reason: Option<&str>,
    idempotency_key: Option<&str>,
    json: bool,
) -> Result<(), CccError> {
    let repo = resolve_repo(store, repo_path).await?;
    let id = MemoryId::from_str(memory_id)
        .map_err(|_| CccError::invalid_request(format!("invalid memory id {memory_id:?}")))?;
    let key = idempotency_key.map(str::to_string).unwrap_or_else(|| {
        jobs::idempotency_key(&[
            "memory_retract",
            &repo.repository_id.to_string(),
            &id.to_string(),
            actor,
            reason.unwrap_or(""),
        ])
    });
    let job = jobs::begin_job(&**store, repo.repository_id, "memory_retract", &key, None).await?;

    match ccc_memory::retract_memory(store.as_ref(), id, actor, reason).await {
        Ok(record) => {
            let _job = if job.state.is_terminal() {
                job
            } else {
                jobs::finish_success(&**store, job, Some(&record.memory_id.to_string())).await?
            };
            emit_memory(&record, &repo, None, json)?;
            Ok(())
        }
        Err(e) => {
            jobs::finish_failure(&**store, job, &e).await?;
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn cmd_memory_resolve(
    store: &Arc<SqliteStore>,
    repo_path: Option<&std::path::Path>,
    keep: &str,
    drop: &str,
    rationale: &str,
    actor: &str,
    idempotency_key: Option<&str>,
    json: bool,
) -> Result<(), CccError> {
    let repo = resolve_repo(store, repo_path).await?;
    let keep_id = MemoryId::from_str(keep)
        .map_err(|_| CccError::invalid_request(format!("invalid memory id {keep:?}")))?;
    let drop_id = MemoryId::from_str(drop)
        .map_err(|_| CccError::invalid_request(format!("invalid memory id {drop:?}")))?;
    let key = idempotency_key.map(str::to_string).unwrap_or_else(|| {
        jobs::idempotency_key(&[
            "memory_resolve",
            &repo.repository_id.to_string(),
            &keep_id.to_string(),
            &drop_id.to_string(),
            rationale,
            actor,
        ])
    });
    let job = jobs::begin_job(&**store, repo.repository_id, "memory_resolve", &key, None).await?;

    match ccc_memory::resolve_conflict(
        store.as_ref(),
        repo.repository_id,
        keep_id,
        drop_id,
        rationale,
        actor,
    )
    .await
    {
        Ok(resolution) => {
            let _job = if job.state.is_terminal() {
                job
            } else {
                jobs::finish_success(
                    &**store,
                    job,
                    Some(&resolution.dropped.memory_id.to_string()),
                )
                .await?
            };
            if json {
                print_json(&envelope(&resolution, meta_for(&repo, None, None))?)?;
            } else {
                println!(
                    "Resolution recorded: {} retracted; {} kept (returns to candidate).",
                    resolution.dropped.memory_id, resolution.kept.memory_id
                );
            }
            Ok(())
        }
        Err(e) => {
            jobs::finish_failure(&**store, job, &e).await?;
            Err(e)
        }
    }
}

async fn cmd_conflicts(
    store: &SqliteStore,
    repo_path: Option<&std::path::Path>,
    json: bool,
) -> Result<(), CccError> {
    let repo = resolve_repo(store, repo_path).await?;
    let pairs = ccc_memory::list_conflicts(store, store, repo.repository_id).await?;
    if json {
        print_json(&envelope(&pairs, meta_for(&repo, None, None))?)?;
    } else {
        if pairs.is_empty() {
            println!("No conflicted evidence.");
        }
        for p in &pairs {
            println!(
                "{} {} — scope: {} (snapshots {})",
                p.kind,
                p.label,
                p.shared_scope,
                if p.snapshot_match { "match" } else { "differ" }
            );
            println!("  left  {} [{}]: {}", p.left_id, p.left_title, p.left_text);
            println!(
                "  right {} [{}]: {}",
                p.right_id, p.right_title, p.right_text
            );
            println!("  resolution: `ccc memory resolve --keep <id> --drop <id> --rationale … --actor …`");
        }
    }
    Ok(())
}

async fn cmd_impact_analyze(
    store: &Arc<SqliteStore>,
    _artifacts: &FsArtifactStore,
    repo_path: Option<&std::path::Path>,
    base: &str,
    head: Option<&str>,
    format: &str,
    json: bool,
) -> Result<(), CccError> {
    let repo = resolve_repo(store, repo_path).await?;
    let root = PathBuf::from(&repo.root_path);
    let key = jobs::idempotency_key(&[
        "impact_analyze",
        &repo.repository_id.to_string(),
        base,
        head.unwrap_or("working_tree"),
    ]);
    let job = jobs::begin_job(&**store, repo.repository_id, "impact_analyze", &key, None).await?;

    let analyzer = ImpactAnalyzer::new(&**store, &**store, &**store, &GitCli, 3);
    match analyzer
        .analyze(repo.repository_id, &root, base, head)
        .await
    {
        Ok(report) => {
            let _job = if job.state.is_terminal() {
                // Idempotent replay: the logical job already completed.
                job
            } else {
                jobs::finish_success(&**store, job, Some(&report.impact_id)).await?
            };
            match (format, json) {
                (_, true) => print_json(&envelope(&report, meta_for(&repo, None, None))?)?,
                ("markdown", false) => print!("{}", render_impact_markdown(&report)),
                _ => print_json(&envelope(&report, meta_for(&repo, None, None))?)?,
            }
            Ok(())
        }
        Err(e) => {
            jobs::finish_failure(&**store, job, &e).await?;
            Err(e)
        }
    }
}

async fn cmd_export(
    store: &SqliteStore,
    repo_path: Option<&std::path::Path>,
    output: &std::path::Path,
    json: bool,
) -> Result<(), CccError> {
    let repo = resolve_repo(store, repo_path).await?;
    let repositories = vec![repo.clone()];
    let claims = store.list_claims(repo.repository_id).await?;
    let evidence = store.list_evidence(repo.repository_id).await?;
    let memories = store.list_memories(repo.repository_id).await?;
    let mut memory_docs = Vec::new();
    for m in &memories {
        memory_docs.push(serde_json::json!({
            "memory": m,
            "claim_ids": store.claim_ids_for_memory(m.memory_id).await?,
            "status_events": store.list_memory_status_events(m.memory_id).await?,
        }));
    }
    let bundles = store.list_bundles(repo.repository_id).await?;
    let mut bundle_docs = Vec::new();
    for b in &bundles {
        let items_json = store.get_bundle_items(b.bundle_id).await?;
        bundle_docs.push(serde_json::json!({
            "bundle": b,
            "items": serde_json::from_str::<serde_json::Value>(&items_json).unwrap_or_default(),
        }));
    }
    let export = serde_json::json!({
        "schema_version": SCHEMA_VERSION,
        "repositories": repositories,
        "claims": claims,
        "evidence": evidence,
        "memories": memory_docs,
        "bundles": bundle_docs,
    });
    let text = serde_json::to_string_pretty(&export)
        .map_err(|e| CccError::internal_error(format!("export serialization: {e}")))?;
    std::fs::write(output, text)
        .map_err(|e| CccError::internal_error(format!("cannot write export: {e}")))?;
    if json {
        print_json(&envelope(
            &serde_json::json!({
                "output": output.to_string_lossy(),
                "claims": claims.len(),
                "evidence": evidence.len(),
                "memories": memories.len(),
                "bundles": bundles.len(),
            }),
            meta_for(&repo, None, None),
        )?)?;
    } else {
        println!(
            "Exported {} claim(s), {} evidence record(s), {} memory record(s), {} bundle(s) to {}",
            claims.len(),
            evidence.len(),
            memories.len(),
            bundles.len(),
            output.display()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// verification + policies (TIS §11, §13.4)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn cmd_verify(
    store: &Arc<SqliteStore>,
    artifacts: &FsArtifactStore,
    data_dir: &std::path::Path,
    repo_path: Option<&std::path::Path>,
    base: &str,
    head: Option<&str>,
    policy_name: &str,
    format: &str,
    json: bool,
) -> Result<(), CccError> {
    let repo = resolve_repo(store, repo_path).await?;
    let root = PathBuf::from(&repo.root_path);
    let cfg = ccc_config::load_config(data_dir)?;
    // Policies come only from the data dir; an unknown name is INVALID_REQUEST
    // (no silent fallback, no false availability).
    let policy = ccc_verification::load_policy(data_dir, policy_name)?;

    // Idempotency: same repository + patch + policy replays one logical job.
    let head_commit = GitCli.rev_parse_head(&root)?.unwrap_or_default();
    let dirty = !GitCli.status_porcelain(&root)?.is_empty();
    let key = jobs::idempotency_key(&[
        "verify",
        &repo.repository_id.to_string(),
        base,
        head.unwrap_or("working_tree"),
        policy_name,
        &policy.hash(),
        &head_commit,
        &format!("{dirty}"),
    ]);
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
    match engine.verify(&repo, &root, base, head, &policy).await {
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
            match (format, json) {
                (_, true) => print_json(&envelope(&report, meta_for(&repo, None, None))?)?,
                ("markdown", false) => {
                    print!(
                        "{}",
                        ccc_verification::render_verification_markdown(&report)
                    )
                }
                _ => print_json(&envelope(&report, meta_for(&repo, None, None))?)?,
            }
            Ok(())
        }
        Err(e) => {
            jobs::finish_failure(&**store, job, &e).await?;
            Err(e)
        }
    }
}

async fn cmd_policies_list(data_dir: &std::path::Path, json: bool) -> Result<(), CccError> {
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
    if json {
        print_json(&envelope(&policies, EnvelopeMeta::default())?)?;
    } else {
        if policies.len() == 1 {
            println!("1 policy available (source: data dir; repository content can never define policies):");
        } else {
            println!("{} policies available (source: data dir; repository content can never define policies):", policies.len());
        }
        for p in &policies {
            println!(
                "{} — execution: {}, hash: {}",
                p["name"].as_str().unwrap_or_default(),
                if p["allow_execution"].as_bool().unwrap_or(false) {
                    "allowed (allowlisted commands only)"
                } else {
                    "not allowed (analysis-only)"
                },
                p["policy_hash"].as_str().unwrap_or_default()
            );
        }
    }
    Ok(())
}

async fn cmd_policies_show(
    data_dir: &std::path::Path,
    name: &str,
    json: bool,
) -> Result<(), CccError> {
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
    if json {
        print_json(&envelope(&doc, EnvelopeMeta::default())?)?;
    } else {
        println!(
            "Policy {} — execution: {}, hash: {}",
            policy.name,
            if policy.allow_execution {
                "allowed (allowlisted commands only)"
            } else {
                "not allowed (analysis-only)"
            },
            policy.hash()
        );
        println!("  allowlist: {:?}", policy.allowlist);
        println!("  disallowlist: {:?}", policy.disallowlist);
        println!(
            "  timeout: {}s, output cap: {} bytes",
            policy.timeout_secs, policy.max_output_bytes
        );
        println!("  env allowlist: {:?}", policy.env_allowlist);
        println!(
            "  claim revalidation: {}",
            if policy.revalidate_claims {
                "enabled"
            } else {
                "disabled"
            }
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// evaluation (SDD §20.2, TIS §19)
// ---------------------------------------------------------------------------

/// A retrieval-policy override file: a `[weights]` table with the policy
/// weight names (ADR-008 — experiments are recorded, never silent edits).
#[derive(Debug, Deserialize)]
struct WeightsFile {
    weights: std::collections::BTreeMap<String, i64>,
}

const KNOWN_WEIGHTS: [&str; 7] = ["w_l", "w_d", "w_t", "w_h", "w_f", "w_r", "w_x"];

fn load_weights_override(
    path: &std::path::Path,
) -> Result<std::collections::BTreeMap<String, i64>, CccError> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        CccError::invalid_request(format!(
            "cannot read policy override {}: {e}",
            path.display()
        ))
    })?;
    let file: WeightsFile = toml::from_str(&raw).map_err(|e| {
        CccError::invalid_request(format!(
            "invalid policy override {} (expected a [weights] table): {e}",
            path.display()
        ))
    })?;
    for key in file.weights.keys() {
        if !KNOWN_WEIGHTS.contains(&key.as_str()) {
            return Err(CccError::invalid_request(format!(
                "policy override {} has unknown weight {key:?}; known: {KNOWN_WEIGHTS:?}",
                path.display()
            )));
        }
    }
    Ok(file.weights)
}

#[allow(clippy::too_many_arguments)]
async fn cmd_eval_run(
    store: &Arc<SqliteStore>,
    artifacts: &FsArtifactStore,
    data_dir: &std::path::Path,
    repo_path: Option<&std::path::Path>,
    suite_name: &str,
    policy_path: Option<&str>,
    json: bool,
) -> Result<(), CccError> {
    let repo = resolve_repo(store, repo_path).await?;
    let cfg = ccc_config::load_config(data_dir)?;
    let config_hash = cfg.indexing().configuration_hash()?;
    let suite_path = ccc_evaluation::resolve_suite_path(data_dir, suite_name)?;
    let suite = ccc_evaluation::load_suite(&suite_path)?;
    let weights_override = match policy_path {
        Some(p) => Some(load_weights_override(std::path::Path::new(p))?),
        None => None,
    };
    // The effective weights hash rides in the idempotency key so an A/B
    // experiment with different weights is a distinct logical job.
    let weights_hash = match &weights_override {
        Some(w) => ccc_schema::hash_canonical(w)
            .map_err(|e| CccError::internal_error(format!("weights hash: {e}")))?,
        None => "current".into(),
    };
    let key = jobs::idempotency_key(&[
        "eval_run",
        &repo.repository_id.to_string(),
        &suite.suite_hash,
        &config_hash,
        &weights_hash,
    ]);
    let job = jobs::begin_job(&**store, repo.repository_id, "eval_run", &key, None).await?;

    let search = FtsSearchBackend::new(&**store, &**store, &**store);
    let runner = ccc_evaluation::EvalRunner::new(
        &**store, &**store, &**store, &**store, artifacts, &search, &**store,
    );
    match runner
        .run_suite(&repo, &suite, &config_hash, weights_override.as_ref())
        .await
    {
        Ok(metrics) => {
            let run = store
                .list_runs(repo.repository_id)
                .await?
                .into_iter()
                .last()
                .ok_or_else(|| CccError::internal_error("run record missing after eval run"))?;
            let _job = if job.state.is_terminal() {
                // Idempotent replay: the logical job already completed.
                job
            } else {
                jobs::finish_success(&**store, job, Some(&run.run_id.to_string())).await?
            };
            if json {
                print_json(&envelope(
                    &serde_json::json!({
                        "run_id": run.run_id.to_string(),
                        "metrics": metrics,
                    }),
                    meta_for(&repo, None, None),
                )?)?;
            } else {
                println!(
                    "Evaluation run {} — suite {:?} (hash {})",
                    run.run_id, metrics.suite_name, metrics.suite_hash
                );
                println!(
                    "Policy: {} (hash {}), config: {}",
                    metrics.retrieval_policy_version, metrics.policy_hash, metrics.config_hash
                );
                for group in ["tuning", "held_out"] {
                    let g = if group == "tuning" {
                        &metrics.tuning
                    } else {
                        &metrics.held_out
                    };
                    if g.cases == 0 {
                        continue;
                    }
                    println!(
                        "{group}: {} case(s) — recall {}‰, MRR {}‰, precision@{} {}‰, \
                         stale {}‰, token cost {}",
                        g.cases,
                        g.recall_millis / 10,
                        g.mrr_millis / 10,
                        ccc_evaluation::PRECISION_K,
                        g.precision_millis / 10,
                        g.stale_rate_millis / 10,
                        g.token_cost
                    );
                }
                for case in &metrics.cases {
                    println!(
                        "  case {} [{}] — items: {}, matched: {}/{} (recall {}‰), stale {}‰, tokens {}",
                        case.case_index,
                        case.kind,
                        case.items,
                        case.matched,
                        case.expected,
                        case.recall_millis / 10,
                        case.stale_rate_millis / 10,
                        case.estimated_tokens
                    );
                }
            }
            Ok(())
        }
        Err(e) => {
            jobs::finish_failure(&**store, job, &e).await?;
            Err(e)
        }
    }
}

async fn cmd_eval_show(store: &SqliteStore, run_id: &str, json: bool) -> Result<(), CccError> {
    let id = EvaluationRunId::from_str(run_id)
        .map_err(|_| CccError::invalid_request(format!("invalid run id {run_id:?}")))?;
    let record = store.get_run(id).await?.ok_or_else(|| {
        CccError::snapshot_not_found(format!("evaluation run {run_id} not found"))
    })?;
    let metrics: serde_json::Value =
        serde_json::from_str(&record.metrics_json).unwrap_or(serde_json::json!({}));
    if json {
        print_json(&envelope(
            &serde_json::json!({
                "run_id": record.run_id.to_string(),
                "suite_name": record.suite_name,
                "suite_hash": record.suite_hash,
                "policy_version": record.policy_version,
                "policy_hash": record.policy_hash,
                "config_hash": record.config_hash,
                "created_at": record.created_at,
                "metrics": metrics,
            }),
            EnvelopeMeta::default(),
        )?)?;
    } else {
        println!("Evaluation run {}", record.run_id);
        println!(
            "  suite: {} (hash {})",
            record.suite_name, record.suite_hash
        );
        println!(
            "  policy: {} (hash {}), config: {}",
            record.policy_version, record.policy_hash, record.config_hash
        );
        println!("  created: {}", record.created_at);
        println!(
            "  metrics: {}",
            serde_json::to_string_pretty(&metrics).unwrap_or_else(|_| record.metrics_json.clone())
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// snapshot + context diff (PRD Phase 2, SDD §11.4)
// ---------------------------------------------------------------------------

async fn cmd_snapshot_diff(
    store: &SqliteStore,
    a: &str,
    b: &str,
    json: bool,
) -> Result<(), CccError> {
    let id_a = SnapshotId::from_str(a)
        .map_err(|_| CccError::invalid_request(format!("invalid snapshot id {a:?}")))?;
    let id_b = SnapshotId::from_str(b)
        .map_err(|_| CccError::invalid_request(format!("invalid snapshot id {b:?}")))?;
    store
        .get_snapshot(id_a)
        .await?
        .ok_or_else(|| CccError::snapshot_not_found(format!("snapshot {a} not found")))?;
    store
        .get_snapshot(id_b)
        .await?
        .ok_or_else(|| CccError::snapshot_not_found(format!("snapshot {b} not found")))?;

    let files_a = store.list_files(id_a).await?;
    let files_b = store.list_files(id_b).await?;
    let map_a: std::collections::HashMap<&str, &str> = files_a
        .iter()
        .map(|f| (f.path.as_str(), f.content_hash.as_str()))
        .collect();
    let map_b: std::collections::HashMap<&str, &str> = files_b
        .iter()
        .map(|f| (f.path.as_str(), f.content_hash.as_str()))
        .collect();

    let mut added: Vec<String> = Vec::new();
    let mut changed: Vec<serde_json::Value> = Vec::new();
    let mut removed: Vec<String> = Vec::new();
    // Renames: a removed path and an added path sharing identical content.
    let mut renamed: Vec<serde_json::Value> = Vec::new();
    let mut consumed_targets: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut removed_paths: Vec<&str> = map_a
        .iter()
        .filter(|(p, _)| !map_b.contains_key(*p))
        .map(|(p, _)| *p)
        .collect();
    removed_paths.sort();
    let mut added_paths: Vec<&str> = map_b
        .iter()
        .filter(|(p, _)| !map_a.contains_key(*p))
        .map(|(p, _)| *p)
        .collect();
    added_paths.sort();
    for old in &removed_paths {
        let hash = map_a[*old];
        if let Some(new) = added_paths
            .iter()
            .find(|c| map_b[**c] == hash && !consumed_targets.contains(**c))
        {
            consumed_targets.insert((*new).to_string());
            renamed.push(serde_json::json!({ "from": old, "to": new }));
        } else {
            removed.push((*old).to_string());
        }
    }
    for path in added_paths {
        if !consumed_targets.contains(path) {
            added.push(path.to_string());
        }
    }
    for (path, hash) in &map_a {
        if let Some(other) = map_b.get(*path) {
            if other != hash {
                changed.push(serde_json::json!({ "path": path, "from": hash, "to": other }));
            }
        }
    }
    added.sort();
    removed.sort();
    changed.sort_by(|x, y| {
        x["path"]
            .as_str()
            .unwrap_or_default()
            .cmp(y["path"].as_str().unwrap_or_default())
    });
    renamed.sort_by(|x, y| {
        x["from"]
            .as_str()
            .unwrap_or_default()
            .cmp(y["from"].as_str().unwrap_or_default())
    });

    if json {
        print_json(&envelope(
            &serde_json::json!({
                "base_snapshot": a,
                "target_snapshot": b,
                "added": added,
                "removed": removed,
                "changed": changed,
                "renamed": renamed,
            }),
            EnvelopeMeta::default(),
        )?)?;
    } else {
        println!("Snapshot diff {a} → {b}");
        println!("added: {}", added.len());
        for entry in &added {
            println!("  + {entry}");
        }
        println!("removed: {}", removed.len());
        for entry in &removed {
            println!("  - {entry}");
        }
        println!("changed: {}", changed.len());
        for c in &changed {
            println!(
                "  ~ {} ({} → {})",
                c["path"].as_str().unwrap_or_default(),
                c["from"].as_str().unwrap_or_default(),
                c["to"].as_str().unwrap_or_default()
            );
        }
        println!("renamed: {}", renamed.len());
        for r in &renamed {
            println!(
                "  {} → {}",
                r["from"].as_str().unwrap_or_default(),
                r["to"].as_str().unwrap_or_default()
            );
        }
    }
    Ok(())
}

async fn cmd_context_diff(
    store: &SqliteStore,
    a: &str,
    b: &str,
    json: bool,
) -> Result<(), CccError> {
    let id_a = BundleId::from_str(a)
        .map_err(|_| CccError::invalid_request(format!("invalid bundle id {a:?}")))?;
    let id_b = BundleId::from_str(b)
        .map_err(|_| CccError::invalid_request(format!("invalid bundle id {b:?}")))?;
    let bundle_a = store
        .get_bundle(id_a)
        .await?
        .ok_or_else(|| CccError::snapshot_not_found(format!("bundle {a} not found")))?;
    let bundle_b = store
        .get_bundle(id_b)
        .await?
        .ok_or_else(|| CccError::snapshot_not_found(format!("bundle {b} not found")))?;
    let items_a: Vec<ccc_schema::BundleItem> =
        serde_json::from_str(&store.get_bundle_items(id_a).await?).unwrap_or_default();
    let items_b: Vec<ccc_schema::BundleItem> =
        serde_json::from_str(&store.get_bundle_items(id_b).await?).unwrap_or_default();

    // Join key is the logical identity (kind + path + title), stable across
    // snapshot-scoped entity ids — a reindex regenerates ids, not items.
    let by_item = |items: &[ccc_schema::BundleItem]| {
        items
            .iter()
            .map(|i| (format!("{}|{}|{}", i.item_kind, i.path, i.title), i.clone()))
            .collect::<std::collections::HashMap<String, ccc_schema::BundleItem>>()
    };
    let map_a = by_item(&items_a);
    let map_b = by_item(&items_b);

    let added: Vec<&ccc_schema::BundleItem> = items_b
        .iter()
        .filter(|i| !map_a.contains_key(&format!("{}|{}|{}", i.item_kind, i.path, i.title)))
        .collect();
    let removed: Vec<&ccc_schema::BundleItem> = items_a
        .iter()
        .filter(|i| !map_b.contains_key(&format!("{}|{}|{}", i.item_kind, i.path, i.title)))
        .collect();
    let mut changed: Vec<serde_json::Value> = Vec::new();
    for item in &items_b {
        let key = format!("{}|{}|{}", item.item_kind, item.path, item.title);
        if let Some(old) = map_a.get(&key) {
            let content_changed = old.provenance.content_hash != item.provenance.content_hash;
            let status_changed = old.evidence_status != item.evidence_status;
            if content_changed || status_changed {
                changed.push(serde_json::json!({
                    "entity_id": item.entity_id,
                    "title": item.title,
                    "path": item.path,
                    "content_changed": content_changed,
                    "evidence_status": {
                        "from": old.evidence_status,
                        "to": item.evidence_status,
                    },
                }));
            }
        }
    }

    if json {
        print_json(&envelope(
            &serde_json::json!({
                "bundle_a": a,
                "bundle_b": b,
                "added": added,
                "removed": removed,
                "changed": changed,
            }),
            EnvelopeMeta::default(),
        )?)?;
    } else {
        println!(
            "Context diff {} → {}",
            bundle_a.bundle_id, bundle_b.bundle_id
        );
        println!("added ({}):", added.len());
        for i in &added {
            println!(
                "  + {} ({}, evidence: {})",
                i.title, i.path, i.evidence_status
            );
        }
        println!("removed ({}):", removed.len());
        for i in &removed {
            println!(
                "  - {} ({}, evidence: {})",
                i.title, i.path, i.evidence_status
            );
        }
        println!("changed ({}):", changed.len());
        for c in &changed {
            println!(
                "  ~ {} — evidence: {} → {}{}",
                c["title"].as_str().unwrap_or_default(),
                c["evidence_status"]["from"].as_str().unwrap_or_default(),
                c["evidence_status"]["to"].as_str().unwrap_or_default(),
                if c["content_changed"].as_bool().unwrap_or(false) {
                    " (content changed)"
                } else {
                    ""
                }
            );
        }
    }
    Ok(())
}
