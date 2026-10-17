# traceweave / ccc

Change-aware context compiler — Step 2 (verification + evaluation).

`ccc` indexes a git repository into immutable, content-addressed snapshots,
extracts Rust symbols and name-level relationships with Tree-sitter, and
compiles token-budgeted, provenance-carrying context bundles for engineering
tasks. Step 2 closes the loop: *change → predicted impact → verification
under an explicit execution policy → scoped evidence revalidation*, plus an
offline evaluation harness that scores the retrieval pipeline against hashed
golden suites. Everything runs locally; no LLM calls are made in this build
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
| `ccc-context-compiler` | task → ranked, budgeted bundle (policy-v1) |
| `ccc-impact` | change → impact report with conservative classification |
| `ccc-verification` | execution policies, test selection, authorized executor, evidence revalidation |
| `ccc-evaluation` | offline retrieval/integrity harness over hashed suites |
| `ccc-jobs` | idempotent job records (TIS §15.1 state machine) |
| `ccc-cli` | the `ccc` user surface |

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

Doctor output on the reference environment (Windows):

```console
$ ccc doctor
data_dir_writable    OK   — ~/.ccc
git_on_path          OK   — git available
database             OK   — opened and migrated
tree_sitter_rust     OK   — grammar parses
```

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

## Development

```console
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all -- --check
```

105 tests cover the Step 2 surface — verification policies/executor per
status, scoped revalidation, evaluation determinism and integrity gates,
tier-ordered selection, plus the golden CLI integration test
(`crates/ccc-cli/tests/golden.rs`) and the dogfood fixture
(`tests/fixtures/rust-sample/`).
