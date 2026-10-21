# Staging fixture census

The `staging-fixture-census` workflow runs the census script in
`fixture-scope` mode: a point-in-time census-at-time over the seven
fixture tables the read-only role can `SELECT`. One dispatch is one
capture for the run record it is filed under: both query groups run
back-to-back with a single observation instant and no sleep.

## Query groups

E. Per-table totals plus fixture-marker counts. Seeded rows carry the
fixed `fixture-run:` prefix in one opaque per-table marker column; the
queries match the fixed prefix only, never a per-run value, so they stay
pinned while each run names its exact marker in its own run record.
`invite_snapshots` and `retention_guild_purge_queue` have no marker
column and report totals only. Two auxiliaries ride the same rows: the
bot-assigned staging/test classification count on `community_facts`,
and the pending/completed split on `retention_deletion_requests`.

F. Anonymous guild inventory per table: the distinct-guild count plus
the sha256 of the sorted guild set and of the sorted per-guild
row-count distribution. Existence and stability without naming: guild
and member identities never print, because run logs are public. When
pgcrypto is absent the inventory reports counts without digests.

| Table | Marker column | Auxiliaries |
| --- | --- | --- |
| `events` | `idempotency_key` | — |
| `members` | `join_source` | — |
| `invite_snapshots` | none (totals only) | — |
| `community_facts` | `idempotency_key` | staging/test classification count |
| `event_store` | `dedup_key` | — |
| `retention_deletion_requests` | `request_id` | pending/completed split |
| `retention_guild_purge_queue` | none (totals only) | — |

## Census-at-time procedure

A fixture run files exactly two captures under its run record:

1. **Pre-seed**, after the seed plan is approved and before the first
seeded row lands. The capture must show zero marked rows in every
table with a marker column. The totals are the non-fixture baseline:
they prove the fixture scope held no fixture rows before seeding.
2. **Post-cleanup**, after cleanup completes. The capture must again
show zero marked rows, and its totals must agree with the pre-seed
totals table by table. Agreement proves cleanup removed what seeding
added and touched nothing else; any disagreement stops the run and is
reported with the exact residual counts.

Unmarked rows are the non-fixture baseline, never fixture evidence:
the census counts what is marked, and the two zero-marked captures
bracket the run.

## Exclusions and limits

- `member_erasure_audit` is excluded: the role matrix
(`sql/database_role_matrix.sql`) grants it to the migrator group
only, so the read-only login cannot `SELECT` it. Its fixture rows
are verified through the approved operator path, never by widening
this census. The report carries the exclusion as a permanent line.
- An empty table reports count 0 with no digests; a missing table
label is a shape error, never a zero-fill.
- The report carries counts, digests and one timestamp only. No raw
rows, member IDs, guild IDs, payloads or connection strings.

## Fences

The fixture mode reuses every fence of the retention mode
(`scripts/staging_retention_census.py`): the plan-database login on
the pinned staging host/database/branch, `SET ROLE two_bot_migrator_ro`
before each `READ ONLY` transaction, statement and lock timeouts, and
fixed-vocabulary failures that never echo psql output. The workflow
shape is pinned by `scripts/ci/test_workflows.py`
(`staging_fixture_census_errors`): dispatch-only with no inputs,
main-only, the `staging-migrate-plan` environment binding, log only
with no artifact, and the exact `fixture-scope` command.

Do not dispatch until the security review for the fixture queries is
recorded. The offline suite (`scripts/ci/test_staging_retention_census.py`)
pins the queries, the parsers, the role-matrix scope and the report
shape without touching staging.
