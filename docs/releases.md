# Release architecture

The container has one release version and one flat `vX.Y.Z` tag. Internal
crates are not released separately.

## Release on production promote

A release is cut by the production promote itself. When `deploy-production`
deploys (mode `deploy`, not `rollback`) and its readiness gate passes, its
`release` job dispatches `.github/workflows/release.yml` with the exact guarded
SHA. That is a separate run, so the deploy run and its `deploy-production`
concurrency group (the single production rollback path) end at the readiness
gate and never wait for the tag + SBOM chain:

1. The `tag` job runs `.github/scripts/release-on-promote.cjs`. It refuses a
   SHA that is not on `origin/main` and finds the previous `vX.Y.Z` tag
   reachable from the SHA. From the Conventional Commit subjects
   (squash-merged PR titles) since that tag, it computes the next version and
   renders the notes. `gh release create --target <sha>` then creates the tag
   and the GitHub Release.
2. `sbom-target`, `release-sbom` and `attach-sbom` build, scan and attach the
   SBOM assets to that release, exactly as before (`docs/supply-chain.md`).

There is no release PR, so there is nothing to regenerate, re-check, review or
freeze `main` for. The version and notes come from commits that already passed
CI and review on `main`. The release-please release PR this replaces went stale
on every merge and needed a freeze, dispatched required checks and a separate
review to land.

## Versions and notes

- Bump rules and note sections are read from `release-please-config.json`
  (`bump-minor-pre-major`, `bump-patch-for-minor-pre-major`,
  `changelog-sections`), so versions continue the existing tag line.
- `release-as` in `release-please-config.json` forces the next version while it
  is above the previous tag. It is `1.0.0`: production cutover happened on
  2026-10-10 on the 0.4.0 line, so the next production promote cuts `v1.0.0`.
  Once `v1.0.0` exists it is ignored and normal bumps resume (remove it in any
  later PR).
- Before `1.0.0`, `feat!` / `BREAKING CHANGE` and `feat` bump the minor version;
  everything else bumps the patch version. From `1.0.0` onward, breaking bumps
  major, `feat` bumps minor, anything else bumps patch. Production cutover stays
  the deliberate `1.0.0` boundary.
- Every promote of new commits gets a tag, even one with only hidden types
  (`chore`, `ci`, ...); its notes say there are no user-facing changes.
- Promoting a commit that already has a tag reuses that tag and only repairs a
  missing GitHub Release. Promoting a commit older than the newest release
  creates no tag.
- Notes use only commit subjects (plus `BREAKING CHANGE:` footers), so a PR body
  can no longer drop a note. The `pr-lint` commit-parse guard
  (`.github/scripts/commit-parse-guard.cjs`, `.github/release-parse-exceptions.txt`)
  still keeps every squash commit parseable as a Conventional Commit.
- Cargo package versions stay at `0.4.0`; the tag is the release version.
  `CHANGELOG.md` keeps the history up to `v0.4.0`; later notes live on the
  GitHub Releases page.

## Repair and dry run

```sh
# A promote deployed but its release job failed: tag and publish that SHA (idempotent).
gh workflow run release.yml --ref main -f sha=<deployed 40-hex SHA>
# Repair SBOM assets on an existing release (skips tagging).
gh workflow run release.yml --ref main -f release_tag=vX.Y.Z
# SBOM dry run (no tag, no publication).
gh workflow run release.yml --ref main -f dry_run=true
```

## Offline verification

`scripts/test-release-on-promote.cjs` (worker CI) pins the parsing, the bump
rules and the note rendering. It also dry-runs the script on a throwaway git
repository: the version, the rollback guard, `release-as`, the off-main refusal and an
invalid SHA. `scripts/ci/test_workflows.py` and `scripts/test-deploy-production.py` pin
the workflow shapes and the `contents: write` grant on the release jobs.

The root `src/lib.rs` release-metadata library was added for release-please's
Rust strategy, which needed a root `[package]` version. It stays as is; nothing
reads it for releases any more.
