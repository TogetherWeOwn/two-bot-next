# Birthdays

Opt-in birthdays store a month and day, **never a birth year or age**. The
command and job use the existing announcements gate and boot-identity policy;
this change does not clear a production identity or enable production delivery.
Apply the additive birthday migration and the reviewed database role plan before
starting the runtime. The runtime performs no schema changes.

## Member commands

- `/birthday set month:5 day:1` saves May 1, replacing your previous choice.
- `/birthday show` returns only your own month/day, privately.
- `/birthday remove` deletes your month/day. An outstanding birthday role is
  removed on the next sweep; its cleanup intent survives the deletion.

Replies are immediate and ephemeral. Invalid dates, year input, unknown or
repeated options, and target-member options are refused without changing state.
February 29 is valid; in non-leap years its cheer is February 28. Days are UTC,
not the invoking member's timezone. A server can enroll at most 100 members;
existing members can correct their date even when the list is full.

## Configuration

`/birthday config channel:<text-channel> role:<cosmetic-role> cheer:"HBD {user}!"`
requires **Manage Server**, enforced by the router. Each option is optional;
omitted values preserve the current configuration. Channel-only configuration
posts cheers without roles; role-only configuration grants roles without posts.
With neither configured, a sweep writes nothing to Discord.

The channel must be a text/news channel in the same server. The role must be
unmanaged, not everyone, have **zero permissions**, and have no channel access
grants through overwrites. These checks run both when saving and when sweeping;
changing a role later cannot turn a birthday opt-in into a privilege grant.
Discord additionally enforces the bot's Manage Roles capability and hierarchy.
If the role is moved above the bot or writes are refused, the job reports failure
rather than dropping its cleanup witness.

Cheer templates are nonempty, at most 200 Unicode characters, have no control
characters, and cannot contain mass-pings or literal member/role mentions.
`{user}` names the celebrant; if absent, the mention is appended. Rendered text
stays within 200 characters and every post uses `allowed_mentions.parse = []`:
**the cheer names the member without pinging anyone**. Replies never echo the
configuration input. The default is a short birthday greeting.

## Daily delivery and recovery

The supervised `birthdays` job reconciles every five minutes with no overlapping
runs, a 180-second deadline, and a UTC-date delivery identity. Disabling
`TWO_ANNOUNCEMENTS` (unset or `0`) stops new cheers/grants but keeps this
staging-only job active to remove **tracked grant intents**, including after a
restart with the delivery flag off. Registration depends on boot identity,
not that flag; an unauthorized identity cannot run cleanup. Gate-off ticks do
not scan untracked role holders. There is no
midnight catch-up flood. Each guild/day can claim at most **25** cheers, lowest
numerical member ids first. Overflow celebrants still receive the cosmetic role.
Repeated ticks, concurrent sweeps, removal/re-enrollment and a fresh process use
the same persistent delivery fences; they cannot reset the daily cap.

A guild advisory transaction serializes the sweep with configuration and member
edits. Command reads/writes use that same transaction and commit before success
or success logging; cancellation rolls them back before releasing the lock, so
a timed-out set cannot later resurrect a removed opt-in. Sweep effect intents
commit on separate connections before Discord writes.
Before a role PUT the runtime saves `(guild, member, role, celebration date)`.
Expiry, changed configuration and removed profiles revoke that exact role, even
if the previous process died before receiving the PUT response. Already-held
roles are not redundantly PUT; absent or departed members are not announced.
Only a definitive 404 establishes departure. A forbidden or unreadable member
lookup retains its cleanup intent and reports failure, never pretending the
member left or silently clearing the role witness. Stale roles on enrolled non-celebrants are cleaned too. The runtime does not
scan arbitrary non-enrolled Discord members.

Before a cheer POST the runtime commits a receipt with a deterministic nonce.
A confirmed send records its message id. **An unconfirmed send is never retried
merely because time passed or the process restarted**: Discord's nonce cache is
finite. A typed executor refusal that proves **no effect** releases only that
attempt's fence, permitting a later tick to retry after the failure is fixed;
unknown wire outcomes never take that path. This prefers an explicitly unresolved
cheer over a duplicate. Readiness
reports `recovery_required` while any unconfirmed receipt remains. Inspect only
this guild's `birthday_cheers` rows with a null `message_id`, then inspect the
stored channel/nonce via the approved operator process. Record a proven existing
message id to settle the receipt. If delivery cannot be established, keep the
fence; do not delete it or retry blindly. There is no automatic release-by-age or
unreviewed repair endpoint.

### Operator settlement of a confirmed cheer

Use the existing Operator database-change contract, not a member command or
an ad hoc production test. This slice authorizes no live database mutation.
Read the unresolved receipt for one guild; do not read profile month/day rows:

```sql
SELECT guild_id, member_id, celebration_date, channel_id, nonce
FROM birthday_cheers
WHERE guild_id = $1 AND message_id IS NULL;
```

Before settling, establish a message by **this bot**, in the stored channel,
with the stored nonce and matching celebrant/date. A matching greeting alone
is not proof. Preserve the message link and inspection evidence in the change
record. With those exact values and a validated Discord message id, the
Operator's reviewed transaction takes the same guild lock as the sweep:

```sql
BEGIN;
SELECT pg_advisory_xact_lock(hashtextextended($1, 455));
UPDATE birthday_cheers SET message_id = $7
WHERE guild_id = $1 AND member_id = $2 AND celebration_date = $3
  AND channel_id = $4 AND nonce = $5 AND message_id IS NULL
  AND $6 = 'confirmed_existing_message'
RETURNING guild_id, member_id, celebration_date;
-- Commit only after verifying exactly one returned row; otherwise ROLLBACK.
COMMIT;
```

Bind parameters; expect exactly one returned row. Zero or multiple rows is a
failed contract, not a successful repair. Never manufacture a message id,
mark an uninspectable/deleted channel as delivered, delete a fence, or reset it
to force another POST. If evidence is unavailable, retain `recovery_required`
and record the unresolved erasure/purge hold for the Operator; there is no
safe automatic release. An incorrect settlement can be undone under the same
lock by clearing only that receipt's `message_id`; its daily fence stays in
place, so undo never authorizes a duplicate send.

Month/day rows are deleted by member erasure. Role intents and same-day or
uncertain cheer receipts block erasure until cleanup/reconciliation, preventing
PII deletion from silently losing role recovery or reopening a delivery fence.
Older settled receipts can then be erased. Birthday event logs carry only guild
and member identifiers, never month/day, templates, tokens or raw database errors.

## Verification

Use only the disposable test service, never staging or production databases:

```sh
TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_local \
  python3 scripts/cargo_cache.py run -- test -p two-bot --bin two-bot birthday_acceptance -- --test-threads=1
```

Without `TWO_TEST_DATABASE_URL`, database acceptance cases skip and their green
status is not evidence of persistence, delivery or recovery. Registry/reference
checks still run. Regenerate the command reference through the documented
`reference_docs` test; never hand-edit it.
