# Next-window delta report for rollback decisions

`rollback-delta` measures the full captured Next delta from the `T_f`
baseline, read-only. It answers the rollback question in `docs/cutover.md`
(§Rollback): how many rows were inserted or updated after `T_f`, per
bot-owned table. It never copies rows forward and never imports anything
back: a reverse import stays out of scope. Code:
`crates/cutover/src/rollback_delta.rs`, CLI:
`crates/cutover/src/bin/rollback_delta.rs`.

## Invocation

```sh
# TWO_DATABASE_URL must be supplied through an authorized binding.
# On the controller, compilation/testing always uses the bounded-cache wrapper.
python3 scripts/cargo_cache.py run -- build -p two-bot-cutover --bin rollback-delta
# Run the built executable from the admitted slot identified by the wrapper:
<admitted-slot>/target/debug/rollback-delta \
  --since 2026-09-30T12:00:00Z \
  > <run-scratch>/rollback-delta.json
# With an NDJSON export of every post-T_f row (one {"table", "row"} per line):
<admitted-slot>/target/debug/rollback-delta \
  --since 2026-09-30T12:00:00Z --export <run-scratch>/rollback-rows.ndjson \
  > <run-scratch>/rollback-delta.json
```

`--since` is required, RFC3339, and must not be in the future; a missing,
malformed, or future value exits 2 with no database connection attempt.
`TWO_DATABASE_URL` is required; no migrations are applied. Exit **0** means a
complete summary was written; **2** means refused arguments or an export-cap
refusal; **1** means the database could not be opened. Database errors and
URLs are not printed.

## What the report proves

Each table reports `{table, status, columns?, count?, reason?}`. `status` is
`measured` (exact post-`T_f` row count), `unmeasurable` (no usable timestamp
column, with a reason: `community_scorecard_attempts`,
`guild_settings_revision`, `level_role_rewards`,
`lfg_roles`, `moderation_channel_executions`, `rank_ladder`,
`web_contract_meta`), or `missing` (table absent
from this database, e.g. a partially migrated one). Every classified table
appears exactly once: the unit test fails on any unclassified
`sql/database_role_matrix.sql` / backup-allowlist table, and on any spec
entry that appears in neither list.

- All reads run inside one `REPEATABLE READ, READ ONLY` transaction (see
  `legacy_verify::read_only_transaction`), so the per-table counts share a
  single snapshot. The transaction explicitly rolls back; errors also roll
  back. A write inside the snapshot fails with SQLSTATE 25006.
- Recency uses each table's timestamp column(s): a plain comparison for one
  column, `GREATEST` over null-tolerant casts for several (so projection
  tables such as `members` catch updates to any column). Every predicate
  casts to `timestamptz`: a no-op on real timestamps, and a parse of the
  canonical ISO-8601 UTC text in the legacy-shaped TEXT columns (e.g.
  `tickets.created_at`).
- The NDJSON export refuses rather than truncates when any table exceeds
  50,000 post-`T_f` rows, so a mis-set `--since` cannot silently spill a
  partial window into a rollback decision.

Read access does **not** authorize execution against real databases for
tests. Acceptance uses only the disposable test service below. Operational
rollback execution requires its own authorized endpoint/credential context.

## Acceptance

Unit coverage (no database): `--since` validation, matrix/allowlist
classification, predicate shape. Real-SQL acceptance:
`crates/cutover/tests/rollback_delta_db.rs` seeds rows before and after `T_f`
across `events`, `members`, `member_levels`, `tickets`, and
`guild_settings`, asserts exact counts, unmeasurable/missing reporting, the
25006 read-only proof, the export row total, and the usage-error exits:

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --test rollback_delta_db
```
