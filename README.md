# traceweave / ccc

Change-aware context compiler — Step 4 (integration layer: service, REST, MCP).

`ccc` indexes a git repository into immutable, content-addressed snapshots,
extracts Rust symbols and name-level relationships with Tree-sitter, and
compiles token-budgeted, provenance-carrying context bundles for engineering
tasks. Step 2 closed the loop: *change → predicted impact → verification
under an explicit execution policy → scoped evidence revalidation*, plus an
offline evaluation harness. Step 3 adds the persistent engineering memory:
human-authored records with provenance and immutable history (FR-032),
agent-finding capture from failed authorized runs — always `candidate`
(FR-033, FR-080), conservative contradiction detection with both sides kept
inspectable (FR-034), memory FTS search (FR-035), a memory retrieval channel
in the compiler, conflict-aware bundles (FR-060), and a freshness projection
where linked memories follow their claims' statuses with recorded causes
(FR-043). Step 4 exposes it all through integrations (SDD §16, TIS §13/§20.1):
every operation now lives once in a shared **service layer** consumed by the
CLI, the REST API (`ccc serve`, FR-093), and a read/analysis MCP server
(`ccc mcp`, FR-094) — evidence and freshness semantics are never duplicated.
Everything runs locally; no LLM calls are made in this build
(NFR-012: provider boundary is an explicit opt-in extension point).

## Stack

- **Rust (edition 2021), primary language** (TIS §2 amendment — Python is
  restricted to offline ML/eval in later steps).
- **SQLx + SQLite** with WAL; FTS5 for the derived lexical index (verified
  available in the bundled build — plan risk R1 resolved by smoke test).
- **Tree-sitter** (`tree-sitter-rust`) for symbol/reference extraction.
- **BLAKE3** content addressing: files, parsed artifacts, manifests, bundles,
  verification reports, evaluation suites.
- **Tokio** bounded-concurrency parsing; single serialized writer connection.

## Workspace layout (TIS §21)

| Crate | Responsibility |
|---|---|
| `ccc-domain` | IDs, enums, error taxonomy, entity records, canonical JSON |
| `ccc-config` | configuration precedence + `configuration_hash` |
| `ccc-repository` | `GitProvider`/`GitCli` adapter, registration, history log |
| `ccc-snapshot` | snapshot identity, manifest hash |
| `ccc-storage` | store traits, SQLite adapter, artifact store |
| `ccc-schema` | versioned serialized contracts |
| `ccc-tree-sitter` | Rust syntax extraction |
| `ccc-indexer` | incremental indexing pipeline, atomic publication |
| `ccc-search` | lexical + exact symbol lookup |
| `ccc-evidence` | evidence ledger, claim staleness seam (FR-040) |
| `ccc-memory` | engineering memory: capture, contradiction detection, freshness projection, conflict resolution |
| `ccc-context-compiler` | task → ranked, budgeted bundle (policy-v1) |
| `ccc-impact` | change → impact report with conservative classification |
| `ccc-verification` | execution policies, test selection, authorized executor, evidence revalidation |
| `ccc-evaluation` | offline retrieval/integrity harness over hashed suites |
| `ccc-jobs` | idempotent job records (TIS §15.1 state machine) |
| `ccc-service` | the shared operation layer (one home per operation; SDD §16.1) |
| `ccc-api` | Axum REST `/v1` router + OpenAPI over `ccc-service` (FR-093) |
| `ccc-mcp` | rmcp stdio server, 8 read/analysis tools (FR-094, TIS §13.5) |
| `ccc-cli` | the `ccc` user surface (thin adapter: parsing + output selection) |

## Quick start (smoke trail, dogfooded on this repository)

```console
$ ccc init
Initialized ccc at ~/.ccc
Config: ~/.ccc/config.toml

$ ccc repo add C:\hackwell\traceweave
Registered repository 01a14530-caa0-… at C:/hackwell/traceweave (job 01a14530-caa5-…)

$ ccc index
Snapshot 01a14530-cbe2-7214-b769-c174d5a98053
Generation 01a14530-cbe8-746d-ae7f-07d891e05d0e published
Parsed: 35, reused: 0, excluded: 33 (unsupported: 33), failed: 0

$ ccc search "canonical_json" --limit 3
[Exact]   symbol   "crates::ccc-domain::src::util::canonical_json" (crates/ccc-domain/src/util.rs)

$ ccc context compile --task "how does the snapshot manifest hash stay deterministic" \
      --budget 2000 --format markdown
# Context bundle 01a14531-f45f-…
- Snapshot: `01a14530-cbe2-…` (generation `01a14530-cbe8-…`)
- Task (exploration): how does the snapshot manifest hash stay deterministic
- Budget: 2000 tokens (estimated ~1978, basis: approximate_chars_div4)
- Stopping reason: budget_exhausted
## Selected items …
## Dependency paths …
```

