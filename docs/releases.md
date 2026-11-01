# Release architecture

The container has one release version and one stable CalVer digest tag a day,
`vYYYY.MDD.P` (the upstream Paperclip syntax). Internal crates are not
released separately. Deploys never create a version tag.

## Daily digest release

A release is cut once a day by the `daily-digest` workflow, not by the
promote itself. It resolves the live production SHA (the `/readyz` build
revision, falling back to the newest recorded successful production
Deployment: only rows whose description is a deploy ID count, since Actions'
automatic `environment:` rows carry the workflow revision rather than the
deployed commit). The shared CalVer releaser in `TogetherWeOwn/.github`
(`actions/calver`, `live-digest` policy) then plans the release. The plan and
publish steps pin the action to one full commit SHA. The bot adapter
(`.github/scripts/daily-digest.cjs`) wraps both steps:

- **Quiet day** (production matches the newest stable tag), **rollback**
  (production is older than, or diverged from, the newest stable tag) and
  **already published today**: the planner returns a classified green skip
  and nothing is tagged. The planner recovers a SHA's earlier plan before
  those checks, so production still on (or rolled back to) a CalVer digest
  comes back as that digest's plan. When its Release exists the adapter
  treats the day as a skip too and never re-publishes it.
- **SBOM reconciliation:** every non-dry run checks the Latest release (and a
  replayed or repaired digest) for its SBOM assets and redispatches the
  idempotent SBOM chain once per tag that lacks them, so a green rerun the
  next morning restores the inventories no later run would backfill. A
  lookup failure stays green without dispatching.
- **Stalled production:** on a quiet day where the Latest release is more
  than three days old and `main` is ahead of the live SHA, the run writes a
  warning annotation and a step summary naming the live SHA, the tag and the
  commit gap. The run stays green. From 2026-10-23 to 2026-10-31 production
  did not move because every automatic `deploy-production` run skipped at its
  guard while still reporting success. The digest correctly found nothing to
  tag, but nothing on its run page said so.
- **Production moved forward:** the shared releaser tags `vYYYY.MDD.P` on the
  live commit and publishes its GitHub Release, whose notes come from the
  Conventional Commit subjects (squash-merged PR titles) since the previous
  stable tag. The adapter then dispatches `release.yml` with the tag, as its
  own run, so the digest run never waits for the SBOM chain.
- **Retry of the same build:** the plan operation is `daily-digest/<sha>`, so a
  retry for the same live SHA recovers its frozen plan (same version, even
  across midnight) instead of allocating a new one. A tag whose Release
  creation failed is finished by the retry (`repair: true`).
- **Unfinished tag, production moved on:** when today's CalVer tag exists
  without its Release and production has since moved, the planner skips the
  new SHA as already published. The adapter then reads today's tag
  annotation and finishes that tag from its own frozen plan with
  `repair: true`. It never retargets the tag or cuts a second one.
- **Publication fails:** the adapter records `prod_changed=true` and
  `released=false` before the publish step. The `alert` job consumes those
  outputs even on failure and opens (or comments on) the one `daily-digest`
  GitHub issue labelled `incident`; the agent who cuts the missing tag closes
  it. Success overwrites `released=true`. Quiet days, rollbacks, dry runs and
  SBOM-only failures do not raise a missing-digest alert. Failures before the
  live SHA or plan are established remain red without claiming a production
  change.

`sbom-target`, `release-sbom` and `attach-sbom` build, scan and attach the
SBOM assets to the digest release (`docs/supply-chain.md`).

There is no release PR, so there is nothing to regenerate, re-check, review or
freeze `main` for. The version and notes come from commits that already passed
CI and review on `main`.

## Versions and notes

- The version is `YYYY.MDD.P` from the UTC date the plan was made. The month
  is not zero-padded and the day is two digits: 1 November 2026 is
  `2026.1101.0`, 5 January 2027 is `2027.105.0`. `P` comes from tag history at
  publish time. The digest cuts at most one tag a day, so `P` stays 0.
