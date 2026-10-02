# V12a monthly-cap ledger core integration seam

`two_bot_core::voice_assistant_cap` is an original, pure implementation derived
only from [the approved voice-room specification](voice-rooms.md#v12-template-assistant-optional-config-gated).
It requires no `db` feature, Discord wire types, clock, HTTP client, or
external I/O.

## Inputs and decisions

- `CapLedger::record(guild_id, month_start_secs, builds_used, now_secs)` is the
  whole ledger call. `month_start_secs` and `builds_used` are the
  caller-supplied persisted row (a V12-wiring DB column on the parent card
  [TOG-10117](/TOG/issues/TOG-10117)); `now_secs` is the processing time, an
  `i64` Unix timestamp matching `RoomContext::timestamp`. It returns
  `Allow { remaining }` (builds left after this one: zero on the month's last
  allowed build) or `Deny { limit, resets_at }` (the next 1st 00:00 UTC, when
  the next attempt is allowed again). An allowed build is counted in the
  mirror; a denial counts nothing.
- The month key is the UTC year-month of `now_secs`; the reset boundary is the
  1st at 00:00 UTC. A stored row from an older month resets to zero for the
  current month, so the first build after the 1st is always allowed. A clock
  that moved before the stored row's month start is refused with
  `ClockRollback` rather than granting a fresh quota; a stored row that is not
  a 1st 00:00 UTC is refused with `InvalidMonthStart`.
- `MonthKey` (`year`, `month`) orders chronologically and exposes the boundary
  math directly: `from_unix_secs`, `start_unix_secs`, `next_start_unix_secs`.
  Leap safety comes from civil date math (Howard Hinnant's algorithms, as in
  the V5/V11 cores) — February 29th belongs to February with no special case,
  so February caps and March resets are exact.
- `validate_request_shape` is the §V12 send allowlist: the `AssistantRequest`
  type carries the admin's prompt, the guild templates (`channel_id` plus name
  and optional status template), the "no game" label and the locale, and
  nothing else. No member-name, presence or ID fields exist in the type, so
  the privacy rule holds by construction; the validator additionally bounds
  every field (prompt ≤ 2000 chars, ≤ 128 templates, templates ≤ 4096 chars,
  label ≤ 100 chars, locale a 2–35-char BCP-47-ish tag). Guild routing and cap
  keying happen server-side from the authenticated guild, never from this
  payload.

## Replay and concurrency contract

The same persisted row and clock always produce the same decision; the parent
must serialize concurrent `/templateassistant` attempts per guild and persist
the counted row atomically, exactly as the V4 vote core requires for ballots.
Dropping the ledger loses only the in-memory mirror (`usage`); the persisted
row stays authoritative, and restart recovery belongs to the parent. Per-guild
isolation is the caller's persisted-row keying; the ledger's guild-scoped
mirror (`usage`) never leaks a count across guilds. Stale snapshots are not
detected by this pure core. No Discord effects, HTTP calls, or crash recovery
are claimed by these tests.

## Residual parent work

The per-guild month-start/build-count column, the OpenAI-compatible endpoint
configuration gate ("disabled unless configured"), the actual endpoint call
with the validated payload, the Apply/Refine/Cancel flow, the six-scenario
output validation, and the admin-only command gate all remain outside this
slice. The V12 parent must authenticate the caller, route the persisted row,
enforce the endpoint-configured gate, and consume this module for the cap
check before sending. Unit tests establish domain behavior only, not runtime
parity or staging readiness.

## Hermetic verification

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- clippy -p two-bot-core --all-targets --locked -- -D warnings
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_assistant_cap --locked
```

The acceptance fixture covers the 0/199/200/201-build table, the Dec→Jan
midnight reset, leap-day February caps with March resets on both leap and
non-leap years, the exact remaining counts, guild isolation, and the
invalid-input refusals. A 256-case property test proves monotonic denial
(denied stays denied within the same month key at any later clock and any
higher count); a second proves the month-boundary invariant
(`start <= now < next`, and the reset instant always opens a strictly newer
key); a third proves the allow-remaining formula exact. Shape tests pin the
allowlist bounds and every refusal. No tests in this fixture use a database,
Redis, Discord, or a staging identity.
