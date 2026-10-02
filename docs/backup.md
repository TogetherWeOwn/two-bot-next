# Backups, restore drills, and sealed guild-config snapshots (TOG-9881)

Rust port of the legacy `two-bot` cutover-data surface (frozen source
`two-bot` `main @ d5d11793`): v4 dump format with v3 read compatibility,
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

## The format

Gzipped NDJSON, one object per line: `manifest` / `row` / `end`. New dumps
write **v4**; the reader also accepts frozen **v3**. Other versions refuse.
The manifest carries per-table `{name, columns, column_types, count}` taken
inside one `REPEATABLE READ` transaction, the `events` high-water mark, and
the source's applied migrations. Values are stored in Postgres text-output
form with a `$n::type` cast on restore — faithful for every owned type
without per-type decoding. `column_types` is additive to the legacy envelope.
Port-written row cells are strings/nulls. Frozen legacy v3 dumps have no
`column_types` and carry native numbers/booleans; these decode to bound
PostgreSQL input using the target's type, never silently to NULL. JSON-looking
TEXT stays unchanged, and native JSON cells serialize for JSON/JSONB targets.
Unsupported composite cells refuse; see `tests/fixtures/README.md` for scope
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

### Coverage and recovery semantics (v4, TOG-11142)

`DUMP_TABLES` in `crates/core/src/backup/dump_file.rs` is the single ordered
inventory for both dump and restore. It includes **54 durable tables from the
current cutover migrations**, including the bot-owned website-contract backing
tables, plus **5 optional retired legacy tables**. A dump from a fresh Rust
schema has 54 table entries; a compatible legacy-extended schema may have up to
59. A missing current table refuses a dump/restore: migrate the target first.
A v4 archive written before a table joined the inventory is refused at inspect
("manifest is missing tables"); take a fresh dump after upgrading.
An optional legacy table may be absent only when there are no archived rows for
it. Nonempty legacy data without a matching target table refuses **before any
truncate**, rather than silently discarding it.

The only application exclusion is `xp_cooldowns` (short-lived award throttles);
restore clears target cooldowns. Migration ledgers (`_sqlx_migrations` and
`schema_migrations`) describe target DDL and are never restored. Replay guards,
idempotency records, gateway sessions, lease-bearing durable tables and audit
history are **not** ephemeral exclusions. Derived `web_v1` views contain no
independent table data; the website service's own separate database is out of
scope. The migration-backed coverage test compares real tables against these
classifications, so adding an unclassified table fails CI.

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
`guild_settings_version_seq` is also restarted. Next allocation is beyond restored
values (or the target's configured start for an empty table), respecting sequence
increment/bounds; exhaustion refuses and rolls back. `ALTER SEQUENCE RESTART` is
transactional, unlike `setval`, so a failed restore cannot advance the standalone
settings sequence outside the rolled-back transaction. Identity `GENERATED ALWAYS`
columns use `OVERRIDING SYSTEM VALUE` during inserts.

PostgreSQL behavior references:
- [Serial/identity ownership discovery](https://www.postgresql.org/docs/16/functions-info.html)
- [Transactional RESTART semantics](https://www.postgresql.org/docs/16/sql-altersequence.html)
- [Identity override](https://www.postgresql.org/docs/16/sql-insert.html)

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
   tools at a one-line script). Unset → warn, not fail.

```bash
sudo cp deploy/two-bot-next-backup.{service,timer} /etc/systemd/system/
sudo systemctl enable --now two-bot-next-backup.timer
systemctl list-timers two-bot-next-backup     # when it next runs
sudo systemctl start two-bot-next-backup      # run one now
journalctl -u two-bot-next-backup -n 30       # what it did
```

## Off-box destination

`two-bot backup-upload <file>` PUTs one dump via SigV4 single-PUT. Canonical
and signed headers are sorted alphabetically, per
[AWS's signing specification](https://docs.aws.amazon.com/IAM/latest/UserGuide/create-signed-request.html#create-canonical-request).
Tests pin AWS key derivation, an independently calculated complete PUT signature,
and a loopback verifier that rejects noncanonical order before re-deriving HMAC.

| Variable | Required | Meaning |
|---|---|---|
| `TWO_BACKUP_S3_ENDPOINT` | yes | `https://…` (plain `http` only to loopback, else refused) |
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
snapshot is an error, not success.

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
FILE` for the hash-proven receipt. Tampered backups are refused (exit 3)
before any Discord read or write; pre-seal snapshots warn and proceed.
Apply ends with the post-restore hash compared against the remapped source:
mismatch → exit 1 with the residual operations listed.

## Monthly restore drill — 1st, 05:30

`deploy/two-bot-next-restore-drill.{service,timer}` restores the newest
backup into the **scratch** database and requires `RESTORE VERIFIED`.
A red drill means the backups are not real — better on the 1st than during
an outage.

Manual drill (what the timer does, step by step):

```bash
# 1. Is last night's file any good? (writes nothing; TWO_RESTORE_URL optional)
two-bot restore /var/backups/two-bot-next/two-funnel-<stamp>.ndjson.gz --dry-run
# → DRY RUN VERIFIED

# 2. Rehearse into scratch. Never restore straight to prod.
TWO_RESTORE_URL=postgres://.../two_scratch two-bot restore /var/backups/two-bot-next/two-funnel-<stamp>.ndjson.gz --force
# → RESTORE VERIFIED (every table count matches the manifest)
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
