# Personal channel reminders

Use `/remind me in:<minutes> text:<text>` to schedule a reminder in 1–525600
minutes. Text may use up to 240 UTF-16 units, including safety formatting.
Use `/remind list` to view your pending reminders, or `/remind cancel
target:<id-or-full-text>` to cancel one. Each member can have five pending
reminders per server. Reminders require `TWO_AUTOMATIONS=1` and the existing
live-identity automations clearance, just like their delivery ticker.

- Any member of the configured guild may use the command; no staff permission
  is needed. Creating a reminder requires View Channel and Send Messages in
  that channel (Send Messages in Threads for threads), using the requester's
  resolved channel permissions including overwrites. Missing permission or
  channel metadata is refused before storing anything. List and cancel do not
  require posting authority. All command replies are ephemeral.
- The delay is an integer number of minutes. Natural-language dates, recurring
  reminders and DMs are not supported.
- Delivery goes to the original requesting channel, names the requester by
  literal user id, and never pings. Mass mentions and Discord invite hosts are
  neutralized without removing the surrounding reminder text. Reminder text
  is visible to everyone who can read that channel when delivery happens.
- List shows only your pending reminders in this server, including their ids,
  channels and due times. Cancel accepts a full id, a unique id prefix, or
  unique full reminder text. Unknown or ambiguous targets leave all rows alone.
- Invalid inputs and over-cap attempts add no reminder. A cancellation frees a
  slot; completed one-shots also free a slot.

## Delivery and recovery

The existing scheduled-message job checks every 15 seconds, at most ten
occurrences per tick. A reminder may be a little late while the bot is offline
or a backlog drains. A restart while waiting does not lose the reminder;
repeated ticks do not repost a completed occurrence.

Creation uses an idempotent canonical invocation hash in the reserved
`remind-` schedule-id namespace. Cancelled and completed rows remain disabled
as replay tombstones, so replaying an old request cannot re-enable it. Personal
rows do not consume staff schedule capacity and are excluded from staff
schedule listing, prefix resolution and deletion. Existing backups include the
rows, and existing member erasure removes them by creator/updater identity.
Cancellation invalidates an in-flight claim; the shared ticker attempts to
clean up any stale accepted message with the existing confirmed-delete path.

The durable pre-send uncertainty fence is deliberately conservative: a crash
at an ambiguous external-send boundary cannot authorize a new POST, even after
Discord's short nonce-deduplication window. This prevents duplicate deliveries,
not proof of successful delivery at every possible crash boundary. A held
reminder appears as **delivery held; inspect channel before cancelling** in
`/remind list`, consumes a pending slot and never automatically resends.
Inspect the original channel before cancelling it or making a new request.

No live staging/production execution is implied by the deterministic acceptance
tests. Command publication and deployment still follow their normal gates.
