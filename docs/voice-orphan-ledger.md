# Voice orphan evidence

`/voice-orphans [page:integer]` privately lists failed creator-channel
compensations. It is published with `TWO_VOICE=1`. Manage Channels or
Administrator must be established from trusted guild-role facts before any
store is constructed or queried; a payload permission claim alone is not enough.
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

## Disposable verification

The protected suite exercises real command dispatch, SQL evidence, restart,
concurrent cap enforcement, denial-before-SQL, paging and reconcile controls:

```sh
TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_local \
  python3 scripts/cargo_cache.py run -- test -p two-bot --bin two-bot --locked \
  voice_rooms::tests::orphan_ledger_acceptance:: -- --nocapture --test-threads=1
```

Use only the authorized disposable database. Never test against staging or
production, and never invoke live Discord writes for this suite.
