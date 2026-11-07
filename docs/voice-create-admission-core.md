# Room-create admission core

`two_bot_core::voice_create_admission` is an original, pure implementation
derived only from legacy `TogetherWeOwn/two-bot` at frozen revision
`bffccf3e3a9f56a3da37de67c6f272ac10ecb3b3`:

| Contract | Pinned source |
| --- | --- |
| Burst constants, refusal codes, `reserveIfUnderCaps` claim order | `src/tempVoice/store.ts:27-32,77-126` |
| User-facing refusal messages | `src/tempVoice/service.ts:321-327` |
| Config bounds (`maxPerUser` 1..10, `maxPerGuild` 1..45, `createCooldownSeconds` 0..3600) | `src/tempVoice/config.ts:143-147` |

It requires no `db` feature, Discord wire types, clock, store or external I/O,
and does not depend on room lifecycle, naming, permissions or ownership.

## Inputs and decisions

- `CreateAdmissionConfig`: validated `max_per_user`, `max_per_guild` and
  `cooldown_secs`. `new` refuses each out-of-range knob with a typed
  `AdmissionConfigError`; `default` carries the legacy defaults (1, 40, 30).
- `AdmissionRequest`: the requesting member, their live-plus-reserved room
  counts, their last accepted create time (or `None`), and the previously
  accepted `create_reservation` audit entries. The caller supplies all history
  and the clock (`now_secs`); the core never queries a store. Timestamps are
  `i64` Unix seconds.
- `decide_admission`: returns `Allow` or `Deny { reason }`, or
  `AdmissionError::InvalidUserId` for a zero user id. The same inputs always
  decide the same way; refusals change nothing.

## Refusal order

The first trip wins, exactly as legacy's claim:

1. `user_cap` — `owned_by_user >= max_per_user`. Reservations count: a create
   already in flight holds its owner's slot.
2. `guild_cap` — `rooms_in_guild >= max_per_guild`.
3. `cooldown` — when `cooldown_secs > 0` and the member has a previous create,
   `now_secs - last_created_at_secs < cooldown_secs`. Equality passes; a zero
   cooldown disables the check; no previous create never throttles.
4. `user_burst` — the member's accepted reservations strictly inside the
   trailing 60 s window reach 3.
5. `guild_burst` — anyone's accepted reservations inside the same window
   reach 10.

`RefusalReason::code` is the stable legacy string (`user_cap`, `guild_cap`,
`cooldown`, `user_burst`, `guild_burst`), also used as the durable audit
reason. `user_message` renders the legacy reply text: the user-cap message is
singular at `max_per_user == 1` and plural otherwise, and the cooldown message
quotes the active `cooldown_secs`; the rest are fixed text quoting the burst
constants.

## Burst semantics

The window is strict: a reservation stamped exactly `now_secs - 60` has left
the window, matching legacy's `created_at > now - window` scan. Burst history
counts accepted reservations, not live rooms: deleting a room, rolling a
reservation back, or restarting frees no burst slot, and the attempt under
decision is never part of the history it is checked against (legacy counts
before inserting the new audit row). Pass only previously accepted
reservations, including ones whose rooms are since gone. Window and cooldown
subtraction saturate; a `last_created_at_secs` in the future refuses under any
positive cooldown, as legacy's millisecond comparison does.

## Durable runtime wiring

The pure core is now called by the runtime with persisted history:

- **Table.** `crates/cutover/migrations/0422_voice_create_reservations.sql`
  stores one row per accepted create (`guild_id`, `user_id`, `created_at`, plus
  `channel_id` and `settled_at`). Rows are never deleted when a room is deleted,
  a create is rolled back or the bot restarts, so the burst window and the
  per-member cooldown count accepted creates, not live rooms.
- **Claim.** `PgRoomStore::claim_create` opens one transaction, takes a
  per-guild advisory lock (`voice_create:<guild>`), reads the caps inputs and
  burst history, calls `decide_admission` and, only when allowed, inserts the
  reservation. Two concurrent joins (in one process or two) therefore cannot
  both pass the guild cap. A refusal writes nothing. Cap counts are live rooms
  (`voice_rooms`) plus **all** unsettled reservations (`settled_at IS NULL`).
  The caller supplies `now_secs` for history, not capacity expiry. Age cannot
  distinguish a crashed process, a slow live 429 waiter or an unknown POST
  outcome, so no TTL refunds capacity (superseding the initial migration's
  expiry commentary).
- **Transfer/settle.** `PgRoomStore::persist_create` inserts the room and binds
  its reservation in one transaction under the same guild lock as claimers.
  No missing or double-counted capacity is visible between those writes; an
  insert/UPDATE failure rolls both back. Exact replay is idempotent while the
  room exists, but cannot recreate a deleted room. `bind_create_channel` writes
  the Discord channel id onto the claim as soon as the create returns, before the
  room row, so the channel has a durable witness. `settle_create` only releases a
  claim after known no-side-effect failure, confirmed channel absence or a
  successful guarded delete, and never while its channel is a tracked live room.
  `orphaned_create_channels` lists unsettled claims bound to a channel with no
  room. Either way the row stays for burst/cooldown history.
