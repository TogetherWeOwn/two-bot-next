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
require `--force`: aiming at production must be said twice, on purpose.

## The format

Gzipped NDJSON, one object per line: `manifest` / `row` / `end` (v3, frozen).
The manifest carries per-table `{name, columns, column_types, count}` taken
inside one `REPEATABLE READ` transaction, the `events` high-water mark, and
the source's applied migrations. Values are stored in Postgres text-output
form with a `$n::type` cast on restore — faithful for every owned type
without per-type decoding. `column_types` is additive to the legacy envelope.
Row cells are strings or nulls; a native JSON cell (number, boolean, array,
object) is refused by `inspect` rather than restored as NULL. Dumps past
1 GiB (`MAX_DUMP_BYTES`) are refused from file metadata before buffering.

All 22 bot-owned tables are dumped (see `DUMP_TABLES` in
`crates/core/src/backup/dump_file.rs`); the website's tables are not ours.

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

`two-bot backup-upload <file>` PUTs one dump via SigV4 single-PUT (pinned
against AWS's worked example in `backup::s3` tests; the fake-S3 round trip
re-derives the signature server-side like `test/helpers/fakeS3.ts` did).

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

# 3. Only then, for real: say the target twice.
TWO_RESTORE_URL="$TWO_DATABASE_URL" two-bot restore /var/backups/two-bot-next/two-funnel-<stamp>.ndjson.gz --force
```

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

## S6 hook (Founding Engineer)

`cmd_restore` does **not** run migrations: two-bot-next migrations land
under S6, so the target must already carry the schema and `dump()` refuses
with a named table when it does not. S6 plugs `migrate()` in at the marked
`NOTE` in `crates/bot/src/backup_cli.rs` (same position legacy
`pg-restore.ts` ran it). The dump reader already tolerates dumps whose
columns the target lacks (`droppedColumns` report, target types win).
