# Handoff — 2026-11-10 (UTC)

Dated snapshot for resuming work from this repo alone. This is not a
live queue: Paperclip stays the authority for assignments,
dependencies and current status. If this file and the live queue
disagree, the live queue wins and this file needs a dated update.

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
- A public roadmap entry point is in review as a separate docs change
  (#2021). This snapshot does not duplicate it; the approved-vs-proposed
  summary above already names what counts as scope without relying on
  that entry point.
- M2 state: lockdown preview is implemented, dark and unpublished
  (planner #1920, executor reads #1925, dark slash #1929); `{args}`
  build slices merged in #1970/#1974 (spec Status correction open as
  #2002); automod preview slices #1959, #1961–#1965 are open; warning
  history is spec-only. See the current-state plan for the per-item
  table.

## Relevant public PRs and branches

- The roadmap entry-point PR is #2021, open and in review at snapshot.
  Do not modify its pending review from a handoff; integrate it once
  merged.
- M2 build branches are the only build work in flight: automod
  preview slices #1959, #1961–#1965 (open), lockdown confirm/cancel
  #1938 (open), and the `{args}` status correction #2002 (open;
  builds merged in #1970/#1974). No M3 build PR has been opened.

## Blockers (public-safe summary)

- **Blocked on staging recovery:** automod preview (W3), warning
  history (W4) and `{args}` (W5, built in a stack with merges pending
  per its spec Status line; builds merged in #1970/#1974, status
  correction open as #2002). Staging recovery means database re-home,
  then staging migrations, then the staging lifecycle drill. Details
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
[`CONTRIBUTING.md`](../CONTRIBUTING.md#pre-push-checklist). In short:

```sh
cargo fmt --all -- --check
python3 scripts/generate_command_reference.py
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
```

On the persistent controller, run compiling Cargo commands through
`python3 scripts/cargo_cache.py run -- <check|test|clippy|build> ...`
(see [build cache](build-cache.md)). Database tests run only against
a disposable test database; see
[`CONTRIBUTING.md`](../CONTRIBUTING.md#database-tests). Full suites
run on GitHub Actions; record the exact-head run links in the PR body.

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

## Next unfinished step

Resume the three remaining M2 items once staging recovery lands:
automod preview (W3) first, then warning history (W4), then `{args}`
(W5) gated release — re-checking each spec's seams on current `main`
first, building behind the spec's own gates and staging behavior
evidence before any merge approval. The lockdown preview is already
implemented dark; only its publication/activation remains, behind its
own spec §8 gates. Until then, keep M3 deferred and keep release
holds in force.
