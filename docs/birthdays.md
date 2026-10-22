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
`TWO_ANNOUNCEMENTS` stops new cheers/grants but keeps this staging-only cleanup
job active until outstanding grants have been removed. There is no
midnight catch-up flood. Each guild/day can claim at most **25** cheers, lowest
numerical member ids first. Overflow celebrants still receive the cosmetic role.
Repeated ticks, concurrent sweeps, removal/re-enrollment and a fresh process use
the same persistent delivery fences; they cannot reset the daily cap.

A guild advisory transaction serializes the sweep with configuration and member
edits. Effect intents commit on separate connections before Discord writes.
Before a role PUT the runtime saves `(guild, member, role, celebration date)`.
Expiry, changed configuration and removed profiles revoke that exact role, even
if the previous process died before receiving the PUT response. Already-held
roles are not redundantly PUT; absent or departed members are not announced.
Stale roles on enrolled non-celebrants are cleaned too. The runtime does not
scan arbitrary non-enrolled Discord members.

Before a cheer POST the runtime commits a receipt with a deterministic nonce.
A confirmed send records its message id. **An unconfirmed send is never retried
merely because time passed or the process restarted**: Discord's nonce cache is
finite. This prefers an explicitly unresolved cheer over a duplicate. Readiness
reports `recovery_required` while any unconfirmed receipt remains. Inspect only
this guild's `birthday_cheers` rows with a null `message_id`, then inspect the
stored channel/nonce via the approved operator process. Record a proven existing
message id to settle the receipt. If delivery cannot be established, keep the
fence; do not delete it or retry blindly. There is no automatic release-by-age or
unreviewed repair endpoint.

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
