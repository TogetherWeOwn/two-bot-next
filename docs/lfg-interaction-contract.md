# LFG discovery, Select validation, audit loss and redelivery

## Find open game posts in this channel

`/lfg-list` requires Manage Events and returns an ephemeral list from the invoking
channel only. It retains the announcements and configured-guild gates, but an
automation pause does not block this read. No new role grants are required.

Only open posts with a confirmed, canonical Discord message identity are listed.
Unconfirmed/delivery-uncertain and closed posts are excluded. Today's LFG policy
has no automatic start-time expiry, so past starts remain visible and labeled
`past`; unreadable start times are labeled `unknown`, never fabricated.

Rows contain the stable management ID, inert complete title, Discord-localized
start time and validated guild/channel/message jump link. No signup, creator or
role data is selected. The SQL query filters guild and channel before its fixed
six-row limit (five rows plus lookahead), ordered by creation time then ID. It
uses no OFFSET, counts or Discord history scan.

Each page contains at most five whole rows, possibly fewer to fit Discord's
2,000 UTF-16-unit content limit including its footer. Copy `/lfg-list cursor:<token>`
to continue in the same channel. Restart without a cursor when a page is invalid
or stale, including after a process restart. Cursors are versioned authenticated
keysets bound to this guild, channel and query; they carry no foreign-channel
counts. Concurrent closures are skipped and earlier inserts appear on restart,
not through offset-based skipping. End and empty states are explicit. Store
failure is an unavailable reply, not an empty list. A malformed or individually
oversized persisted row fails explicitly instead of silently shortening it.

Discovery is separate from mutation/delivery code: no signup/close operation,
audit append, channel write, identity/history lookup or delivery-fence reset.
`crates/discord/tests/lfg_list_acceptance.rs` exercises the actual interaction
path and bounded SQL against disposable fixtures and loopback Discord only;
it is not staging or production evidence.

The Select parser and direct interaction service validate post IDs with the
same `lfg::valid_post_id` predicate as Close: nonempty and at most 128 UTF-16
units. Whitespace is not trimmed; multibyte UTF-8 byte length is not the
bound. Invalid Select IDs produce no store, audit or channel operation.

## Edit an existing game post

`/lfg-edit id:<id> title:<optional title> starts-at:<optional RFC3339 timestamp>`
requires **Manage Events**, enabled announcements and an open post in this
guild. Supply at least one metadata field. The ID is nonempty and at most 128
UTF-16 units; titles use the existing trimmed 1–100 UTF-16-unit limit. A supplied
start time must be in the future and is normalized to UTC. Empty optional
values are invalid, not omitted.

Only `title` and `starts_at` change. Post/channel/message IDs, author, creation
time, role definitions, capacities and every signup (including join timestamps)
are preserved. Omitted timestamps retain database precision. A dedicated store
transaction takes the signup/close lock; the existing per-post runtime lock
serializes mutation plus refresh with signup, leave and close across instances.
An edit never reopens a closed post or creates a replacement message.

Normalized identical metadata returns `LFG unchanged` without a domain/audit
write or channel refresh. A changed edit returns `LFG edit saved` and the
canonical saved start time, then refreshes the existing message with mention
suppression. Invalid metadata, missing/wrong-guild IDs and closed posts produce
concise corrections with no domain/audit/channel mutation.

The protected `crates/discord/tests/lfg_edit_acceptance.rs` exercises the real
router, store and REST executor with disposable migrated databases and loopback
Discord, including tuple preservation, concurrent select/leave/close orderings,
no-op/refusal effects, hostile text and post-commit failures. This does not claim
staging usage or activate the feature in production.

### Increase one role's seats

`/lfg-edit id:<id> role:<existing role key> seats:<absolute total>` uses the
same configured-guild, announcements and **Manage Events** gates on every
interaction. The selector is the stable signup key (for example `tank`), not its
label or a Discord role ID. Supply `role` and `seats` together, without `title`
or `starts-at`; metadata edits remain a separate mode of the same command.
Unknown, duplicate, mistyped, unpaired or mixed-mode options refuse before any
mutation. Keys use the existing 1–32 ASCII letter/digit/underscore/hyphen bound;
seats must be a strict integer from 1 to 99, not a delta.

Only an open post whose start is still in the future can change. A dedicated
transaction takes the shared signup/close advisory lock, the guild-fenced post
row lock, then the selected role row lock. After waiting, it checks the current
capacity and execution-time clock. A smaller target refuses, including a stale
request behind a newer increase. An equal target returns `LFG seats unchanged`
with no row rewrite, audit append or channel refresh, including a replayed
successful target. A larger target updates only that role's `slots` in place.
Post metadata, lifecycle, role identities/labels/order, message identity and
all signups with their original join times remain untouched.

