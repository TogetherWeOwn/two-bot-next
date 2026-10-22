# Invite attribution and `/invites`

`/invites [member]` shows the selected member's net attributed invite count
(or your own when `member` is omitted), followed by the server's top ten
inviters. The reply is public, with mentions suppressed. The XP
`/leaderboard` remains separate.

## What counts

Only a witnessed, unambiguous invite-use increase credits an inviter. A first
snapshot establishes a baseline; it does not import Discord's historical use
counter. Unknown, vanity, expired/deleted, failed-read and ambiguous joins
never credit a guessed inviter. Invite counters cannot identify every join,
so these are **observed attributed members**, not a claim about all referrals.

The board uses **net** counts. Leaving removes the latest join's credit;
duplicate or uncredited leaves cannot drive a count below zero. A rejoin uses
its new attribution, not the previous inviter. Unknown rejoins do not retain
an old credit. Counts come from committed Postgres gateway state, not a
process-local counter, and survive a bot restart.

## Welcome attribution

Eligible welcome messages name the inviter from the member's latest committed
join, using a bounded, escaped display name (or a plain member ID fallback).
Only the welcomed member may be notified; the inviter is never added to the
mention allowlist. Unknown current joins and leaves never inherit an earlier
join's inviter. Suppressed attribution remains absent. This read does not
re-diff invite counters or bypass the existing welcome eligibility/dry-run gates.

Same-timestamp membership facts retain their durable event order. On a legacy
event-key collision, a distinct gateway membership delivery gets a stable
session/sequence suffix; the checkpoint still rejects replay atomically with
the effects. Non-membership event keys and existing first-delivery keys stay
unchanged. No historical rows are rewritten.

## Missing access

The bot must be able to read the server's invites (Manage Server /
`MANAGE_GUILD`). A denied read produces a permission notice for an admin,
not a zero-invite claim. Other invite-read failures report unavailability;
they are not evidence that nobody invited anyone. Restoring access cannot
reconstruct joins whose invite use was not witnessed.

## Scope and verification

This command does not enable production features, import legacy data, or
change the database schema. The protected gateway acceptance suite exercises
join/leave → committed gateway effects → a rebuilt command runtime. The
Discord attribution suite pins the snapshot edge cases. Use only the
disposable agent test database for local tests, through the bounded Cargo
wrapper; never point tests at staging or production.
