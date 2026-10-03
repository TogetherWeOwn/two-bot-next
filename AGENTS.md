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
3. [`.github/pull_request_template.md`](.github/pull_request_template.md): the seven sections every PR fills in.
4. [`SECURITY.md`](SECURITY.md): private vulnerability reporting and secret handling.

## Pull request contract

- Work on a branch and open a PR. Never push to `main`.
- Branch names look like `type/short-slug`, for example `fix/gateway-resume`.
- Merges are squash-only. The squash commit takes the PR title and body.
- The PR title is a Conventional Commits header: `type(scope): summary`, at most
  100 characters, no trailing period. Types: `feat`, `fix`, `perf`, `refactor`,
  `test`, `docs`, `build`, `ci`, `chore`, `revert`, `style`, `security`. Release
  automation reads these headers, so write them for the changelog.
- Fill in every section of the PR template, in short, active sentences: Thinking
  Path, Linked Issues or Issue Description, What Changed, Verification, Risks,
  Model Used, Checklist.
- Keep references public-safe. Put no secret, token, private URL or internal
  tracker ID (`TOG-`, `PAP-`) in any title, body, commit, comment or branch name.
  Link public GitHub issues as `Closes #123`.
- Disclose the model and the tests honestly. Name the exact model ID in Model
  Used, or write "None — human-authored". Report only runs you saw. Say what you
  did not run.
- Address every review finding, or reply with why it does not apply.
- Credit the contributors whose work you build on.
- Done means merged. Do not leave an orphan PR open: merge it, or close it with a
  comment that names what replaced it.
- Never bypass a required check, a ruleset or a failing test.

## Test and build commands

Run the commands that cover your change before every push. The full list, in CI
order, is in [`CONTRIBUTING.md`](CONTRIBUTING.md#pre-push-checklist).

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
```

On the persistent Paperclip controller, run the compiling commands through the
cache wrapper described under [Controller builds](#controller-builds).

The `wrangler/` Worker wrapper has its own `npm ci`, `npm run typecheck` and
`npm test`. Database tests run only against a disposable test database, never
staging or production; see [`CONTRIBUTING.md`](CONTRIBUTING.md#database-tests).

## Definition of done

- The behavior matches the request.
- The tests that cover it pass locally.
- CI is green on the exact head commit.
- Review is complete on that same head commit.
- The PR is squash-merged.
- The docs the change makes stale are updated.

## Controller builds

On the persistent Paperclip controller, run compiling Cargo commands through
`python3 scripts/cargo_cache.py run -- <check|test|clippy|build> ...` from your own
isolated workspace. Pick the smallest test/package that proves the change.
The wrapper supplies offline/locked mode, lean dev/test settings and one of two
independent cache leases. See [the build-cache runbook](docs/build-cache.md).

Do not create a per-worktree `target/` or an external/container `/tmp` Cargo target,
point Cargo at the old unbounded shared cache, change agent environments/rosters,
or evade a busy/refused/missing pool by running Cargo directly. The wrapper places
cooperative temporary output in quota-covered lease scratch, not container `/tmp`.
Run the wrapper with a Bash timeout that covers the build (600000 ms) or in the
background; interrupted partial output stays in the slot and counts against its
budget. A missing pool/quota/scratch-coverage receipt needs the Operator rollout;
a full pool needs safe retention/continuation, not another cache. Agents must not
clear crash sentinels, delete host caches, mount filesystems or restart services.

`cargo fmt --all -- --check` does not compile and can run directly. The cache
regressions are offline Python fixtures:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_cargo_cache.py' -v
```

Ephemeral hosted CI and Docker image builds keep their existing Cargo commands.
Do not use production/staging databases for tests; use only authorized test
containers/CI services. Source, evidence, active/shared workspaces, backups and
incident-preservation archives are never build-cache retention candidates.
