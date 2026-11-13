# AGENTS.md

Guidance for every contributor to `two-bot-next`, human or AI. This repo is
public and holds the Together We Own Discord bot, written in Rust. It runs as
one always-on container with a live Discord gateway, a shared Postgres
database and signed internal actions. A mistake here can moderate real
members, leak a token, or corrupt shared data, so keep each change small,
tested and reviewable.

## Read first

1. [`CONTRIBUTING.md`](CONTRIBUTING.md): commits, releases, local checks and database tests.
2. [`README.md`](README.md): architecture, status and operator references.
3. [`.github/pull_request_template.md`](.github/pull_request_template.md): the PR body; Linked Issues and Verification are required, the rest is optional.
4. [`SECURITY.md`](SECURITY.md): private vulnerability reporting and secret handling.
5. [`CONSTRAINTS.md`](CONSTRAINTS.md): the quality bar. Read it before writing code. Do not weaken it to make a change pass.

## Resume from the repo alone

Durable product and engineering knowledge lives in the repo. To resume
work without any external tracker, read in this order:
[`docs/handoff.md`](docs/handoff.md) (dated snapshot, checks, next
step), [`docs/plans/current-state-2026-11.md`](docs/plans/current-state-2026-11.md)
(approved vs deferred scope), [`docs/adr/0002-m2-m3-sequencing.md`](docs/adr/0002-m2-m3-sequencing.md)
and [`docs/adr/0003-m3-deferred.md`](docs/adr/0003-m3-deferred.md)
(material decisions). Accepted specs live flat in `docs/`; deploy and
rollback procedures live in `docs/production-deploy.md` and
`docs/runbook.md`. Specs, plans and decisions change in the same PR as
the work they describe.

Browse the [documentation map](docs/README.md) before searching individual guides.
When adding a doc, add its purpose/link there; run `python3 scripts/check_docs_index.py --json`.
The index merges with union (`.gitattributes`); after rebasing a docs change, run `python3 scripts/check_docs_index.py --dedupe --json` and repair any rows it still reports instead of hand-merging the index.
Union is a local merge/rebase aid, not proof of GitHub mergeability: GitHub
can still report an index conflict when local `git merge-tree` is clean.
Check GitHub's mergeability;
resolve a reported conflict locally, run the dedupe checker above and repair
its remaining findings, then push the resolution before expecting GitHub to merge.

## Pull request contract

- Work on a branch and open a PR. Never push to `main`.
- Branch names look like `type/short-slug`, for example `fix/gateway-resume`.
- Merges are squash-only. The squash commit takes the PR title and body.
- The PR title is a Conventional Commits header: `type(scope): summary`, at most
  100 characters, no trailing period. Types: `feat`, `fix`, `perf`, `refactor`,
  `test`, `docs`, `build`, `ci`, `chore`, `revert`, `style`, `security`. Release
  automation reads these headers, so write them for the changelog.
- Use the PR template, in short, active sentences. Required: Linked Issues or
  Issue Description, and Verification (commands run and their results). What
  Changed and Notes for reviewers are optional.
- PRs follow the org contract in `TogetherWeOwn/.github` CONTRIBUTING + this repo's overrides; use this repo's PR template, which replaces the org template; its Attestation section is mandatory.
- Keep references public-safe. Put no secret, token, private URL or internal
  tracker ID (`TOG-`, `PAP-`) in any title, body, commit, comment or branch name.
  Link public GitHub issues as `Closes #123`.
- Disclose the tests honestly in Verification. Report only runs you saw. Say what
  you did not run.
- Address every review finding, or reply with why it does not apply.
- Credit the contributors whose work you build on.
- Done means merged. Do not leave an orphan PR open: merge it, or close it with a
  comment that names what replaced it.
- Never bypass a required check, a ruleset or a failing test.

## Test and build commands

