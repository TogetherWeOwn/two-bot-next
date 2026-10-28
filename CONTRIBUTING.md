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
`pr-lint` runs `.github/scripts/pr_standards.py` (tests beside it): it checks
the title format, the 100-character limit, the trailing period and a non-empty
body as errors, and the two required template sections (Linked Issues and
Verification), the advisory title/body remainder and branch names in warn mode
(`PR_STANDARDS_MODE: "warn"`). The strict title/body subset (tracker id,
instance UI link, internal host) always fails. On a push to `main` it
also checks every commit subject. All required checks must be green on the
exact head commit. Never bypass or weaken a check.

`check.yml` runs its Rust work as parallel lanes after `job-inputs`:
`check` (lint: offline guards, cargo-deny, fmt, clippy), the sharded `rust tests`
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

`test-partition-plan` emits five Rust partitions and five runtime-DB partitions
through `scripts/ci/run-test-partition.py --lane <lane> --list`. Integration
ownership is deterministic over Cargo-discovered package/target pairs; adding a
target does not require an allowlist edit. Unit/bin, gateway and onboarding
work have separate owners. Each worker retains its disposable database service,
prepare/verify guards and isolated replays. Stable `rust tests` and
`ignored db (runtime)` jobs require every worker result and exactly one successful
receipt per partition. Rust receipts also preserve the original **combined**
property-test execution budget below 30 seconds; compiling all integrations in
one timing-only worker would reintroduce the critical path.

Coverage splits both ignored and nonignored sweeps by package before assigning
legs to eight runners. A remainder partition includes root/testsupport and future
workspace packages; each package still discovers all of its targets. The
workspace-package features are resolved from the full workspace before selecting
packages: enabling only `two-bot-core/db` would lose the consumer's own `db`
feature. Unrelated packages cannot accept that dependency feature flag. The
shared `cargo_feature_selection.py` helper preserves workspace-member feature
unification without copying external platform/build feature unions. Inspect
resolved sweep descriptors with `python3 scripts/ci/plan-coverage-shards.py
--describe-sweeps`; `FULL_LEGS` contains templates, not graph-independent final
commands. Changed-crate/reverse-dependency selection, conservative widening,
guarded replay ownership and the HARD added-line coverage bar remain mandatory.
Synthetic ownership checks and workspace-feature qualification are not real LLVM
parity or latency evidence; external dependency feature differences still need
real Cargo/LLVM qualification.

A normal reviewed revert of a partitioning follow-up restores its prior sharded
configuration. Restoring the historical single coverage job requires reverting
both the follow-up and the coverage foundation together in one reviewed rollback
commit, preserving later unrelated changes and passing the same required gates.

A job added to `check.yml` must go into `ci-ok`'s `needs:`, result env and result
evaluation, or carry a reviewed `# ci-ok: exempt <reason>` marker with a pin
in `scripts/ci/test_required_checks.py`. Only advisory `container smoke` is
exempt: it can take up to 30 minutes, and image security stays gated through
the SBOM job. The legacy `required-checks` aggregate is removed.

Deterministic quality gates (per-PR advisories HARD, ast-grep and Semgrep
HARD, required voice synthetic, test-target inventory HARD) run as steps of
the `check` lane on the existing routed runner; see
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

### Pull-request size labels (advisory)

