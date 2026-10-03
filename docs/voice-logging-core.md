# V10 logging-channel resolution core

`two_bot_core::voice_logging` is an original, pure implementation derived only
from [the voice-room specification](voice-rooms.md#v10-logging-health-errors-utilities)
(Logging part). It needs no `db` feature, Discord types, clock, or I/O.

## Inputs and decisions

- `DetailLevel`: `Off` / `Brief` / `Full`. `parse_detail_level` accepts
  trimmed ASCII case-insensitive `off` / `brief` / `full`; anything else
  (including empty) is refused fail-closed as `LoggingError::UnknownLevel`
  with at most `MAX_LEVEL_ECHO_CHARS` of input echoed. Nothing ever falls
  back to a default level.
- `should_log(setting, is_detailed)`: brief events log under `Brief` and
  `Full`, detailed-only events log under `Full`, nothing logs under `Off`.
- `LoggingCandidates`: caller-supplied availability snapshot — optional
  system channel ID, optional DM user ID plus `dm_reachable`, optional
  creator channel chat ID, and `setup_user_id` (whoever last set up the bot,
  for the mention only). `None` or zero IDs and `false` reachability both
  mean "try the next destination".
- `resolve_log_target`: first working destination wins — guild system
  channel (carrying `mention_user_id`; the adapter drops the mention when it
  is `None` but still posts), then DM, then creator chat. Returns `None`
  only when nothing is available.
- `RepeatLedger`: bounded pure-counter repeats (`MAX_LOG_SENDS` = 3 sends:
  one initial notice plus two repeats). `should_send` is true while sends
  remain; `record_send` counts one send and returns false without counting
  once the bound is reached, so the failure stays listed but silent;
  `reset` restarts the budget after a resolve/recur. No timestamps live
  here: the caller owns the clock, persistence and per-failure mapping.

All IDs are plain `u64` (`Snowflake`); only enums and numeric IDs travel
here, so no names, message text, URLs or tokens can leak through this core.
The runtime authenticates identity, persists settings and ledger state,
chooses wording, sends notices, and lists current failures in `/setup`.

## Replay and concurrency contract

Decisions are pure functions of the supplied facts: the same inputs always
produce the same outcome, and refusals change nothing. The runtime must
serialize per-guild `/logging` updates, persist validated settings and
ledger counts atomically, and deduplicate interaction IDs. Stale snapshots
and delayed replays are not detected by this core.

## Residual parent work

Room lifecycle (V1), placement/permissions/health evaluation, actual
sending, `/setup` rendering, and runtime wiring remain outside this slice.

## Hermetic verification

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_logging
```

The acceptance fixture covers fallback order, mention handling, zero-ID
handling, unknown-level refusal with echo bound, detail gating, and repeats
stopping after the bound with reset. No network, Discord, database, or
staging identity is used.

## `/logging` (admin) and persistence

`/logging` (Manage Channels, the spec's "admin") sets the guild's logging
choices: `show`, `level level:<off|brief|full>`, `channel [channel]` and
`mention [role]` (no channel or role clears it). Each change is a
read-modify-write under the runtime's admin lock, saved whole to
`voice_logging_settings` (migration 0228; no row means brief notices through
the fallback chain with no mention). Unknown level text is refused
fail-closed through `parse_detail_level`, and a failed read or write changes
nothing and says so. Only a mention *role* is stored, so the table holds no
member IDs.

Sending notices (health check, error routing, repeat ledger) reads these
settings in the next V10 slice; this slice only stores and edits them.
