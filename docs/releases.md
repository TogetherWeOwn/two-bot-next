# Release architecture

The container has one release version, one root changelog, and one flat
`vX.Y.Z` tag. Internal crates are not released separately.

## Why there is a root library

The root manifest originally defined only a virtual Cargo workspace.
release-please's native Rust strategy requires a root `[package]` version;
adding a targetless package is invalid Cargo, even with `default-members`.
The root now has a minimal, non-published release-metadata library at
`src/lib.rs`. Explicit `default-members` preserve the previous selection of
all four application crates and also include the metadata library.

Cargo documents [root packages](https://doc.rust-lang.org/cargo/reference/workspaces.html#root-package)
and the [default library target](https://doc.rust-lang.org/cargo/reference/cargo-targets.html#library).

## Pinned native strategy

The release action is pinned to
[`45996ed1f6d02564a971a2fa1b5860e934307cf7`](https://github.com/googleapis/release-please-action/tree/45996ed1f6d02564a971a2fa1b5860e934307cf7)
(v5.0.0), whose lockfile and bundled implementation use release-please
17.6.0. The single root `rust` strategy synchronizes the root package,
member versions, local dependency requirements, and root lockfile:

- [Rust strategy](https://github.com/googleapis/release-please/blob/v17.6.0/src/strategies/rust.ts)
- [Cargo manifest updater](https://github.com/googleapis/release-please/blob/v17.6.0/src/updaters/rust/cargo-toml.ts)
- [Cargo lock updater](https://github.com/googleapis/release-please/blob/v17.6.0/src/updaters/rust/cargo-lock.ts)

There is no workspace plugin and no independently configured member release.
This avoids duplicate flat tags and ensures that changes outside `crates/bot`
(including shared domain code and the Worker) are eligible for release notes.
The excluded `fuzz` workspace's local dependency requirements and the copyable
`CONTRIBUTING.md` testsupport example use `extra-files` with inline
`x-release-please-version` markers. The native lifecycle fixture checks them on
every generated snapshot and its next release; the unpublished fuzz package
keeps version `0.0.0`, and its external dependencies stay unchanged.

The `0.1.0` manifest is only a seed: it does not assert a published release.
Features and breaking changes advance the minor version while pre-1.0;
fixes advance the patch. Production cutover is the deliberate 1.0.0 boundary.

## Offline verification

`scripts/test-release.cjs` uses a fail-closed mock GitHub client and the
actual repository configuration. It does not use a token or mutate GitHub.
Install its exact library outside the checkout, then run:

```sh
npm install --prefix "$RELEASE_TEST_DEPS" --ignore-scripts --no-audit --no-fund release-please@17.6.0
NODE_PATH="$RELEASE_TEST_DEPS/node_modules" node scripts/test-release.cjs
```

Set `RELEASE_TEST_DEPS` to a disposable dependency directory (in agent runs,
use a directory under `PAPERCLIP_RUN_SCRATCH_DIR`). The immutable bootstrap
changelog fixture is separate from the live `CHANGELOG.md`, which automation
changes after release. The fixture covers tagged and untagged seeds, features
in the root/four crates/Worker, breaking changes, fixes (including security-only
commits), Common Changelog headings, the absence of any tracker footer, synchronized manifest and
lock updates, and exactly one componentless release candidate. Each generated
snapshot, including every manifest/dependency/lock update and the migrated
changelog, then feeds a second native release to verify the post-release state.
`security` entries appear under Fixed and advance the patch.
Before dispatching checks, `scripts/migrate-release-notes.cjs` consumes the
native updater's first-release bootstrap tail: it merges the existing
Added/Fixed/Changed notes (plus the hand-written Notes tail and Security
section the live changelog carries) into the generated version section and
the release PR body,
removing the duplicate title and Unreleased section. The PR body matters because
release-please uses it, not the changelog file, for GitHub Release notes. The
changelog and body are reconciled independently, so retries recover if only one
side was updated. Once they agree the reconciliation is a no-op; unexpected
layouts fail closed. The lifecycle fixture asserts each historical note in both
outputs and the resulting release payload, with one title and no stranded notes.
The configured PR header keeps the seven PR-template sections (Thinking Path,
Linked Issues or Issue Description, What Changed, Verification, Risks, Model
Used, Checklist) before the FIRST native `---` delimiter, so normal and
overflow canonical bodies carry the same metadata. Placing them inside the notes
before the version heading makes 17.6.0 parse zero releases and silently publish
an empty notes payload. Worker CI runs `scripts/test-release-publication.cjs`
against an immutable real first-PR body, asserting the full native publication
payload, the seven-section header in both the normal body and the stored
overflow representation, and a misplaced-template negative control. Reconciliation also repairs the first-release comparison
against the unpublished 0.1.0 seed in both changelog and body; regeneration must
not restore a link to a nonexistent tag.
Cargo CI still validates compilation and the real release flow still validates
GitHub writes.

`python3 scripts/test-pr-lint.py` executes the workflow's actual inline scripts
with mocked PR metadata, including delimiter collisions and stale/foreign PR
rejection. `python3 scripts/test-docker-deps.py` recreates the manifest/stub
layer from the actual Dockerfile and verifies all five package targets. Add
`--cargo` to run that layer's `cargo fetch --locked` (as Rust CI does). These
fixtures do not contact a database; full Docker builds remain a deployment gate.

## Release triggers and procedure

`.github/workflows/release.yml` treats its events differently (TOG-12931).
Previously every push to `main` regenerated the release PR: the head was
rewritten about a minute after each merge and a full `check` (~20 min) started
on it, so the PR could never stay green long enough to merge while `main` moved
and every merge wasted a run. Now:

| Event | Publishes a merged release PR | Regenerates the PR | Reconciles notes and dispatches `check` / `supply-chain` |
| --- | --- | --- | --- |
| `push` to `main` | yes | **no** (`skip-github-pull-request`) | **no** |
| `workflow_dispatch` on `main` | yes | yes, unless the branch already holds this main snapshot | yes |
| `schedule` (Mondays 04:23 UTC) | yes | same as dispatch | yes |

The tag/release (and the SBOM assets, see `docs/supply-chain.md`) are published
by the `push` run for the release PR's merge commit, exactly as before: the
publication path and the job permissions did not change, and `skip-github-release`
is never set. A missed publication is recovered by the next run of any kind. The
`dry_run` and `release_tag` dispatch inputs keep their SBOM-only meaning.
`pr-lint`, `gitleaks` and `check` remain required on the release PR; the
dispatched runs are still the only way they start on a `GITHUB_TOKEN`-created PR.

With no push regeneration the open release PR lags `main` by design. Cut a
release with a short freeze: release-please drops commits that land between the
PR's generation snapshot and its merge commit (they ship in the tag but appear
in neither release's notes).

1. **Announce the freeze.** The Director (or the COO) posts on the release card
   that nothing merges to `main` until the freeze is lifted, and confirms no
   merge is in flight.
2. **Dispatch regeneration.** `gh workflow run release.yml --ref main`, then wait
   for the run. It regenerates the PR from the current `main`, preserves the
   bootstrap notes and dispatches `check.yml` and `supply-chain.yml` on the head.
3. **Confirm the PR is fresh.** From a checkout of `main`:
   ```sh
   GH_REPO=<owner>/<repo> GITHUB_SHA="$(gh api "repos/<owner>/<repo>/commits/main" --jq .sha)" \
     GITHUB_OUTPUT=/dev/stdout node scripts/release-pr-state.cjs plan
   ```
   `reuse_pr=true` means the PR was generated from today's `main`. `reuse_pr=false`
   means there is no open release PR or `main` moved after its snapshot: do not
   merge, dispatch again.
4. **Exact-head green and review.** `check`, `pr-lint` and `gitleaks` are green on
   the PR's current head SHA, and the Code Reviewer approved that same SHA.
   A re-push (including a new regeneration) restarts both.
5. **Reviewer merges.** The approving reviewer squash-merges with
   `expectedHeadSha` set to the reviewed head. The merge's `push` run publishes
   the tag and GitHub Release and attaches the SBOMs; verify the new `vX.Y.Z`
   release, the `attach-sbom` job and the `autorelease: tagged` label.
6. **Lift the freeze** once publication is verified. The next release PR appears
   at the next dispatch or Monday run.

The freeze is the exception path for the cut, not a standing hold: outside steps
1-6 `main` merges freely and nothing rewrites the release PR.

## Retry-safe PR reconciliation

`scripts/release-pr-state.cjs` selects only an open, same-repository, main-base
root release PR labeled `autorelease: pending` on the native component head
(`release-please--branches--main--components--two-bot-next`, derived from the
root package name). A compare API merge-base check proves whether that branch
already includes the main snapshot; reuse is additionally bound to the
generation snapshot (the first parent of the newest `chore(main): release X`
commit, which is how native parents its force-replaced branch commit), because
an "Update branch" merge keeps ancestry while leaving the generated metadata
stale. If both hold, the
pinned action's [`skip-github-pull-request` input](https://github.com/googleapis/release-please-action/blob/45996ed1f6d02564a971a2fa1b5860e934307cf7/action.yml)
skips only PR regeneration, avoiding body-comparison resets of migrated notes.
Release publication stays enabled. A new main snapshot enables native PR
regeneration; a closed/merged PR also leaves publication and creation enabled.

Selection after the action queries GitHub, rather than relying on `prs_created`:
a native no-op or prior migration failure must still reconcile the existing PR
and dispatch its checks (on `schedule` and `workflow_dispatch`; a `push` skips
planning, reconciliation and dispatch entirely). When publication leaves no open PR, selection emits
`pr_available=false` and valid empty-object `pr={}` JSON. Actions evaluates
step environment expressions even when the step's `if` is false, so an empty
string would make the skipped reconciliation step fail at `fromJSON` after
successful publication. The availability guards skip checkout, reconciliation
and check dispatch; native publication stays enabled. The retry fixture parses
the actual no-PR CLI output and checks these guards without making mutations.
The workflow pushes a changelog diff only when needed,
then PATCHes a body diff independently via the supported REST API. A successful
push followed by a failed PATCH therefore repairs only the body on retry.
Unchanged reconciliation makes no commit, push or body-PATCH calls. Checks may
be dispatched again on an explicit rerun; they still target the existing head.

Oversized release notes overflow natively: the visible PR body becomes a
single-line link while the full notes live in `release-notes.md` on the
derived `<head>--release-notes` branch. The workflow reconciles that stored
file (never PATCHing the native-owned link) only when the visible body parses
as the exact native overflow link; a dangling link fails closed before any
push or dispatch, and a stale notes branch alongside a normal body is ignored.
The stored-notes Contents PUT carries the branch inside the JSON payload
(`gh api --input` moves `-f` flags to the URL query, which the Contents API
ignores). PR lint resolves the same validated notes-branch file before its
body checks, so required lint passes on overflow PRs without weakening the
empty-body gate. Migration can also grow a large normal body
past the native 65,536-char limit; the workflow routes that reconciled output
through the same overflow representation (stored notes plus link) instead of an
oversized PATCH that GitHub would reject on every retry. The next run resolves
the new representation exactly like a native overflow.

`python3 scripts/test-release-retry.py` runs the workflow's actual reconciliation
shell and state CLI using complete disposable local Git repositories and a
fail-closed GitHub mock. It also evaluates the workflow's real `if` and
`skip-github-pull-request` expressions per event, showing a `push` skips
planning, regeneration, reconciliation and dispatch while a dispatch or schedule
runs them, and that `main` running ahead of an untouched PR reads as stale.
`scripts/test-release-publication.cjs` additionally replays a push run against
the fail-closed mock: it publishes `v0.2.0` once, swaps the labels, publishes
nothing on a rerun and has no pull-request write to reach. It covers
native-output-free recovery, failures before
PATCH, failed push, failed PATCH after push, one-sided migration, unchanged-main
no-op, new-main regeneration, Update-branch stale regeneration, overflow
stored-notes reconciliation (including failed notes-restore and dangling-link
fail-closed), stale-notes-branch ignore and foreign-head rejection. It never
uses a token, contacts GitHub or accesses a database, and runs in Worker CI.

## Required-check dispatch

`GITHUB_TOKEN`-created PRs do not trigger ordinary PR workflows. The release
workflow, on `schedule` and `workflow_dispatch` only, dispatches `check.yml` and `supply-chain.yml` (the folded `pr-lint` + `gitleaks` gate) on the
release PR branch. `GH_REPO` explicitly names the repository because that job
has no checkout. PR lint reads the open release PR's title and body via the
API, verifies that its open same-repository head SHA and branch match the
dispatched run and that its base is main, then applies the same rules as an
ordinary PR event. Metadata uses collision-checked random multiline delimiters
so arbitrary descriptions cannot break or replace workflow outputs.

No App key or PAT is added to Actions. A reviewer must approve the exact head
SHA and all required checks must be green before squash merge. The first
release is not complete until a release PR has passed those checks, merged,
and the automation has published its tag and GitHub Release.
