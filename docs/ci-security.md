# CI security gates

The existing `check`, `pr-lint`, and `gitleaks` names remain unchanged. The
`check` job also runs the offline source-gate fixtures, source snowflake gate,
CODEOWNERS verification, and **zizmor 1.30.1**. zizmor is installed from its pinned
binary wheel without dependencies and runs with `--offline --strict-collection`;
it needs no GitHub token. All workflows default to `permissions: {}` and declare
job-specific grants. Every checkout sets `persist-credentials: false`.

The release job retains contents/PR write to publish and reconcile releases;
only the dispatch job gets actions write. The existing release-branch push uses
an ephemeral Git credential helper for that one command, not a persisted token.
Untrusted PR metadata remains in environment variables, never interpolated into
shell source. The deploy job does not restore caches from PR checks.

## Staging deployment (active)

AUTOMATED staging deployment is ACTIVE (TOG-12856, unblocking the TOG-12852
rehearsal): the `deploy` job in `deploy-staging.yml` runs unconditionally on
every push to `main` that changes a runtime input, and on manual
`workflow_dispatch`, in the GitHub `staging` environment, with the single
fenced `release_fence` dispatch input (opt-in boolean, defaults to false). The
push trigger carries a workflow-level `paths-ignore` deny-list of non-runtime
paths (`docs/**`, root `*.md`, license, editor/ignore/gitleaks config, issue and
PR templates, CODEOWNERS, dependabot config). GitHub skips the run only when
every changed path matches, so a mixed push, an unclassified path or any
workflow, Cargo, `crates/**` or `wrangler/**` change still deploys, and a
skipped push creates **no run at all** (not a skipped job). There is no
job-level `if:` — a condition there (including the old `if: ${{ false }}`
suspension, removed here) would silently skip deploys on some SHAs and leave
the production guard refusing those SHAs with no deploy ever running. The
deny-list is the one reviewed exception: it skips only SHAs whose diff cannot
change the Worker or container, and `scripts/ci/test_workflows.py` pins it
against sample runtime paths. A skipped job is **not** deployment, readiness,
protected-environment or E2E success; only a `success` conclusion with the
intended-rollout evidence counts. Existing running staging resources are
unchanged; no alternate manual deploy route is authorized.

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
routed runner expression for job `deploy`, and the fenced `release_fence`
dispatch shape, rejects statically-disabled steps that would report success
without deploying, and rejects new jobs/workflows or known CI jobs repurposed
as unapproved deploy/probe alternatives. Negative fixtures cover job-condition
insertion/mutation, step-level disables, dispatch-shape widening and alternate
deployment jobs. It also preserves visibility-aware runner routing,
job-container service isolation, per-job grants and nonpersistent checkouts.

The active job retains GitHub environment `staging`. **A YAML environment
name alone does not create protection rules or scope repository-level secrets.**
The environment scoping stays in place, and secret placement must remain
environment-scoped with no unintended fallback:

- The `staging` environment keeps its approved deployment approval and branch
  restrictions (only approved staging code, normally `main`).
- `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID` are environment-scoped, with no
  unintended repository/organization fallback exposing them to other jobs.
- The existing deployment/approval principal can still deploy and the
  `STAGING_WORKER_URL` variable resolves in that environment.

## Staging schema-drift gate (automatic, plan-only)

Every `deploy-staging` run starts with the `soak-hold` job and then the
`schema-gate` job, and `deploy` needs the gates, so a staging database behind
the source fails the run before any deploy step instead of staying red
silently. The hold reads only the Operator-owned `STAGING_SOAK_HOLD_UNTIL`
repo variable and fails the run while a drill hold is armed, so a deferred
push never reports a success the production guard could mistake for staging
history. The gate job runs only in the
`staging-migrate-plan` environment with `contents: read`, builds the reviewed
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

`staging-rollback-drill.yml` has a daily automatic recovery proof and a manual
[Worker-version rollback](runbook.md#worker-version-rollback) option. The daily
proof is described in [production-deploy.md](production-deploy.md#daily-staging-proof):
only the fixed staging dispatch gets `actions: write`; the dispatcher has no
Cloudflare or ownership credentials. Failed proofs alert through the existing
`synthetic-alerts` environment. An independent daily read-only watcher catches
missing/stale scheduled proof; healthy checks post nothing.

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
`immediate` because Wrangler 4.151's default is `deferred` with a 300 s maximum
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
routed runner for job `read`, with `contents: read`, a 5-minute timeout and its
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
`staging-privacy-verify-read` environment, on the routed runner for job
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
environment, on the routed runner for job `census`, with `contents: read`, a
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
environment, the routed runner for job `fixture-census`,
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
and must skip while an operator dispatch is active (the lane's single pending slot would otherwise let the
automatic arrival cancel a queued recovery run). Only
`production` and `release` hold the `deploy-production` lane (a job-level
concurrency group, never workflow-level); the guard, gates, soak and recheck
carry none, so a sleeping promotion can never delay a rollback behind it.
The `guard` job may not have an environment, call a reusable workflow, or contain
deploy/probe steps. Its script must still require a deploy-staging run with
`status="success"` on the SHA, plus required reviewers on the production
Environment. Negative fixtures cover extra triggers, removed `needs` or
environment, a bypass condition, deploy steps in the guard, and dropping those
checks.

With staging active (TOG-12856), every `main` push that changes a runtime input
runs deploy-staging on its SHA; the guard accepts only a run that concluded
`success` on that SHA, so a `skipped` or failed run still refuses production
for that SHA. A docs-only push (the `paths-ignore` list above) starts no run, so
the guard also refuses a docs-only head with `deploy-staging has no successful
run`. This fails closed and is the intended trade: pin the latest
runtime-affecting commit, or dispatch `deploy-staging` for the head you need
([production-deploy.md](production-deploy.md)). This change does not authorize
a production bypass. A pinned drill dispatch (a `sha` below the workflow head)
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

The `release-please@17.6.0` commit-parser fixture install (commit-parse guard)
has one inline `adhoc-packages` suppression. The direct package is pinned; lifecycle scripts are disabled and its check job has
no write grant. Transitive dependencies remain unlocked. A future fixture
lockfile can remove this exception; it is not an exception for deployment
package installation.

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
existing pinned Rust toolchain Actions. They are intentionally retained; all
existing Action SHAs and pin comments are preserved.