- **Worker.** `GuildRoomWorker::dispatch_one` claims after its cheap checks and
  before the only Discord create call. A refusal records
  `LifecycleFailure::CreateRefused` (stable `RefusalReason::code`, legacy
  `user_message`) and never calls Discord; a claim-time store error records a
  persistence failure, also without calling Discord. A long/repeated 429 requeue
  keeps its one non-expiring claim without counting a second burst attempt.
  Unknown creates never retry the POST or refund their hold. The worker binds
  the returned channel to its claim, then persists the room; a failure of either
  retains the hold through delayed, denied or occupied-channel compensation, and
  releases it only when the delete/absence is confirmed. `GuildRoomWorker::load`
  reads the orphaned claims, so after a restart the first authoritative
  reconcile deletes each held channel (or confirms its absence in the snapshot)
  and only then settles the claim.
  A join cancelled before a retry is a confirmed no-effect outcome. Settlement
  errors reach the bounded setup failure report; a credential refusal halts all
  subsequent writes. A transient compensation settlement error retries only SQL
  after the channel is known gone. A refusal is a policy outcome, not a lifecycle
  failure: it is counted as `two_bot_voice_operations_total{outcome="refused"}`
  but never pages (`voice_failures` excludes it).

## Residual parent work

Caps and cooldown resolve at boot from the `TWO_TEMP_VOICE_MAX_PER_USER`
(1 to 10), `TWO_TEMP_VOICE_MAX_PER_GUILD` (1 to 45) and
`TWO_TEMP_VOICE_CREATE_COOLDOWN_SECONDS` (0 to 3600) settings through
`configured_admission`, and each guild actor applies the result via
`GuildRoomWorker::set_admission_config` at spawn. An unset or blank value
keeps today's effective default (1 room per member, 40 per guild, no
cooldown); an unparsable or out-of-range value warns with reason
`invalid_value` and fails closed to that knob's default only.
A refusal reaches the operator as a `/setup` failure line and a notice; there
is no per-member reply on a gateway voice join, so the recorded refusal stays
context-free (no member room ids); a refused member finds their rooms with
`/myroom`, which reads live worker state only. No retention job prunes
`voice_create_reservations`. Unknown-outcome and crash-orphan holds are
conservative capacity debt across restarts, not automatically expired leases.
They require evidence-led reconciliation before release; this slice adds no
operator release API or automatic reconciliation for unknown POSTs. A crash
after the channel is bound is recovered at the next load; only a crash between
Discord's response and the bind (or an unknown POST outcome) leaves a hold with
no channel witness, and that hold is retained, not refunded. Transient settlement errors on cancelled/no-effect creates
also retain the hold for reconciliation. This trades availability for the cap
invariant; do not clear holds based solely on age. Naming, permissions, ownership
and room lifecycle are separate slices, and unit and database tests establish behaviour only, not
staging readiness (staging proof stays on the voice acceptance card).

## Private owned-room discovery and rollback

`/myroom` replies once, ephemerally, with the invoker's current owned live
rooms as channel mentions only. The requester is the authenticated invoker;
readiness and channel presence come from one live snapshot of the connected,
unhalted worker. Another member's rooms are never shown, and a channel
already gone from the snapshot is omitted. An unavailable worker or
unready snapshot returns a transient notice, not a guessed room. An empty
answer means the snapshot found no owned live room — never that the worker
is missing — and appends the join guidance only when creation is actually
open (creation enabled and a creator channel exists). Large lists stop at
complete rows below the completion limit and state how many rooms were
omitted. The read performs no room, permission or policy write.

`/myroom` carries no per-command role policy and no persisted vocabulary:
the restrictable list (`VOICE_COMMANDS`) excludes it, so `/access restrict
command:myroom` is refused truthfully without a write, configuration import
rejects `settings.command_roles` entries for it, and no other writer can
save the key. The guild-wide `required_role` gate still applies to it
through the normal invocation gate. Because no `myroom` key can be
persisted, the preceding strict policy reader never meets one and the
automatic image rollback needs no cleanup step — there is also no schema
migration to reverse.

Existing saved custom `myroom` definitions remain publishable and executable
with voice off, and support same-name edits and pause/resume. Fresh custom
writes still reserve the name. With voice on the voice slash handler takes
precedence; the custom text trigger remains distinct. Custom catalog and
import capacity count actual all-enabled publication slots: 67 builtins plus
33 noncolliding customs fit the 100-command ceiling. The unpublished assistant
reserves its name but does not consume another slot.

**Historical catalogs beyond the ceiling:** when the complete valid
baseline (gate-constrained builtins plus every persisted enabled custom
row) would fit at 100 but adding `/myroom` alone would overflow, the
serialized registry owner publishes the baseline without only that
builtin — e.g. `66 builtins + 34 customs` stays bootable rather than
failing at `67 + 34 = 101`. No custom row is truncated, disabled or
deleted, an invalid baseline still fails, and the next serialized
registry sync re-evaluates: `/myroom` is admitted by a normal republish
once capacity allows. When withheld, the publish log records the deferral
and the confirmed command set (help/discovery included) does not
advertise it; a stale invocation still obeys the application/guild/voice
and builtin-precedence fences.

## Hermetic verification

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- check -p two-bot-core
python3 scripts/cargo_cache.py run -- clippy -p two-bot-core --all-targets -- -D warnings
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_create_admission
python3 scripts/cargo_cache.py run -- test -p two-bot --lib -- admission_
# disposable agent-testdb database only; the test creates and drops its own
TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_ci \
  python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --test voice_create_admission_store -- --ignored
```

The fixture pins the refusal order, cap equality, the exact-cooldown pass,
cooldown-0 disabling, the strict window edge, burst counting of deleted rooms'
reservations, the current attempt never counting, every config bound edge, the
stable codes and the legacy messages. The core fixture and the worker tests use
no database, Redis, Discord or staging identity; the store test uses only a
disposable database on the CI/agent test service.
