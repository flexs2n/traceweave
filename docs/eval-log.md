# Evaluation Log (ADR-008)

Every retrieval-policy weight change requires a recorded experiment: a hashed
suite, two recorded `evaluation_runs` (baseline vs candidate), and a written
comparison here before promotion. No weight changes without an experiment
record (controlled promotion, SDD §20.6–20.7).

---

## E-001 — policy-v2 candidate (A/B against policy-v1)

- **Date:** 2026-10-17
- **Suite:** `fixture` (`eval/suites/fixture.toml`), suite hash
  `e218f46e6dcde810980caf9e8bb044f27c60e808c647458855053048ac3ee2fc`
- **Repository:** rust-sample fixture (3 indexed files; identical config hash
  on both runs `fca50f39b4dd36da…`)
- **Baseline (policy-v1):** run `01a14960-c879-703e-a5d2-4613da4693cf`,
  policy hash `70ab49e8e6891a99a6dc22dd43d7e4a51e936b73654fb73df16fc707075b9955`
- **Candidate (policy-v2, `eval/policies/policy-v2.toml`):**
  run `01a14960-c949-711c-8b34-0cac3dfb590c`,
  policy hash `56f1d1804932da59acbc8999721db7b9aa1553c135d720f18270e4c46732bf96`
- **Candidate hypothesis:** raising `w_d` (dependency proximity, 900 → 1100)
  and lowering `w_x` (redundancy penalty, 500 → 400) improves precision@10
  without hurting recall.

### Measured result

| Metric (fixture suite) | policy-v1 | policy-v2 candidate |
| --- | --- | --- |
| tuning recall | 100‰ | 100‰ |
| tuning MRR | 60‰ | 60‰ |
| tuning precision@10 | 70‰ | 70‰ |
| tuning stale rate | 0‰ | 0‰ |
| tuning token cost | 148 | 148 |
| held-out recall | 100‰ | 100‰ |
| held-out MRR | 100‰ | 100‰ |
| held-out token cost | 62 | 62 |

### Decision: keep policy-v1 (no promotion)

The candidate produced identical metrics on every measured dimension: on the
small fixture the channel tier ordering dominates the ranking, and the
within-tier score shifts from `w_d`/`w_x` did not cross any tie-break
boundary. Per ADR-008, weight changes are promoted only on measured
improvement — there is none here, so **policy-v1 remains the shipping
weights** and the v2 file stays as the recorded candidate for future, larger
suites (e.g. the `dogfood` suite on traceweave itself).

### Notes

- Both runs are immutable and queryable via `ccc eval show <run-id>`.
- Held-out cases are scored separately from tuning cases (anti-overfit,
  TIS §19.4); neither group was used to tune weights in this experiment.
- Determinism was verified: rerunning the candidate reproduced byte-identical
  metrics JSON (`policy_ab_records_two_runs_with_distinct_policy_hashes`).

---

## E-002 — tier-ordered selection (dogfood finding D-4, promoted)

- **Date:** 2026-10-17
- **Suite:** `dogfood` (`eval/suites/dogfood.toml`), suite hash
  `bf09cb4fd7917dcb33ebe17b5824374e8145e99e658e2dd470f96cd132778c6c`, run on
  traceweave itself (same config hash both sides, `fca50f39b4dd36da…`).
- **Change under test:** the greedy utility sweep inside
  `select_within_budget` ignored the documented channel-tier pre-order
  (tier → score desc → token asc → entity id), letting cheap
  `dependency_depth_3` items with high utility crowd out exact task seeds.
  The fix processes tiers in order and applies marginal utility within a
  tier. Found by the evaluation harness during dogfooding (finding D-4).

### Measured result (policy-v1 weights both sides)

| Metric (dogfood suite) | before (utility-only) | after (tier-ordered) |
| --- | --- | --- |
| tuning recall | 12‰ | **93‰** |
| tuning MRR | 2‰ | **20‰** |
| tuning precision@10 | 5‰ | **25‰** |
| tuning token cost | 11480 | 11469 |
| held-out recall | 25‰ | **50‰** |
| held-out MRR | 6‰ | **30‰** |
| case 0 recall | 0/3 | **3/3** |

### Decision: promote

The tier pre-order was already the documented contract (TIS §9.3–9.4);
selection now implements it, and every measured dimension improved or held
(on the `fixture` suite the golden assertions are unchanged). The weight-only
candidate from E-001 remains non-promoted: on the dogfood suite the v2
weights measured slightly *below* v1 under tiered selection (tuning recall
87‰ vs 93‰) — recorded, not adopted.

## Dogfood findings (2026-10-17, traceweave @ step-2)

Real issues surfaced by running the full Step 2 flow against traceweave
itself; each was either fixed in-repo or documented:

- **D-1 (fixed):** test selection suggested `cargo test --test fixtures` for
  nested `tests/fixtures/rust-sample/lib.rs` — not a cargo target. The
  heuristic now only suggests real targets (`tests/<name>.rs`,
  `tests/<dir>/main.rs`); anything else falls back to the labeled broad
  suite.
- **D-2 (documented, policy knob):** the default minimal check environment
  strips the inputs the MSVC linker detection needs, so a cold `cargo test`
  reports `failed` (linker not found). Policies that run cargo on MSVC hosts
  list the toolchain environment explicitly (`env_allowlist`) — the shipped
  dogfood policy does; the limitation is recorded in every check's
  environment summary.
