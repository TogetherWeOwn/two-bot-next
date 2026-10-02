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

## Staging suspension and activation prerequisite

AUTOMATED staging deployment is suspended by the unconditional job-level
`jobs.deploy.if: ${{ false }}` in `deploy-staging.yml`, for both push/main and
workflow_dispatch. This blocks the whole runner/checkout/install/deploy/probe
path; no input, variable, actor or secret can enable it. A skipped green job is
**not** deployment, readiness, protected-environment or E2E success. Existing
running staging resources are unchanged; no alternate manual route is authorized.

CISO accepted this disabled CI-only scope in TOG-10958 and TOG-11179 plan revision
2. The frozen 31-occurrence baseline and independent exact-head green-CI review
and non-author squash-merge gates remain. Actual staging provisioning and later
activation are separate on TOG-11271; this change does not satisfy that receipt.

The offline workflow regression uses pinned PyYAML 6.0.3, rejects duplicate
keys, inventories every workflow/job, requires all staging jobs to have the exact
static-false guard, and rejects new jobs/workflows or known CI jobs repurposed as
deploy/probe alternatives. Negative fixtures cover guard deletion/mutation,
step-only guards, mutable activation flags and alternate deployment jobs. It
also preserves private self-hosted routing, job-container service isolation,
per-job grants and nonpersistent checkouts.

The suspended job retains GitHub environment `staging`. **A YAML environment
name alone does not create protection rules or scope repository-level secrets.**
Before separately approved activation, an authorized provisioning principal must
verify and retain evidence for:

- An existing protected `staging` environment with approved deployment approval
  and branch restrictions (only approved staging code, normally `main`).
- `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID` are environment-scoped, with no
  unintended repository/organization fallback exposing them to other jobs.
- The existing deployment/approval principal can still deploy and the
  `STAGING_WORKER_URL` variable resolves in that environment.

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

While staging is statically disabled, every deploy-staging run on a new SHA
concludes `skipped`, so the guard refuses production for that SHA. This fails
closed. Production for a newer SHA therefore waits on approved staging
activation (TOG-11271); this change does not authorize a bypass.

`supply-chain.yml` holds the required `pr-lint` and `gitleaks` jobs (main #187).
The `pipeline-benchmark` reusable call in `nightly.yml` is the only permitted
`uses:` job, and its caller grants `contents: read`. Under a default-deny top
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

The existing `release-please@17.6.0` API-double fixture install has one inline
`adhoc-packages` suppression. The direct package is pinned to the release
Action's bundled library; lifecycle scripts are disabled and its check job has
no write grant. Transitive dependencies remain unlocked. A future fixture
lockfile can remove this exception; it is not an exception for deployment
package installation.

The static-false staging condition has a narrow `obfuscation` suppression:
its explicit expression is the reviewed suspension invariant, not untrusted
input or a mutable activation flag. The offline regression requires it exactly.

Pedantic zizmor may report informational `superfluous-actions` advisories for
existing pinned Rust toolchain Actions. They are intentionally retained; all
existing Action SHAs and pin comments are preserved.
