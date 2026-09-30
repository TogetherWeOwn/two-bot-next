# Legacy Postgres copy (implementation in progress)

The copy engine is in `crates/cutover/src/legacy_copy.rs`. The operator binary,
source-backed mapping registry and two-database acceptance suite are not yet
implemented. This document is a checkpoint, **not cutover authorization**.
No real database has been read or written while building this tool.

## Engine contract

- Every table has explicit source/target columns, target Postgres conversion
  types, a source primary-key tuple and a target conflict tuple.
- Tables resolve only in `public`. Identifiers are quoted; table, column and
  conversion names must come from compiled mappings, never operator input.
- The source is one UTC, read-only, repeatable-read transaction. All selected
  source counts, source projections and target INSERT plans are resolved before
  the first target write. `EXPLAIN` does not run the INSERT.
- Dry run (`apply = false`) only returns source counts. It runs no target DML
  and does not run migrations on either database.
- Copy pages use the source primary key's native PostgreSQL types and order,
  reconstructed from a JSON cursor. There is no OFFSET or lexicographic numeric
  sorting. Batches are bounded at 1–10,000 rows.
- Each target batch is one atomic INSERT/ON CONFLICT statement. Conflict updates
  are conditional on mapped non-key columns being distinct; unchanged rows are
  not rewritten. Unmapped target columns are not updated.
- Restart from the beginning after interruption. Already committed batches
  replay as no-ops; a lost acknowledgement cannot create duplicate primary keys.
  This is replay-based resumption, not an external checkpoint protocol.
- Stop legacy writers before the final cutover copy. A snapshot is internally
  consistent, but this tool does not capture changes after its start, remove
  rows deleted at the source, or arbitrate concurrent next-bot writes.

## Schema evidence at this checkpoint

This checkout contains **16 next SQL migrations / 41 tables**, all under
`crates/cutover/migrations`; `crates/store/migrations` is absent. Migration
headers record legacy ancestry, but a complete authoritative legacy
`0001–0042` migration set is not vendored here. The checked-in
`crates/core/tests/fixtures/legacy_sticky.sql` is a frozen subset only.
The shared mapping registry/spec is being coordinated with the separate
verification-tool task; the copier must not invent a divergent schema.

Important mapping constraints from the shipped target DDL:

- Not every timestamp-looking column is TIMESTAMPTZ. Website-contract,
  presence, community and channel-moderation timestamps remain TEXT.
- Discord IDs and permission masks remain TEXT. Some audit IDs are TEXT,
  while events, XP awards and community facts use BIGINT IDs.
- `guild_settings_audit` rejects UPDATE/DELETE/TRUNCATE. It requires an
  insert-only replay policy, not the generic conflict-update engine policy.
- Settings revision/trigger behavior and generated BIGSERIAL sequences need
  explicit reconciliation. A correct row copy alone is insufficient.
- Gateway sessions have a next table but no verified local legacy DDL.
- Alternate unique keys (event idempotency, reward role IDs, scorecard run
  identity, etc.) must fail explicitly on collisions, never overwrite an
  unrelated primary-key row.
- Parent tables must precede FK children (for example rank ladder and LFG
  posts/roles/signups).

Target schemas are absent for tickets/transcripts, automod ledgers,
moderation warnings/scheduled unbans, containment/risk ledgers,
automation commands/scheduled messages, self-role panels/audit,
feed relays/deliveries and invite campaigns. They must remain **pending**
until source and target DDL are available. Rota extensions are explicitly
retired rather than pending.

## Required before shipping

1. Vendor authoritative legacy SQL (0001–0042), recording its source revision;
   map it to the next migrations actually shipped in this checkout.
2. Add the `legacy_copy` binary with dry-run default, explicit `--apply`, strict
   argument validation and a fail-closed `--allow-live-target` fence. URLs must
   not appear in output. No fallback credentials.
3. Publish the per-group, per-column mapping table here. Name pending groups
   and refuse their selection before any database connection/write. Do not
   guess schemas from feature names or silently drop legacy columns.
4. Handle generated identities/sequences explicitly where the schema requires
   them; preserve operational replay semantics rather than enqueueing work
   accidentally.
5. Create two uniquely named scratch databases behind the existing disposable
   test-database guard; apply vendored source migrations and shipped target
   migrations, then assert every mapped column, dry-run no writes, unchanged
   replay and recovery after a committed-batch interruption.
6. Ship one PR with exact-head CI and independent Code Reviewer approval/merge.

Tests only use `agent-testdb` / the CI Postgres service. Executing this tool
against real databases and building a verification/diff tool are out of scope.