Each saved increase commits one bounded `lfg.edit.seats` / `increased` audit in
the same transaction. An audit insert failure or identity collision rolls back
the capacity update. No refusal or no-op writes an audit. The runtime retains
the per-post lock through one mention-suppressed PATCH to the existing message;
it never creates a new public message or pings members. The private receipt
says `LFG seats saved`. If the PATCH fails, it still says saved, then explains
that the visible post could not be refreshed. Check saved state before retrying:
an equal target remains a no-op, not a refresh retry. No claim of exactly-once
Discord delivery or automatic refresh recovery is made.

`crates/core/tests/lfg_seat_edit_catalog_acceptance.rs` pins publication/help.
`crates/discord/tests/lfg_seat_edit_acceptance.rs` covers router/store/REST
behavior, tuple preservation, private replies, stale targets and actual lock
contention with signup, close and metadata edits in disposable fixtures. These
are hermetic checks, not production or staging activation evidence.

## Applied state is not an audited success

Except for the atomic seat/audit transaction above, LFG store mutations commit
independently of the runtime's advisory-lock transaction. If a signup, leave or
close succeeds but its audit append fails,
the service returns the actual outcome followed by:

> Audit could not be recorded; check the saved state before retrying.

It does not invent an audit row, echo the storage error or refresh the channel
after that failure. The existing `Message refresh failed; the saved state is
unchanged.` suffix still applies when the audit succeeds but refresh fails.
Create/nonce recovery keeps its existing failure contract.

For metadata edits the receipt always says the edit was saved and shows its
canonical start time. An audit failure additionally says `Audit could not be
recorded; the visible post could not be refreshed` and skips refresh, retaining
the independent domain commit. A failed Discord PATCH retains the edit and
its audit and says `The visible post could not be refreshed.` Neither failure
creates a replacement message or implies that saved metadata rolled back.

Audit loss is not atomic rollback or exactly-once delivery. Without a saved
audit row there is no durable replay receipt; inspect saved state before
retrying. Likewise, this change does not address a crash between the store
commit and audit append. It adds no migration or separate replay store.

## Closed-post refresh repair

Closing a post commits the closed row before its Discord PATCH runs. When
that edit fails (a 403, a timeout, a restart), the close transition leaves a
durable repair intent behind instead of losing it: the lifecycle sweep keeps
retrying the exact saved guild/channel/message under the existing runtime
lock and send-admission fences, with durable backoff (10 minutes after the
first failed repair, doubling per failure and capped at 6 hours). Repair
candidates share the sweep's 20-row cap and per-tick budget, so one stuck row
cannot starve expiry work. A confirmed
edit settles the intent, as does a definitive Unknown Message outcome
(Discord code 10008); a bare 404 without that code stays retryable. The
repair never recreates a message, sends, deletes or reopens, and historical
closed rows are never reclassified as pending.

Each terminal attempt logs a content-free `outcome` (`repaired`,
`settled_missing`, `retry_scheduled`) with an `error_class` in fixed
vocabulary (`unknown_message`, `rejected`, `timeout`, `unavailable`,
`rate_limited`, `admission_blocked`, `guard_*`). No member text, SQL or
remote bodies appear. A `retry_scheduled` line needs no action: the next
eligible tick retries on its own. Pending state lives on the closed row
(`close_refresh_pending`, `close_refresh_attempts`, `close_refresh_next_at`)
and clears only on a confirmed edit, a definitive Unknown Message outcome,
a closed post with no saved message to repair, or erasure of the row itself.
If a closed post first gains its saved message later (recovered from channel
history), saving the identifier re-arms the same repair intent, so stale
controls on the late-bound message keep a durable retry.

## Bind a receipt to its request

Both signup and leave use `lfg:signup:<interaction-id>` as the shared audit ID,
retaining the existing signup namespace. The `reason` column holds
`lfg-select:v1:<sha256>`: a digest of the version, guild, actor, post, action
and role (empty for leave). Each field is prefixed by its UTF-8 byte length as
a big-endian u64 before hashing. The 78-byte result avoids storing arbitrary
role text. The format is versioned and pinned by unit tests.

An interaction-scoped advisory lock precedes the existing per-post lock,
serializing even deliveries that name different posts or guilds. Under those
locks, the service looks up the receipt by interaction ID **before** filtering
request fields. An existing receipt with a different or absent binding refuses
without mutation, audit append or channel refresh. This includes legacy rows
without a binding: their original role cannot be proved, so they cannot safely
be replayed. Inspect the saved state and use a fresh interaction when needed.

An identical bound request replays its original outcome and may retry message
refresh, but does not reapply the mutation or append a second audit. A signup
replay cannot resurrect a later leaver; a leave replay cannot remove a later
signup. This protection lasts while the receipt is retained, not indefinitely.

`crates/discord/tests/lfg_select_audit_acceptance.rs` exercises the real service
with offline SQL/store/transport boundaries. It proves validation and reply/
replay decisions, not PostgreSQL locking, transactions or a live deployment.
The disposable-database runtime suite is
`crates/discord/tests/lfg_interactions.rs`. Neither suite authorizes feature
activation, production tests or deployment.
