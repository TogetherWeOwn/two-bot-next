# Member data and operator erasure

This is an engineering inventory and runbook, not a new legal retention policy.
It describes the **migrated Next schema**, not legacy tables that do not exist here.
Discord-side message deletion, automatic retention timers and production execution
are outside this implementation's scope.

## What is stored

- Discord guild/member IDs, join/leave/activity milestones, invitation attribution
  and event metadata (`members`, `events`, `invite_snapshots`).
- XP totals, awards and cooldowns, rank projections and raid exclusions
  (`member_levels`, `xp_awards`, `xp_cooldowns`, `member_ranks`, `member_exclusions`).
- Join-risk evidence copied from legacy anti-raid scoring: member ID, account
  age, join time, score and reasons (`join_risk_flags`). Erased by member ID; the
  legacy TEXT `event_id` and `reasons_json` are also checked for the snowflake.
- Tickets: `tickets` and `ticket_transcripts` (opener or claimer; 90-day transcript purge applies independently).
- Event RSVPs and attendance/community facts, including attribution and compound
  voice-session/event keys (`event_rsvps`, `community_facts`). Scorecards normally
  store aggregates; the erasure plan also checks their serialized payloads.
- LFG posts, creators, role slots and member signups; feed configuration, creators
  and delivery state. Sticky bodies and their creator/editor IDs are stored. Scheduled messages store creator/editor IDs and are erased with them.
- Moderation, announcements, automation and operational audit rows, actor/target
  IDs, reasons and metadata. Some legacy-compatible payload columns are arbitrary
  TEXT, not database-validated JSON.
- Automod violation counts and once-per-message ledgers store member and message
  IDs and filter names, never content (`automod_violations`,
  `automod_processed_messages`). Per-delivery replay claims may keep a matched
  author ID (`automod_delivery_claims.matched_author_id`); its erasure needs a
  replay guard and is not yet in the manifest (TOG-12360).
- Self-role audit and panel claims store member IDs, role effects, lease/recovery
  state and outcomes. Settled member records are covered; unresolved role effects
  or active leases refuse erasure rather than discard reconciliation evidence.
- Internal action/replay ledgers store actor/target/resource IDs and outcomes,
  with hashed keys/nonces/event identities. Settings and immutable settings audit
  store operator attribution and JSON policy configuration.
- Aggregate counters/probes, gateway checkpoints and shared role/event
  configuration also exist, but are not member activity records.

The authoritative, ordered deletion manifest is
[`member_erasure_plan.json`](../crates/cutover/src/member_erasure_plan.json).
Each row matches the guild **and** any covered identity, not just the main subject
column. A row matching several columns is counted/deleted once. Known serialized
metadata and compound identifiers use digit-boundary matching for the supplied
snowflake, which also handles malformed TEXT JSON and nested numeric/string IDs.
It is intentionally conservative: an unrelated standalone numeric value equal
to that snowflake also matches. This is not a general personal-data recognizer:
encoded/hashed identities, names, arbitrary prose without the ID, or a novel
identity-bearing field require source review and an explicit manifest update.

## Retention and explicit exceptions

No new automatic age-based purger is installed by this change. Covered activity
persists until an authorized erasure or a separate existing feature lifecycle
removes it. Do not claim a 30/90-day TTL where no enforcing timer exists.

The manifest explicitly retains these control-plane records:

| Record/columns | Reason and operator follow-up |
| --- | --- |
| `guild_settings.updated_by`, `value` | Active configuration and its attribution, including staff/raid/test actor-ID lists. Blindly removing a policy can weaken or broaden safety/permission gates. Use a separate reviewed configuration change if an ID must leave policy. |
| `guild_settings_audit.actor`, `old_value`, `new_value` | Immutable configuration history; the database refuses DELETE, UPDATE and TRUNCATE, even a zero-row DELETE. No trigger disabling or audit bypass is permitted. IDs in prior/current policy remain here. |
| `audit_kill_switch.engaged_by` | Global incident-control attribution, with no guild scope. Member erasure never disengages or rewrites the halt. |
| `member_erasure_audit.actor` | Minimal operator accountability, retained independently of member activity. It is not the erased subject field. |

These exceptions currently have **no enforced expiry**. This CLI is not a claim
that all personal data is gone. A request involving retained policy/audit data
needs a separate security/privacy disposition, not a silent exception or bypass.
Guild-local erasure also does not delete NULL-guild internal records, global
hashed nonce/event replay guards, other-guild activity, existing backups,
external logs, or Discord messages/attachments.

