# Contributing

## Pull request contract

The root [`AGENTS.md`](AGENTS.md) carries the same contract for human and AI
contributors. Every change lands through a PR; nobody pushes to `main`.

- **Branch.** Name it `type/short-slug`, for example `fix/gateway-resume`.
- **One logical change per PR.** Keep it small enough to review in one sitting.
- **Squash-merge only.** The `protect-main` ruleset allows no other merge
  method, and the squash commit takes the PR title and body, so `main` holds one
  conventional commit per PR. Do not pass a custom commit title when you merge.
- **Title.** A Conventional Commits header: `type(scope): summary`, at most 100
  characters, no trailing period. Types: `feat`, `fix`, `perf`, `refactor`,
  `test`, `docs`, `build`, `ci`, `chore`, `revert`, `style`, `security`. The
  scope names the area of the code (`gateway`, `automod`, `store`). The release
  automation below reads these headers.
- **Body.** Use the [PR template](.github/pull_request_template.md), in short,
  active sentences. `pr-lint` requires two sections: Linked Issues or Issue
  Description, and Verification (the commands you ran and their results).
  What Changed and Notes for reviewers are optional. A PR created through the
  API gets no template, so paste it in. `docs`, `chore`, `build`, `ci`, `style`,
  `test` and `revert` PRs need no linked issue. The `pr-lint` check fails an
  empty body and, on a public repo, a tracker id, instance UI link or the
  internal host in the title or body; the other body rules warn until open
  PRs migrate.
- **Search first.** Look for an open or recent PR that touches the same area,
  and link what you find.
- **No internal references.** This repo is public: put no internal ticket id,
  instance link, localhost URL or private host in any title, body, commit,
  comment or branch name. Link public GitHub issues as `Closes #123` and say
  the rest in plain words. `pr-lint` fails the title or body when it holds a
  tracker id, an instance UI link or the internal host, and follows
  `PR_STANDARDS_MODE` for the other references and branch names.
- **Honest disclosure.** Report only test runs you saw, and say what you did not run.
- **Review.** Address every review finding, or reply with why it does not apply.
  Re-request review after you push.
- **Credit.** Credit the contributors whose work you build on.
- **Done means merged.** Do not leave an orphan PR open: merge it, or close it
  with a comment that names what replaced it.

### Required checks

The rulesets on `main` require one PR and squash-only merges, and block deletion
and force-pushes. `ci-ok`, `gitleaks` and `pr-lint` are required checks on `main`.
`ci-ok` runs from `check.yml`; the other two run from `supply-chain.yml`.
`Devin Review` is required too. CodeRabbit is advisory: answer its findings when
it reviews, but never wait for it or trigger it with commands. No approving
GitHub review is required; native task review stages still apply.
`pr-lint` runs `.github/scripts/pr_standards.py` (tests beside it): it checks
the title format, the 100-character limit, the trailing period and a non-empty
body as errors, and the two required template sections (Linked Issues and
Verification), the advisory title/body remainder and branch names in warn mode
(`PR_STANDARDS_MODE: "warn"`). The strict title/body subset (tracker id,
instance UI link, internal host) always fails. On a push to `main` it
also checks every commit subject. All required checks must be green on the
exact head commit. Never bypass or weaken a check.

### Complete PR inventories

`pr-lint` requires the paginated PR files inventory to contain exactly the PR's
reported `changed_files` entry count before expanding old and new rename paths.
Empty, truncated, malformed or stale inventories fail closed. GitHub caps this
API at 3000 files; a legitimately larger same-repository PR uses the existing
manual validation route instead:

```sh
gh workflow run supply-chain.yml --ref <pr-head-branch> -f pr_number=<number>
```

The dispatch must target the exact current head branch/SHA of an open PR in this
repository targeting `main`. It checks out complete history and derives every
changed path from a merge-base diff of the immutable current `main`/PR head SHAs,
with rename detection disabled so both sides of a move reach the gate. Dispatch
resolves `main` directly: the PR API can retain an older base SHA. Submodule
ignore settings cannot hide gitlink changes. Missing history or an empty/failed
diff fails closed. The gate itself comes from the trusted PR base (or `main`
for dispatch), and its checked-out commit must match that same inventory base
SHA before any gate code runs. If `main` advances during collection, rerun
validation rather than using a different policy snapshot. All title/body,
T0 attestation, secret scanning, review and
merge requirements still apply; dispatch is validation, not a skip or waiver.
Fork PRs cannot use this same-repository dispatch route; reduce an oversized
fork PR into independently reviewable PRs instead. Rerun validation after any
head/base change; a stale check does not validate a new head.

