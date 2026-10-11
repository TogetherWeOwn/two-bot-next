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

## Staging rollback drill (manual)

`staging-rollback-drill.yml` is a manual, staging-only workflow that runs the
reviewed [Worker-version rollback](runbook.md#worker-version-rollback) drill
without giving any agent the staging ownership token. The regression pins it as
`workflow_dispatch`-only with one required `target_version` input (no default,
never "latest"). Its single `drill` job runs only from `main`, in the `staging`
environment, on the routed runner for job `drill`, and shares the
`deploy-staging` concurrency group without cancelling, so a push deploy cannot
interleave with a drill. It has exactly three unconditional steps: pinned
checkout, pinned setup-node, and one Run step that passes the input through the
environment to `scripts/staging_rollback_drill.py`. The four existing `staging`
bindings (`STAGING_WORKER_URL`, `STAGING_OWNERSHIP_CONTROL_TOKEN`,
`CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_ACCOUNT_ID`) are scoped to that step alone.
It creates, rotates and exports no credential and has no production path.

The script fences the singleton, moves the Worker to the target with one
Cloudflare deployment request, hands ownership to it, times the first `/readyz`
200 against the 60 s budget, then restores the original version the same way.
The deployment request never sends `force`, so a target whose secrets changed
since it was deployed is refused; Wrangler's non-interactive rollback would
answer yes to that prompt. The request sets `code_update_strategy` to
`immediate` because Wrangler 4.147's default is `deferred` with a 300 s maximum
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

## Production route

`deploy-production.yml` (main #243) is the one live deployment workflow. The
regression pins it as `workflow_dispatch`-only. Its `production` job must need
`guard`, run in GitHub environment `production` and carry no job condition. The
`guard` job may not have an environment, call a reusable workflow, or contain
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
a production bypass.

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

The snowflake gate scans `crates/*/src/**/*.rs` for decimal literals/strings with
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

The former `if: ${{ false }}` staging suspension (and its narrow
`obfuscation` suppression) is removed by this re-activation; the workflow
carries no such suppression. The offline regression now requires the deploy
job to carry no job condition at all.

Pedantic zizmor may report informational `superfluous-actions` advisories for
existing pinned Rust toolchain Actions. They are intentionally retained; all
existing Action SHAs and pin comments are preserved.
