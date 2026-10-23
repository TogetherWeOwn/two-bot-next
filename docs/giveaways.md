# Giveaway creation and entry

`/giveaway create`, `/giveaway list` and `/giveaway cancel` use the automations
gate and require **Manage Server**. The shared router checks resolved permission
bits, not just picker visibility. Entry buttons are open to human server members;
roles and ages are checked against the member evidence on each interaction.

- Prize: 1–200 characters after trimming. The picker advertises a 2000-character
  transport ceiling for compatibility; the option help and runtime enforce the
  narrower domain limit.
- Duration: 1–43200 minutes. Winners: 1–100.
- Optional required role, minimum account age and minimum server age (0–3650 days).
  Required-role admission uses the member's held roles, not permission overrides.
- At most 25 active giveaways per guild and 10000 entrants per giveaway.
- Entry attempts have a bounded process-local 10/minute member budget. Successful
  entry writes also share a durable 10/minute guild/member budget across giveaways
  and restarts. Duplicates do not insert another row or refresh the count.
- Missing or future join evidence is refused. Bots cannot enter.
- `gw:<id>` carries only the opaque giveaway id. The server resolves its guild,
  message, deadline, status, requirements and capacity; none comes from the button.

Creation and cancellation commit their audit records with the state mutation.
Entry admission locks the giveaway before checking its deadline, status, capacity
and uniqueness, so parallel entry/cancel cannot oversubscribe or reopen it.
All confirmations and refusals are ephemeral; public messages suppress mentions.

## Message delivery and recovery

The giveaway is recorded before the message is sent. A confirmed message id is
attached once. If sending or attachment fails, do **not** blindly recreate it:
inspect the channel for a delivered message and cancel the recorded id if needed.
An unconfirmed definition consumes an active slot and does not accept entries.
No automatic re-post loop can duplicate an uncertain send.

Successful entry updates the count. Discord renders the relative deadline itself.
The existing supervisor runs `giveaway_countdown` every 15 seconds (missed ticks
skip); durable claims permit at most one cosmetic refresh at each 1h, 10min, 1min
and 30s/ending-soon threshold. All edits pass through the shared REST rate-limit
and pacing guard. A crash after a refresh claim may miss that cosmetic edit,
not repeat it indefinitely. The entry row remains authoritative if a count edit
fails; a later entry or threshold can refresh the display.

This slice does **not** draw winners or send winner DMs. Entry closes at the
stored deadline regardless of the displayed message; timed draws, announcements,
rerolls and pause/resume/extend belong to the subsequent draw slice.

## Verification boundaries

Automated database tests use only a disposable agent-testdb/CI database. Staging
smoke is a separate rollout check: create a giveaway, enter from two test member
accounts, verify two rows/count and ephemeral replies, cancel, and verify a later
entry is refused. A passing fixture suite is not evidence that this live smoke
has run.