### Step 2: verify, evaluate, compare (dogfooded on this repository)

```console
$ ccc verify --base HEAD~3 --head HEAD
{"…","mode":"analysis","statuses":{"not_run":2},"checks":[{"command":"cargo build",
 "relation":"directly","status":"not_run","note":"recommendation — not executed
 (analysis policy; suggestions are advisory)"}, …]}

$ ccc policies list
2 policies available (source: data dir; repository content can never define policies):
analysis — execution: not allowed (analysis-only), hash: ae4b7c58104a…
local-build — execution: allowed (allowlisted commands only), hash: aa2d6783d2d9…

$ ccc verify --base HEAD~3 --head HEAD --policy local-build
{"…","mode":"authorized","statuses":{"passed":1,"failed":1}, …}

$ ccc eval run --suite dogfood
Evaluation run 01a1498d-2a80-… — suite "dogfood" (hash bf09cb4f…)
Policy: policy-v1 (hash 70ab49e8…), config: fca50f39…
tuning: 4 case(s) — recall 93‰, MRR 20‰, precision@10 25‰, stale 0‰, token cost 11469
held_out: 1 case(s) — recall 50‰, MRR 30‰, precision@10 20‰, stale 0‰, token cost 2975

$ ccc eval show 01a1498d-2a80-…        # the immutable recorded run
$ ccc snapshot diff <snap-a> <snap-b>  # added/removed/changed/renamed paths
$ ccc context diff <bundle-a> <bundle-b>  # item-level + evidence-status flips
```

### Step 3: engineering memory (dogfooded on this repository)

```console
$ ccc memory add --title "Conflict detection scope rule: shared symbol ids only" \
    --summary "Detection groups claims by identical resolved symbol dependencies…" \
    --memory-type convention --scope crates/ccc-memory --created-by human:me
Memory 01a149e1-27b8-…
  status: candidate
Detected 1 conflict pair(s) — inspect with `ccc conflicts`; conflicts are
never auto-resolved.

$ ccc conflicts
memory_pair conflicted — scope: memory_scope:crates/ccc-memory (snapshots match)
  left  01a149e1-… [Conflict detection scope rule: …]: …
  right 01a149e2-… [Conflict detection scope rule: …]: …

$ ccc memory resolve --keep 01a149e1-… --drop 01a149e2-… \
    --rationale "the symbol-scoped rule is the implemented behavior" --actor human:me
Resolution recorded: 01a149e2-… retracted; 01a149e1-… kept (returns to candidate).

$ ccc memory search "memory transitions"     # FTS over title+summary
$ ccc memory show 01a149e1-…                 # claim links + immutable status trail
$ ccc memory list --status stale             # freshness projection output (FR-043)
$ ccc evidence list --status conflicted      # the claim side of conflicts
```

Memory rules (SDD §5.11/§9.4, TIS §12): memory is **not a second source of
truth** — it is subordinate to current source, configuration and test
results; every transition appends an actor+reason event; retraction removes
a record from active retrieval but preserves history; agent findings from
failed authorized verifications are captured as `candidate` memories and are
never auto-promoted (FR-080); conflicts always show both sides with
provenance, and only an explicit human `resolve` retires a side.

Doctor output on the reference environment (Windows):

```console
$ ccc doctor
data_dir_writable    OK   — ~/.ccc
git_on_path          OK   — git available
database             OK   — opened and migrated
tree_sitter_rust     OK   — grammar parses
```

### Step 4: integrations — serve, MCP, jobs (dogfooded on this repository)

Every operation moved once into `ccc-service` (Step 4 T1); the CLI became a
thin adapter and the golden CLI tests stayed byte-identical (JSON payloads
are pre-serialized `RawValue`s, so typed records keep their exact field
order).

