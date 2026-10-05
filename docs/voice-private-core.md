# V3b private-room core integration seam

`two_bot_core::voice_private` is an original, pure implementation derived only
from [the approved voice-room specification](voice-rooms.md#v3-owner-room-controls).
It requires no `db` feature, Discord wire types, clock, store, or external I/O,
and it depends on no V1 lifecycle state. V3a (limits, bitrate, unique names) is
a separate slice; this module covers only the privacy half of `/private`,
`/public` and the "⇩ Join ‹owner›" request flow.

## Inputs and decisions

- `PrivateRoom`: one room's privacy state. `owner_id` is the current owner and
  `owner_display` the name (or `/nick` name) the Join channel is named after.
  `private` is the flag, `join_channel` the optional companion channel, and
  `blocked` the room's block list. `granted` tracks Connect allows from approved
  requests, `pending` holds at most one request per member, and
  `next_request_id` allocates request IDs that are never reused within a room.
  All IDs are plain newtypes over `Snowflake`; zero IDs are refused.
- `make_private` (`/private`): denies Connect to @everyone while leaving View
  Channel untouched, so the room stays visible, and plans a "⇩ Join ‹owner›"
  voice channel directly next to the room. Repeating it is a no-op; on a private
  room whose Join channel was lost it plans only a new Join channel.
- `make_public` (`/public`): restores @everyone Connect, revokes per-member
  grants, deletes the Join channel and withdraws pending requests. The block
  list is kept: it belongs to the room, survives privacy toggles and ownership
  changes, and dies with the room. Repeating it is a no-op. A creation still in
  flight is forgotten here and deleted when it arrives.
- `enter_join_channel`: someone entered a channel. Only the room's current Join
  channel counts; anything else (a deleted or replaced channel, or entry while
  public) is a stale no-op. The owner, a current occupant, or an approved member
  raises no request. A blocked member is silently ignored with no message to
  anyone. Anyone else raises exactly one pending Approve / Deny / Block request
  for the owner; re-entering while it is pending deduplicates.
- `decide`: answers a request. **Only the current owner decides; admin status
  is not an input and admins may not answer requests in this core.** Approve
  grants Connect on the room and moves the member in; Deny grants nothing and
  the member may request again (with a fresh request ID); Block adds the member
  to the block list. A request raised to a previous owner, or in an earlier
  private period, is no longer pending and is refused. The runtime must bind the
  buttons to the request ID: IDs are never reused, so a stale button can never
  match a new request.
- `set_owner`: records the owner and display name after any V2 ownership change
  or `/nick` update. Privacy, grants and the block list are kept; requests
  raised to the previous owner are withdrawn (ownership coming back does not
  revive them). The Join channel is renamed only when its name actually changes,
  so a new owner with the same display name plans no rename.
- `delete_room`: withdraws pending requests and plans deletion of the Join
  channel. A creation still in flight must be deleted on arrival. The runtime
  then forgets this state.
- `join_channel_created` / `join_channel_creation_failed` /
  `join_channel_deleted`: reconcile the planned channel with reality. Only the
  channel the room is waiting for is kept (renamed if its name went stale while
  it was being created); duplicates and orphans are deleted. Failures and manual
  deletions are forgotten quietly so `/private` can plan a new channel; the room
  stays private throughout.

The Join-channel name is "⇩ Join ‹owner›", trimmed and cut to Discord's
100-character channel-name limit. Every transition validates first and never
repairs a corrupt state: a public room must have no Join channel, grants or
pending requests; a blocked member must hold no grant or pending request; every
pending request must match its member and the current owner.

This core does not authorize `/private` or `/public` themselves: gate those
owner commands with `voice_ownership::require_room_owner` first. That gate
covers the commands; answering requests stays owner-only here.

## Replay and concurrency contract

The same inputs always produce the same plan. `make_private`, `make_public`,
replaying the current Join channel, re-entering with a pending request, and
`set_owner` with no effective change are all no-ops returning unchanged state
with no effects. Deciding an already-answered request finds nothing pending.

The caller must serialize events per room, persist the returned state atomically
against the state it was computed from, and deduplicate interaction/event IDs.
Stale snapshots and delayed event replays are **not** detected by this pure
core. No exactly-once Discord effects or crash recovery are claimed by these
tests.

## Runtime wiring: `/private` and `/public`

`crates/bot/src/voice_rooms/private_runtime.rs` runs this core inside the
guild worker. It covers the two owner commands, the persisted privacy state and
the Join channel. The join-request flow is in
[Runtime wiring: join requests](#runtime-wiring-join-requests).

- **Gate.** The caller must be in the room and be its owner or hold Manage
  Channels (`require_room_owner`). Both commands are idempotent and reply
  ephemerally with what was queued, not that Discord finished; a failed write is
  recorded as a lifecycle failure and shows in `/setup`.
- **Discord first, state after.** `/private` and `/public` queue one
  `RoomAction::SetEveryoneConnect`. The private flag, the Join-channel plan and
  the stored record change only after that write lands, so a refused write
  leaves the room as it was. A repeated `/private` on a private room whose
  @everyone overwrite no longer denies Connect re-asserts it. `/public` on a
  room whose stored flag is public but whose @everyone overwrite still denies
  Connect (a room an `/alwaysprivate` creator made) lifts that deny; with no
  deny it changes nothing.
- **The overwrite.** Only the Connect bit moves; every other @everyone bit (View
  Channel included) is carried over unchanged, because the write replaces the
  whole entry. Manage Roles is never emitted as an allow, on any overwrite
  this slice writes. When denying @everyone would leave the bot unable to manage
  the room, a bot-member allow for exactly the missing bits is written first.
  `/public` removes only the Connect deny; an explicit @everyone Connect allow
  that `/private` stripped is not re-added.
- **The Join channel.** Created once in the room's category, directly after the
  room, with no overwrites (it syncs to the category). A retry after an unknown
  outcome adopts the channel the live snapshot shows instead of creating a second
  one. It is deleted on `/public`, and with its room (ahead of the row, so a
  failed delete retries the whole room delete instead of leaking it). On
  `/public` the delete runs inside the same retried action, after the Connect
  write and before the flag flips and the record is saved, so the stored Join id
  outlives a restart until the channel is gone and `/public` can run again to
  finish the job. The Join channel's name carries the owner's display name only
  after it is sanitized like a `/create` name and passes the name filter. The
  worker only deletes Join channel ids it created or loaded from its own store.
- **Persistence** (`0416_voice_room_privacy`). `voice_rooms.private` and
  `join_channel_id`, plus `voice_room_blocks` rows that cascade with the room.
  `PrivacyRecord` is the durable subset of `PrivateRoom`; a corrupt record is
  refused at load rather than repaired.
- **Restart.** A stored Join channel that is in the live snapshot is adopted;
  one that is not is forgotten and the forgetting persisted. The room stays
  private; `/private` plans a new Join channel. Nothing is forgotten without an
  authoritative snapshot that still shows the room.
- **Not yet.** Renaming the Join channel when ownership or the owner's name
  changes (`set_owner`), and persisting grants.

## Runtime wiring: join requests

`crates/bot/src/voice_rooms/join_requests.rs` runs `enter_join_channel` and
`decide` inside the same worker.

- **Entry.** Each reconcile looks at who sits in a private room's Join channel.
  A member counts as having *entered* when the gateway recorded a new
  transition for them, so one stay raises one request: an owner's Deny does not
  raise the same member again every tick. Leaving and coming back asks again
  under a fresh id. Bots are skipped; the core already ignores the owner,
  occupants, approved members and blocked members (silently).
- **Prompt.** One `RoomAction::AskJoinOwner` posts a message in the room's own
  chat with three buttons,
  `two:voice:join-approve|deny|block:<room>:<request>`. Only the owner is pinged;
  the requester is named by id. There is no DM fallback: a press on a DM carries
  no guild, so it could not be routed or role-gated. A request answered or
  withdrawn before the queue reaches the prompt posts nothing. When the bot
  cannot post in the room's chat the request is dropped (the failure shows in
  `/setup`) instead of sitting pending behind buttons nobody got.
- **Answer.** A press runs the guild's `/private` role gate, then the worker
  checks the room's *current* owner (admins do not count) and the request's
  current state. A stale, answered or withdrawn button, and one minted before a
  restart, is answered with an ephemeral refusal and changes nothing; the
  worker never stays silent on a press it owns. Ids outside the three join
  verbs stay with their own handlers. The answer replaces the prompt in place
  (no buttons, no pings).
  - *Approve* queues one write: a Connect allow for that member on the room
    only (never Manage Roles, other bits kept), then a move from the Join
    channel, but only if the member is still there. It refuses when the member
    carries a Connect deny from a passed vote-kick: an owner cannot undo a vote.
    A refused grant is not claimed, so the member can ask again.
  - *Deny* writes nothing.
  - *Block* persists through the privacy record (`voice_room_blocks`), so it
    survives a restart and `/public`.
- **Withdrawal.** An ownership change withdraws requests raised to the previous
  owner, retires their buttons and asks the new owner about everyone still
  waiting. `/public` takes approved members' Connect allow back and retires
  open prompts. A room delete forgets the state; its prompts go with the
  channel.
- **Request ids.** Pending requests, grants and prompts are runtime-only. Each
  button appends the worker's fresh 128-bit CSPRNG epoch to the numeric core
  request id. The worker refuses a different or missing epoch before deciding,
  so a reset counter or a repeated/backward wall clock cannot revive an old
  button. Epochs are collision-resistant, not clock-based; ids remain within
  Discord's 100-character limit even with maximum-width numeric fields.
- **Not yet.** Grants are not durable: a restart forgets which members were
  approved, so a later `/public` cannot take back their Connect allow (Discord
  keeps it). Persisting grants needs a table of its own.

## Residual parent work

V1 room storage, V2 ownership tracking, `/nick` resolution, Discord permission
and channel execution, button routing and expiry, ephemeral replies, and runtime
wiring remain outside this slice. The parent must authenticate actor
identity/permission, serialize per-room writes, execute effects in order, report
created channels back with `join_channel_created`, and translate typed refusals
into ephemeral replies. Unit tests establish domain behavior only, not runtime
parity or staging readiness.

## Hermetic verification

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- check -p two-bot-core
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_private
```

The acceptance fixture covers naming, both privacy toggles, Join-channel
creation races, the entry table, all three decisions, ownership changes and
deletion, invalid states, request-ID exhaustion, and property tests for the two
rules: no sequence grants Connect to a blocked member, and a public room never
has a Join channel. No tests in this fixture use a database, Redis, Discord, or
a staging identity.