Full test suites and CI gates run on **GitHub Actions**, not the shared host.
Push the PR branch and open a **draft PR** while incomplete; mark it ready only
when the work is complete. The existing `.github/workflows/check.yml` owns
`ci-ok`, including both selectors, lint and every selected test/build lane.
Keep all required checks, security, native review and release gates unchanged.
See [`CONTRIBUTING.md`](CONTRIBUTING.md#verification-on-github-actions) for the
push/check workflow and exact-head evidence requirements.

Before pushing docs, run focused static checks such as `git diff --check` and
`python3 scripts/check_docs_index.py --json`. Never run a local full-suite or
CI-mirror command: `python3 scripts/local_testdb.py test`, workspace-wide Cargo
test/Clippy/build, Worker CI sequences, or a `bin/local-ci.sh` recipe. There is
no tracked `bin/local-ci.sh` in this repo. Splitting a suite into local commands
does not make it focused.

Local execution is allowed only to reproduce one failing test or crate. Heavy
focused builds use the guarded `boat` PATH wrapper, its sizing/budget/security
controls, `--no-env`, a source-only export and verified cleanup; never bypass it.
Boat fallback never authorizes a full suite/CI mirror or heavy raw-host build.
If the focused/local limit cannot be met, use Actions or route the actual
prerequisite. The bounded cache rules below still govern permitted non-heavy
focused controller Cargo fallback.

CI runs `python3 scripts/generate_command_reference.py` to generate ignored
`docs/generated/commands.md` before test-target compilation and
`cargo clippy --workspace --all-targets`; these are Actions steps, not local
pre-push commands. For an allowed focused controller reproduction only, the
generator's `--controller` option enforces bounded cache admission, not permission
to mirror CI. [`docs/commands.md`](docs/commands.md) links the artifacts. The generator never
rewrites tracked configuration or independent publication fixtures. Database
tests use only disposable test services, never staging or production; retain
all guards in [`CONTRIBUTING.md`](CONTRIBUTING.md#database-tests).

Record the PR's full head SHA and matching Actions run/job URLs for reviewers
and the Release Manager. Local transcripts are focused reproduction evidence,
not CI evidence. The final-stage reviewer merges the pinned settled head before
final approval (the author merges only without a review stage). A merge to
`main`, including docs-only, needs Release Manager exact-head authorization on
a child task assigned/addressed to it with `resolverPolicy: not_creator`.

## Definition of done

- The behavior matches the request.
- Relevant tests pass in GitHub Actions, with exact-head run/job links recorded.
- Any permitted focused reproduction and checks not run are disclosed.
- CI is green on the exact head commit.
- Review is complete on that same head commit.
- The PR is squash-merged.
- The docs the change makes stale are updated.

## Controller builds

Only for a permitted non-heavy focused failure reproduction/fallback on the
persistent Paperclip controller, run Cargo through
`python3 scripts/cargo_cache.py run -- <check|test|clippy|build> ...` from your own
isolated workspace. Pick one failing test or crate, never the full workspace.
This cache wrapper is not an alternative location for CI or heavy builds.
The wrapper supplies offline/locked mode, lean dev/test settings and one of two
independent cache leases. See [the build-cache runbook](docs/build-cache.md).

Do not create a per-worktree `target/` or an external/container `/tmp` Cargo target,
point Cargo at the old unbounded shared cache, change agent environments/rosters,
or evade a busy/refused/missing pool by running Cargo directly. The wrapper places
cooperative temporary output in quota-covered lease scratch, not container `/tmp`.
Run permitted focused commands in the foreground with a timeout covering the
command; read the result in the same run. Interrupted partial output stays in
the slot and counts against its budget. A missing pool/quota/scratch-coverage
receipt needs the Operator rollout; a full pool needs safe retention/continuation,
not another cache. Agents must not
clear crash sentinels, delete host caches, mount filesystems or restart services.

`cargo fmt --all -- --check` does not compile and can run directly. Cache
regressions are offline Python fixtures run by Actions. Reproduce only a failing
fixture locally, not the fixture suite as a CI mirror.

Ephemeral hosted CI and Docker image builds keep their existing Cargo commands.
Do not use production/staging databases for tests; use only authorized test
containers/CI services. Source, evidence, active/shared workspaces, backups and
incident-preservation archives are never build-cache retention candidates.
