# traceweave / ccc

Change-aware context compiler — Step 1 vertical slice.

`ccc` indexes a git repository into immutable, content-addressed snapshots,
extracts Rust symbols and name-level relationships with Tree-sitter, and
compiles token-budgeted, provenance-carrying context bundles for engineering
tasks. Everything runs locally; no LLM calls are made in this build
(NFR-012: provider boundary is an explicit opt-in extension point).

## Stack

- **Rust (edition 2021), primary language** (TIS §2 amendment — Python is
  restricted to offline ML/eval in later steps).
- **SQLx + SQLite** with WAL; FTS5 for the derived lexical index (verified
  available in the bundled build — plan risk R1 resolved by smoke test).
- **Tree-sitter** (`tree-sitter-rust`) for symbol/reference extraction.
- **BLAKE3** content addressing: files, parsed artifacts, manifests, bundles.
- **Tokio** bounded-concurrency parsing; single serialized writer connection.

## Workspace layout (TIS §21)

| Crate | Responsibility |
|---|---|
| `ccc-domain` | IDs, enums, error taxonomy, entity records, canonical JSON |
| `ccc-config` | configuration precedence + `configuration_hash` |
| `ccc-repository` | `GitProvider`/`GitCli` adapter, registration |
| `ccc-snapshot` | snapshot identity, manifest hash |
| `ccc-storage` | store traits, SQLite adapter, artifact store |
| `ccc-schema` | versioned serialized contracts |
| `ccc-tree-sitter` | Rust syntax extraction |
| `ccc-indexer` | incremental indexing pipeline, atomic publication |
| `ccc-search` | lexical + exact symbol lookup |
| `ccc-evidence` | evidence ledger, claim staleness seam (FR-040) |
| `ccc-context-compiler` | task → ranked, budgeted bundle (policy-v1) |
| `ccc-impact` | change → impact report with conservative classification |
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
Parsed: 29, reused: 0, excluded: 24 (unsupported: 24), failed: 0

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

Doctor output on the reference environment (Windows):

```console
$ ccc doctor
data_dir_writable    OK   — ~/.ccc
git_on_path          OK   — git available
database             OK   — opened and migrated
tree_sitter_rust     OK   — grammar parses
```

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
- `ccc verify` / `ccc eval run` are declared not implemented in this build
  and fail with `EXECUTION_NOT_AUTHORIZED` / `INVALID_REQUEST` rather than
  pretending availability.

## Development

```console
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all -- --check
```

79 tests cover the vertical slice, including the golden CLI integration test
(`crates/ccc-cli/tests/golden.rs`) and the dogfood fixture
(`tests/fixtures/rust-sample/`).