- Legacy `vYYYY.MM.DD` digest tags (last: `v2026.10.23`) and the older `vX.Y.Z`
  tags stay untouched. The releaser reads a legacy daily tag as its calendar
  date, so it never counts towards `P` and every CalVer version sorts higher:
  `2026.1101.0` > `2026.10.23` because minor `1101` > `10`. The first CalVer
  digest ranges its notes from `v2026.10.23`.
- Every digest of new commits gets a tag, even one with only hidden types
  (`chore`, `ci`, ...); its notes say there are no user-facing changes.
- A new digest becomes "Latest" only when no numerically later stable tag
  exists; repairs never move Latest backwards.
- Notes use the shared releaser's sections: Features (`feat`), Bug Fixes
  (`fix`), Performance Improvements (`perf`), Reverts (`revert`) and
  Documentation (`docs`), plus `BREAKING CHANGE` footers. The releaser does
  not yet read `release-please-config.json` (`changelog-sections`), so
  `security` commits no longer appear in the notes.
- Notes use only commit subjects (plus `BREAKING CHANGE:` footers), so a PR body
  can no longer drop a note. The `pr-lint` commit-parse guard
  (`.github/scripts/commit-parse-guard.cjs`, `.github/release-parse-exceptions.txt`)
  still keeps every squash commit parseable as a Conventional Commit.
- Cargo package versions stay at `0.4.0`; the tag is the release version.
  `CHANGELOG.md` keeps the history up to `v0.4.0`; later notes live on the
  GitHub Releases page.

## Repair and dry run

```sh
# Reconcile today's digest (finish a missing Release, redispatch missing SBOM).
gh workflow run daily-digest.yml --ref main
# Plan and validate the publication with zero writes; never alerts.
gh workflow run daily-digest.yml --ref main -f dry_run=true
# Repair SBOM assets on a digest release (vYYYY.MDD.P, or a legacy tag).
gh workflow run release.yml --ref main -f release_tag=vYYYY.MDD.P
# SBOM dry run (no publication).
gh workflow run release.yml --ref main -f dry_run=true
```

A dispatch with every input at its default selects no release path, so the
`verdict` job fails the run and names the skipped jobs and why instead of
reading green with nothing published.

To move to a newer shared releaser, repin both `actions/calver` steps in
`daily-digest.yml` to the same merged `TogetherWeOwn/.github` commit;
`scripts/ci/test_workflows.py` refuses mismatched or non-SHA pins.

## Offline verification

`scripts/test-daily-digest.cjs` (worker CI) runs each adapter mode against a
fixture `gh`: the SHA-bound plan request, the publish request (exact plan,
`repair`, dry-run flag), every classified skip, completed replays (no
re-publish, both Latest and the replayed tag reconciled), the
unfinished-today repair from the tag annotation and its fall-through cases,
SBOM redispatch for missing assets (complete assets and lookup failures stay
green), the stalled-production warning and its silent cases, the record step
for fresh, replayed and dry-run publications, and invalid inputs. `scripts/test_daily_digest_execution.py`
runs the adapter through the workflow's own step bindings and expressions: a
failed publication reaches the alert gate and the incident issue, a fresh
release dispatches `release.yml`, owed tags dispatch once each, skips never
alert, and dry runs publish nothing. `scripts/test_digest_resolve.py` executes
`scripts/digest_resolve.py` with offline readyz/Deployment fixtures, including
automatic rows before recorded rows, failure statuses, malformed bodies, HTTPS
refusal, 503 revisions and fail-closed behavior. `scripts/test_digest_alert.py`
pins alert content/refusals; `scripts/ci/test_workflows.py` pins the workflow
wiring, schedule, alert gate, the shared action pins and the `contents: write`
grant on the tagging job. The shared releaser's own allocation, ordering and
publication tests live in `TogetherWeOwn/.github` (`ci/calver*.test.mjs`).

The root `src/lib.rs` release-metadata library was added for release-please's
Rust strategy, which needed a root `[package]` version. It stays as is; nothing
reads it for releases any more.
