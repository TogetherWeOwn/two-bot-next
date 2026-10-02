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

## Residual parent work

Serializing concurrent claims per guild (legacy's advisory lock), persisting
the reservation row, the `create_reservation` audit row and the
`temp_voice_creates` timestamp, translating refusals into replies, naming,
permissions, ownership and room lifecycle remain outside this slice. Unit tests
establish domain behaviour only, not runtime wiring or staging readiness.

## Hermetic verification

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- check -p two-bot-core
python3 scripts/cargo_cache.py run -- clippy -p two-bot-core --all-targets -- -D warnings
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_create_admission
```

The fixture pins the refusal order, cap equality, the exact-cooldown pass,
cooldown-0 disabling, the strict window edge, burst counting of deleted rooms'
reservations, the current attempt never counting, every config bound edge, the
stable codes and the legacy messages. No test uses a database, Redis, Discord
or a staging identity.