### Production merge evidence

The final-stage reviewer squash-merges the settled, exact reviewed head in the
same run as final approval, after all required checks pass and every review
finding is fixed or answered with evidence. The author merges only when there is
no review stage. Pin the merge to that head SHA; record the squash SHA and the
release decision in the release log.

`main` reaches production through staging and automatic promotion. A path ignored
by `deploy-staging.yml` does not exempt a merge from production safeguards: even a
docs-only PR needs a fresh, per-PR Release Manager `request_confirmation` on its
exact head before merge. Put the confirmation on a child task assigned and
addressed to the Release Manager (`resolverPolicy: not_creator`). A manual
production dispatch or rollback needs its own Release Manager confirmation too.

For two-web-next and two-bot-next, staging qualification is the automatic
**post-merge, pre-production exact-main staging/e2e/soak gate** (owner decision
2026-11-06). Staging accepts only main-ancestor SHAs; an unmerged PR-head staging
run is not a merge prerequisite. Production promotes only the exact main SHA
that passes staging deploy, e2e and soak, enforced by the promotion guard. See the
[production deploy gate](docs/production-deploy.md) and
[staging rollout gate](docs/staging-rollout-gate.md) for the pipeline evidence.
This describes the required gate, not a claim that any SHA has qualified.
Automatic qualification starts on runtime-input pushes to `main`;
`deploy-staging.yml` ignores docs-only merges. To promote an otherwise unqualified
exact main SHA, including a docs-only squash SHA, first obtain authorized staging
dispatch/qualification using the existing
[dispatch/recovery procedure](docs/staging-rollout-gate.md#full-window-workflow-and-recovery-semantics).
Do not wait for a docs-only push to trigger it, skip qualification or dispatch
through an existing hold.

This contract supersedes the earlier artifact-identical staging exception.
Unmerged-head staging and paired artifact-equality proofs are not substitute
merge prerequisites. Do not demand either to satisfy the staging item before
merge, or treat artifact equality as permission to skip post-merge qualification.
The pipeline's exact-SHA artifact and promotion evidence remains required.

Before merge, retain required GitHub Actions checks and Devin Review on the
exact PR head, all applicable specialist and native review, and evidence-backed
finding dispositions. Include the migration ledger entry (or `no migration`)
and rollback target in the fresh per-PR exact-head Release Manager decision.
Keep incident, freeze and hold safeguards, identity/capability holds and red
staging-drill or other deterministic gates; this staging contract lifts none of
them. A previous rejection remains a rejection, not retroactive approval. Submit
a fresh confirmation only after its remaining conditions are met. Any new PR
head requires fresh review/check evidence and Release Manager acceptance before
the SHA-pinned squash merge and release-log recording. This documentation
authorizes no pipeline skip/no-op implementation, hold lift, deployment or other
production action.

### CI lanes

`check.yml` runs its Rust work as parallel lanes that all start once `job-inputs`
finishes: `check` (lint: offline guards, cargo-deny, fmt, clippy), `rust tests`
(unit, integration and doc tests), the sharded `patch coverage` measurement
(`coverage-plan` selects the legs, the `coverage-shard` matrix runs them,
`patch-coverage` merges and gates, so the unit-test signal and the coverage
verdict fail independently),
`rust nextest` (workspace lib/bins under
cargo-nextest with one retry, JUnit and a flake ledger) and two database lanes,
`ignored db (stores)` and `ignored db (runtime)`. The single `ci-ok` aggregate waits on both selectors
(`job-inputs` and `container-inputs`), every selected lane, `worker check`, the
SBOM dry-run and standalone database jobs. Both selectors and lint must always
succeed; a heavy lane may skip only when selection explicitly deselects it.
A job added to `check.yml` must go into `ci-ok`'s `needs:`, result env and result
evaluation, or carry a reviewed `# ci-ok: exempt <reason>` marker with a pin
in `scripts/ci/test_required_checks.py`. Only advisory `container smoke` is
exempt: it can take up to 30 minutes, and image security stays gated through
the SBOM job. The legacy `required-checks` aggregate is removed.

Deterministic quality gates (per-PR advisories HARD, ast-grep and Semgrep
HARD, required voice synthetic, test-target inventory HARD) run as steps of
the `check` lane on standard GitHub-hosted `ubuntu-latest`; see
[`docs/quality-gates.md`](docs/quality-gates.md). Patch coverage (HARD) runs
sharded across `coverage-plan`, the `coverage-shard` matrix (with per-shard
disposable Postgres services and ignored database-backed tests) and the
`patch-coverage` merge. New checks start in WARN (report, never fail) and flip
to HARD only with their backlog burned down.

Every Rust setup step in `check.yml` installs the exact version from
`rust-toolchain.toml`, including fmt/clippy components where required. Do not use
floating `stable`: Cargo still follows the repository pin, so differing versions
can cause component-install conflicts before fmt runs. The offline workflow
policy suite pins all ten installers to the repository version; bump them and
the toolchain and builder-image pins together.

### Nightly test partitions

`nightly.yml` retains its wiring-change PR selector and event timeout allow-list:
45 minutes per heavy PR job (also the fallback for any other event), 180 minutes
only for schedule/manual runs. Do not raise the PR cap to accommodate workload.
The original serial all-feature workload is split into:

- `sweep-plan`: generate the command reference, compile Cargo's default workspace
  test selection (`--no-run`, including ordinary examples), and discover each
  libtest target's original runnable inventory with the existing substring skips.
- `sweep-partition`: six deterministic partitions, at most two in parallel. Each
  re-discovers the same workspace/feature inventory and executes exact subsets
  within binaries, including the bot binary, with one test thread per disposable
  database. Duplicate names in different targets are separate identities. Unknown,
  missing, duplicated or empty expected execution fails rather than passing.
- `sweep-guarded`: the original guarded-URL routes and dedicated backup proofs,
  including the CLI build and scratch prerequisites, still executed serially.
- `sweep-docs`: workspace doctests with the plan's original include-ignored/skip
  semantics, followed by `cargo doc --workspace --all-features --no-deps --locked`
  under `RUSTDOCFLAGS=-D warnings`.

The `nightly ignored tests and docs` aggregate requires the selector, plan, all six
partitions, guarded suites/backups and docs. It compares exact-source-SHA receipts
against the complete/disjoint plan; a failed, cancelled, missing or skipped
selected job cannot pass. Matrix fail-fast is disabled. Plan and receipts are
seven-day run artifacts, not executable build downloads. Artifact names are stable
within one workflow run so failed-job reruns can reuse successful producers;
rerun producers replace their own artifact, and SHA/plan-digest checks still apply.
Native test processes
retain their package working directory; the partition helper does not use
nextest's per-test process isolation or llvm-cov's failure-tolerance flags.
Each invocation uses a fresh libtest `--logfile` for exact status/name evidence,
independent of inherited child stdout/stderr. Console diagnostic bytes still stream
unchanged, including non-UTF-8 output. Only the closing harness summary supplies
summary/count evidence; earlier summary-looking child lines are diagnostics.
The successful closing summary/counts and process exit remain mandatory.
Missing, empty, duplicate, failed, ignored or unknown result records fail closed.
The pinned Rust 1.99.1 supports this deprecated flag; recheck its protocol when
upgrading the toolchain rather than falling back to parsing mixed console output.

Verify a change with an actual executing `pull_request` run on its exact head,
record each heavy job's time and headroom, and aim for at most 35 minutes per
leg. Test counts are not a runtime guarantee. Rebalance only from measured
results; a skipped job or a 180-minute manual pass is not PR-cap evidence.
Offline contracts run in the existing source-level CI suite:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/ci -p 'test_nightly_partition*.py' -v
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_nightly_routing.py' -v
```

### Pull-request size labels and required size gate

`.github/workflows/pr-size.yml` labels every pull request `size/S` (up to 100
counted lines), `size/M` (up to 400), `size/L` (up to 1000) or `size/XL`, and
keeps one advisory comment on pull requests over the 400-line threshold.
Counted lines are additions + deletions excluding lockfiles, generated files
and fixtures (plus `migrations.lock`, the generated migration inventory; the
hand-written migration SQL stays counted). The labeller and the gate live
once in the org
[TogetherWeOwn/.github](https://github.com/TogetherWeOwn/.github) repository
(CI-STANDARD.md, "Advisory PR-size labels" and "PR size gate"); the workflow
here is a thin caller pinned to a central SHA and passes only this repo's
generated paths. The labels are advisory only: the `pr-size` job is never a
required check and never blocks a merge. The separate `pr-size-gate` job in
the same workflow is required once added to the repository ruleset and fails
pull requests too large for the automated reviewer (warn from 200 counted
lines, fail over 400 lines or 20 files, hard stop at 1000); it is promoted to
the ruleset only after it has reported on the default branch (two-step
ship). A Monday run of the same workflow prints the weekly tally of merged
pull requests per size; the labels are the durable record behind it.

### Issues

Use an issue form: bug report, feature request, enhancement or documentation
issue. Blank issues are off. Report a vulnerability privately; see
[`SECURITY.md`](SECURITY.md).

### Design-only specs

A new design-only spec under `docs/` needs a linked tracking card or issue that
names the date the owner will decide it. Say so in the PR body. The spec's
`Status:` line then records the decision: accepted for a milestone, deferred to
a month or a roadmap, declined (no build), or merged into another spec. A spec
with no decision stays no-build. Keep a declined spec as the record.

## Releases

The daily `daily-digest` workflow tags the live production commit with a
stable CalVer version `vYYYY.MDD.P` (for example `v2026.1101.0`) when
production has moved forward since the last digest, and publishes a GitHub
Release from Conventional Commit subjects. The shared releaser in
`TogetherWeOwn/.github` (`actions/calver`, `live-digest` policy) cuts it, at
most once a day, so `P` stays 0. Deploys never create version tags.
There is no release PR or freeze. One digest tag covers the whole repository,
not one release per crate. Legacy `vYYYY.MM.DD` and `vX.Y.Z` tags stay as they
are. See [`docs/releases.md`](docs/releases.md) for repair and dry-run
dispatches. Never tag or release by hand.

Release notes use the [Common Changelog](https://common-changelog.org/)
categories, in its order: **Changed** (`perf`, `revert`), **Added** (`feat`),
**Fixed** (`fix`, `security`). The digest adapter passes the
`changelog-sections` from `release-please-config.json` to the shared
releaser; `chore`, `docs`, `test`, `ci`, `build`, `refactor`, `style` and
`deps` stay out of the notes. Each squash-merged PR title becomes one
entry, so write it for a reader of the release notes: imperative mood, one
user-facing change.

Release tags use the UTC date the release was planned. Cargo package versions
stay at `0.4.0`; `feat!` and `BREAKING CHANGE` still appear in the
breaking-changes notes but never choose a version bump.

## Verification on GitHub Actions

Full test suites and CI gates run on **GitHub Actions**, not on the shared host.
Push the PR branch to run the existing
[`check.yml`](.github/workflows/check.yml) workflow. Open the PR as a **draft**
while work is incomplete; mark it ready only when the change is complete.
The workflow runs on PR opened/synchronize/reopened events, including drafts;
marking a PR ready is not a substitute for pushing its final head.

```sh
git push origin HEAD:docs/describe-the-change
gh pr create --draft --base main --head docs/describe-the-change --body-file pr.md
# After the work is complete:
gh pr ready <pr-number>
gh pr checks <pr-number>
gh run list --workflow check.yml --commit <head-sha>
gh run view <run-id> --json headSha,url,jobs
```

Use your own public-safe branch name and the repository PR template. Batch each
review round into one push. `ci-ok` is the aggregate CI verdict: both selectors,
lint and every selected lane must pass. A heavy lane deselected by the existing
selectors is not a locally missing prerequisite; do not run it on the host to
compensate or change selection to manufacture a pass. Full main/scheduled runs
and any authorized manual Actions run also stay on GitHub.

### Exact-head CI evidence

Record the PR URL, full head SHA, Actions run URL
(`https://github.com/TogetherWeOwn/two-bot-next/actions/runs/<run-id>`) and relevant
job URLs (`.../job/<job-id>`) in Verification and the task evidence. Confirm the
run's `headSha` matches the PR's current head and inspect its jobs and results;
an older green run, a cancelled run, or a local transcript is not CI evidence for
a new head. Reviewers and the Release Manager use these exact-head Actions links,
including `ci-ok` and relevant selected jobs, alongside the other required checks
and release evidence. Report pending/failed/skipped work honestly. Never claim a
local reproduction is a full CI pass.

The toolchain remains pinned in `rust-toolchain.toml`. CI runs
`python3 scripts/generate_command_reference.py` to generate ignored
`docs/generated/commands.md` before test-target compilation and
`cargo clippy --workspace --all-targets`, then runs read-only reference tests.
These are Actions steps, not a local command checklist. For an allowed focused
controller reproduction only, the generator's `--controller` option enforces
bounded cache admission; it never authorizes a local CI mirror. The generator never
rewrites tracked configuration or independent publication fixtures. See the
[command-reference landing page](docs/commands.md#read-the-reference) for artifact
identity: a PR run is associated with its head, but default checkout generates
from the synthetic merge commit. Retain the checkout-log source SHA as well as
the PR head and run/job links; exact-head CI evidence is not proof of artifact
bytes from the PR-head tree.
CI isolates the environment-mutating ops-override acceptance test from parallel
workspace tests; nextest already isolates tests in separate processes. Reuse the
workflow's steps and logs rather than mirroring those lanes locally.

### Pre-push checklist

Inspect the diff and run only focused static checks appropriate to the changed
files; this is **not** a local CI checklist. For documentation changes, use:

```sh
git diff --check
python3 scripts/check_docs_index.py --json
```

A formatting-only check such as `cargo fmt --all -- --check` does not compile.
Do not execute a local full suite or CI mirror, including
`python3 scripts/local_testdb.py test`, workspace-wide Cargo test/Clippy/build
commands, or a `bin/local-ci.sh` wrapper. This repository has no tracked
`bin/local-ci.sh`; do not copy a local-CI recipe from another repo. The legacy
testdb harness remains implementation code, not an approved verification path.
Do not run it to verify documentation.

### Focused failure reproduction only

Local test execution is allowed only to reproduce **one failing test or crate**,
not as routine pre-push CI. Record the failing Actions job, narrow command and
result. Heavy focused builds use the guarded `boat` PATH wrapper with its sizing,
budget and security controls, `--no-env`, a source-only export of gitleaks-clean
tracked files and verified box cleanup. Never bypass the wrapper or upload
credentials, `.env`, incidental
untracked files or a whole unchecked working tree. Preserve the tested source
SHA/input identity. Do not use `--keep`.

A Boat fallback does not authorize a full suite, CI mirror or heavy raw-host
build. A permitted non-heavy focused Cargo fallback on the persistent controller
still uses the [bounded cache wrapper](docs/build-cache.md); never run Cargo
directly to evade admission. If the focused/local limit or any prerequisite
cannot be met, defer to GitHub Actions or route that actual prerequisite.
Do not create per-worktree Docker Rust target volumes to reproduce CI.

### Database tests

Tests run only on the disposable `agent-testdb` service or a CI Postgres service
container aliased as `agent-testdb:5432`, **never staging or production**. The
shared guard refuses loopback URLs even with CI flags set. Do not
use `DATABASE_URL`, application credentials, credential fallbacks, or libpq
`PG*` connection variables. A configured connection/setup failure must fail the
test, not skip it.

#### Actions database lanes, not a local mirror

The `rust tests`, ignored database and coverage lanes in `check.yml` provision
disposable Postgres and their test environment on GitHub Actions. Push the PR
branch and link the exact-head run and failing/passing job; do not run the legacy
`python3 scripts/local_testdb.py test` CI mirror. Its per-checkout Docker Rust
target volumes are not an approved local build cache. Do not substitute
`local_testdb.py exec` to run the same suite in pieces.

Existing Docker volumes and containers are not permission to reuse the harness.
Retirement/cleanup belongs to the operator after exact resource identification
and verification that no active checkout or container references them; never
prune another checkout's volume or delete evidence as part of a docs change.

#### Focused database failure reproduction

Only a single failing test or crate may be reproduced under the
[focused execution policy](#focused-failure-reproduction-only). If its approved
sandbox cannot reach a disposable service with the guarded identity below,
reproduce on Actions instead; never relax the guard or use a live database.
An authorized disposable service uses one empty `two_bot_test_local` bootstrap,
owned by `agent_test` (empty password, `CREATEDB` permission), with
`TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_local`.
This describes fixture setup, not permission to start a local full-suite runner.

The shared guard requires `postgres`/`postgresql`, literal `agent_test:@`, an
allowlisted host, explicit port 5432, and a lowercase `two_bot_test_*` database
name (1–63 bytes, letters/digits/underscores, no trailing underscore). It refuses
credentials, query overrides, fragments, encoded/ambiguous targets, sockets,
and inherited libpq connection settings. SQLx's pgpass fallback is disabled.
The name is a **bootstrap**, not permission to reset that database.

New slices use `crates/testsupport` through a **dev-dependency only**, with the
workspace's release version:

```toml
[dev-dependencies]
two-bot-testsupport = { path = "../testsupport", version = "0.4.0" } # x-release-please-version
```

```rust,ignore
use two_bot_testsupport::TestDatabase;

#[tokio::test]
async fn persists_a_row() {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("test bootstrap required");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await.expect("create migrated fixture");
    // Exercise the store using db.pool(). No hand-written guard or reset DDL.
    // Use db.independent_pool().await for multi-worker/restart assertions;
    // close peer pools before closing the fixture.
    db.close().await.expect("verify teardown");
}
```

Each fixture creates a unique database, applies the supplied migrations, and
closes/drops only that owned database. Migrations and all fixture connections,
including independent pools, enforce a five-second SQL statement timeout. Only
the teardown admin session gets a finite 30-second `DROP DATABASE` budget, since
Postgres can wait for a forced checkpoint on a loaded test service. That timeout
is set and the drop is executed on the same acquired connection. The bootstrap
is never migrated or dropped. Call `close().await` explicitly so teardown errors
fail the test;
`Drop` provides only best-effort cleanup while a Tokio runtime remains alive.
Migration failures clean up immediately. Cancelling explicit close detaches
teardown rather than cancelling it, but the runtime must stay alive. Do not rely
on panic/runtime shutdown cleanup. The lifecycle test proves concurrent
isolation, failure/cancellation cleanup, and the held-lock timeout.

Feature-gate store tests with `#![cfg(feature = "db")]`, but do not mark new
DB suites ignored. The normal CI integration step already supplies the single
`two_bot_test_ci` bootstrap and runs integration targets, so new slices need no
new `createdb` name, URL variable, or bespoke workflow step. Website contracts,
internal actions, and leveling are migrated; older suites retain their existing
explicit opt-ins until migrated separately.

The `wrangler/` Worker/DO wrapper has its own `npm ci`, `npm run typecheck`
and `npm test`. The `worker check` lane in `.github/workflows/check.yml` also
runs `npm audit --audit-level=high --include=dev --json` from `wrangler/` before
installation whenever worker inputs are selected (including full main and
scheduled runs). It audits the committed lockfile, including dev tooling, and
prints npm's machine-readable JSON report directly in the CI step log. High or
critical advisories and registry errors fail the lane and therefore `ci-ok`;
low/moderate advisories remain visible but do not fail it. There are no audit
suppressions. Fix new high/critical findings before merging; any proposed
exception needs an explicit tracking note and expiry, not a silent ignore.

Use the selected Actions `worker check` job for audit and Worker-suite evidence,
not a local `npm ci`/typecheck/test CI mirror. Its offline fixtures in
`scripts/ci/test_worker_npm_audit.py` stub npm and check the workflow command's
exit-status and JSON-log propagation; the audit
queries the registry's current advisories. Reproduce one failing fixture only
under the focused execution policy above. Audits detect known advisories, not
every supply-chain compromise.
Never commit secrets, `.env` files or `target/`. See [README.md](README.md) for
the full service reference.

### Secret-scan allowlists

The required `gitleaks` check scans the full history reachable from the PR head.
On a pull request it applies the base branch's `.gitleaks.toml` and
`.gitleaksignore` and ignores inline `gitleaks:allow` comments, so a PR cannot
allowlist its own leak. If a change legitimately needs a new allowlist entry,
merge that entry first as a small, separately reviewed PR; the next scan honours
it. Push-to-main scans use the repo's own files. Run
`GITLEAKS_BIN=<path> .github/scripts/test-gitleaks-scan.sh` to repeat the offline
self-test the `gitleaks` job runs. The scanner archive is pinned by SHA-256 in
`supply-chain.yml`; bump `GITLEAKS_SHA256` from the release's
`gitleaks_<version>_checksums.txt` together with `GITLEAKS_VERSION`.