`.github/workflows/pr-size.yml` labels every pull request `size/S` (up to 100
counted lines), `size/M` (up to 400), `size/L` (up to 1000) or `size/XL`, and
keeps one advisory comment on pull requests over the 400-line threshold.
Counted lines are additions + deletions excluding lockfiles, generated files
and fixtures (plus `migrations.lock`, the generated migration inventory; the
hand-written migration SQL stays counted). The labeller lives once in the org
[TogetherWeOwn/.github](https://github.com/TogetherWeOwn/.github) repository
(CI-STANDARD.md, "Advisory PR-size labels"); the workflow here is a thin
caller pinned to a central SHA and passes only this repo's generated paths.
This is advisory only: it is not a required check and never blocks a merge.
A Monday run of the same workflow prints the weekly tally of merged pull
requests per size; the labels are the durable record behind it.

### Issues

Use an issue form: bug report, feature request, enhancement or documentation
issue. Blank issues are off. Report a vulnerability privately; see
[`SECURITY.md`](SECURITY.md).

## Releases

The daily `daily-digest` workflow tags the live production commit `vYYYY.MM.DD`
when production has moved forward since the last digest, and publishes a GitHub
Release from Conventional Commit subjects. Deploys never create version tags.
There is no release PR or freeze. One digest tag covers the whole repository,
not one release per crate. See [`docs/releases.md`](docs/releases.md) for repair
and dry-run dispatches. Never tag or release by hand.

Release notes use the [Common Changelog](https://common-changelog.org/)
categories, in its order: **Changed** (`perf`, `revert`), **Added** (`feat`),
**Fixed** (`fix`). `chore`, `docs`, `test`, `ci`, `build`, `refactor` and
`style` stay out of the notes. Each squash-merged PR title becomes one
entry, so write it for a reader of the release notes: imperative mood, one
user-facing change.

Release tags use UTC calendar dates. Cargo package versions stay at `0.4.0`;
`feat!` and `BREAKING CHANGE` still appear in the breaking-changes notes but
no longer choose a SemVer bump.

## Local development

The toolchain comes from `rust-toolchain.toml` (rustup and CI honour it):

```sh
cargo fmt --all -- --check
python3 scripts/generate_command_reference.py
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
```

Generate `docs/generated/commands.md` before compiling workspace tests or
all-targets Clippy: one test embeds the generated Markdown at compile time.
On the persistent controller use `python3 scripts/generate_command_reference.py --controller`,
which builds only the generator through bounded Cargo admission. The output is
ignored; never commit generated registry rows. The authored
[command-reference landing page](docs/commands.md) links to same-head CI artifacts.
Normal reference-doc tests compare bytes without writing. Missing or stale output
fails until explicit generation; tracked configuration drift still fails and is
never automatically repaired.

The ops-override DB acceptance test mutates process environment. With the test
DB configured, run it separately from parallel workspace tests (CI does this in
both the unit-test and coverage lanes):

```sh
cargo test --workspace --features two-bot-core/db --locked -- --skip settings_jobs::tests::ops_overrides_acceptance_tests::
cargo test -p two-bot --bin two-bot --features two-bot-core/db --locked settings_jobs::tests::ops_overrides_acceptance_tests:: -- --test-threads=1
```

Nextest already isolates each test in its own process. On the controller, use the
bounded cache wrapper for both compiling commands, as required by `AGENTS.md`.

### Pre-push checklist

Run this before every push. It mirrors the CI `check` job's fail-fast order:
cheap lint/format/offline checks first, so a local failure is found in seconds
instead of after CI restores the ~4 GiB cargo cache.

```sh
cargo fmt --all -- --check
python3 scripts/check-secret-debug.py --test
python3 scripts/check-secret-debug.py
python3 scripts/test-docker-deps.py
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_cargo_cache.py' -v
python3 scripts/check_soak_checklist.py
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_soak_checklist.py' -v
python3 tests/voice_templates/validate.py
python3 -m unittest discover -s tests/voice_templates -p 'test_*.py' -v
```

(`test-docker-deps.py` without `--cargo` covers the offline layer shape; CI
additionally runs the real locked `cargo fetch` after the cache restore.)

### Database tests

Tests run only on the disposable `agent-testdb` service or a CI Postgres service
container aliased as `agent-testdb:5432`, **never staging or production**. The
shared guard refuses loopback URLs even with CI flags set. Do not
use `DATABASE_URL`, application credentials, credential fallbacks, or libpq
`PG*` connection variables. A configured connection/setup failure must fail the
test, not skip it.

Create one empty bootstrap database `two_bot_test_local` on the disposable
service, owned by its documented `agent_test` principal (empty password, with
`CREATEDB` permission). Then run:

```sh
export TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_local
cargo test -p two-bot-testsupport --locked
cargo test -p two-bot-core --features db --locked --test website_contract --test internal_action_store --test leveling_store
```

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

Rerun the audit and its offline pass/failure fixtures from the repository root
with the existing npm and Python CI tools (no new local prerequisites):

```sh
npm --prefix wrangler audit --audit-level=high --include=dev --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/ci -p 'test_worker_npm_audit.py' -v
```

The offline fixtures stub npm and check the actual workflow command's exit-status
and JSON-log propagation; the native audit above queries the registry's current
advisories. Audits detect known advisories, not every supply-chain compromise.
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
