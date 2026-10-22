# Timed polls

Create a single-question poll with `/poll create question option1 option2 duration-minutes`,
adding `option3` through `option10` in order as needed. Questions must be
1–256 trimmed characters, each choice 1–80, and polls must have 2–10 choices
and last 1–10080 minutes (up to seven days). Each non-bot server member gets
one vote and can change it until expiry. Limits are 10 open polls per server,
3 per author, and 1000 distinct voters per poll; seated voters can still change
at the cap. The stored `max_voters` override may lower that cap, not raise it.
There is no public cap-override command.

Poll messages show current counts, the poll ID and changeable vote buttons.
Buttons are disabled at close. The author can close early using `/poll close id`.
Automatic close uses the same path: edit the poll, then post one results embed.
Ties go to the earliest-listed choice; zero votes produce zero counts and no
winner. Mass mentions are rendered inert and every message suppresses mentions.
No voter list or vote history is published or logged.

## Runtime and persistence

The existing `TWO_ANNOUNCEMENTS` flag and configured-guild fence cover creation,
votes, early close and the scheduled job. The boot identity further narrows
that flag. The `polls` job runs every 15 seconds through the shared supervisor,
which skips missed ticks and never overlaps one job's attempts. A tick processes
at most ten due, closed-unposted or dirty-display polls. Votes expire against the database
clock, not the next tick. Expect up to one cadence plus workload for the close
message, not a strict delivery deadline.

Migration `0458_timed_polls.sql` creates `polls` and `poll_votes`. Migrations
0459 and 0460 add the lookup indexes concurrently, each as one statement outside
a transaction. For an interrupted index build, inspect index validity and the
migration ledger before retrying; follow [migration recovery](migrations.md).
Migration 0461 adds the durable dirty-display marker. Apply the migrations and
reviewed database role inventory before deploying the binary.
Creation quotas share a guild advisory lock. A vote and its dirty-display marker
commit before the live edit, so cancellation cannot undo a displayed vote.
The edit re-reads durable counts under the same row lock as close, rather than
sending a stale pre-close snapshot. Failed/cancelled edits remain dirty and the
next tick retries only the idempotent message PATCH. The vote's live edit
completes before the acknowledgement settles. The
closed row freezes final counts so erasing a voter later does not rewrite the
published aggregate. Member erasure removes that member's votes and authored
polls (including their question/options); author deletion removes dependent
votes first. Guild erasure uses the ordinary guild-local inventory. This is
not an anonymous poll service: member IDs are stored for vote changes.

## Delivery recovery and limitations

A restart automatically closes due polls and recovers closed polls that have
not yet started results delivery. Competing recoverers share the same durable
fence. Creation reservations and results send fences commit **before** sending.
A confirmed result receipt atomically records `results_posted_at` and clears
the in-flight fence. Stable Discord nonces also suppress immediate duplicates,
but their short lifetime is not durable replay safety.

An ambiguous send (including process cancellation after the fence commits)
is **not automatically repeated**. This favors no duplicate results over silently
sending a second message. It does not promise exactly-once delivery across the
Discord-send/database-settlement crash window. An open poll without a message
receipt or a closed poll with an armed results fence requires reconciliation;
there may have been no send, or Discord may already have accepted it. Expired
message-less reservations are closed and fenced without any creation/results
send, freeing quota while preserving the unresolved receipt for inspection.

Inspection is read-only first:

```sql
SELECT id, channel_id, message_id, status, ends_at, closed_at,
       results_started_at, results_posted_at, results_message_id
FROM polls
WHERE guild_id = :guild
  AND (message_id IS NULL OR
       (status = 'closed' AND results_posted_at IS NULL));
```

Inspect the corresponding Discord channel and the deterministic `p-<id>`
(creation) / `r-<id>` (results) nonce under the bot's identity. A timeout,
missing receipt, denied history or bounded history search is not proof of
absence. Do not clear a fence or replay a send until the authorized operator
has proven absence or recorded the accepted message receipt. There is no
member-facing replay/reset command. Never log message content, raw errors,
voter IDs or callback tokens during investigation.

Rolling back the binary leaves these additive tables intact; do not drop poll
rows or delivery evidence. Backup and rollback-delta inventories include both
tables. A delta-only export does not replace the full baseline's parent poll
rows; preserve the baseline when importing child vote deltas.

## Verification and rollout evidence

`crates/discord/tests/poll_timed_acceptance.rs` exercises slash creation,
changeable votes, bounds/caps, bots, hostile mentions, automatic/author close,
concurrent recovery and restart behavior with MockRest and a disposable DB.
These are fixture tests, not live Discord evidence.

Before claiming runtime delivery, use an authorized staging fixture: create a
one-minute poll, cast/change two fixture-member votes, verify counts, wait for
the supervised close, confirm disabled buttons and one results embed, restart
and confirm no second results message. Capture the exact deployed SHA and
`polls` job health. Repeat author-close on a separate poll and confirm a
non-author refusal. Follow the existing deployment, identity and rollback
contract; never run the acceptance suite against staging or production.
