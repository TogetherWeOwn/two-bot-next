# Handoff — 2026-11-10 (UTC)

Dated snapshot for resuming work from this repo alone. Confirm current
assignments, dependencies and status with the project lead and live GitHub
PRs and issues. Record corrections as dated updates.

## Where to read first

1. [Current-state plan](plans/current-state-2026-11.md) — approved vs
   deferred scope, what is done, and how each item is verified.
2. [Documentation map](README.md) — every guide and specification,
   including the accepted M2 specs.
3. [ADR 0002](adr/0002-m2-m3-sequencing.md) — why M2 finishes before
   later milestones merge.
4. [ADR 0003](adr/0003-m3-deferred.md) — why five later-milestone
   items are deferred, undecided.

## Current source state (2026-11-10)

- The bot is a Rust rewrite running as one always-on container with a
  shared Postgres database and signed internal actions. Architecture
  and status: see [`README.md`](../README.md).
- Four M2 specifications are accepted (2026-11-01): lockdown preview,
  automod verdict preview, warning history and custom-command
  arguments. Acceptance is intent only; build, publication and
  production-activation gates remain. See the current-state plan for
  the per-item table.
- Five later-milestone ideas are retained but deferred, undecided
  (2026-11-10). No build is committed.
- For public direction, see the [roadmap](ROADMAP.md), which landed
  after this snapshot in #2021. It summarizes direction without changing
  the approved scope.
- M2 state: lockdown preview merged slices are implemented, dark and
  unpublished (planner #1920, executor reads #1925, dark slash #1929);
  the confirm/cancel build step #1938 was open; `{args}` build slices
  merged in #1970/#1974; automod preview slices #1959, #1961–#1965
  were open; warning history was spec-only. See the current-state plan
  for the per-item table.

## Relevant public PRs and branches

- At snapshot, the in-flight M2 builds were automod preview slices
  #1959, #1961–#1965 and lockdown confirm/cancel #1938. No M3 build
  PR had been opened.
- The `{args}` spec status correction #2002 landed after this snapshot.
  It records the builds merged in #1970/#1974, not production activation.

## Blockers (public-safe summary)

- **Blocked on staging recovery at snapshot:** automod preview (W3),
  warning history (W4) and `{args}` gated release (W5). Staging recovery
  means database re-home, then staging migrations, then the staging
  lifecycle drill. Details
  live in the staging and cutover runbooks, not here; no incident
  internals are reproduced.
- A production incident with a protective hold is in progress at
  snapshot. Release holds, staging qualification and rollback proof
  follow [`CONTRIBUTING.md`](../CONTRIBUTING.md#production-merge-evidence)
  and the [production deploy runbook](production-deploy.md). This file
  makes no claim that a docs change clears any hold.
- Later-milestone merges are gated on M2 finishing (ADR 0002).

## How to run the checks

Follow the pre-push checklist in
[`CONTRIBUTING.md`](../CONTRIBUTING.md#pre-push-checklist). That file is
the authoritative list; this handoff does not reproduce it.

Run full suites on GitHub Actions and record the exact-head run links in
the PR body. Local compilation is for
[focused failure reproduction](../CONTRIBUTING.md#focused-failure-reproduction-only)
only; see the [build cache](build-cache.md) for permitted controller fallback.
[Database tests](../CONTRIBUTING.md#database-tests) use only disposable databases.

## Last verification

- Snapshot base: `main` at
  `a8ca2c02cfcced654a680853b93655f898bbc51f`.
- Candidate verification: PR #2027, `ci-ok` run
  <https://github.com/TogetherWeOwn/two-bot-next/actions/runs/42885988083>
  (check workflow; ci-ok job
  `130093576599` passed at head
  `c0a5e801b47da865fba7c8b799d7aa6abca10f07`). Re-verify at each
  resubmitted head; the PR link resolves the current head.
- At snapshot, accepted-spec links, relative targets and the docs
  index were checked with `python3 scripts/check_docs_index.py --json`
  and `git diff --check`.
- No deployment, drill or production activation was performed to
  write this file.

## Next unfinished step (2026-11-10 snapshot)

For current next steps and remaining evidence, see the
[November 21 source reconciliation](plans/current-state-2026-11.md#source-reconciliation-2026-11-21-utc).

Resume the three remaining M2 items once staging recovery lands:
automod preview (W3) first, then warning history (W4), then `{args}`
(W5) gated release. Re-check each spec's seams on current `main`
first, then follow the spec's gates and the current merge and staging
requirements. The lockdown preview's merged slices are implemented
but dark; publication and activation remain behind its spec §8 gates.
At snapshot, the confirm/cancel build step (#1938) was still open
and the candidate accepted only application-command interactions.
Keep M3 deferred and leave release holds in force.