```console
$ ccc serve                             # REST API on 127.0.0.1:7713
```

```console
$ curl -s localhost:7713/v1/health
{"schema_version":"ccc.schema/1","data":{"ok":true,"version":"0.1.0", …}}

$ curl -s localhost:7713/v1/repositories/<id>
{"schema_version":"ccc.schema/1","data":{…repository…},
 "meta":{"index_current":true}}
```

REST facts (TIS §13.1 table + a documented `GET /v1/search` extension):

- Success bodies are the **same versioned envelopes** the CLI prints
  (`ccc.schema/1`); errors are `ErrorEnvelope` documents with stable TIS
  §13.2 codes mapped to HTTP statuses (`INVALID_REQUEST`→400,
  `*_NOT_FOUND`→404, `PERMISSION_DENIED`→403, `INDEX_STALE`/`JOB_CONFLICT`
  →409, `EXECUTION_NOT_AUTHORIZED`→403, `DEPENDENCY_UNAVAILABLE`→503, …).
- Heavy operations (`POST /v1/repositories/{id}/index`, `/v1/verification`,
  `/v1/evaluation/runs`) return **`202 Accepted` + `Location: /v1/jobs/{id}`**
  and execute through a bounded runner: at most 2 heavy jobs at once,
  per-repository serialization, and duplicate submissions of a
  non-terminal job never double-execute. Clients poll `GET /v1/jobs/{id}`
  (state machine TIS §15.1); job result payloads persist in the ledger.
  There is no cancel endpoint (documented limitation).
- `Idempotency-Key` header (TIS §13.3) replaces the derived key when
  present — one logical job per key; repeated submits against the same tree
  replay the same job.
- Security (NFR-020): loopback-only by default; `Authorization: Bearer
  <token>` when `[api] token` or `CCC_API_TOKEN` is set; a non-loopback
  bind **without** a token is refused at startup. Config precedence:
  defaults < `[api]` in `config.toml` < `CCC_API_BIND`/`CCC_API_TOKEN` <
  `--bind`.
- `GET /v1/openapi.json` serves the live schema.

```console
$ ccc mcp          # MCP stdio server (agent integration)
```

Agent configuration:

```json
{
  "mcpServers": {
    "ccc": {
      "command": "ccc",
      "args": ["mcp"]
    }
  }
}
```

MCP facts (TIS §13.5): exactly **8 read/analysis tools** —
`repository_status`, `search_code`, `lookup_symbol`, `trace_dependencies`,
`compile_context`, `inspect_evidence`, `analyze_change_impact`,
`get_verification_results` — every one annotated `read_only` and pinned by
test (no execution-capable or memory-mutating tool is exposed; memory
mutation and verification execution are separately permissioned later-step
work, SDD §16.2). Each invocation is repository-scoped (explicit `repo`
argument or the configured default via `ccc mcp --repo <id|path>` — no cwd
fallback); unknown repositories are tool-level errors carrying the stable
TIS code. Responses embed the same envelope shapes as REST
(`structuredContent`) with explicit caps: item limits (default 20, hard cap
50), compile budget cap 16 000 tokens (over-cap is refused before
compiling), a 1 MB serialized-result cap whose trimming sets
`"truncated": true` — results are never silently byte-chopped. The MCP
session smoke verified the full initialize → tools/list → tools/call
handshake over stdio (protocol version negotiated with the rmcp SDK).

