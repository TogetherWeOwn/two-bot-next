# CI security gates

The existing `check`, `pr-lint`, and `gitleaks` names remain unchanged. The
`check` job also runs the offline source-gate fixtures, source snowflake gate,
CODEOWNERS verification, and **zizmor 1.30.1**. zizmor is installed from its pinned
binary wheel without dependencies and runs with `--offline --strict-collection`;
it needs no GitHub token. All workflows default to `permissions: {}` and declare
job-specific grants. Every checkout sets `persist-credentials: false`.
The [public-hosted runner contract](#public-hosted-runner-contract) covers explicit
standard Ubuntu selections, compatibility evidence and qualification limits.

The daily digest job holds `contents: write` to tag and publish its release,
`deployments: read` to resolve the live production commit, and `actions: write`
to dispatch the SBOM workflow. That workflow keeps script execution read-only;
only `attach-sbom` holds `contents: write` to upload verified release assets.
Untrusted PR metadata and selector event names travel through environment
variables, never interpolated into shell source. The deploy job does not restore
caches from PR checks. The two inline Python metadata guards use supported Bash
steps with isolated Python (`python3 -I -`) and quoted heredocs, preserving their
base-branch gate provenance without importing modules from the PR checkout.
Explicit job names match their previous jobs-API identities; permission comments
explain the existing grants without changing their values.

For the advisory `pr-evidence` check, follow the maintainer procedure and
registration criteria in [Exact-head CI evidence](../CONTRIBUTING.md#exact-head-ci-evidence).

### Rehearse a same-head description edit

On a task draft, capture `GET /pulls/{number}` before a description-only edit
while the advisory check is green. Retain the matching workflow run attempt
(`GET /actions/runs/{run_id}/attempts/{attempt}`) and its jobs response
(`GET /actions/runs/{run_id}/attempts/{attempt}/jobs`). A job may omit
`run_attempt` on the first attempt. Reruns require an explicit match to
exclude jobs from earlier attempts.
After the edit, capture the PR again and keep the later advisory run attempt
and jobs response triggered by that edit. Use the Actions job
log to confirm the reason if `Verify and publish evidence` fails. Keep each
description stable through its run and capture. Restore a valid description
after the rehearsal and confirm the resulting advisory run succeeds.

Use a same-repository draft against a stable base SHA. A moved base or a run
without the PR in its associations makes the comparison unavailable.
Save each set of REST response objects in a local JSON file under the keys
`pr`, `run` and `jobs`. Omit both `run` and `jobs` in the current capture if the
edit has no run receipt yet. The files contain PR descriptions. Keep them out
of the repository and retain only the run and job links and the reported results.

```sh
python3 -B scripts/ci/rehearse_pr_evidence.py pr before.json current.json
python3 -B scripts/ci/rehearse_pr_evidence.py group checks-requested.json
```

The PR report distinguishes a failed verification step in the current advisory
run (`current_advisory=failure`) from a prior passing run paired with a different
description at the same head (`prior_hosted=stale_green`). Both can appear in
one report. A workflow failure after verification passed reports
`current_advisory=failed_after_verification`. Exit status is 0 for current
advisory success, 1 for a reported failure, and 2 for unavailable or unmatched
evidence. The advisory job publishes no description digest. The comparison relies
on the operator pairing each stable description with its run and job. GitHub can retain the
earlier successful result on the same head, so the receipt cannot establish
freshness at merge time.

The group command accepts the `merge_group.checks_requested` webhook payload
and returns `merge_group=unavailable` with exit status 2 under the membership
contract below. Required registration still needs hosted evidence of complete
group membership and enforcement of a current description at merge time, then
the qualification runs listed in [Exact-head CI evidence](../CONTRIBUTING.md#exact-head-ci-evidence).

### Merge-group evidence and PR membership

GitHub supports `merge_group: checks_requested` for protected ruleset
workflows. The event identifies a synthetic group commit and ref. The
`merge_group` object has `head_sha`, `head_ref`, `base_sha`, `base_ref` and
`head_commit`, with no member list or per-PR head SHAs. GitHub documents
that the group includes changes from PRs ahead of the named entry. Its
two-PR example shows the temporary branch for PR #2 containing both PRs.
The queue ref's PR number and the group's base and head SHAs therefore
cannot supply all the PR identities the verifier needs.
[Ruleset events][ruleset-events] · [Webhook schema][group-schema] ·
[Merge queue behavior][queue-behavior]

GraphQL `Repository.mergeQueue(branch:)` exposes the current queue.
`MergeQueue.entries` exposes each entry's position, PR, and nullable base
and head commits. The schema describes those commits at entry scope and
provides no documented mapping from the event's group SHA to the complete
set of entries and their PR heads. Queue positions can change as groups
are rebuilt. The REST commit-to-PR endpoint promises commit associations,
and `GET /pulls/{number}` can refresh an already identified PR. Neither
supplies that mapping. For example, the archived run below has an empty
`pull_requests` array in the workflow-run API, while the commit-to-PR
lookup for its group SHA returned #1861. That PR's recorded merge commit
equals the group SHA. Those responses provide no proof of the full group
membership at check time.
[GraphQL queue entries][queue-entry] · [Commit associations][commit-pulls] ·
[PR details][pr-details] · [Archived run][archived-group] ·
[Run API response][archived-run-api] · [Commit lookup][archived-commit-pulls] ·
[Merged PR metadata][archived-pr]

The advisory `pr-evidence` workflow subscribes to PR events and pushes to
`main`. Its verifier refuses `merge_group` in verify mode before publishing
evidence. A future adapter needs a supported source that binds the exact
group to every PR number and queued PR head SHA. Confirm each PR's
current base ref matches the protected target. Pass the target repository
(`base.repo.full_name`, including for fork PRs), PR number, current PR base
SHA and group-bound PR head SHA to `evaluate_current`. Before use, recheck
membership and each target ref and call `matches_current` for every verdict.
Missing or ambiguous membership, or a changed member snapshot at the final
read, must refuse the whole group. Even with that mapping, a PR description
can change after the last read without changing a commit SHA. The
documented merge API precondition pins the PR head SHA, and GitHub documents
no group invalidation on same-head description edits. Keep the advisory
edit procedure in force.
[PR merge API][merge-api] · [Exact-head CI evidence](../CONTRIBUTING.md#exact-head-ci-evidence)

#### Validate in a disposable queue

1. Use an already-provisioned disposable repository with no deploy or
   release workflows and an isolated queue that can build two groups
   concurrently. Start with two drafts with synthetic descriptions that
   each add a distinct marker file. Confirm both markers are absent
   from the fixture target branch. GitHub blocks draft merges, so mark
   the PRs ready before queueing them in order. Use an existing required
   check that passes on the PR heads and stays pending on group SHAs
   to prevent merging once the PRs are ready. Leave that check pending and
   collect the observations before the queue's status-check timeout.
   [Draft PR behavior][draft-prs]
2. From a trusted recorder, capture the group base and head refs and SHAs
   for each `checks_requested` delivery. The group for the second PR
   should contain both fixture PRs while the first remains ahead of it.
   Inspect the commit tree at that group's exact head SHA and confirm
   both marker files have their expected contents before interpreting
   the API results. Compare both fixture PRs to the candidate API's
   result for that SHA. Read all queue-entry pages and collect each
   entry's position, base and head OIDs, PR number and PR head OID.
   Also read the REST workflow-run associations, commit-to-PR associations
   and current metadata for each known fixture PR. Record the response
   status, identity fields and description digest. Keep the raw bodies,
   actor data and credentials out of the capture.
3. Edit only the first PR's description. Check whether the group SHA and
   deliveries change, and compare both PR metadata snapshots. Then dequeue
   the first PR and repeat the lookup for the original and replacement
   group SHAs. A missing, partial or ambiguous membership response, or a
   changed snapshot, is a refusal case. A matching sample documents observed
   behavior. Require a documented binding before relying on it as a gate.
   Remove both fixture PRs from the queue and close them.

[ruleset-events]: https://docs.github.com/en/enterprise-cloud@latest/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/available-rules-for-rulesets#supported-event-triggers
[group-schema]: https://docs.github.com/en/webhooks/webhook-events-and-payloads?apiVersion=2022-11-28#merge_group
[queue-behavior]: https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/configuring-pull-request-merges/managing-a-merge-queue#how-merge-queues-work
[queue-entry]: https://docs.github.com/en/graphql/reference/pulls#object-mergequeueentry
[commit-pulls]: https://docs.github.com/en/rest/commits/commits?apiVersion=2022-11-28#list-pull-requests-associated-with-a-commit
[pr-details]: https://docs.github.com/en/rest/pulls/pulls?apiVersion=2022-11-28#get-a-pull-request
[archived-group]: https://github.com/TogetherWeOwn/two-bot-next/actions/runs/40761465005
[archived-run-api]: https://api.github.com/repos/TogetherWeOwn/two-bot-next/actions/runs/40761465005
[archived-commit-pulls]: https://api.github.com/repos/TogetherWeOwn/two-bot-next/commits/76b048e3b39c757742e0f7d0af9ce2c425354dfb/pulls
[archived-pr]: https://api.github.com/repos/TogetherWeOwn/two-bot-next/pulls/1861
[merge-api]: https://docs.github.com/en/rest/pulls/pulls?apiVersion=2022-11-28#merge-a-pull-request
[draft-prs]: https://docs.github.com/en/pull-requests/reference/pull-requests#draft-pull-requests

## Recover a failed default CodeQL upload

Use this maintainer checklist when GitHub's default CodeQL run on a PR has
completed with a runner or results-upload failure.

1. Open the failed run and its job logs. Record the run URL, attempt, full
   head SHA, affected languages and failing step. Confirm the job output
   identifies a runner or upload service error, such as a server error while
   uploading SARIF. Check GitHub Status for related degradation. Treat the
   failed upload as incomplete scan evidence. If GitHub reports a security
   finding, follow the normal security triage. If the failure is ambiguous,
   stop and investigate before using this checklist.
2. Read the open PR's `head.sha`, head repository and branch in the GitHub
   REST API. Check the actual remote branch tip and `refs/pull/<number>/head`
   with `git ls-remote`, using the head repository for the branch and the
   base repository for the PR ref. Require both refs, the PR metadata and the
   failed run to agree on the full SHA and verify the run belongs to this PR.
   Stop on a mismatch. Once the run is terminal and the affected service has
   recovered, check whether GitHub accepts a manual rerun of that run. If
   accepted, use the original SHA and the new attempt in step 4. Record an
   explicit run-level refusal before considering a fresh head. Resolve
   permission, quota or transient API errors through their normal paths.
3. If GitHub refuses the rerun and the failure is confirmed infrastructure-only,
   the branch maintainer may make one normal fast-forward on the existing PR
   branch. Prepare an empty commit whose sole parent is the failed head. Verify
   both commit tree IDs match, then re-read the PR and both remote refs
   immediately before pushing. Proceed only while the incident is clear and
   all three still point to the failed head. Stop if the push is rejected.
4. After a same-head rerun, verify the branch, PR metadata and
   `refs/pull/<number>/head` still show the original SHA. After a fast-forward,
   verify all three show the child SHA. Stop if they disagree or the PR ref
   stays stale. Require a completed, passing default CodeQL scan for this PR
   ref and exact SHA. Check the current attempt of the rerun or replacement
   scan and its jobs for every required language. Confirm GitHub shows the
   required code scanning results on that head. Record the run and job links
   with the full SHA and keep the PR blocked if any required check is missing
   or failing on that head.
5. Before protected squash, follow the existing
   [review policy](../CONTRIBUTING.md#independent-review). Reuse prior review
   only when the reviewed PR diff is unchanged, including any effect of a
   base change, and record both SHAs and why the review still applies. Check
   all [required checks](../CONTRIBUTING.md#required-checks) and
   [production merge evidence](../CONTRIBUTING.md#production-merge-evidence)
   for the exact head you intend to merge.

## Staging deployment

Staging deployment is governed by `deploy-staging.yml`. Consult the workflow
at the relevant ref for its triggers; a merge is not evidence of deployment,
readiness, activation or qualification. Verify completed qualifying evidence
for the intended exact `main` SHA. If it is absent, keep promotion held and
follow the separately authorized [qualification/recovery procedure](staging-rollout-gate.md#full-window-workflow-and-recovery-semantics).
A skipped job is not deployment, protected-environment or E2E success; no
alternate manual deploy route is authorized.

CISO accepted the prior disabled CI-only scope in TOG-10958 and TOG-11179 plan
revision 2. This re-activation ships as a reviewed workflow PR with exact-head
green `check`, `pr-lint`, `gitleaks` and an independent Code Reviewer pass
(the reviewer decides whether CISO review is also needed). The frozen
31-occurrence baseline and independent exact-head green-CI review and
non-author squash-merge gates remain. The staging control secret is already
provisioned (TOG-12030); no credential creation or rotation is included here.

The offline workflow regression uses pinned PyYAML 6.0.3, rejects duplicate
keys, inventories every workflow/job, requires the staging `deploy` job to
carry no job condition while keeping the `staging` environment scope, the
explicit `ubuntu-latest` runner for job `deploy`, and the fenced `release_fence`
dispatch shape, rejects statically-disabled steps that would report success
without deploying, and rejects new jobs/workflows or known CI jobs repurposed
as unapproved deploy/probe alternatives. Negative fixtures cover job-condition
insertion/mutation, step-level disables, dispatch-shape widening and alternate
deployment jobs. It also preserves explicit public-hosted runner selection,
job-container service isolation, per-job grants and nonpersistent checkouts.

The active job retains GitHub environment `staging`. **A YAML environment
name alone does not create protection rules or scope repository-level secrets.**
The environment scoping stays in place, and secret placement is
environment-scoped with no repository-level fallback:

- The `staging` environment keeps its approved deployment approval and branch
  restrictions (only approved staging code, normally `main`).
- `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID` are stored only as
  Environment secrets, in `staging` and in `production`; there are no
  repository-scope copies, so only jobs running in those Environments can
  read them. The offline regression rejects any job that references the pair
  without declaring one of those Environments.
- As an in-file backstop behind the Environment branch policy, each
  `deploy-staging` Environment job (`schema-gate`, `roles-gate`, `deploy`)
  fails closed when its workflow ref is not `main`. The refusal is a step in
  the existing SHA check, not a job condition, so a wrong-ref run goes red
  instead of skipping silently.
- The existing deployment/approval principal can still deploy and the
  `STAGING_WORKER_URL` variable resolves in that environment.

## Staging schema-drift gate (automatic, plan-only)

Every `deploy-staging` run starts with the `soak-hold` job and then the
`schema-gate` job, and `deploy` needs the gates, so a staging database behind
the source fails the run before any deploy step instead of staying red
silently. The hold reads only the Operator-owned `STAGING_SOAK_HOLD_UNTIL`
repo variable. While armed, a run subject to the hold fails fast rather than
reporting success without deploying. Ordinary manual dispatches bypass this
hold; error-rate rollback rehearsal dispatches honor it. The schema gate job
runs only in the `staging-migrate-plan` environment with `contents: read`, builds the reviewed
`staging-migrate` binary at the exact deploy SHA, and runs
`scripts/ci/staging_schema_gate.py`, which invokes the binary's read-only
`--plan` through the plan environment's existing
`TWO_BOT_STAGING_PLAN_DATABASE_URL` binding (a login holding only
`two_bot_migrator_ro`). It never applies migrations, never reads the migrator
binding, and never creates or grants roles: non-empty `pending_before` exits
non-zero naming the pending versions and the manual `staging-migrate` apply
dispatch that fixes them, and the reviewed-apply fence is unchanged.

The expected staging identity is pinned independently of that secret: the
non-secret variables `TWO_BOT_STAGING_HOST`, `TWO_BOT_STAGING_DATABASE` and
`TWO_BOT_STAGING_BRANCH_ID`, scoped to the `staging-migrate-plan` environment
and Operator-owned. The gate (and then the binary's own `verify_target`)
refuses before any plan when the binding's host, database or PlanetScale
branch suffix does not match those pins, when a pin is empty, production-like
or a pooler endpoint, or when a PlanetScale pin carries no branch id; refusal
messages are fixed strings that echo neither the URL nor the pins. PlanetScale
hosts remain supported through the explicit branch pin — there is no blanket
host-suffix rejection. Until the three variables are set, the gate fails
closed on every deploy.

The plan manifest's `audit.connection_usage` (sessions summed from
`pg_stat_database.numbackends`, which the unprivileged plan login can read in
full, against `effective_max_connections`, or `max_connections` minus
`superuser_reserved_connections` when the effective field is absent) also
drives a warning, never a failure, when usage is strictly above 80% of that
non-superuser ceiling; older manifests without the effective/reserved fields
fall back to raw `max_connections` and label the ceiling as such.

## Staging rollback drill (manual)

`staging-rollback-drill.yml` offers an [on-demand automatic proof](production-deploy.md#on-demand-staging-proof)
and a [Worker-version rollback](runbook.md#worker-version-rollback). The proof dispatcher has no Cloudflare
or ownership credentials. A separate job alerts on failure with `GITHUB_TOKEN` (`issues: write`).

For the manual Worker-only option, pass a named, previously served
`target_version` (never "latest"). It is an optional string with empty default:
empty selects the automatic proof, not an inferred Worker target. The `drill`
job runs only on a manual dispatch from `main` with a non-empty target, in the
`staging` environment, and holds the `deploy-staging` job-level concurrency
lane without cancelling. Pinned checkout, pinned setup-node and the script run
are unconditional within that job; a fourth `always()` step uploads evidence.
The input travels through the environment to `scripts/staging_rollback_drill.py`.
The four existing staging bindings (`STAGING_WORKER_URL`,
`STAGING_OWNERSHIP_CONTROL_TOKEN`, `CLOUDFLARE_API_TOKEN`,
`CLOUDFLARE_ACCOUNT_ID`) are scoped to the script step alone. It creates,
rotates and exports no credential and has no production path.

The script fences the singleton, moves the Worker to the target with one
Cloudflare deployment request, hands ownership to it, times the first `/readyz`
200 against the 60 s budget, then restores the original version the same way.
The deployment request never sends `force`, so a target whose secrets changed
since it was deployed is refused; Wrangler's non-interactive rollback would
answer yes to that prompt. The request sets `code_update_strategy` to
`immediate` because Wrangler 4.152's default is `deferred` with a 300 s maximum
delay, which would keep the old Durable Object code serving for up to five
minutes. A 401/403 stops the run and skips the restore; it never retries with
another credential. Recovery then is a `deploy-staging` dispatch with
`release_fence=true` once the binding is fixed. Only allowlisted ids, integers,
statuses and timestamps reach the log, the job summary and the evidence file.

## Staging events read (manual)

`staging-events-read.yml` is a manual, staging-only, read-only workflow that
gives the B2 soak its processed-side evidence ([evidence-route.md](evidence-route.md)
step 2) without handing any agent a database credential. The regression pins it
as `workflow_dispatch`-only with exactly two required string inputs
(`window_start`, `window_end`; no member, guild or default). Its single `read`
job runs only from `main`, in the `staging-events-read` environment, on the
`ubuntu-latest` runner for job `read`, with `contents: read`, a 5-minute timeout and its
own non-cancelling concurrency group. It has exactly three unconditional steps:
pinned checkout without persisted credentials, one Run step that passes the
inputs through the environment (never an inline expression) to
`scripts/staging_events_read.py`, and the 14-day upload of
`staging-events-read-<run_id>.json`. Its three secrets are
`TWO_BOT_STAGING_EVENTS_RO_DATABASE_URL` (role `two_bot_events_ro`, column-limited
`SELECT` on `events`), `STAGING_EVENTS_READ_EXPECTED_HOST` (independently pinned
staging endpoint hostname), and `STAGING_FIXTURE_MEMBER_ID`, scoped to that Run
step. The host and member are secrets rather than variables because a step's
`env` block prints variable values on the public run page. No `set -x`, no
wrangler, no production path.

The script refuses before connecting unless the window is a past UTC window of
at most 15 minutes, the guild is the pinned TWO Staging guild (the live guild
refuses), the login names role `two_bot_events_ro` on database `two_bot`, and
the URL host exactly matches the independent staging-host pin. It forces
libpq `sslmode=verify-full` with the runner's system CA bundle, so the server
certificate must authenticate that host. It keeps the password out of the
process list, runs every statement in a `READ ONLY` transaction, never echoes
the client's error text, and writes only an ordinal, `event_type` and
`recorded_at` per row.

## Staging privacy verify read (manual)

`staging-privacy-verify-read.yml` is a manual, staging-only, read-only
workflow that gives the privacy fixture its purge-verification counts
([privacy.md](privacy.md) staging section) without handing any agent a
database credential. The regression pins it as `workflow_dispatch`-only
with exactly one required choice input (`guild`: `g` or `h`, no member,
window or default). Its single `read` job runs only from `main`, in the
`staging-privacy-verify-read` environment, on the `ubuntu-latest` runner for job
`read`, with `contents: read`, a 5-minute timeout and its own
non-cancelling concurrency group. It has exactly three unconditional steps:
pinned checkout without persisted credentials, one Run step that passes the
input through the environment (never an inline expression) to
`scripts/staging_privacy_verify_read.py`, and the 14-day upload of
`staging-privacy-verify-read-<run_id>.json`. Its four secrets are
`TWO_BOT_STAGING_PRIVACY_RO_DATABASE_URL` (role
`two_bot_privacy_verify_ro`, column-limited `SELECT` on exactly six
tables), `STAGING_PRIVACY_VERIFY_EXPECTED_HOST` (independently pinned
staging endpoint hostname), `STAGING_FIXTURE_MEMBER_ID`, and
`STAGING_PRIVACY_VERIFY_H_GUILD_ID` (second-guild pin, unset until
recorded and verified not-live), scoped to that Run step. The pins and
member are secrets rather than variables because a step's `env` block
prints variable values on the public run page. No `set -x`, no wrangler,
no production path.

The script refuses before connecting unless the guild alias is `g` or `h`
(`h` refuses until its pin is bound, well-formed, distinct from staging and
not the live guild), the login names role `two_bot_privacy_verify_ro` on
database `two_bot`, and the URL host exactly matches the independent
staging-host pin. It forces libpq `sslmode=verify-full` with the runner's
system CA bundle, so the server certificate must authenticate that host. It
keeps the password out of the process list, runs every statement in a
`READ ONLY` transaction, never echoes the client's error text, and writes
only the guild alias, all-time pair counts, sweep counts, receipt actors
and timestamps. The 15-minute events-smoke window does not apply; counts
above one million fail closed.

## Staging retention census (manual)

`staging-retention-census.yml` is a manual, staging-only, read-only workflow
that reports the event-store retention-expiry evidence (migration ledger and
expiry-index presence, eligible-old vs fresh counts by retention family, and
a bounded stable-cohort digest before and after one natural ~900 s DryRun
interval) without handing any agent a database credential. The regression
pins it as `workflow_dispatch`-only with no inputs. Its single `census` job
runs only from `main`, in the existing no-reviewer `staging-migrate-plan`
environment, on the `ubuntu-latest` runner for job `census`, with `contents: read`, a
30-minute timeout covering the script's single in-job sleep, and its own
non-cancelling concurrency group. It has exactly two unconditional steps:
pinned checkout without persisted credentials, and one bash Run step that
calls `scripts/staging_retention_census.py` with the plan secret plus the
three non-secret staging pins via the environment only. There is no artifact
upload: counts, digests and timestamps never leave the run log and summary.
No `set -x`, no wrangler, no apply route, no production path.

The script refuses before connecting unless the login is a bare staging
login on the pinned staging database at the pinned staging host (pooler
endpoints and production-like names refuse; a PlanetScale host requires a
matching branch pin). The login is a separate identity holding only
`two_bot_migrator_ro` (that group is `NOLOGIN`), mirroring the plan binary:
every connection runs `SET ROLE two_bot_migrator_ro` before its `READ ONLY`
transaction, the server must then report that role and database, and a
login holding `two_bot_migrator` refuses. It forces libpq
`sslmode=verify-full` with the runner's system CA bundle, keeps the
password out of the process list, runs every statement in a `READ ONLY`
transaction with statement and lock timeouts, runs no DDL (no `CREATE
EXTENSION`: a missing pgcrypto yields count/min/max without a digest),
never echoes the client's error text, and never invokes retention or
activates Execute. The ledger scope is exactly the two expiry migrations
(444/445); both cohort captures reuse one server-anchored window, so the
bounds cannot move between them. An empty cohort or empty family is a
reported limitation, never a pass.

### Agent observation contract

The default text report and optional `--json` output project the same admitted
result. JSON is exactly one document (no progress prose): `schema_version: 1`,
`status`, `observations`, `limitations`, `error`. Each observation has a stable
`id`, `code`, typed `value` and `next_action`; limitations omit `value`. IDs are
`observed_before`, `observed_after`, `migrations`, `indexes`, `counts`,
`cohort_before`, `cohort_after`, optional `digest_match`, `mode` and `counters`.
Codes are `observed` or `unobserved`; mode/counters always have null values.
Limitation IDs/codes are `empty_cohort`, `pgcrypto_absent` and `empty_families`.
Empty cohorts and absent pgcrypto suppress `digest_match`; a mismatch is observed
data, not a read failure. None of these observations proves retention execution.

Database timestamps accept ISO dates with space/T, up to six fractional digits,
and explicit Z or signed HH, HHMM, HH:MM or HH:MM:SS zones (hours 00–15,
minutes/seconds 00–59). Calendar-invalid, infinite, unzoned or suffixed values
fail closed. Output is UTC `YYYY-MM-DD HH:MM:SS[.ffffff]+00`; bounds are compared
as instants and every cohort count must be 0–1000, in both digest variants.
Capture clocks retain millisecond RFC3339. SQL and the frozen window are unchanged.

`observed` exits 0, including limitations. `refused` exits 2; `read_failed` and
`publication_failed` exit 1. Failed JSON documents contain no observations and
an error with only `code`, boolean `retryable` and fixed `next_action`. Text-mode
failures prefix the structured line with the fixed refusal or read cause. Summary
write failures are protected too: no raw OS/exception text, traceback or success
marker. On success `GITHUB_STEP_SUMMARY` receives the same text projection even
in JSON mode; read/refusal failures do not append a partial census.

Verify offline, without database/network access or a real interval sleep:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/ci -p 'test_staging_retention_census.py' -v
```

## Staging fixture census (manual)

`staging-fixture-census.yml` is the point-in-time companion: it runs the
same script in `fixture-scope` mode over the seven fixture tables the
read-only role can `SELECT` (per-table totals plus fixture-marker
counts with the two pinned auxiliaries, and the anonymous per-table
guild inventory). The regression pins the same shape as the retention
census: `workflow_dispatch`-only with no inputs, one `fixture-census`
job from `main` only, the existing no-reviewer `staging-migrate-plan`
environment, the `ubuntu-latest` runner for job `fixture-census`,
`contents: read`, exactly two unconditional steps, log only with no
artifact, and no `set -x`, wrangler, apply, seed or purge route. The
only deliberate differences are its own non-cancelling concurrency
group, a 10-minute timeout (no in-job sleep), and the exact `fixture-scope`
command. Both fixture query groups share one psql session and one
`REPEATABLE READ, READ ONLY` transaction, so the evidence has a single
database snapshot despite concurrent staging writes. The report timestamp
records runner-clock capture completion. The identity, role, timeout and
output fences are the shared ones above; the marker prefix is fixed in
the queries, so no per-run value ever crosses the dispatch boundary.
Do not dispatch until the security review for the fixture queries is
recorded. Procedure: `docs/staging-fixture-census.md`.

## Production route

`deploy-production.yml` (main #243) is the one live deployment workflow. The
regression pins it to `workflow_dispatch`, a completed `deploy-staging` run on
`main`, and the daily schedule backstop — never push or PR. Its `production`
job must need `guard`, `soak`, `recheck`, `schema-gate` and `roles-gate`, run in GitHub environment `production`
and carry exactly the pinned condition (not cancelled, guard success, no skip, a completed soak, a fresh recheck pass — or a
skipped recheck on manual runs only — and both gates green unless a
rollback or live-SHA redeploy waives them); any other condition fails the regression. The `recheck`
job must need `guard`, `soak`, `schema-gate` and `roles-gate` so its fresh hold/live/dispatch read happens after the last long build,
and must skip while an operator dispatch is active. Deploy and ownership status share the
bounded `deploy-production` queue. The guard, gates, soak and recheck
carry none, so a sleeping promotion can never delay a rollback behind it.
The `guard` job may not have an environment, call a reusable workflow, or contain
deploy/probe steps. Its script must still require a deploy-staging run with
`status="success"` on the SHA, plus required reviewers on the production
Environment. Negative fixtures cover extra triggers, removed `needs` or
environment, a bypass condition, deploy steps in the guard, and dropping those
checks.

A merge is not evidence of staging deployment or qualification. Consult
`deploy-staging.yml` at the relevant ref for its triggers and verify completed
qualifying evidence for the intended exact `main` SHA. A skipped, failed or
missing run does not qualify that SHA. If evidence is absent, keep promotion
held and follow the separately authorized [qualification/recovery procedure](staging-rollout-gate.md#full-window-workflow-and-recovery-semantics).
This does not authorize a production bypass. A pinned drill dispatch (a `sha`
below the workflow head)
deploys the pin but fails its run red in the `pinned-history-guard` job after
the synthetic, so it never mints a successful run the guard could mistake for
staging history of the head.

`supply-chain.yml` holds the required `pr-lint` and `gitleaks` jobs (main #187).
The permitted same-repo reusable calls are the `pipeline-benchmark` call in
`nightly.yml` and the read-only SBOM inventory calls (`supply-chain` in
`check.yml`, `release-sbom` in `release.yml`, both to `sbom.yml`, TOG-10893);
each caller grants `contents: read`. Under a default-deny top
level, a called workflow can only narrow the caller's grant.

Environment settings, secrets, branch protection, and deployments are not
modified by this code change. An inaccessible environment API response is not
proof of protection or proof of absence. Never create/substitute credentials to
resolve such a response.

## Source snowflakes

The first command includes the [offline workflow fixtures](#offline-enforcement).

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/ci -p 'test_*.py' -v
python3 scripts/ci/check-src-snowflakes.py
python3 scripts/ci/verify-codeowners.py
```

The snowflake gate scans `crates/*/src/**/*.rs` alongside `src/**/*.rs` for decimal literals/strings with
17–20 digits, including numeric separators and raw strings. Comments and exact
`#[cfg(test)]` inline modules are excluded. External test modules must start with
an explicit `#![cfg(test)]`; filenames alone do not exempt source. Integration-test
fixtures under `crates/*/tests` are not production source. A production item after a test module
is still scanned. This is a lexical guard, not a Rust constant-expression
interpreter: computed, encoded, and non-decimal values still need code review.

`scripts/ci/snowflakes-allowlist.json` pins **31 existing occurrences**, each by
path, value, and full source line with a reason. This is a compatibility baseline,
not a claim that existing source is ID-free:

- 24 frozen onboarding catalog/parity constants and destinations already shipped
  on main. Replacing these would change public constants and role/channel
  routing; runtime wiring receives per-guild configured IDs.
- Five backup preflight identity/guild pins, which reject incorrect bot identity
  and unsafe live-guild operations. Removing safety pins is not a lint fix.
- Two cutover constants: the live-guild refusal sentinel and the legacy staging
  soak fixture.

There are no file-wide or value-wide exceptions. A new use, duplicate, changed
value/line, or stale allowance fails the gate. Any baseline reduction or expansion
requires a justified reviewed change; never encode/move literals merely to hide
from the gate. Existing compatibility data should be removed only with a
separately verified runtime/config migration.

## Console macros and ownership

All six Cargo packages inherit workspace Clippy `print_stdout = "deny"` and
`dbg_macro = "deny"`. Only the nine cutover operator binary modules and the bot
backup/database-role CLI modules, the preflight report/help functions, and the
local metrics measurement example allow stdout for intentional human/JSON/SQL output. Debug
macros remain denied even in CLI code. Runtime/library code uses tracing.
Regression fixtures verify every package inherits the lint policy and the
stdout exception paths remain CLI-only. CI Clippy provides compilation-based
enforcement; those offline configuration fixtures are not a replacement for it.

CODEOWNERS verification requires a non-empty repository-wide `*` rule, validates
owner syntax, rejects unsupported patterns, and rejects later ownership-erasing
rules. It verifies offline coverage, not GitHub account/team write access or
branch protection. The existing advisory ownership policy is unchanged.

## Static audit exceptions

Both commit-parser consumers stage the reviewed manifest and lockfile from
`scripts/fixtures/commit-parser/` into their own `RUNNER_TEMP` directories and run
`npm ci --ignore-scripts --no-audit --no-fund`. The exact `release-please` version
remains 17.6.0; transitive registry tarballs carry integrity hashes. The fixture
has no lifecycle scripts or application dependencies, and the PR-lint sparse
checkout includes both fixture files. The existing fixture-only `adhoc-packages`
suppressions and read-only job grants are unchanged. This installation is not a
dependency audit or an exception for deployment package installation.

`deploy-production.yml` carries two inline suppressions for its automatic
promotion trigger. `dangerous-triggers` on the `workflow_run` notifier: the
trigger only wakes the run, and the guard re-resolves the SHA from the API
and refuses unless it is on `origin/main` with green `ci-ok`, `worker
check` and a successful `deploy-staging` run, inside the protected
`production` Environment. `github-env` on the deploy-config render step:
the only value written is a fixed `$RUNNER_TEMP` path, with no trigger,
input or API data reaching `GITHUB_ENV`.

The former `if: ${{ false }}` staging suspension (and its narrow
`obfuscation` suppression) is removed by this re-activation; the workflow
carries no such suppression. The offline regression now requires the deploy
job to carry no job condition at all.

Pedantic zizmor may report informational `superfluous-actions` advisories for
remaining pinned Rust toolchain Actions. Their Action SHAs and pin comments
are preserved until those callers migrate.

## Public-hosted runner contract

Ordinary jobs select `runs-on: ubuntu-latest` explicitly, including the local
reusable nextest and SBOM jobs. The runner policy fixtures pin the permitted
Boat and Blacksmith routes.

The `rust-tests` job (`rust tests`) can use Boat on the first attempt of a
same-repository PR targeting `main`, with the repository explicitly public.
Both the original and triggering actors must
match Boat's allowlist of `Rick7C2` and `togetherweown[bot]`. The prepaid
reserve switch selects `boat` while starts fit the allowance. The job accepts
only that label from `CI_HEAVY_RUNNER` and falls back to `ubuntu-latest` for
any other value, including an unset variable.

Fork PRs, Dependabot, off-list or missing actors, reruns, pushes, queue groups,
schedules, and manual dispatches use `ubuntu-latest`. GitHub retains the
original actor on a rerun and may report a different triggering actor.
Reruns use the hosted route even when both actors are allowlisted. The workflow
tests pin the job's expression and event set. External pinned reusable workflows
use their own runner policy. A plan limit can still delay or refuse a job that
has already been assigned to Boat before the reserve switch changes.

The `ignored-db-runtime` job (`ignored db (runtime)`) can use the 2 vCPU
Blacksmith runner through `CI_BLACKSMITH_2VCPU` for same-repository PRs and
existing non-PR events. It uses `ubuntu-latest` for forks or whenever the
variable is unset, set to Ubuntu by the cutoff, or set to an unapproved size.
The cutoff estimates cycle usage and reserves headroom before routing new jobs.
GitHub merge-group events lack the source repositories of all member PRs.
Verify queue trust before enabling the merge queue with Blacksmith routing.

The other directly hosted `ci-ok` gates and their dependencies in `check.yml`,
`pr-lint` and `gitleaks` in `supply-chain.yml`, and the required size gate in
`pr-size.yml` use the same validated 2 vCPU variable on same-repository
`pull_request` events. The size gate also uses it on same-repository
`pull_request_review` events. These routes use `ubuntu-latest` for forks,
missing or unapproved labels, and events outside those PR triggers.

This contract is for a **public repository**. GitHub's
[standard runner specification](https://docs.github.com/en/actions/reference/runners/github-hosted-runners#standard-github-hosted-runners-for-public-repositories)
lists public `ubuntu-latest` x64 as 4 CPUs, 16 GB RAM and 14 GB SSD, with free
standard-runner usage. This is not a larger-runner selection or permission to
buy overflow. Verify repository visibility before execution and merge. A future
private conversion requires fresh runner, network and cost qualification
**before** running workflows; these definitions would otherwise consume private
hosted minutes. The offline fixtures cannot observe repository visibility.

### Compatibility and limits

[Boat CI](https://docs.boat.dev/ci.md#github-actions-runners-runs-on-boat) starts
a fresh Linux VM for each job and deletes it when the job ends. It supports job
and service containers, cache and artifact actions. The `boat` label selects
4 vCPU, 8 GB RAM and 4 GB swap. The `rust-tests` job keeps its existing pinned
images, service limits, disk guard and 45-minute timeout. `ci-ok` waits for the
job and requires success whenever the Rust selector selects it. Only GitHub-hosted
pushes to `main` can save the shared Rust cache, which the hosted nextest lane
restores on runs with R2 credentials. GitHub scopes caches written by PR jobs
to the PR merge ref, outside the main cache scope.

[Blacksmith's 2 vCPU Ubuntu runner](https://docs.blacksmith.sh/blacksmith-runners/overview#x64-runners)
provides an ephemeral VM with 8 GB of RAM and 80 GB of disk. The runtime database
job keeps its 60-minute timeout, pinned disposable PostgreSQL service and
job-private Docker network. `ci-ok` requires it whenever Rust is selected.

GitHub's [service-container contract](https://docs.github.com/en/actions/concepts/use-cases/about-service-containers)
supports Linux job/service containers on hosted Ubuntu. Container jobs reach
services through job-private Docker networks and service DNS; they do not need
published host ports. The existing disposable Postgres services keep that
isolation, their memory/tmpfs/health limits and guarded test URLs. Root inside
job containers remains available for package installation and loopback test
forwarding. Host-level Docker/BuildKit jobs retain `linux/amd64`; Worker jobs
install Node explicitly. No persistent host cache or private runner mount is
required by these jobs.

Hosted disk is finite and CPU timing is variable: the existing disk-floor
check, build/job timeouts, benchmark thresholds and concurrency limits remain
in force. Cached builds are optimizations, not a requirement for a particular
machine. A disk, memory or duration failure is an unresolved qualification,
not permission to omit tests, relax a threshold or select a paid runner.

### Existing execution evidence

Read back on 2026-11-04: the jobs below completed on `ubuntu-latest` with runner
group `GitHub Actions`. These are **historical compatibility evidence**, not
runs of this change, proof of future network availability, or release approval.
A workflow-level success alone is insufficient: skipped jobs do not qualify.

| Job family | Existing successful hosted jobs |
| --- | --- |
| Rust, nextest, coverage, disposable Postgres, Worker Node, Docker/BuildKit and SBOM | [check run](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/41765306074) includes successful selected lanes, coverage shards, container smoke, image scan and SBOM verification |
| Nightly and benchmark | [nightly run](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/41759826693) includes guarded suites, partitions, docs and benchmark; [standalone benchmark](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/41470381075) also succeeded |
| Bounded fuzz smoke | [vote-kick](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/38454021749) and [Helix token](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/38968322463) smoke jobs succeeded |
| Staging deploy and synthetics | [staging run](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/41700522944) includes schema/role gates, deploy, user-flow/help/voice synthetics and immutable-artifact certification |
| Production promotion | [production run](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/41699655458) includes successful schema/role gates, soak/evidence/recheck and deploy/readyz, not just a skip receipt |
| Migration plan, claim and apply | [staging migration](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/41375889399) and [production migration](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/41493082701) each ran all three jobs successfully |
| Manual read-only operation | [staging ownership status](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/39173020313) executed successfully |

These executions demonstrate hosted access to the configured endpoints and
credentials used by those jobs at those times. They do not establish every
manual drill/recovery/census branch, every credential's present validity, or
access to an optional deployment-notification webhook. Standard hosted IPs can
change; an endpoint requiring a fixed IP or private network needs an explicit
technical prerequisite, not a silent self-hosted/paid fallback. No live
production dispatch or credentialed probe is part of runner qualification here.
The split nightly legs retain the same container, six-partition inventory,
fail-closed receipt aggregate and event-specific budgets. Bounded staging
startup/cleanup jobs retain their exact-SHA readiness, finite-use grants,
native expiry and failure/cancellation cleanup. Their Node/HTTPS runner needs
are unchanged, but the historical runs above do not qualify those newer paths.
Current-head CI and independent reviews remain required; production merge and
any subsequent production exercise retain their separate release approval.

### Offline enforcement

`scripts/test-runner-routing.py` checks every tracked workflow definition,
rejecting dormant routing, unapproved selectors, nonstandard runner labels,
and misplaced runner keys. It also pins the size gate's reusable runner input.
The parsed
workflow suite, supply-chain, production deploy, channel CI and staging
ownership fixtures also pin the direct hosted contract while retaining their
other isolation and behavior assertions.

Use Node.js 22 on `PATH` for `test_workflows.py`, matching the hosted check.
The offline manual rollback drill fixture loads the Node helpers from the
sparse file set.

```sh
python3 scripts/test-runner-routing.py
python3 -m unittest discover -s scripts/ci -p 'test_workflows.py' -v
python3 scripts/test-supply-chain.py
python3 scripts/test-deploy-production.py
python3 -m unittest discover -s scripts -p 'test_channel_ci.py' -v
python3 -m unittest discover -s scripts -p 'test_staging_ownership_status.py' -v
```

## Native Rust setup

Rust setup in `check.yml`, `nightly.yml`, `pipeline-benchmark.yml`,
`reusable-rust-nextest.yml`, `help-embed-synthetic.yml` and the two fuzz smoke
workflows executes `.github/scripts/install-rust-toolchain.sh` with an exact
`RUST_TOOLCHAIN` and explicit `RUST_COMPONENTS`. Other workflows retain their
pinned Rust setup action. Stable lanes match `rust-toolchain.toml`;
the libFuzzer lanes retain `nightly-2026-10-01` and `rust-src`. Native setup
installs the minimal profile without self-update, selects the same default, and
verifies the explicit toolchain. Existing fail-closed retry steps and their
conditions remain. No migrated lane requested extra targets or a directory
override, or consumed the former action's outputs. The Cargo incremental/color
defaults are preserved.

Hosted Ubuntu and the pinned Rust job containers must supply `rustup`, `cargo`
and `rustc` on PATH. Missing tools fail explicitly; setup never runs a downloaded
bootstrap or silently uses the image's moving default. Offline shim tests prove
argument boundaries and failures, not hosted tool availability or compilation.
