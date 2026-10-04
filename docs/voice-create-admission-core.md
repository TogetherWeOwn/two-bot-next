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

- **Table.** `crates/cutover/migrations/0413_voice_create_reservations.sql`
  stores one row per accepted create (`guild_id`, `user_id`, `created_at`, plus
  `channel_id` and `settled_at`). Rows are never deleted when a room is deleted,
  a create is rolled back or the bot restarts, so the burst window and the
  per-member cooldown count accepted creates, not live rooms.
- **Claim.** `PgRoomStore::claim_create` opens one transaction, takes a
  per-guild advisory lock (`voice_create:<guild>`), reads the caps inputs and
  burst history, calls `decide_admission` and, only when allowed, inserts the
  reservation. Two concurrent joins (in one process or two) therefore cannot
  both pass the guild cap. A refusal writes nothing. Cap counts are live rooms
  (`voice_rooms`) plus in-flight reservations (`settled_at IS NULL`, younger
  than `IN_FLIGHT_RESERVATION_TTL_SECS`, 300 s, so a reservation abandoned by a
  crash stops holding a slot). The caller supplies `now_secs`.
- **Settle.** `PgRoomStore::settle_create` binds the reservation to the created
  room or rolls the create back. Either way the row stays for burst and
  cooldown history.
- **Worker.** `GuildRoomWorker::dispatch_one` claims after its cheap checks and
  before the only Discord create call. A refusal records
  `LifecycleFailure::CreateRefused` (stable `RefusalReason::code`, legacy
  `user_message`) and never calls Discord; a claim-time store error records a
  persistence failure, also without calling Discord. A 429 requeue keeps its
  one claim; a create that ends without a room (Discord error, failed persist,
  a join that went stale before the retry) rolls its reservation back. A refusal
  is a policy outcome, not a lifecycle failure, so it adds no
  `two_bot_voice_operations_total` outcome.

## Residual parent work

Caps and cooldown use the legacy defaults (1 room per member, 40 per guild,
30 s) through `CreateAdmissionConfig::default`; wiring the `TWO_TEMP_VOICE_MAX_PER_USER`,
`TWO_TEMP_VOICE_MAX_PER_GUILD` and `TWO_TEMP_VOICE_CREATE_COOLDOWN_SECONDS`
settings into `GuildRoomWorker::set_admission_config` is not part of this slice.
A refusal reaches the operator as a `/setup` failure line and a notice; there
is no per-member reply on a gateway voice join. No retention job prunes
`voice_create_reservations`. Naming, permissions, ownership and room lifecycle
are separate slices, and unit and database tests establish behaviour only, not
staging readiness (staging proof stays on the voice acceptance card).

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
