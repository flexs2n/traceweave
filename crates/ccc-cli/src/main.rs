//! `ccc` — the CLI user surface (TIS §13.4).
//!
//! Human-readable output is the default; `--json` produces full,
//! never-truncated machine envelopes. Errors exit non-zero with a stable
//! error-code JSON on stderr (ui.md §3.2: no false availability).
//!
//! Since Step 4 the operation logic lives in `ccc-service` (shared with the
//! REST API and the MCP adapter); this binary is a thin adapter: argument
//! parsing, output selection (`--json` vs the pre-rendered human text), and
//! the `init`/`doctor` diagnostics that need no store.

use ccc_domain::*;
use ccc_schema::{ErrorEnvelope, SCHEMA_VERSION};
use clap::{Parser as ClapParser, Subcommand};
use std::path::PathBuf;

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

fn emit(out: ccc_service::OpOutput, json: bool) -> Result<(), CccError> {
    if json {
        print_json(&out.envelope())
    } else {
        print!("{}", out.human);
        Ok(())
    }
}

async fn dispatch(cli: &Cli) -> Result<(), CccError> {
    let data_dir = ccc_config::resolve_data_dir(cli.data_dir.as_deref());
    match &cli.command {
        Command::Init => cmd_init(&data_dir, cli.json).await,
        Command::Doctor => cmd_doctor(&data_dir, cli.json).await,
        Command::Policies { command } => match command {
            PoliciesCommand::List => emit(ccc_service::policies_list(&data_dir).await?, cli.json),
            PoliciesCommand::Show { name } => {
                emit(ccc_service::policies_show(&data_dir, name).await?, cli.json)
            }
        },
        // Everything below requires an initialized store.
        _ => {
            let ctx = ccc_service::AppContext::open(&data_dir).await?;
            match &cli.command {
                Command::Repo { command } => match command {
                    RepoCommand::Add {
                        path,
                        idempotency_key,
                    } => emit(
                        ccc_service::repo_add(&ctx, path, idempotency_key.as_deref()).await?,
                        cli.json,
                    ),
                    RepoCommand::Status { path } => emit(
                        ccc_service::repo_status(&ctx, path.as_deref()).await?,
                        cli.json,
                    ),
                },
                Command::Index {
                    full,
                    snapshot,
                    repo,
                } => emit(
                    ccc_service::index_repo(&ctx, repo.as_deref(), *full, snapshot.as_deref())
                        .await?,
                    cli.json,
                ),
                Command::Status { repo } => {
                    emit(ccc_service::status(&ctx, repo.as_deref()).await?, cli.json)
                }
                Command::Search { query, limit, repo } => emit(
                    ccc_service::search(&ctx, repo.as_deref(), query, *limit).await?,
                    cli.json,
                ),
                Command::Context { command } => match command {
                    ContextCommand::Compile {
                        task,
                        budget,
                        format,
                        repo,
                    } => emit(
                        ccc_service::context_compile(&ctx, repo.as_deref(), task, *budget, format)
                            .await?,
                        cli.json,
                    ),
                    ContextCommand::Show { bundle_id } => {
                        emit(ccc_service::context_show(&ctx, bundle_id).await?, cli.json)
                    }
                    ContextCommand::Diff { bundle_a, bundle_b } => emit(
                        ccc_service::context_diff(&ctx, bundle_a, bundle_b).await?,
                        cli.json,
                    ),
                },
                Command::Evidence { command } => match command {
                    EvidenceCommand::List { status, limit } => emit(
                        ccc_service::evidence_list(&ctx, status.as_deref(), *limit).await?,
                        cli.json,
                    ),
                    EvidenceCommand::Show { evidence_id } => emit(
                        ccc_service::evidence_show(&ctx, evidence_id).await?,
                        cli.json,
                    ),
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
                    } => emit(
                        ccc_service::memory_add(
                            &ctx,
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
                        )
                        .await?,
                        cli.json,
                    ),
                    MemoryCommand::List {
                        status,
                        memory_type,
                        limit,
                        repo,
                    } => emit(
                        ccc_service::memory_list(
                            &ctx,
                            repo.as_deref(),
                            status.as_deref(),
                            memory_type.as_deref(),
                            *limit,
                        )
                        .await?,
                        cli.json,
                    ),
                    MemoryCommand::Show { memory_id } => {
                        emit(ccc_service::memory_show(&ctx, memory_id).await?, cli.json)
                    }
                    MemoryCommand::Search {
                        query,
                        status,
                        memory_type,
                        limit,
                        repo,
                    } => emit(
                        ccc_service::memory_search(
                            &ctx,
                            repo.as_deref(),
                            query,
                            status.as_deref(),
                            memory_type.as_deref(),
                            *limit,
                        )
                        .await?,
                        cli.json,
                    ),
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
                    } => emit(
                        ccc_service::memory_supersede(
                            &ctx,
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
                        )
                        .await?,
                        cli.json,
                    ),
                    MemoryCommand::Retract {
                        memory_id,
                        actor,
                        reason,
                        idempotency_key,
                        repo,
                    } => emit(
                        ccc_service::memory_retract(
                            &ctx,
                            repo.as_deref(),
                            memory_id,
                            actor,
                            reason.as_deref(),
                            idempotency_key.as_deref(),
                        )
                        .await?,
                        cli.json,
                    ),
                    MemoryCommand::Resolve {
                        keep,
                        drop,
                        rationale,
                        actor,
                        idempotency_key,
                        repo,
                    } => emit(
                        ccc_service::memory_resolve(
                            &ctx,
                            repo.as_deref(),
                            keep,
                            drop,
                            rationale,
                            actor,
                            idempotency_key.as_deref(),
                        )
                        .await?,
                        cli.json,
                    ),
                },
                Command::Conflicts { repo } => emit(
                    ccc_service::conflicts(&ctx, repo.as_deref()).await?,
                    cli.json,
                ),
                Command::Impact { command } => match command {
                    ImpactCommand::Analyze {
                        base,
                        head,
                        format,
                        repo,
                    } => emit(
                        ccc_service::impact_analyze(
                            &ctx,
                            repo.as_deref(),
                            base,
                            head.as_deref(),
                            format,
                        )
                        .await?,
                        cli.json,
                    ),
                },
                Command::Export { output, repo } => emit(
                    ccc_service::export_data(&ctx, repo.as_deref(), output).await?,
                    cli.json,
                ),
                Command::Eval { command } => match command {
                    EvalCommand::Run {
                        suite,
                        policy,
                        repo,
                    } => emit(
                        ccc_service::eval_run(&ctx, repo.as_deref(), suite, policy.as_deref())
                            .await?,
                        cli.json,
                    ),
                    EvalCommand::Show { run_id } => {
                        emit(ccc_service::eval_show(&ctx, run_id).await?, cli.json)
                    }
                },
                Command::Snapshot { command } => match command {
                    SnapshotCommand::Diff { a, b } => {
                        emit(ccc_service::snapshot_diff(&ctx, a, b).await?, cli.json)
                    }
                },
                Command::Verify {
                    base,
                    head,
                    policy,
                    format,
                    repo,
                } => emit(
                    ccc_service::verify(
                        &ctx,
                        repo.as_deref(),
                        base,
                        head.as_deref(),
                        policy,
                        format,
                    )
                    .await?,
                    cli.json,
                ),
                Command::Init | Command::Doctor | Command::Policies { .. } => {
                    unreachable!("handled above")
                }
            }
        }
    }
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<(), CccError> {
    let text = serde_json::to_string(value)
        .map_err(|e| CccError::internal_error(format!("json serialization: {e}")))?;
    println!("{text}");
    Ok(())
}

// ---------------------------------------------------------------------------
// init / doctor (CLI-only diagnostics; no store required)
// ---------------------------------------------------------------------------

async fn cmd_init(data_dir: &std::path::Path, json: bool) -> Result<(), CccError> {
    std::fs::create_dir_all(data_dir)
        .map_err(|e| CccError::internal_error(format!("cannot create data dir: {e}")))?;
    let config_path = ccc_config::write_default_config(data_dir)?;
    let _store = ccc_storage::SqliteStore::connect(&data_dir.join("ccc.db")).await?;
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
    match ccc_storage::SqliteStore::connect(&data_dir.join("ccc.db")).await {
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
