# Migration numbering and checksum guard

`python3 scripts/check-migrations.py` checks all files under
`crates/*/migrations`, including new crates such as `crates/store`. Files must be
flat, regular (not symlinked) SQL files named `NNNN_name.sql`; names use lowercase
ASCII letters, digits and underscores, starting with a letter. Numbers are
unique across the entire workspace and remain in the bot allocation **0001–0999**.
The shared database's web allocation **1000–1999** is not available to this bot
(see [parity §5](parity.md#5-db-tables--queries)). Gaps are allowed; this guard does not
renumber any migration or execute SQL.

## Lock format and adding a migration

The root `migrations.lock` is JSON with version `1` and a `migrations` object keyed
by repository-relative filename. Every migration has exactly one entry:

```json
{
  "version": 1,
  "migrations": {
    "crates/cutover/migrations/0001_example.sql": {
      "sha256": "<64 lowercase hex characters>",
      "justification": "Add example table; explain the change and reference its work item."
    }
  }
}
```

1. Pick an unused bot number across **all** crates, not just the target directory.
2. Add the SQL and its lock entry together. Compute SHA-256 over the exact file
   bytes, e.g. `sha256sum crates/cutover/migrations/NNNN_name.sql`.
3. Write a nonempty, one-line `justification` explaining why the migration exists.
4. Run both checks below and include the SQL/lock changes in the same PR.

An unlisted new migration fails, as does a stale lock entry, invalid/duplicate
JSON key, malformed filename, duplicate number or checksum mismatch. Byte changes
include comments and line endings. All 18 existing migrations are initially
locked without changing their contents; this is a source-control baseline, **not
an attestation that a database has applied them**. CI never queries migration
history in staging or production.

## Risky-statement lint (required, with one advisory rule)

`python3 scripts/ci/check-squawk-migrations.py --base-ref <sha>` lints the
migration files changed since the baseline (CI passes the PR base SHA, the
push-before SHA, or `origin/main`; without `--base-ref` it lints every
file). It is a deterministic squawk subset: standard library only, no
database, no network. CI runs with `--strict`: `adding-required-field`,
`creating-index`, `adding-check-constraint`, and `changing-column-type` findings
fail the PR. `dropping-object` remains a warning because safe, coordinated
removal still ends with a `DROP TABLE` or `DROP COLUMN`. The four documented
transactional index exceptions below are matched by exact migration path and
complete normalized statement. Omit `--strict` for a local advisory preview of
all five rules.

Write migrations that stay silent:

- New required column: `ADD COLUMN x bigint DEFAULT 0`, backfill existing
  rows, then `ALTER TABLE ... ALTER COLUMN x SET NOT NULL` in a later
  migration.
- New index on an existing table: `CREATE INDEX CONCURRENTLY` in a
  separate migration run outside a transaction (sqlx no-transaction;
  `CONCURRENTLY` cannot run inside a transaction block). An index on the
  same qualified relation (schema and table; an unqualified name means
  `public`) created unconditionally earlier in the same migration file
  targets an empty table and is not flagged. `CREATE TABLE IF NOT EXISTS`
  earns no such exemption: when the relation already exists it is a no-op
  and the table may hold data. Creating a same-named table in a different
  schema does not exempt the index.
- If a concurrent index migration fails before it is recorded, the interrupted
  build may leave an invalid index, and a successful build may be unrecorded.
  Do not retry blindly: inspect the named index and migration ledger first. If
  the migration is unapplied and the index exists, remove that index with a
  standalone `DROP INDEX CONCURRENTLY IF EXISTS <schema>.<index>` command, then
  retry through the normal migration runner. Do not bundle the drop with the
  create in one migration query; PostgreSQL rejects concurrent index operations
  inside a transaction block.
- New check: `ADD CONSTRAINT ... CHECK (...) NOT VALID`, then
  `VALIDATE CONSTRAINT` separately.
- Type change or drop: prefer an additive migration (new column, backfill,
  swap) and coordinate removal across deploys.

Four exact `creating-index` findings are waived: the transactional event-store
indexes in migrations 0444 and 0445, and the same-file new-table live-lookup
indexes in migrations 0456 and 0457. The waiver matches only each migration path
and the complete normalized `CREATE INDEX` statement; any changed statement,
additional index in any of these files, or index in another migration remains
subject to strict lint. This exception is documented beside the checker
configuration and is grounded in the observed CI deadlock: `CREATE INDEX
CONCURRENTLY` waits on open snapshots while parallel disposable-database test
migrators wait on SQLx's advisory lock, producing PostgreSQL `40P01`, and it
cannot run inside the migration transaction at all. The 0456/0457 tables are
empty at first apply, and the `IF NOT EXISTS` re-apply no-ops both statements.
It does not relax any other migration check or the required lint gate.

The checker discovers `crates/*/migrations/*.sql` dynamically with
`--migration-glob` and `--exclude` overrides and no hardcoded crate list,
so the same script can be adopted 1:1 by a central reusable workflow called
from every repo. The remaining org rollout follow-up is a central repo plus
an org-ruleset required workflow; `--strict` is already enabled here.

## Rollback compatibility at promotion

A production rollback restores the previous Worker version and container
image, never the schema. Before every production promotion,
`scripts/rollback_target_preflight.py` (see
[production-deploy.md](production-deploy.md#rollback-target-preflight))
scans each migration added between the serving build's SHA and the
candidate. A statement the serving build may not survive refuses the
promotion: `DROP TABLE`/`COLUMN`/`TYPE`/`VIEW`/`SCHEMA`/`FUNCTION`/`SEQUENCE`,
`RENAME`, `ALTER COLUMN ... TYPE`, `SET NOT NULL`, `DROP DEFAULT`,
`TRUNCATE`, `REVOKE`, and `ADD COLUMN ... NOT NULL` without a `DEFAULT`.
`COLUMN` may be omitted, as PostgreSQL allows; any top-level `DROP` other than
`DROP INDEX` refuses; statements inside `DO` and function bodies are checked
too. Comments and string literals are lexed out first, so a `--` or `;` in a
literal hides nothing; quoted identifiers (including schema-qualified ones
like `"public"."members"`) and `E'...'` backslash escapes are handled the
same way. A literal, comment or dollar-quoted body the lexer cannot close
refuses as `unparseable_migration`: an unreadable migration never passes.
`TRUNCATE` in a `CREATE TRIGGER ... BEFORE|AFTER|INSTEAD OF` event clause is
not an executed truncate and does not refuse. Dynamic SQL inside an
`EXECUTE '...'` string is not visible to the scan: keep it out of migrations,
or annotate the file. A rewritten or deleted migration always refuses.

Ship expand first, contract later: the contract migration lands only after
every build that still reads the old shape has left production. When a
flagged statement is safe anyway (for example it drops a table no shipped
build ever read), say why in the file, reviewed like any other line:

```sql
-- rollback-compatible: no build since 0412 reads guild_legacy_flags
DROP TABLE IF EXISTS guild_legacy_flags;
```

The reason must be at least ten characters. The marker covers the whole
file, so keep a reviewed contract migration in its own file.

## Existing migration changes

Prefer an **additive new migration**, especially after a migration has shipped.
Do not remove, rename or renumber existing files. If an exceptional edit is
necessary before rollout, intentionally replace that file's `sha256` **and** its
`justification` with a fresh explanation of the specific edit and why it is safe.
An unchanged justification (including whitespace-only changes) fails the
baseline comparison even when the checksum has been regenerated.

Changing this source lock does **not** change sqlx's applied-migration checksums
or make an edit safe on an already-migrated database. Database rollout and any
exception remain separate reviewed work; never modify database migration history
to satisfy this guard. The guard enforces a recorded explanation, while the
independent PR reviewer assesses its substance.

## Local fixtures and CI

```sh
PYTHONDONTWRITEBYTECODE=1 python3 scripts/test-check-migrations.py -v
python3 scripts/check-migrations.py --base-ref origin/main
```

Fixtures use temporary local files and Git repositories only. On the controller
they use `PAPERCLIP_RUN_SCRATCH_DIR` when supplied; no database, Rust compilation
or network access is involved. The checker defaults to the repository containing
the script; `--root` is available for fixtures/other checkouts.

The existing required `check` job runs these fixtures and the checker **before**
Rust compilation. It checks the PR base SHA on pull requests, the push-before SHA
on `main` pushes, and `origin/main` on manual dispatches (including release
branches). Git history is fetched with `fetch-depth: 0`, as documented by
[actions/checkout](https://github.com/actions/checkout#fetch-all-history-for-all-tags-and-branches).
Missing Git baselines fail closed. Symlinked migrations and ancestors (including
`crates` itself), and a symlinked `migrations.lock` itself, are rejected in
both the working tree and Git baseline, so Git and filesystem discovery cannot
silently disagree. A baseline without
`migrations.lock` is supported for the first introduction: baseline SQL bytes
are still compared.

Without `--base-ref`, the checker verifies current numbering and hashes only;
it cannot determine whether a justification was changed. CI always supplies the
baseline so edits accompanied by silent checksum rewrites and removals of both a
migration and its lock entry cannot pass.
