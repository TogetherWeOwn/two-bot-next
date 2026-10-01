# Backups, restore drills, and sealed guild-config snapshots (TOG-9881)

Rust port of the legacy `two-bot` cutover-data surface (frozen source
`two-bot` `main @ d5d11793`): v3 dump format, SigV4 off-box upload, retention
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

Gzipped NDJSON, one object per line: `manifest` / `row` / `end` (v3, frozen).
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

All 23 bot-owned tables are dumped (see `DUMP_TABLES` in
`crates/core/src/backup/dump_file.rs`); the website's tables are not ours.
This includes `moderation_member_bans`: acceptance, insertion-order generation
and prepared/rejected fences are read in the same repeatable-read snapshot as
scheduled unbans and idempotency. **Restore refuses any destination with member
ban, scheduled-unban, audit, idempotency or warning history**, before truncation.
It locks all replaced tables before checking so concurrent writes cannot slip
through. Preserve the existing database; use a fresh migrated target, not a
manual deletion of evidence to satisfy this precondition. This deliberately
avoids guessing how to merge incompatible ownership generations or forgetting
post-backup PUT/DELETE evidence. Other bot-owned tables are still replaced.
Generation resumes after the restored MAX (empty ownership restarts at 1).

Every imported staged, pending or running expiry is **quarantined**, including
one with matching accepted ownership. A snapshot's accepted tempban cannot
prove current Discord ownership: a newer permanent ban may have superseded it
after the backup. The file keeps the original states; restored acceptance,
generations, reasons and timestamps remain historical evidence, not remote
reconciliation. Ordinary activation/recovery never re-enables quarantined rows.
Imported running DELETEs keep their independent uncertainty fence, original
claim token and timestamps. Resolving one DELETE does not authorize an expiry.

The frozen 22-table v3 envelope remains readable without the additive ownership
table. A fresh-target restore does not invent acceptance or order from
request IDs or timestamps. The CLI warns in both dry-run and apply and reports
the quarantine count; row-count verification is data fidelity, **not**
moderation-enable approval. Stop consumers and keep `TWO_MODERATION` off until
an authorized reconciliation considers the original snapshot **and preserved
destination history**, proves the actual remote outcomes/order, and records
expiry dispositions before any activation. Consumer shutdown alone is not
remote reconciliation. No automatic release or destructive in-place override
is provided by this slice. Every other v3 table remains mandatory.

## Nightly DB backup — daily 04:17

`deploy/two-bot-next-backup.{service,timer}` runs `two-bot backup`:

1. Parse `TWO_BACKUP_KEEP` **before** dumping — a malformed value aborts the
   run while it is still a no-op (a typo must never prune everything).
2. Dump to `TWO_BACKUP_DIR/two-funnel-<stamp>.ndjson.gz` (default 14 kept).
   Empty event log → exit non-zero after upload: a backup that quietly
   reports zero events is worse than none.
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
migrations, then performs the normal guarded restore. It never reuses or drops
previous targets. A destination with moderation history still refuses direct
restore, even with `--force`; do not erase history to pass that guard.

The currently authorized provisioning path is only the disposable
`agent-testdb:5432` service, explicitly empty-password `agent_test` and bootstrap
`postgres`. Production/staging, arbitrary hosts, runtime credentials, URL query
options and inherited libpq `PG*` settings refuse before allocation. No login,
role, runtime grant or Discord consumer is created. Extending this binding to
another environment requires separate authorization and review, not a URL edit.

Operator installation must provide protected `/etc/two-bot-next/restore-drill.env`
with `TWO_RESTORE_DRILL_BOOTSTRAP_URL=postgres://agent_test:@agent-testdb:5432/postgres`.
The unit does not read shared `backup.env`, source or upload credentials. Its
`StateDirectory` supplies `/var/lib/two-bot-next-restore-drills`, writable under
the unit sandbox. No service installation/execution is authorized by this PR.

Each run retains a private archive copy and exclusive `planned`, `allocated`,
`migrated` and `verified` JSON receipts as those stages complete. A provisioning,
migration or restore failure records a `failed` classification without raw SQL
errors or credentials; any partially allocated target and completed evidence
remain. Archive-validation failures allocate no database and preserve the private
copy for diagnosis. No prior target, archive or receipt is overwritten or pruned.
Capacity/retention decisions require a separate authorized evidence-preservation
policy; do not clear drill history to free a build cache or make the next run pass.

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

`cmd_restore` does **not** run migrations: two-bot-next migrations land
under S6, so the target must already carry the schema and `dump()` refuses
with a named table when it does not. S6 plugs `migrate()` in at the marked
`NOTE` in `crates/bot/src/backup_cli.rs` (same position legacy
`pg-restore.ts` ran it). The dump reader already tolerates dumps whose
columns the target lacks (`droppedColumns` report, target types win).