Step 4 also adds the FR-010 **drift projection**: `index_current` (active
generation's HEAD + dirty flag vs. the repository now) rides in response
**meta only** — `ccc status` (human + JSON) and the API/compile responses —
never inside a bundle DTO, so bundle bytes stay identical (pinned by
tests).

## Verification model (TIS §11, SDD §15, ADR-008)

- **Policies live only in the data dir** (`~/.ccc/policies/<name>.toml`).
  Repository content can never define, extend, or override a policy; an
  unknown policy name is `INVALID_REQUEST` — no silent fallback.
- **The built-in `analysis` policy executes nothing.** Executable
  suggestions are recorded `not_run` (never `passed` when skipped); the
  PATH-poisoned zero-subprocess property is pinned by test.
- **Authorized execution** requires `allow_execution = true` plus an exact
  argv-prefix allowlist (the disallowlist wins). Timeouts kill the process
  tree (`timed_out`), missing binaries are `unavailable` (never `failed`),
  output is size-capped with truncation flagged, and the executed
  environment is minimal — the env allowlist must name everything the
  toolchain needs (MSVC hosts: see the D-2 note in `docs/eval-log.md`).
- **Revalidation is scoped** (SDD §15.6): a fully passing run flips only the
  claims whose dependencies intersect the verified files/symbols to
  `supported` (with `validated_at` + a verification-evidence link); failing
  checks mark covered claims `conflicted`; ambiguous runs touch nothing.
  Negative results are preserved as evidence rows and records, never
  discarded after a later pass.
- **Evaluation is recorded, never assumed** (`docs/eval-log.md`): every
  ranking/weight change needs a hashed suite, recorded runs on both sides,
  and a measured comparison before promotion. Suites are immutable
  (content-hashed); runs with identical (suite, policy, config) inputs
  produce byte-identical metrics JSON; held-out cases are scored separately.

## Correctness posture (what the numbers mean)

- Name-level references are `candidate` by default: syntax parsing is never
  presented as semantic resolution (TIS §7.2). Uniqueness upgrades to
  `resolved` only on exact, unique qualified-name matches.
- Exclusions are recorded with reasons (`excluded_by_pattern`, `binary`,
  `oversized`, `unsupported_language`) and visible in `ccc status` — nothing
  is silently dropped.
- Token estimates are `approximate_chars_div4` and labeled as such; they are
  never presented as exact measurements.
- Index publication is atomic: interrupted runs leave the previous published
  generation active; mixed-snapshot data is rejected by referential checks.
- Two identical compiles produce identical bundle content hashes
  (determinism pinned by tests); each compile is a new immutable bundle.
- Claims of changed files start `stale` after a reindex — the source changed
  beneath them, so revalidation is owed even when the text re-describes the
  current source (TIS §6.3; SDD §20.5 integrity scenario, covered by the
  evaluation harness).
- Memory freshness is a projection, never a judgment: a memory linked to a
  claim follows that claim's status (`stale`/`conflicted` with a recorded
  cause; recovery to `candidate` only for claim-derived staleness), while
  memories without claim links are untouched by invalidation. Terminal
  states (`superseded`, `retracted`) are never touched by any transition.
- The memory channel is tier-4 (last) in bundle selection and reuses the
  existing `w_f`/`w_r` features — measured in E-003/E-004 with no weight
  change; bundles without memory data stay byte-identical.

## Development

```console
scripts/check.ps1        # fmt + clippy -D warnings + cargo test --workspace
```

```console
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all -- --check
```

158 tests cover the Step 4 surface — everything from Steps 1–3 (memory
stores/transitions/detection, agent-finding capture, freshness projection
and recovery, conflict resolution with recorded rationale, verification
policies/executor per status, scoped revalidation, evaluation determinism
and integrity gates, tier-ordered selection with the memory channel, the
golden CLI integration tests in `crates/ccc-cli/tests/golden.rs`, and the
dogfood fixture in `tests/fixtures/rust-sample/`) plus the Step 4 additions:
service-layer extraction (byte-identical golden output via `RawValue`
payloads), the FR-010 drift guard (clean/dirty/commit-advanced/unindexed),
the REST `/v1` router (envelope shapes, error-status mapping, token on/off,
loopback-bind validation, 202+Location job flow with replay and failure
paths), the bounded job runner, and the MCP adapter (initialize handshake,
pinned read-only tool surface, budget/trim caps, unknown-repo tool errors).

Open limitations carried forward deliberately:

- No job cancellation endpoint (CLI parity; the local runner re-runs a
  stale `running` job on resubmit).
- The MCP surface is read-only by design; memory mutation and verification
  execution arrive behind explicit permissioning (SDD §16.2).
- `repo add`/`index` still read `[indexing]` config from the **default**
  data dir (`CCC_HOME`/`~/.ccc`) rather than `--data-dir` — pre-Step-4
  behavior preserved verbatim to keep config-hash identity stable; a
  deliberate unification is future work.
- D-2 (MSVC env allowlist) and D-3 (locked `ccc.exe` when dogfooding from
  the same `target/`) still apply; integration tests exercise the router
  in-process for exactly this reason.