- **D-3 (documented):** verifying traceweave while running the locally built
  `ccc` from the same `target/` fails builds with "failed to remove
  ccc.exe: access denied" (Windows locks the running executable). The
  verification honestly reports `failed`; work around by running a binary
  outside the verified tree. Self-hosting limitation, not a code defect.
- **D-4 (fixed + promoted):** see E-002.

---

## E-003 — memory channel A/B (fixture repo, no weight change)

- **Date:** 2026-10-17
- **Suite:** `memory` (`eval/suites/memory.toml`, new in Step 3), suite hash
  `c93a9d3927440797fb3f2489446eaacddabe14324fa92fe06715649e23075472`, run
  against a fresh `rust-sample` fixture repository. Same config hash both
  sides (`fca50f39b4dd36da…`), **policy-v1 on both sides** (the channel
  reuses the existing `w_f`/`w_r` features — no new weight, per ADR-008).
- **Change under test:** the new `Memory` retrieval channel (TIS §9.2
  candidate source 7). Side A: no memory records exist (empty channel).
  Side B: three titled memory records seeded via `ccc memory add`.

### Measured result

| Metric (memory suite) | A: empty channel | B: channel active |
| --- | --- | --- |
| tuning recall | 0‰ | **1000‰** |
| tuning MRR | 0‰ | **583‰** |
| tuning precision@10 | 0‰ | **583‰** |
| tuning token cost | 86 | 159 |
| held-out recall | 0‰ | **1000‰** |
| held-out token cost | 82 | 116 |

Run A: `01a149e0-339b-73ea-815b-e54f6b4c9d27` · Run B:
`01a149e0-7d20-73ab-9e64-f588eab80173` (both immutable, `ccc eval show`).

### Decision: keep the channel active in policy-v1 (no weight change)

The channel surfaces memory records exactly when they match the task
(0 → 1000‰ recall) at a modest token cost (+73 tuning tokens on a 3-file
repo), and produces zero candidates — byte-identical bundles — when no
memory data exists (the `w_h = 0` precedent, pinned by
`memory_channel_surfaces_labeled_memories_and_conflicts`). The code-expectation
fixture suite is unaffected (its golden run is unchanged). No new weight is
introduced, so nothing to promote: **policy-v1 remains the shipping policy**.

---

## E-004 — dogfood suite v2 (memory cases on traceweave itself)

- **Date:** 2026-10-17
- **Suite:** `dogfood` v2 — two memory-channel cases appended
  (new suite hash `046fdb67cb827ce02ee46610fb4fa408436f775dc34919b80dae29f249dad220`;
  the v1 suite `bf09cb4f…` stays recorded). Run on traceweave itself, same
  config hash both sides, **policy-v1 on both sides**.
- **Change under test:** same as E-003, measured on a real 36-file
  repository with realistic budgets (2500–4000 tokens).

### Measured result

| Metric (dogfood suite) | A: no memories | B: 3 memories seeded |
| --- | --- | --- |
| tuning recall (5 code cases + 1 memory case) | 850‰ | 850‰ |
| tuning MRR | 148‰ | 148‰ |
| tuning token cost | 15429 | 15429 |
| held-out recall (2 cases) | 500‰ | 500‰ |
| memory-title expectations (cases 5–6) | 0/1, 0/1 | 0/1, 0/1 |

Run A: `01a149e0-fca2-76a6-b6ff-e1130c4ca2b4` · Run B:
`01a149e1-d1c6-73c3-aa4d-584ddde2c602`.

**Key observation:** the memory candidates *are generated* (case 5's
`Memory transitions keep history` appears in the run-B omissions with reason
`lower_relevance`) but are **not selected**: tier-4 priority (TIS §9.4
priority 5 — engineering decisions come after task-critical code, deps and
tests) exhausts the 2500–4000-token budgets on a large repository before the
memory tier is reached. Code recall is unchanged (A ≡ B on every code
expectation) — the channel adds no regression risk, but tight budgets keep
memory items out on large repos.

### Decision: keep policy-v1; record the budget-priority limitation

No promotion: there is no measured improvement to adopt and the tier
pre-order is the promoted E-002 contract — weakening it to boost memory
visibility would need its own measured experiment (e.g. a small reserved
memory quota in a policy-v3 candidate). The omission is visible, not silent:
omitted memories are listed with their reason in every bundle.

## Dogfood findings (2026-10-17, traceweave @ step-3)

- **D-5 (documented):** PowerShell drops empty-string CLI args, so
  `--scope ""` fails clap parsing with "a value is required". Repository-wide
  memories are created by *omitting* `--scope` (the flag's default is empty);
  the human output renders that as `(repository-wide)`.
- **D-6 (documented, follow-up):** on large repositories with realistic
  budgets, tier-4 memory items are generated but omitted
  (`lower_relevance`/`budget_exceeded`) — see E-004. Candidate future work: a
  versioned memory-quota policy, only behind a measured A/B.
- **D-7 (documented):** the manual down-migration harness splits files on
  `;` and skips chunks that begin with a comment line — a `;` or a leading
  comment inside a down file silently changes which statements run. The 0003
  down file therefore lists statements first, comments last; up files are
  unaffected (sqlx's migrator handles comments).
- **Freshness projection verified end-to-end:** the golden flow links a
  memory to a `read_config` symbol claim, edits `util.rs`, reindexes, and the
  memory flips `stale` with the recorded cause while unrelated memories stay
  untouched (`golden_memory_and_conflict_flow`).

