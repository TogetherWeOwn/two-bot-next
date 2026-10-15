# CI speed rollout (SDLC 2.3): durable by default

One definition, inherited everywhere. Anything on this page used by more
than one repo lives ONCE at org level; per-repo files only call it.

## Promotion (2026-10-15): nextest is required

The advisory pilot graduated. `check.yml` calls the shared lane as the
`nextest` job (rust-selector gated, aggregated by `ci-ok`, 45-minute
ceiling pinned in `scripts/ci/test_workflows.py`); `ci-speed.yml` keeps
the affected-paths selection and the minutes-per-merged-PR report.
No new required-check context was created: `ci-ok` stays the single
aggregate, so no branch-protection change was needed.

Red-group ejection without a revert: the reusable lane takes a
`quarantine` input (whitespace-separated nextest filters, empty by
default). Each filter becomes one `--skip` after the `--` separator
(pinned cargo-nextest 0.9.99 only accepts `--skip` there), so the rest
of the suite still gates while the red group is out. Drilled on a
throwaway draft PR (synthetic failing test went red, then green under
quarantine, PR closed unmerged) before the promotion merged; the first
quarantine run caught a `--skip`-before-`--` usage error, which the fix
and the re-run receipts on the promotion PR close out.

## What shipped in the pilot (advisory, this repo)

- `dorny/paths-filter` in `.github/workflows/ci-speed.yml` (`changes` job):
  affected-only pre-filter for the advisory lane. Required lanes keep the
  existing `job-inputs` selector; the required `ci-ok` aggregator is
  untouched, so no required context can strand.
- `Swatinem/rust-cache` (restore-only from the warm `check` entry) plus
  `sccache` in `.github/workflows/reusable-rust-nextest.yml`: push-to-main
  and workflow_dispatch runs use the R2 backend when
  `SCCACHE_BUCKET`/`SCCACHE_ENDPOINT` secrets exist and degrade to the
  GitHub-cache backend otherwise. Every other event (pull_request,
  merge_group, ...) uses sccache on local disk only: no R2 write credential
  ever reaches code that is not on `main` yet (the lane compiles and
  executes code at an unmerged head, and a shared R2 bucket has no
  per-PR scope), and the lane writes no shared Actions-cache entry that
  could crowd out the warm `check` entry. No new paid SaaS, no new Actions
  minutes beyond the advisory job itself.
- `cargo-nextest --profile ci` (`.config/nextest.toml`: `retries = 1`,
  `fail-fast = false` so every test runs and ledger rows stay comparable,
  JUnit to `junit.xml`) with `scripts/ci/flake_ledger.py` parsing JUnit
  into a JSONL flake ledger, uploaded as an artifact with the JUnit.
- `scripts/ci/ci_minutes.py` + `docs/ci-speed-baseline.json`: the
  before/after mechanism. The advisory `report` job uploads a comparison
  skeleton per run; maintainers append measured rows post-merge with
  `record`. No savings claim until ledger means exist on both sides.

## Org-level home (target, not this repo)

- Reusable workflows live in `TogetherWeOwn/.github`
  (`.github/workflows/rust-nextest.yml`, `.github/workflows/ci-speed.yml`),
  called via `uses:` with a pinned SHA, mirroring the `test-guard`
  precedent (`TogetherWeOwn/.github/.github/workflows/test-guard.yml`).
  The files in this repo are the same content with a local `uses:` path so
  the pilot runs before the central move; the move is a one-line `uses:`
  retarget plus the SHA pin.
- GitHub **org rulesets** target all repositories with explicit exclusions:
  merge queue, required checks (`ci-ok`, `gitleaks`, `pr-lint` plus the
  shared gates), PR rules. No per-repo rulesets.
- Org **required workflows** via rulesets run the shared checks everywhere
  they apply (size label, workflow lint, test-missing, security scan).
- The **repo template** plus Paperclip project bootstrap give a new repo the
  shared caller workflows, CODEOWNERS/policy-bot config and the
  staging-migrate contract on day one.
- Host scripts discover repos dynamically with one exclude list; no
  hardcoded repo names.

## Per-repo exceptions (genuinely repo-specific only)

- The bot deploy singleton (`deploy-staging`/`deploy-production` and the
  staging-migrate environments) stays per-repo.
- DB-backed lane selection (`job-inputs` outputs) stays per-repo until the
  central selector learns every repo's layout.

## Evidence and backtest

- Deterministic first: `flake_ledger.py` and `ci_minutes.py` are pure
  file I/O with offline fixtures (`test_flake_ledger.py`,
  `test_ci_minutes.py`) that run in the `check` lane's `scripts/ci`
  discover step. Identical input bytes give identical output bytes.
- The lane graduated from advisory to required (see Promotion above):
  it gates merge through `ci-ok` and follows the routed-runner,
  minimal-grant and inventory pins in `scripts/ci/test_workflows.py`.
- The phase0/blocker ledger was not present in this repo, so there was no
  local ledger to backtest against; the parser fixtures above are the
  backtest, and the first advisory runs attach real JUnit evidence.
