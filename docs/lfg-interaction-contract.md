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

## Applied state is not an audited success

LFG store mutations commit independently of the runtime's advisory-lock
transaction. If a signup, leave or close succeeds but its audit append fails,
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
