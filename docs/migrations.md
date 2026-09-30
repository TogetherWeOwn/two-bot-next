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
include comments and line endings. All 16 existing migrations are initially
locked without changing their contents; this is a source-control baseline, **not
an attestation that a database has applied them**. CI never queries migration
history in staging or production.

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
Missing Git baselines fail closed. A baseline without `migrations.lock` is
supported for the first introduction: baseline SQL bytes are still compared.

Without `--base-ref`, the checker verifies current numbering and hashes only;
it cannot determine whether a justification was changed. CI always supplies the
baseline so edits accompanied by silent checksum rewrites and removals of both a
migration and its lock entry cannot pass.
