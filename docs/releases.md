# Release architecture

The container has one release version and one flat `vYYYY.MM.DD` digest tag.
Internal crates are not released separately. Per-promote `vX.Y.Z` tagging is
retired: no deploy creates a version tag.

## Daily digest release

A release is cut once a day by the `daily-digest` workflow, not by the
promote itself. It resolves the live production SHA (the `/readyz` build
revision, falling back to the newest recorded successful production
Deployment: only rows whose description is a deploy ID count, since Actions'
automatic `environment:` rows carry the workflow revision rather than the
deployed commit) and compares it to the last digest tag's target:

- Production already matches the last digest: quiet day, no tag, green run.
  When the last digest's release lacks SBOM assets (a lost dispatch), the run
  redispatches the idempotent SBOM chain for that tag instead of skipping, so
  a green rerun the next morning restores the inventories no later run would
  backfill.
- Production serves an older commit (a rollback): never tagged; the next
  forward deploy digests. A rollback run likewise redispatches SBOM when the
  last digest's release lacks its assets.
- Production moved forward and today's tag is missing: cut one tag
  `vYYYY.MM.DD` on the live commit plus its GitHub Release, whose notes come
  from the Conventional Commit subjects (squash-merged PR titles) since the
  previous digest. That run dispatches `release.yml` with the digest tag, as
  its own run, so the digest run never waits for the SBOM chain.
- Today's tag already exists: repair a missing Release, else reuse it. Both
  paths redispatch the idempotent SBOM chain, even on a quiet day, so rerunning
  a failed dispatch restores the inventory path. Tomorrow's digest covers
  any later production move. Never a second tag for one day.
- A cut or repair is owed but fails before publication: the script records
  `prod_changed=true` and `released=false` before note rendering/publishing.
  The `alert` job consumes those outputs even on failure and posts one Discord
  ops-channel alert through the synthetic-alert line. Success overwrites
  `released=true`; quiet days, rollbacks, dry runs and SBOM-only failures do
  not raise a missing-digest alert. Failures before the revision/digest state
  can be established remain red without claiming a production change.

`sbom-target`, `release-sbom` and `attach-sbom` build, scan and attach the
SBOM assets to the digest release (`docs/supply-chain.md`).

There is no release PR, so there is nothing to regenerate, re-check, review or
freeze `main` for. The version and notes come from commits that already passed
CI and review on `main`.

## Versions and notes

- The version is the UTC calendar date of the digest. Note sections are read
  from `release-please-config.json` (`changelog-sections`); bump rules and
  `release-as` are retired with per-promote tagging.
- Every digest of new commits gets a tag, even one with only hidden types
  (`chore`, `ci`, ...); its notes say there are no user-facing changes.
- The first digest ranges its notes from the latest reachable `vX.Y.Z` tag, so
  the pre-digest history stays linked; later digests range from the previous
  digest.
- Only the newest digest date is marked "Latest". Backdated cuts and older
  repairs pass `--latest=false`, preserving the newer release's Latest status.
- `DIGEST_DATE`, when supplied to the script for an offline/manual repair,
  must be a real `YYYY-MM-DD` calendar date; nonexistent dates are refused
  before any git or GitHub operation.
- Notes use only commit subjects (plus `BREAKING CHANGE:` footers), so a PR body
  can no longer drop a note. The `pr-lint` commit-parse guard
  (`.github/scripts/commit-parse-guard.cjs`, `.github/release-parse-exceptions.txt`)
  still keeps every squash commit parseable as a Conventional Commit.
- The git log streams through a 32 MiB buffer, not Node's 1 MiB default,
  which crashed with ENOBUFS on a 200-commit range.
- Cargo package versions stay at `0.4.0`; the tag is the release version.
  `CHANGELOG.md` keeps the history up to `v0.4.0`; later notes live on the
  GitHub Releases page.

## Repair and dry run

```sh
# Reconcile today's digest (repair a missing Release and redispatch SBOM).
gh workflow run daily-digest.yml --ref main
# Preview the digest notes without tagging, publishing or alerting.
gh workflow run daily-digest.yml --ref main -f dry_run=true
# Repair SBOM assets on a digest release (or a pre-digest vX.Y.Z release).
gh workflow run release.yml --ref main -f release_tag=vYYYY.MM.DD
# SBOM dry run (no publication).
gh workflow run release.yml --ref main -f dry_run=true
```

A dispatch with every input at its default selects no release path, so the
`verdict` job fails the run and names the skipped jobs and why instead of
reading green with nothing published.

## Offline verification

`scripts/test-daily-digest.cjs` (worker CI) pins the parsing, the digest tag
shape and the note rendering. It also dry-runs the script on a throwaway git
repository: a fresh cut, the quiet-day skip, the rollback guard, the
already-cut today skip, semver continuity, the off-main refusal and invalid
inputs. Its ENOBUFS regression builds a 200-commit range above 1 MiB and proves
the digest completes. Its one non-dry-run case runs a fresh cut against a
mocked `gh` and pins the single `release create` call's target and its
`--latest` flag. `scripts/test_daily_digest_execution.py` drives a failed
publication through the workflow alert expression and a stubbed Discord
poster, verifies quiet/dry-run/cancel suppression, executes SBOM redispatch
for an existing release and for next-day quiet/rollback runs whose last
digest lacks its assets (complete assets and lookup failures still skip),
and checks older repairs, backdated cuts and invalid calendar inputs. `scripts/test_digest_resolve.py` executes the extracted
`scripts/digest_resolve.py` with offline readyz/Deployment fixtures, including
automatic rows before recorded rows, failure statuses, malformed bodies, HTTPS
refusal, 503 revisions and fail-closed behavior. Both run in worker CI.
`scripts/test_digest_alert.py` pins alert content/refusals;
`scripts/ci/test_workflows.py` pins the workflow wiring, schedule, alert gate
and the `contents: write` grant on the tagging job.

The root `src/lib.rs` release-metadata library was added for release-please's
Rust strategy, which needed a root `[package]` version. It stays as is; nothing
reads it for releases any more.
