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
`self_role_exchange_baselines`, `web_contract_meta`), or `missing` (table absent
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

## Reviewed disposition: unmeasurable voice tables (voice window)

`rollback-delta` marks a table `unmeasurable` when it has no usable
timestamp column (`crates/cutover/src/rollback_delta.rs`, `TABLE_SPECS`).
Thirteen of those specs are voice-prefixed. The rollback trigger document
fires its watermark-gap trigger on any `unmeasurable` table without a
reviewed disposition; this section is that disposition for the voice
window, recorded 2026-10-04 from an offline tabletop replay of the trigger
document against merged source (verdict on the day: no data, no reviewed
disposition on record). Non-voice unmeasurable tables keep their existing
handling.

| Tables | Why unmeasurable | Disposition | Alternate evidence for the window |
|---|---|---|---|
| `voice_channel_templates`, `voice_game_aliases`, `voice_random_lists`, `voice_random_list_choices`, `voice_logging`, `voice_logging_mention_members`, `voice_logging_mention_roles`, `voice_guild_settings`, `voice_command_roles`, `voice_command_role_members` | No timestamp column; replaced wholesale by `PgVoiceConfigStore::apply` (migration `0229_voice_config.sql`) | Accept the reason. `apply` rewrites every section for a guild in one advisory-locked transaction (`crates/cutover/src/voice_config_store.rs`), so a window write is all-or-nothing per guild and per-row timestamps would add no signal | Freeze-time vs rollback-time configuration export pair: `snapshot` the guild (`PgVoiceConfigStore::snapshot`, the same read the `/export` command serves in `crates/bot/src/voice_rooms.rs`) and serialize with `export_configuration` (`crates/core/src/voice_config.rs`; it refuses with `ExportTooLarge` a file over the 256 KiB import cap, so for such a guild diff the two `snapshot` values directly). Diff the two exports: any difference is the window's config delta, an empty diff proves no config write |
| `voice_creators` | No timestamp column; upserted in place (`PgRoomStore::add_creator`, `write_creators` in `apply`) | Accept with the evidence alongside. The creators section rides the same export pair, except the V9b text-channel name and viewer-role columns, which the configuration does not carry (see the codec-gap note in `voice_config_store.rs`) | (1) The export pair above for all carried columns. (2) Per-guild `voice_creators` full-row dump comparison (freeze vs rollback) plus a row-count check, covering the non-carried columns |
| `voice_logging_settings`, `voice_access_controls` | Mutable per-guild settings rows with no timestamp column (migrations `0228_voice_logging_settings.sql`, `0227_voice_access_controls.sql`); defaults apply when absent | Accept the reason. Each table holds at most one row per guild, replaced atomically by a single upsert (`save_logging_settings`, `save_access_controls` in `crates/cutover/src/voice_rooms.rs`) | Read-back pair at freeze vs rollback through the store readers (`logging_settings`, `access_controls`) or a single-row `SELECT`; any difference is the window's settings delta |

### Replay rule for the voice window

When a `rollback-delta --since T_f` report's only `unmeasurable` entries
are the thirteen tables above, the watermark-gap trigger does not fire for
lack of disposition: each table is covered either by an accepted reason or
by the named alternate evidence, which the rollback operator attaches to
the window record. The trigger still fires on an export-cap refusal (any
table over the 50,000-row cap) or an incomplete journal capture, exactly
as the trigger document states. The measured voice tables (`voice_rooms`
via `created_at`/`owner_touched_at`/`name_touched_at`/`privacy_touched_at`,
`voice_create_reservations` via `created_at`/`settled_at`,
`voice_room_blocks` via `created_at`, `voice_join_grants` via `created_at`,
`voice_text_companions` via
`created_at`, and the append-only `voice_vote_kick_audit` via `occurred_at`) need no
disposition: their counts are the evidence. A reservation accepted before the baseline
but bound or rolled back after it is included in both count and export. A block removed
after the freeze deletes its row, so `voice_room_blocks` measures additions only; a grant
revoked or refused after the freeze deletes its row, so `voice_join_grants` measures
approvals only.

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
