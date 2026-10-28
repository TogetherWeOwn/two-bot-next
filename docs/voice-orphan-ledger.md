# Voice orphan evidence

`/voice-orphans [page:integer]` privately lists failed creator-channel
compensations. It is published with `TWO_VOICE=1`. Manage Channels or
Administrator must be established from trusted guild-role facts before any
store is constructed or queried; a payload permission claim alone is not enough.
Cold guild-role facts return the retryable warming-up reply, not a staff denial.
Replies are ephemeral and mention-safe. Page 1 is the default; each page has
up to ten complete newest-first rows and a copyable continuation. Invalid or
past-end pages refuse rather than clamp.

Only `/create` with a successful channel receipt, failed creator persistence
and failed compensating delete appends evidence. `voice_orphan_ledger` stores
only guild/channel IDs, `creator_compensation_failed` and observation time.
The guild/channel primary key preserves the first retained observation.
A per-guild transaction lock serializes insert and oldest-out trimming to
1000 rows, including across independent runtimes. Timestamp ties use channel
ID order. Reads and their totals share one SQL snapshot.

This is **evidence, not deletion authority**. Reads never change Discord or
ledger rows. Tracked ghosts and room-rollback witnesses retain their existing
reconciliation paths. Inspect the named channel before a separate cleanup
decision; no new automatic deletion is enabled.

If the database cannot record evidence, the existing private manual-cleanup
reply remains authoritative and `voice_orphan_ledger_write_failed` warns with
a constant outcome, without a raw driver error. No persisted success is claimed.
Retention is a bounded operational window, not permanent audit history.

The additive cutover migration creates only this table and index. Restoring
an older binary leaves the evidence inert and intact; do not drop it as part
of application rollback. This change does not perform a migration or deploy.

Durable backups use **v16** to preserve retained ledger rows, including their
original timestamps. The reader still accepts v15 and older archives; those
recovery points predate the ledger and clear destination-only evidence rather
than inventing observations. See [backup recovery semantics](backup.md#coverage-and-recovery-semantics-v4-v5-v6-v7-v8-v9-v10-v11-v12-v13-v14-v15-and-v16).

## Disposable verification

The protected suite exercises real command dispatch, SQL evidence, restart,
concurrent cap enforcement, denial-before-SQL, paging and reconcile controls.
Direct store regressions also pin page bounds, empty-page totals, the database
reason constraint and tied ordering/retention; backup tests exercise fresh
recovery and frozen-v15 clearing:

```sh
export TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_local
python3 scripts/cargo_cache.py run -- test -p two-bot --lib --locked \
  voice_rooms::tests::orphan_ledger_acceptance:: -- --nocapture --test-threads=1
python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --test voice_orphan_ledger_regressions
python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db --test backup_schema_roundtrip
```

Use only the authorized disposable database. Never test against staging or
production, and never invoke live Discord writes for this suite.