Backups follow the existing [backup runbook](backup.md): retention is measured
in retained snapshots (default 14 newest), **not 14 days**. Historical snapshots
can contain erased records. Restrict backup access and prevent reintroduction
through a restore: re-apply separately authorized erasure requests before opening
restored data to users. The erasure receipt deliberately does not retain a subject
list. Backup destruction/rotation or a new retention policy is not authorized by
this CLI.

## Procedure

1. Verify an authorized member request, guild and exact Discord snowflake. Use an
   approved operator database identity with the required read/delete/lock rights
   and receipt-insert access. The command uses only `TWO_DATABASE_URL`; it never
   falls back to another credential or applies migrations. Provision migration
   `0410_member_erasure_audit.sql` through the normal reviewed migration process.
2. Pause the guild's ingestion, command/internal-action workers and replay queues
   through the existing operational procedure. Erasure is not a suppression or
   opt-out tombstone: live activity/backfills/replays can recreate data after
   commit. Resolve matching pending/unknown action intents through their existing
   reconciliation path first; never relabel them completed just to run erasure.
3. Dry run with the authorized database binding already supplied securely:

   ```sh
   two-bot erase-member --guild <guild-id> --user <member-id>
   ```

   Output is `table<TAB>count`, then `DRY RUN: no changes`. No audit row is written.
   Review counts and collateral scope: deleting a creator's LFG post also removes
   its roles and all associated signups; deleting a creator's feed removes its
   deliveries. Another member's unrelated post/feed/activity is preserved. A
   serialized row mentioning the subject is removed even if its main actor is
   someone else. Counts include these child rows explicitly, not hidden cascades.
4. After the deletion authorization and count review, set `TWO_ERASURE_ACTOR` to
   the accountable operator identity (nonempty, at most 128 characters, no control
   characters; never put a secret or the subject's content there), then run:

   ```sh
   two-bot erase-member --guild <guild-id> --user <member-id> --execute
   ```

   Exit 0 plus `ERASURE COMMITTED` is success. Argument refusal is exit 2;
   connection/transaction failure is exit 1 with redacted details. Any SQL,
   schema-coverage, replay-guard, count or receipt failure rolls back all deletions.
   A commit transport error has an uncertain outcome: inspect a fresh dry run
   before retrying; do not infer rollback solely from a failed client response.
5. A fresh dry run must report zero in every covered table. A second execute also
   reports zero and exits 0 (and writes its own minimal operator receipt).
   Resume ingestion only after deciding whether future activity is authorized.

Dry run uses one read-only repeatable-read snapshot. Execute locks the covered
and receipt tables in a consistent order before counting and deleting in **one
transaction**, with a 5-second lock timeout and 15-second statement timeout.
Execute reports the same per-table counts as dry run when no data changed between
runs; there is no cross-process snapshot guarantee. Locks briefly affect unrelated
guilds sharing these tables. Maintenance/quiescence is required, not a licence to
run a disruptive command against an active production service.

### Erasure receipt and output privacy

The transaction inserts exactly **operator actor + database timestamp** into
`member_erasure_audit`. There is no subject ID/hash, guild, request key, content,
counts, or reason in that table. stdout contains table names/counts only; errors
never print SQL, IDs or database URLs. The actor value is operator-supplied
attribution, not independently authenticated by this command; access control is
provided by the approved execution and database identity. Do not put subject IDs
in the actor label or capture credential-bearing environment variables.

## Coverage regression and acceptance tests

The integration suite applies **all** embedded migrations to its own disposable
database through `two-bot-testsupport`, then queries `information_schema.columns`.
Member/user names (including new `*_user_id`/`*_member_id` columns), legacy actor,
inviter/target aliases and `*_by` attribution require a manifest entry or a
nonempty, column-specific retention reason. Stale entries also fail. Both runtime
modes refuse an incomplete visible schema. The schema check is a naming tripwire,
not semantic proof of arbitrary JSON/prose or tables invisible to the DB identity;
operators need read access to the complete application schema.

Fixtures seed two members in two guilds across every deletion-plan table and seed
retained settings/audit/control records. Tests prove equal dry/execute counts,
byte-equivalent unrelated survivors, no covered remaining references, idempotence,
minimal receipts, nested inviter/session identifiers, explicit child counts,
refusal of unresolved side-effect guards, and full rollback on a late receipt
failure. A synthetic migration adding a recipient/observer `*_user_id` is a
negative coverage probe. CI's existing workspace integration step runs this suite
with its ephemeral Postgres service; configured test DB errors never silently skip.

On the controller, use the approved cache wrapper (never direct compiling Cargo):

```sh
TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_ci \
  python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --test member_erasure
python3 scripts/cargo_cache.py run -- test -p two-bot --bin two-bot erasure_cli
cargo fmt --all -- --check
```

The bootstrap database must already exist; the fixture creates/drops only its own
unique test database. Never use a staging/production binding for these tests.
