//! `ccc` — the CLI user surface (TIS §13.4).
//!
//! Human-readable output is the default; `--json` produces full,
//! never-truncated machine envelopes. Errors exit non-zero with a stable
//! error-code JSON on stderr. Declared-but-unimplemented commands (`ccc
//! verify`, `ccc eval run`) say so explicitly instead of pretending
//! availability (ui.md §3.2 principle, no false availability).

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
    /// Patch verification (NOT IMPLEMENTED in this build; analysis-only, ADR-008).
    Verify {
        #[arg(long)]
        policy: String,
    },
    /// Offline evaluation (NOT IMPLEMENTED in this build).
    Eval {
        #[command(subcommand)]
        command: EvalCommand,
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
    /// Show one evidence record with provenance.
    Show { evidence_id: String },
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
    Run {
        #[arg(long)]
        suite: String,
    },
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
        Command::Verify { .. } => Err(CccError::execution_not_authorized(
            "verification execution is not implemented in this build (analysis-only, ADR-008)",
        )),
        Command::Eval { .. } => Err(CccError::invalid_request(
            "evaluation is not implemented in this build",
        )),
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
                },
                Command::Evidence { command } => match command {
                    EvidenceCommand::Show { evidence_id } => {
                        cmd_evidence_show(&store, evidence_id, cli.json).await
                    }
                },
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
            let payload = serde_json::to_string(&outcome).unwrap_or_else(|_| "{}".into());
            jobs::finish_success(&**store, job, Some(&payload)).await?;
            if json {
                print_json(&envelope(
                    &serde_json::json!({
                        "snapshot_id": outcome.snapshot_id.to_string(),
                        "generation_id": outcome.generation_id.to_string(),
                        "counters": outcome.counters,
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
        &**store,
        artifacts,
        &search_backend,
        &HeuristicNormalizer,
    );
    match compiler
        .compile(&CompileRequest {
            repository_id: repo.repository_id,
            task_text: task.to_string(),
            budget_tokens: budget,
        })
        .await
    {
        Ok(bundle) => {
            let _job = if job.state.is_terminal() {
                // Idempotent replay: the logical job already completed.
                job
            } else {
                jobs::finish_success(&**store, job, Some(&bundle.bundle_id.to_string())).await?
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
                "bundles": bundles.len(),
            }),
            meta_for(&repo, None, None),
        )?)?;
    } else {
        println!(
            "Exported {} claim(s), {} evidence record(s), {} bundle(s) to {}",
            claims.len(),
            evidence.len(),
            bundles.len(),
            output.display()
        );
    }
    Ok(())
}
