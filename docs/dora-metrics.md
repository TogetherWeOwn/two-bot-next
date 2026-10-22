# Weekly DORA metrics

Every Monday at 07:17 UTC the `dora-metrics` workflow computes four delivery
numbers over the trailing 30 days and commits them to `ops/metrics/` on the
`automation/dora-metrics` branch as `dora-YYYY-Www.json` plus a short
`dora-YYYY-Www.md` summary. The files are generated evidence; do not edit
them by hand.

## The four numbers

| Metric | Definition | Target |
| --- | --- | --- |
| G2 change failure rate | failed prod deploys / resolved prod deploys | <5% |
| G3 recovery time | failed deploy -> restored service, per incident | auto <5 min, worst <30 |
| G4 merge-to-prod lead time | merge time -> prod success, p50/p90 nearest-rank | p50 <60 min, p90 <3 h |
| G5 throughput | deploys/day plus commits-per-deploy | ships hourly when green work exists; <=15 commits/deploy |

## Sources

- Production Deployments (`environment=production`), newest first, down to
  the window start. Only rows carrying a canonical deploy ID
  (`YYYY.MM.DD.N+sha7`) count; the Actions automatic `environment:` rows
  carry none and are tallied as `excluded_automatic_rows`, never scored.
- Each deployment's latest status (success/failed/in-flight/unknown).
- Enrichment per deploy, degraded loudly when a read fails: the commit's
  committer date for lead time, the `compare` ahead-by count for
  commits-per-deploy, and the Actions run behind the status `log_url`
  (conclusion + head SHA) for audit.
- Recovery pairs each failed deploy with the next green one; the mechanism
  reads `rollback` when the restoring record's payload says so, else
  `next_green`. Failures with no later green deploy list under
  `unrecovered` for a human look.

Every input ID (deployment IDs, SHAs, run IDs, verdicts) lands in the JSON
`deployments` block, so each number spot-checks against the API.

## Gaps read as gaps

- A deployment with no status yet, or still in flight, stays out of the
  failure rate and is counted beside it.
- An unreadable commit date or compare count becomes a null field plus an
  `unresolved_*` count; percentiles cover only resolved values.
- An empty window reports nulls, never zeros.

## Operating the job

- Dispatch `dora-metrics` with `dry_run` from any branch to run the real
  computation and upload the files as an artifact without committing.
- A live dispatch (or the Monday schedule) on `main` commits straight to
  the `automation/dora-metrics` branch with the job token: `main` only
  accepts pull requests, and the automation branch carries no rulesets. The
  checkout stays credentialless and the push authenticates per command. A
  refused or conflicting push fails the run closed; the artifact still
  preserves that week's numbers.
- The weekly side-branch push triggers no deploys; `ops/**` is also in
  `deploy-staging`'s push ignore list, so any future main-side delivery
  evidence restarts and promotes nothing.
