# Backups, restore drills, and sealed guild-config snapshots (TOG-9881)

Rust port of the legacy `two-bot` cutover-data surface (frozen source
`two-bot` `main @ d5d11793`): v16 dump format with v3/v4/v5/v6/v7/v8/v9/v10/v11/v12/v13/v14/v15 read compatibility,
SigV4 off-box upload, retention
math, sealed guild-config snapshot + restore planner, and the three timer
equivalents. Code: `crates/core/src/backup/`, CLI: `two-bot backup |
restore | backup-upload | guild-config-snapshot | guild-config-restore`
(`crates/bot/src/backup_cli.rs`). Units: `deploy/two-bot-next-*`.

**Never production services or tokens.** Dumps, restores and drills run
against scratch Postgres only (tests: agent-testdb `two_next_backup_test` /
`two_next_restore_drill`). The restore target is a different variable
(`TWO_RESTORE_URL`, never `TWO_DATABASE_URL`) and real restores additionally
require `--force`. This confirmation is not authorization to target production.
`restore` enforces the rule by value: a target that normalizes to the live
database (scheme alias, host case, ports defaulted from `PGPORT` exactly as
the driver seeds them, and credential/query spellings folded away;
`?host=`/`?hostaddr=`/`?dbname=`/`?port=` overrides honored; database
derived exactly as the driver derives it, so `//prod` names `prod`;
socket spellings — encoded authority, `?host=`, redundant separators, `.`
segments, trailing slash — folded component-wise),
or whose database name is a production marker (`prod`, `production`,
`two_prod`, `two_production`, `live`, `two_live`), is refused with a fixed
message on stderr and exit 2 — even with `--force`, and without echoing
either URL. A target whose effective target cannot be proven — omitted
database, an empty `?host=`/`?hostaddr=`/`?dbname=` override, or a socket
directory mixed with a plain host (the driver's socket slot is sticky) —
is refused the same way: name the scratch database explicitly.
`--dry-run` still previews without that comparison.

## Stable failure codes

Every `backup`, `restore`, `backup-upload`, `guild-config-snapshot` and
`guild-config-restore` failure line
ends with a stable `code=<snake>` token so agents can match failure classes
without substring-matching English. The human-readable text is unchanged;
the code is additive. Success lines (`backup: done`, `RESTORE VERIFIED`,
`DRY RUN VERIFIED`, `guild-config-snapshot: stored`,
`backup-upload-s3: stored`, `snapshot and drift report uploaded`) carry no
code and are byte-identical.

`two-bot backup`:

- `backup_usage` — unexpected extra argument (usage refusal, exit 2).
- `backup_missing_database_url` — `TWO_DATABASE_URL` unset or not Postgres.
- `backup_retention_refused` — `TWO_BACKUP_KEEP` would prune every backup.
- `backup_dest_create_failed` — cannot create the destination directory.
- `backup_pool_open_failed` — cannot open the database pool (redacted).
- `backup_stat_failed` — dump written but cannot be statted.
- `backup_empty_event_log` — zero `events` rows; refusing a hollow backup.
- `backup_dump_failed` — the dump itself failed.
- `backup_prune_failed` — retention pruning failed.

`two-bot backup-upload <dump>`:

- `backup_upload_usage` — expected exactly one argument.
- `backup_upload_not_regular_file` — the path is not a regular file.
- `backup_upload_empty_file` — refusing to upload an empty file.
- `backup_upload_dump_too_large` — the dump exceeds the 1 GiB cap.
- `backup_upload_s3_target` — S3 target/env misconfigured.
- `backup_upload_read_failed` — cannot read the dump file.
- `backup_upload_put_status` — PUT returned a non-2xx status.
- `backup_upload_put_failed` — the PUT request failed.

`two-bot restore <dump> --force`:

- `restore_live_target_refused` — `TWO_RESTORE_URL` normalizes to the live
  database, names a production database, or cannot be proven different
  (omitted database, empty overrides, socket/host mixes); fixed refusal,
  exit 2, even with `--force`. `--dry-run` still previews.

`two-bot guild-config-snapshot`:

- `snapshot_usage` — unexpected extra argument (usage refusal, exit 2).
- `snapshot_token_load_failed` — cannot load the staging token.
- `snapshot_token_check_refused` — the token failed the staging check.
- `snapshot_guild_id_failed` — `DISCORD_STAGING_GUILD_ID` missing/wrong.
- `snapshot_governed_api_failed` — cannot build the governed API client.
- `snapshot_identity_failed` — API identity assertion failed.
- `snapshot_capture_failed` — guild capture failed.
- `snapshot_atomic_write_failed` — atomic snapshot/drift write failed.
- `snapshot_reread_failed` — cannot re-read the written snapshot.
- `snapshot_hash_changed` — snapshot hash changed after the write.
- `snapshot_seal_missing` — written snapshot lacks its integrity seal.
- `snapshot_local_only_refused` — no upload command; local-only refused.

`two-bot guild-config-restore --apply`:

- `guild_restore_apply_failed` — a mid-apply failure; the report names
  landed vs pending operations (labels only). Exit 1. Completion prints
  `DID <op>` per operation only as each operation lands.

Bounded upload helper (`backup` and `guild-config-snapshot`: external
upload command; argv and child output never echoed):

- `upload_exit_nonzero` — helper exited nonzero.
- `upload_spawn_failed` — helper failed to start.
- `upload_wait_failed` — waiting on the helper failed.
- `upload_timed_out` — helper exceeded its 30-minute budget; terminated.

## The format

Gzipped NDJSON, one object per line: `manifest` / `row` / `end`. New dumps
write **v16** (including durable voice-create reservations, the retention
queues, the seasonal XP projection, the event store: voice capture plus
presence capture plus member-capture deltas plus message/reaction capture
plus thread lifecycle plus stage/interaction-observe/audit-log capture plus
invite/scheduled-event capture, the stream go-live ledger, guild game-activity
aggregates, the assistant monthly-cap ledger, the scheduled-event
epoch-claim guard, the scheduled-event delete tombstones and the admin API
token registry, event voice pair, three starboard tables, three giveaway tables and retained voice-orphan evidence); the reader also accepts
frozen **v3**, pre-reservation **v4**, pre-v6-tables **v5**, pre-seasonal **v6**,
pre-stream-ledger **v7**, pre-guild-counters **v8**, pre-epoch-claim **v9**,
pre-tombstone **v10**, pre-registry **v11**, pre-event-voice **v12** and
pre-starboard **v13**, pre-giveaways **v14** and pre-orphan-ledger **v15**.
Other versions refuse.
The manifest carries per-table `{name, columns, column_types, count}` taken
inside one `REPEATABLE READ` transaction, the `events` high-water mark, the
source's applied migrations and `sequenceMarks`: for every serial, identity and
standalone settings allocator, `{table, column, lastValue, isCalled, increment}`
read from the sequence itself. Rows cannot show values that were handed out
and then deleted; the marks can. Sequences are not MVCC, so a mark is the
position when the dump read it, at or beyond every value in the snapshot. The
dump role needs `SELECT` on those sequences. Values are stored in Postgres text-output
form with a `$n::type` cast on restore — faithful for every owned type
without per-type decoding. `column_types` is additive to the legacy envelope.
Port-written row cells are strings/nulls. Frozen legacy v3 dumps have no
`column_types` and carry native numbers/booleans; these decode to bound
PostgreSQL input using the target's type, never silently to NULL. JSON-looking
TEXT stays unchanged, and native JSON cells serialize for JSON/JSONB targets.
Unsupported composite cells refuse; see `crates/core/tests/fixtures/README.md` for scope
and actual frozen-writer fixture provenance.

Input is streamed with separate caps: 1 GiB compressed **and decoded**,
8 MiB per decoded line/cell envelope, and 256 MiB of conservatively accounted
retained data (keys plus per-value overhead). Highly compressible input is
refused before unbounded line-buffer growth, not just by compressed metadata.
These budgets also define the writer's supported output: a dump that the same
build cannot inspect is not eligible for successful publication, retention or
upload. Complete output is validated under a unique non-backup temporary name,
flushed/fsynced, then atomically renamed without replacing an existing archive;
the parent directory is fsynced before reporting success. Publication requires
Linux/filesystem support for `renameat2(RENAME_NOREPLACE)` and fails closed if
unsupported. A failed write or validation leaves existing published recovery
points untouched; a crash may leave a temporary file, but retention and drill
selectors ignore it.

### Coverage and recovery semantics (v4, v5, v6, v7, v8, v9, v10, v11, v12, v13, v14, v15 and v16)

`DUMP_TABLES` in `crates/core/src/backup/dump_file.rs` is the single ordered
inventory for both dump and restore. It includes **108 durable tables from the
current cutover migrations**, including the bot-owned website-contract backing
tables, the member-moderation ledger, the self-role send receipts
(`self_role_exchanges`) plus their uncertainty baselines
(`self_role_exchange_baselines`), the automod delivery claims, the gateway
boot directives, the member-erasure audit, the internal clock high-water
mark, the voice configuration tables, the accepted voice-room create history
(`voice_create_reservations`), the owner-grant journal (`voice_owner_grants`,
pending and completed recipient provenance, restored after `voice_rooms`),
the retention queues
(`retention_guild_purge_queue`, `retention_deletion_requests`), the seasonal
XP projection (`member_season_xp`), the event store (voice capture plus
presence capture plus member-capture deltas plus message/reaction capture
plus thread lifecycle plus stage/interaction-observe/audit-log capture plus
invite/scheduled-event capture)
(`event_store`: 0427 voice, 0428 presence, 0429 thread lifecycle, 0430
message/reaction capture, 0431 stage/interaction-observe/audit-log capture
plus 0432 member-capture deltas plus 0439 invite/scheduled-event capture)
the stream go-live ledger (`stream_live_claims`: 0440/0443), guild game-activity
aggregates (`game_activity_guild_aggregates`: 0441/0442), the assistant
monthly-cap ledger (`assistant_monthly_builds`: 0446/0447), the
scheduled-event epoch-claim guard (`scheduled_events_claim`: 0450/0451) and
its delete tombstones (`scheduled_events_tombstone`: 0452/0453) and the
admin API token registry (`admin_api_tokens`: 0456/0457, named-holder and
90-day lifetime repair 0464/0463), the event voice pair
(`event_voice_channels`, `event_voice_presence`: 0455), plus starboard
configuration, distinct starrers and post state (`starboard_config`,
`starboard_starrers`, `starboard_posts`: 0458/0459), plus giveaway definitions,
entries and draw rounds (`giveaways`, `giveaway_entries`, `giveaway_draws`: 0460/0461),
and retained creator-compensation evidence (`voice_orphan_ledger`: 0469).
The [registry contract](admin-registry.md) adds no table or archive version:
new v16 dumps discover `holder_id` / `holder_kind` dynamically. Populated
pre-holder archives refuse transactionally on the required authority columns
instead of inventing holders; older readable archives that omit the table
retain their versioned clearing behavior.
`OPTIONAL_LEGACY_TABLES` is empty: the coverage
test fails if a covered table is not migrated. A dump from a fresh Rust schema
has 108 table entries. A missing current table refuses a dump/restore: migrate
the target first.
Each readable version requires its historical inventory and waives only
versioned additions introduced later:

| Version | Tables it may omit |
| --- | --- |
| v4 | `V5_ADDED_TABLES` (reservations and privacy controls), `V6_ADDED_TABLES` (event store), `V7_ADDED_TABLES` (seasonal XP and retention queues), `V8_ADDED_TABLES` (stream ledger), `V9_ADDED_TABLES` (guild counters), `V10_ADDED_TABLES` (epoch-claim guard), `V11_ADDED_TABLES` (delete tombstones), `V12_ADDED_TABLES` (token registry), `V13_ADDED_TABLES` (event voice pair), `V14_ADDED_TABLES` (starboard tables), `V15_ADDED_TABLES` (giveaway tables), `V16_ADDED_TABLES` (orphan evidence) |
| v5 | `V6_ADDED_TABLES`, `V7_ADDED_TABLES`, `V8_ADDED_TABLES`, `V9_ADDED_TABLES`, `V10_ADDED_TABLES`, `V11_ADDED_TABLES`, `V12_ADDED_TABLES`, `V13_ADDED_TABLES`, `V14_ADDED_TABLES`, `V15_ADDED_TABLES` (giveaway tables), `V16_ADDED_TABLES` (orphan evidence) |
| v6 | `V7_ADDED_TABLES`, `V8_ADDED_TABLES`, `V9_ADDED_TABLES`, `V10_ADDED_TABLES`, `V11_ADDED_TABLES`, `V12_ADDED_TABLES`, `V13_ADDED_TABLES`, `V14_ADDED_TABLES`, `V15_ADDED_TABLES` (giveaway tables), `V16_ADDED_TABLES` (orphan evidence) |
| v7 | `V8_ADDED_TABLES`, `V9_ADDED_TABLES`, `V10_ADDED_TABLES`, `V11_ADDED_TABLES`, `V12_ADDED_TABLES`, `V13_ADDED_TABLES`, `V14_ADDED_TABLES`, `V15_ADDED_TABLES` (giveaway tables), `V16_ADDED_TABLES` (orphan evidence) |
| v8 | `V9_ADDED_TABLES`, `V10_ADDED_TABLES`, `V11_ADDED_TABLES`, `V12_ADDED_TABLES`, `V13_ADDED_TABLES`, `V14_ADDED_TABLES`, `V15_ADDED_TABLES` (giveaway tables), `V16_ADDED_TABLES` (orphan evidence) |
| v9 | `V10_ADDED_TABLES` (epoch-claim guard), `V11_ADDED_TABLES` (delete tombstones), `V12_ADDED_TABLES` (token registry), `V13_ADDED_TABLES` (event voice pair), `V14_ADDED_TABLES` (starboard tables), `V15_ADDED_TABLES` (giveaway tables), `V16_ADDED_TABLES` (orphan evidence) |
| v10 | `V11_ADDED_TABLES` (delete tombstones), `V12_ADDED_TABLES` (token registry), `V13_ADDED_TABLES` (event voice pair), `V14_ADDED_TABLES` (starboard tables), `V15_ADDED_TABLES` (giveaway tables), `V16_ADDED_TABLES` (orphan evidence) |
| v11 | `V12_ADDED_TABLES` (token registry), `V13_ADDED_TABLES` (event voice pair), `V14_ADDED_TABLES` (starboard tables), `V15_ADDED_TABLES` (giveaway tables), `V16_ADDED_TABLES` (orphan evidence) |
| v12 | `V13_ADDED_TABLES` (event voice pair), `V14_ADDED_TABLES` (starboard tables), `V15_ADDED_TABLES` (giveaway tables), `V16_ADDED_TABLES` (orphan evidence) |
| v13 | `V14_ADDED_TABLES`: `starboard_config`, `starboard_starrers`, `starboard_posts`, `V15_ADDED_TABLES` (giveaway tables), `V16_ADDED_TABLES` (orphan evidence) |
| v14 | `V15_ADDED_TABLES`: `giveaways`, `giveaway_entries`, `giveaway_draws`, `V16_ADDED_TABLES` (orphan evidence) |
| v15 | `V16_ADDED_TABLES`: `voice_orphan_ledger` |
| v16 | None: the full current inventory is required |

These are narrow waivers, not permission to omit arbitrary tables. An
incomplete new archive refuses at inspection. The frozen 85-table v4 fixture
is unchanged from before these additions; older v4–v12 archives need not
invent entries for the v9 guild counters, the v10 epoch-claim guard, the
v11 delete tombstones, the v12 token registry, the v13 event voice pair or
the v14 starboard tables, v15 giveaway tables or v16 orphan evidence.
Restoring an older archive clears
destination rows for every omitted later table instead of retaining unrelated
target state. In particular, restoring v8 without guild counters clears both
counters; it cannot recover assistant usage that predates its persistence.
Restoring v9 without the claim guard clears the guard, the tombstones and
the registry and event voice pair; the mirror re-fences on the next winning fetch. Restoring a
claim-only v10 clears the tombstones, registry and event voice pair while keeping the
epoch fence. Restoring a tombstone-only v11 clears issued tokens; they must
be re-issued, since secrets are shown once and cannot be re-derived. Use a
v12 recovery point to preserve the monthly cap, both mirror fences and the
issued tokens. Restoring v12 without the event voice pair clears both
tables; the next tick re-claims open instances, but archived presence spans
are lost. Use a v13 recovery point to preserve claims and spans. Use v14 to
preserve starboard settings, starrers and post records too. Restoring a pre-v14
archive clears those three tables, so the board is off until configured again.
Restoring v14 clears giveaway definitions, entries and draw rounds while
preserving starboard, event voice and issued tokens. Use v15 to preserve giveaways.
Restoring v15 clears retained orphan evidence but preserves the older inventory.
Use v16 to preserve those observations too. Current v16 archives must include the monthly cap, both mirror-fence tables,
the registry, event voice pair, starboard, giveaway and orphan tables, so routine restores
retain their archived state. Tombstone `deleted_at` cells must be real dates in
exact `YYYY-MM-DDTHH:MM:SS.sssZ` form: inspection refuses missing, null,
non-string or noncanonical values before restore opens a write transaction.
Rollback-delta import enforces the same rule during parsing and revalidates the
whole batch before applying constructed import rows. Values are not normalized;
unrelated tables keep their existing timestamp behavior.
An optional legacy table may be absent only when there are no archived rows for
it. Nonempty legacy data without a matching target table refuses **before any
truncate**, rather than silently discarding it.

Four application tables are excluded, each with its reason inline in
`EXCLUDED_TABLES`: `xp_cooldowns` (short-lived award throttles; restore clears
target cooldowns), `discord_send_admission` (per-credential lane state that a
recovered process re-learns), `gateway_onboarding_jobs` (a restart-recovery
queue bound to a gateway session) and `automation_command_registry_resync`
(the pending registry-publication marker; a recovered process republishes
the complete custom-command set on boot regardless). The store chain's `rollback_journal` and
`rollback_watermarks` are excluded too: the journal is append-only
cutover evidence (the `PgFunnelStore` funnel append and the production
gateway batch writer in `crates/cutover/src/gateway_session.rs` journal each
`events` insert inside the primary transaction under a savepoint best-effort
via `journal::record_in`, and their rows stay re-capturable
during the watch window), and restoring pre-backup watermarks could mark
post-backup writes as already journaled, silently breaking rollback coverage.
Migration ledgers (`_sqlx_migrations`, `schema_migrations` and the store
chain's `_two_bot_migrations`) describe target DDL and are never restored.
Replay guards, idempotency records, gateway sessions, lease-bearing durable
tables and audit history are **not** ephemeral exclusions. Derived `web_v1`
views contain no independent table data; the website service's own separate
database is out of scope. The migration-backed coverage test compares real
tables against these classifications, so adding an unclassified table to
either chain fails CI: the cutover chain is migrated for real, and the store
chain (`crates/store/migrations`) is text-scanned for `CREATE TABLE`, so a
new store table without a dump/exclude decision is refused rather than
silently omitted from the backup.

v3 must contain its original 22 table entries. It can lack later tables, but
restore clears their old target rows and emits a warning in both dry-run and
real-restore CLI output. One explicit infrastructure exception is the required
`guild_settings_revision` singleton: an old archive that lacks it initializes
`(TRUE, 0)` so subsequent settings writes work. `RestoreReport.initialized_tables`
and CLI warnings identify that baseline as synthesized, **not archived data**.
No settings values, rank ladder or other application history are synthesized.

Restore ignores manifest ordering and inserts parents before children using the
inventory. It acquires explicit `ACCESS EXCLUSIVE` table locks and truncates the
owned set in one transaction, **without CASCADE**. It temporarily disables only
`trg_guild_settings_revision` and `trg_guild_settings_audit_append_only`, preserving
archived revision and audit rows exactly; their original enable modes are
restored before commit. FK, CHECK and other triggers remain active. An explicit
restore role must already own these tables/sequences (or have equivalent existing
authority); this implementation grants **no** runtime privileges. Run restoration
with application writers stopped and rehearse into an isolated test target.

Serial and identity sequences are discovered from the **target catalog**, not a
hardcoded list of `id` columns or untrusted archive sequence names. This includes
`internal_idempotency.intent_id` and `internal_action_log.audit_id`. The standalone
`guild_settings_version_seq` and `guild_settings_cas_seq` are also restarted.
Archive marks are matched to target sequences by `(table, column)`; a mark never
names a sequence. The ban-ownership `moderation_member_bans.generation`
sequence resumes past restored `moderation_scheduled_unbans.retry_generation`
queue tickets drawn from it, not only past restored generations; the first
source names the allocator and supplies its archive mark. The truncate does
**not** `RESTART IDENTITY`. Each allocator resumes at the furthest of its
configured start, one step past the restored rows, the archive's mark and the
target's own position, in the sequence's direction and within its bounds. A
restore therefore never reissues a value the source or the target handed out,
including one whose row was deleted (the
[cutover allocator gate](cutover.md#data-copy-and-verification)). That
matters beyond the database: `community_scorecard_runs.watermark` is a
`community_facts.id`. Exhaustion, or a mark running the other way from the
target, refuses and rolls back.

The CAS allocator (`guild_settings.cas_version`, descending) moves past the
archive's mark **before** the inserts assign fresh tokens, so no restored row
receives a token a client of the source may still hold. `ALTER SEQUENCE RESTART`
is transactional, unlike `setval`: a failed restore leaves every allocator where
it was. Identity `GENERATED ALWAYS` columns use `OVERRIDING SYSTEM VALUE` during
inserts.

An archive without `sequenceMarks` (frozen v3, or v4 written before marks
existed) carries no deleted-top high-water. Restore logs a warning and
resumes past the restored rows and the target's own position only, so it does
**not** meet the allocator gate on a fresh target. A mark covers allocations up
to the dump: take a cutover or recovery dump with writers stopped.

PostgreSQL behavior references:
- [Serial/identity ownership discovery](https://www.postgresql.org/docs/16/functions-info.html)
- [Transactional RESTART semantics](https://www.postgresql.org/docs/16/sql-altersequence.html)
- [Identity override](https://www.postgresql.org/docs/16/sql-insert.html)

### Moderation history fence and expiry quarantine (TOG-10078)

`moderation_member_bans` (acceptance, insertion-order generation and
prepared/rejected fences) is appended after the frozen v3 prefix and read in the
same repeatable-read snapshot as scheduled unbans and idempotency. **Restore
refuses any destination with member ban, scheduled-unban, audit, idempotency,
warning or channel-execution history**, after locking every replaced table and
before truncation, so concurrent writes cannot slip through; no `CASCADE`
bypass removes unrelated evidence. Preserve the existing database; use a fresh
migrated target, not a manual deletion of evidence to satisfy this
precondition. This deliberately avoids guessing how to merge incompatible
ownership generations or forgetting post-backup PUT/DELETE evidence. Other
bot-owned tables are still replaced.

Every imported staged, pending or running expiry is **quarantined**, including
one with matching accepted ownership. A snapshot's accepted tempban cannot
prove current Discord ownership: a newer permanent ban may have superseded it
after the backup. The file keeps the original states; restored acceptance,
generations, reasons and timestamps remain historical evidence, not remote
reconciliation. Ordinary activation/recovery never re-enables quarantined rows.
Imported running DELETEs keep their independent uncertainty fence, original
claim token and timestamps. Resolving one DELETE does not authorize an expiry.

A v3 envelope has no ownership table. A fresh-target restore does not invent
acceptance or order from request IDs or timestamps. The CLI warns in both
dry-run and apply and reports the quarantine count; row-count verification is
data fidelity, **not** moderation-enable approval. Stop consumers and keep
`TWO_MODERATION` off until an authorized reconciliation considers the original
snapshot **and preserved destination history**, proves the actual remote
outcomes/order, and records expiry dispositions before any activation. Consumer
shutdown alone is not remote reconciliation. No automatic release or
destructive in-place override is provided by this slice.

### Migration-backed regression suite

`crates/core/tests/backup_schema_roundtrip.rs` creates uniquely named isolated
fixtures through `two_bot_testsupport::TestDatabase` and runs the actual cutover
migrations. CI sets `TWO_TEST_DATABASE_URL` to its passwordless `agent_test`
bootstrap on `agent-testdb`; without that variable the DB test reports a skip,
which is not database verification. Fixture cleanup is awaited. Unit/integration
tests must never use staging or production databases.

On the persistent controller, compiling commands must use the bounded cache
wrapper from the isolated workspace:

```bash
python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db --test backup_schema_roundtrip -- --nocapture
python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db --lib backup::dump_file -- --nocapture
```

A missing/refused/busy pool is **not** permission to run Cargo directly or use a
new target directory. Use the existing ephemeral hosted CI path for compilation
and record the local refusal. Existing synthetic/native-v3 and real-CLI publication
regressions remain in `backup_roundtrip` and `backup_dump_publication`.

## Nightly DB backup — daily 04:17

`deploy/two-bot-next-backup.{service,timer}` runs `two-bot backup`:

1. Parse `TWO_BACKUP_KEEP` **before** dumping — a malformed value aborts the
   run while it is still a no-op (a typo must never prune everything).
2. Dump to `TWO_BACKUP_DIR/two-funnel-<stamp>.ndjson.gz` (default 14 kept).
   Empty event log → exit non-zero **before retention/upload**, preserving
   previous archives: a backup that quietly reports zero events is worse than none.
3. Prune to the newest `TWO_BACKUP_KEEP` files (by mtime).
4. Run `TWO_BACKUP_UPLOAD_CMD` with the file path as its **last** argument
   (the `uploadCmd` contract: suits `cp -t DIR FILE`; point wrapper-needing
   tools at a one-line script). Unset → warn, not fail. The helper runs
   under a 30-minute deadline: on expiry the bot terminates and reaps the
   direct child it spawned and fails the run (exit 1) without deleting the
   archive. The deadline covers that direct child only, not grandchildren
   a helper may detach (no process-group kill).

```bash
sudo cp deploy/two-bot-next-backup.{service,timer} /etc/systemd/system/
sudo systemctl enable --now two-bot-next-backup.timer
systemctl list-timers two-bot-next-backup     # when it next runs
sudo systemctl start two-bot-next-backup      # run one now
journalctl -u two-bot-next-backup -n 30       # what it did
```

## Unit bounds and failure signal

Every `deploy/*.service` is bounded and loud. These are templates; shipping the
directives installs nothing and starts no timer (host installation stays with the
Operator). `scripts/test_deploy_units.py` lints every unit, so a new or edited
unit that drops a directive fails CI naming the file. The lint also pins
`Type=oneshot` (the start bound only applies to oneshot units) and refuses
`TimeoutSec=` and `SuccessExitStatus=`/`RestartForceExitStatus=` overrides,
which would silently drop the bound or the failure hook.

| Unit | `TimeoutStartSec` | Why this long |
|---|---|---|
| `two-bot-next-backup.service` | 2 h | dump plus the 30-minute upload deadline |
| `two-bot-next-guild-config-backup.service` | 2 h | Discord REST reads plus two sequential uploads (snapshot and drift report, 30 min each) |
| `two-bot-next-restore-drill.service` | 2 h | scratch provision, migrate and restore |
| `two-bot-next-failure@.service` | 30 s | the handler only writes one log line |

Without a bound, a hung oneshot stays "activating" forever and its timer skips
every later run. On expiry systemd kills the run and the unit goes `failed`.

**Consuming the signal.** Each of the three units sets
`OnFailure=two-bot-next-failure@%n.service`. A failed, timed-out or refused run
starts that template, which writes one `crit`-priority journal line tagged
`two-bot-next-unit-failed`: `unit failed: <unit> - inspect with journalctl -u <unit>`.
Alert on that tag (any journal shipper that filters on priority or tag) or on
`systemctl --failed` / the unit's `failed` state. The handler reads no
credential, touches no database and sends nothing off the host; delivery to a
person is the alerting layer's job. Install it next to the other units:

```bash
sudo cp deploy/two-bot-next-failure@.service /etc/systemd/system/
journalctl -t two-bot-next-unit-failed --since -2d    # recent failures
```

**Drill archive age.** The drill refuses to restore an archive older than
`TWO_RESTORE_DRILL_MAX_ARCHIVE_AGE_S` (unit default `172800`, 48 h: one missed
nightly run), measured from the newest `two-funnel-*.ndjson.gz` file's mtime. The
check runs in the unit's shell script before `restore-drill` allocates any
database, so a stopped nightly backup cannot pass against an old file. It prints
one JSON line and, on refusal, the next action on stderr, then exits 1 (which
fires `OnFailure=`):

```json
{"verdict":"refuse","code":"archive_stale","archive_age_s":9000000}
```

| `code` | Verdict | Next action |
|---|---|---|
| `archive_fresh` | pass | none; the restore proceeds |
| `archive_stale` | refuse | the nightly backup stopped: `journalctl -u two-bot-next-backup`, fix it, take a fresh backup, rerun the drill |
| `archive_missing` | refuse | no archive exists (`archive_age_s` is `null`): run the backup service first |
| `archive_unreadable` | refuse | the archive cannot be inspected or opened for reading: check backup directory permissions and file ownership |
| `archive_from_future` | refuse | the file is dated ahead of the host clock: check the clock |
| `max_age_invalid` | refuse | set the variable to a positive whole number of seconds (no leading zero, at most 9 digits) |

The check is read-only and repeatable, and prints no archive name, path or secret.
Never `touch` an archive to make the drill pass: the age is the point.

## Off-box destination

`two-bot backup-upload <file>` PUTs one dump via SigV4 single-PUT. Canonical
and signed headers are sorted alphabetically, per
[AWS's signing specification](https://docs.aws.amazon.com/IAM/latest/UserGuide/create-signed-request.html#create-canonical-request).
Tests pin AWS key derivation, an independently calculated complete PUT signature,
and a loopback verifier that rejects noncanonical order before re-deriving HMAC.

| Variable | Required | Meaning |
|---|---|---|
| `TWO_BACKUP_S3_ENDPOINT` | yes | `https://…` with a host, no userinfo/query/fragment/backslash, valid port (plain `http` only to loopback, path prefixes kept, else refused) |
| `TWO_BACKUP_S3_BUCKET` | yes | validated bucket name (a stray slash would retarget the write) |
| `TWO_BACKUP_S3_ACCESS_KEY_ID` | yes | — |
| `TWO_BACKUP_S3_SECRET_ACCESS_KEY` | yes | never logged; only `bucket/key` + etag reach the journal |
| `TWO_BACKUP_S3_REGION` | no | default `auto` (what R2 wants) |
| `TWO_BACKUP_S3_PREFIX` | no | normalised to one trailing slash |
| `TWO_BACKUP_S3_TIMEOUT_MS` | no | default 300000 |

Every misconfiguration is a refusal naming the variable, never a default:
an uploader that guesses a bucket reports success and recovery finds nothing.

## Guild-config backup — daily 04:31 UTC

`deploy/two-bot-next-guild-config-backup.{service,timer}` runs
`two-bot guild-config-snapshot`: assert the token is the `Owen QA Test`
application (live/superseded tokens refused, nothing contacted), assert the
guild is TWO Staging, capture roles/channels/overwrites/settings/emoji
(CDN fetch honours `GUILD_CONFIG_CDN_BASE`), seal the snapshot (TOG-3513),
write snapshot + drift report atomically (fsync + rename, `0600`), re-read
and re-verify hash + seal, then upload **both** via
`TWO_GUILD_CONFIG_UPLOAD_CMD` (or `TWO_BACKUP_UPLOAD_CMD`). A local-only
snapshot is an error, not success. Each upload shares the 30-minute
bounded-helper contract from the nightly backup above: a hung helper is
terminated and reaped (direct child only), the run fails, and both files
stay on disk. The snapshot + drift report land in
`TWO_GUILD_CONFIG_BACKUP_DIR` (default `./guild-config-backups`;
`crates/bot/src/backup_cli.rs:1067`, usage at `:200`).

Both guild-config commands first read `CREDENTIALS_DIRECTORY/discord_staging_token`
(the service's `LoadCredential` contract). Only an absent credential/no configured
directory permits the deliberate `DISCORD_STAGING_BOT_TOKEN` fallback. Empty,
invalid or unreadable credentials refuse without substitution or token logging.
Restore preflight uses the **live capture**, not saved permissions/hierarchy;
removed authority refuses before writes, and newly granted authority is honored.
Surviving role/channel/category IDs take precedence over names, preserving role
membership and channel history through renames. Name fallback must be unique
and unclaimed. Plans compare literal-ID overwrite sets without order sensitivity,
repair editable `@everyone` permissions, and validate all resource dependencies
before apply. Hierarchy checks target only planned edits, not unrelated higher
roles; missing managed resources cannot silently become unresolved late writes.

Restore: `two-bot guild-config-restore --snapshot FILE` plans (prints
`WOULD …`); add `--confirm-staging-guild --apply` to write, `--evidence
FILE` for the hash-proven receipt. Snapshots past 8 MiB are refused (exit 2)
before buffering or any Discord read; non-regular `--snapshot` inputs
(devices, FIFOs) are refused the same way. Tampered backups are refused (exit 3)
before any Discord read or write; pre-seal snapshots warn and proceed.
Apply ends with the post-restore hash compared against the remapped source:
mismatch → exit 1 with the residual operations listed.

## Monthly restore drill — 1st, 05:30

`deploy/two-bot-next-restore-drill.{service,timer}` invokes `restore-drill`
for the newest published backup and requires `RESTORE VERIFIED`. **Every run
allocates a distinct fresh scratch database**, applies the same embedded S6
migrations, then prepares seven still-unported legacy archive tables using the
full preserved DDL pinned by an offline provenance regression. This private
scratch-only compatibility layer is not a production migration, feature
initializer or consumer; it adds no roles, grants or backfill. Future S6-owned
schemas remain authoritative (`IF NOT EXISTS`). It then performs the normal
guarded restore and never reuses or drops previous targets. A destination with
moderation history still refuses direct restore, even with `--force`; do not erase
history to pass that guard.

The currently authorized provisioning path is only the disposable
`agent-testdb:5432` service, explicitly empty-password `agent_test` and bootstrap
`postgres`. Production/staging, arbitrary hosts, runtime credentials, URL query
options and inherited libpq `PG*` settings refuse before allocation. No login,
role, membership or Discord consumer is created. Ordinary shipped S6 migrations
can apply their existing scoped grants to an already-present runtime group in
this new test database; the drill adds no special runtime grants or credentials
and changes no existing database. Extending this binding to another environment
requires separate authorization and review, not a URL edit.

Operator installation must provide protected `/etc/two-bot-next/restore-drill.env`
with `TWO_RESTORE_DRILL_BOOTSTRAP_URL=postgres://agent_test:@agent-testdb:5432/postgres`.
The unit does not read shared `backup.env`, source or upload credentials. Its
`StateDirectory` supplies protected `/var/lib/two-bot-next-restore-drills`,
writable under the unit sandbox. The absolute evidence root must already exist;
manual operators must provision a protected retained directory first. The drill
syncs its new child directory entry before database allocation. No service
installation/execution is authorized by this PR.

Each run retains a private archive copy, hashes it with a bounded 32 KiB read
buffer rather than loading the whole compressed archive, and writes exclusive
`planned`, `allocated`,
`migrated` and `verified` JSON receipts as those stages complete. A provisioning,
migration or restore failure records a `failed` classification without raw SQL
errors or credentials; any partially allocated target and completed evidence
remain. Archive-validation failures allocate no database and preserve the private
copy for diagnosis. No prior target, archive or receipt is overwritten or pruned.
The verified receipt includes `dropped_columns` and warns when any archive
columns are absent from the target. `RESTORE VERIFIED` proves per-table row counts,
not that every source column survived; inspect these diagnostics and the retained
archive before accepting data fidelity. Neither receipt nor counts authorize
moderation activation. Capacity/retention decisions require a separate authorized
evidence-preservation policy; do not clear drill history to free a build cache or make the next run pass.

Manual drill (with that explicit scratch authority, no production restore):

```bash
# 1. Is last night's file any good? (writes nothing; TWO_RESTORE_URL optional)
two-bot restore /var/backups/two-bot-next/two-funnel-<stamp>.ndjson.gz --dry-run
# → DRY RUN VERIFIED

# 2. Allocate a NEW migrated scratch target; retain earlier drill evidence.
TWO_RESTORE_DRILL_BOOTSTRAP_URL=postgres://agent_test:@agent-testdb:5432/postgres \
TWO_RESTORE_DRILL_EVIDENCE_DIR=/var/lib/two-bot-next-restore-drills \
two-bot restore-drill /var/backups/two-bot-next/two-funnel-<stamp>.ndjson.gz --confirm-scratch
# → retained evidence <unique directory>, then RESTORE VERIFIED
```

Production restore is **not authorized by this slice**. S6 must establish its
cutover plan and gates separately; these examples are scratch-only.

### Drill evidence — 2026-09-30 (TOG-9881, scratch only)

- Backup: `two-bot backup` against agent-testdb `two_next_backup_test`,
  wrote `two-funnel-20260930T004531Z.ndjson.gz` (1.2 KiB, 14 rows over 22
  tables; tables seeded by the round-trip test). Run with the CHANGES-fix
  build (exact-host loopback allowlist, endpoint path prefix, cell-type and
  size-cap refusals, migration-record propagation, bounded HTTP bodies).
- Dry run: `restore … --dry-run` → `DRY RUN VERIFIED`, 14 rows verified,
  nothing written.
- Restore: `restore … --force` with
  `TWO_RESTORE_URL=postgres://agent_test@agent-testdb:5432/two_next_restore_drill`
  (pre-created 22-table schema) → `RESTORE VERIFIED`, every table `ok`.
- Tamper refusal: crafted sealed snapshot with altered content →
  `refusing tampered backup`, exit 3, zero Discord calls.
- No production or staging services touched; no tokens used (Discord paths
  exercised against in-process fakes in `backup_transport` tests).

### Review-fix verification — 2026-09-30

- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`
  and `cargo test --workspace --locked` pass. The default database harness skips
  without its URL; it is not evidence of a database round trip.
- Separately ran `cargo test -p two-bot-core --features db --locked --test backup_roundtrip -- --nocapture`
  with `TWO_BOT_TEST_DATABASE_URL=postgres://agent_test@agent-testdb:5432/two_next_backup_9881_1a7ac99c`:
  one test passed after real dump/wipe/restore, no-ledger dump, malformed-ledger
  refusal and native frozen-writer fixture restore, including sequence restart.
- Seven real-CLI loopback tests verify credential-file-only snapshot loading,
  absent-only fallback, invalid/unreadable credential refusal, and both directions
  of changed live permissions/hierarchy. Positive preflight reaches a deliberately
  refused write sentinel; it does not claim real Discord restore convergence.
- Streaming-budget tests cover a highly compressible oversized line, many small
  lines over the decoded budget, and retained-value overhead. Four loopback
  transport tests pass with alphabetic SignedHeaders required by the verifier.
- Test connections are confined to agent-testdb and loopback HTTP; no production
  or staging service, usable Discord token or off-box bucket was exercised.

### Second CHANGES verification — 2026-09-30

- Clean integrated-tree `cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets --locked -- -D warnings` and
  `cargo test --workspace --locked` pass after preserving current main's
  configuration, voice and gateway integrations. The unrelated dirty manifest
  is excluded from this archived-tree check. Optional database harnesses skip
  without a URL; their default result is not database execution evidence.
- 27 guild-config regressions cover duplicate/renamed role identities, swapped
  channels, renamed categories, parent moves, editable `@everyone`, actual-target
  hierarchy checks, unchanged/reordered overwrite sets and zero-write dependency
  refusals, including public/mutated plans. Discord execution is loopback-only.
- Writer/reader tests accept exact boundaries and refuse one-byte excess for all
  four budgets using reduced caps on the same production accounting paths.
  Full 1 GiB compressed/decoded and 256 MiB retained boundaries were not
  materialized. Real database coverage separately accepts an exact 8 MiB
  complete decoded line (including envelope/newline) and refuses one byte more.
- Separately built the integrated bot, set `TWO_BOT_TEST_BACKUP_BIN` to it, and
  ran the `backup_roundtrip` and `backup_dump_publication` tests with `db` on
  dedicated agent-testdb `two_next_backup_publication_f2561193_final`: both pass.
  The actual CLI child alone receives `RLIMIT_FSIZE=500`; returned EFBIG cleans
  its temporary, while SIGXFSZ leaves only an ignored temporary. Previous valid
  archives survive; failure neither prunes nor uploads; the shipped drill
  selector ignores partial output and a successful retry retains correctly.
- Scratch databases were dropped and verified absent. No parent/global process
  limits, production/staging resources, usable Discord credentials or off-box
  buckets were used. Physical power-loss durability and real Discord behavior
  remain outside this test evidence.
- CodeQL CLI 2.27.1's name heuristic classified the former `account_decoded`
  byte-budget helper as an account-ID source despite its exclusively numeric
  inputs. It is now accurately named `reserve_decoded_bytes`; no logging,
  scanner configuration, alert disposition or dataflow behavior was weakened.
  Public pinned-model evidence is recorded in the author verification report;
  fresh exact-head CI remains required, not an alert dismissal.

## S6 hook (Founding Engineer)

`cmd_restore` does **not** run migrations. Provision the target with the current
`crates/cutover/migrations` schema through the separately authorized cutover path;
backup/restore refuses a missing required table rather than migrating implicitly.
The reader tolerates archived columns the target lacks (`dropped_columns` report,
target types win), but it never silently drops nonempty archived legacy tables
that have no target. Cutover execution and production restore remain separately
gated; this table-coverage slice grants neither authority.
