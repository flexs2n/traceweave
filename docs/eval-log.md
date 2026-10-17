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
