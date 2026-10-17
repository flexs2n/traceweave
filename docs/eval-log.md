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
