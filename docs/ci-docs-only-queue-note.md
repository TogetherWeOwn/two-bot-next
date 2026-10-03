# Docs-only PR check-queue wait: measurement note

Follow-up read-only slice to the path-scoped fast-pass (#434).
No workflow changed in this note.

## Method

- Sample: 25 merged PRs, #422–#462, merged 2026-10-03 19:30–23:50 UTC.
  Window straddles the fast-pass merge (#434, 21:23Z), so pre/post
  behavior is comparable.
- Classification: each PR's file list run through the
  `scripts/job-inputs.py` selector. "docs-only" means the selector
  picks no `rust` and no `parity` job (13 PRs); everything else is
  "code" (12 PRs).
- Queue wait: per `check`-workflow job, `started_at - created_at`
  from the Jobs API on the merged head SHA's run. Successful jobs
  only; skipped jobs are 0 s by definition.
- Wall clock: run `created_at` to `updated_at` on the same run.

## Results

Queue wait is negligible for both groups on hosted runners —
the fast-pass saves wall clock, not queue position.

| Group | Jobs timed | Median job queue | p90 job queue | Median per-PR max queue | p90 per-PR max queue |
|---|---|---|---|---|---|
| docs-only (13 PRs) | 85 | 2.0 s | 3.0 s | 3.0 s | 9.2 s |
| code (12 PRs) | 148 | 2.0 s | 3.0 s | 4.0 s | 12.6 s |

| Group | Median wall clock | p90 wall clock |
|---|---|---|
| docs-only (12 PRs) | 8.2 min | 9.0 min |
| code (12 PRs) | 27.0 min | 28.2 min |

Heavy-job skips: 12 of 13 docs-only runs skip exactly the same 6
jobs (the five Rust DB jobs plus `parity-docs`); code runs skip at
most one. The `worker check` job still runs on docs-only PRs
(~0.9 min) because `docs/` changes select `worker` by design
(`runbook.test.ts` asserts on the docs listing). The #434
always-run change only affects asset-only (`worker=false`) PRs,
of which this window has none; pre/post docs-only behavior is
identical (compare #429 pre-merge with #453 post-merge: same
6 skips).

Outlier: #423 (docs-only) shows a 62 min wall, zero skips, and
negative queue timestamps — a re-run record artifact. Excluded
from the wall-clock aggregates; it does not change the medians.

Worked example (#462, docs-only, 7.7 min wall): container smoke
6.5 min and the SBOM image scan 6.0 min run in parallel and
dominate the wall; `worker check` takes 0.9 min and the `check`
aggregator 1.2 min.

## Recommended next slice (≤4h)

Gate `container smoke` and the SBOM dry-run jobs for docs-only
PRs, following the #434 pattern: prove with the container-inputs
selector that no `docs/` path feeds the image build or the SBOM
inventory, add the per-job skip, and cover it with fixture tests
in `scripts/test_container_inputs.py`. Expected effect: docs-only
wall drops from ~8 min to ~2 min. If any docs path turns out to
be a real input (e.g. a doc copied into the image), scope the
gate to the proven subset instead and record the exception here.
