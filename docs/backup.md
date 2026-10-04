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
inventory for both dump and restore. It includes **80 durable tables from the
current cutover migrations**, including the bot-owned website-contract backing
tables, the member-moderation ledger, the self-role send receipts
(`self_role_exchanges`) plus their uncertainty baselines
(`self_role_exchange_baselines`), the automod delivery claims, the gateway
boot directives, the member-erasure audit, the internal clock high-water
mark and the voice configuration tables. `OPTIONAL_LEGACY_TABLES` is empty: the
coverage test fails if a covered table is not migrated. A dump from a fresh Rust
schema has 80 table entries. A missing current table refuses a dump/restore:
migrate the target first.
A v4 archive written before a table joined the inventory is refused at inspect
("manifest is missing tables"); take a fresh dump after upgrading.
An optional legacy table may be absent only when there are no archived rows for
it. Nonempty legacy data without a matching target table refuses **before any
truncate**, rather than silently discarding it.

Three application tables are excluded, each with its reason inline in
`EXCLUDED_TABLES`: `xp_cooldowns` (short-lived award throttles; restore clears
target cooldowns), `discord_send_admission` (per-credential lane state that a
recovered process re-learns) and `gateway_onboarding_jobs` (a restart-recovery
queue bound to a gateway session). The store chain's `rollback_journal` and
`rollback_watermarks` are excluded too: the journal is unwired append-only
cutover evidence (no writer calls `record` yet, and its rows are re-capturable
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
