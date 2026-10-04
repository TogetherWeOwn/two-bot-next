# Scheduled-events skip-path acceptance

`crates/core/tests/scheduled_events_skip_acceptance.rs` (TOG-12700) pins the
skip behavior of the `two_bot_core::scheduled_events` poller through the
existing public API only (`normalize_event` / `normalize_events`,
`EventStatus`, `ScheduledEventsSkip`, `SCHEDULED_EVENTS_INTERVAL_MS`); no
REST, timer, or SQL.

## Tick recipe under test

Fetch → `normalize_events` (`None` = `ScheduledEventsSkip::InvalidResponse`,
fetch failure = `DiscordReadFailed`) → swap via the store. Only a `Some`
outcome replaces the snapshot.

## What the tests pin

| Case | Outcome |
| --- | --- |
| Malformed row (bad timestamp, unknown status, missing id) | `normalize_events` returns `None`; the scripted tick reports `InvalidResponse` and leaves the last good snapshot untouched — never an empty snapshot |
| Failed fetch | `DiscordReadFailed`, distinct from `InvalidResponse`; last good snapshot stays |
| Successful empty response | `Some(vec![])` — a valid empty mirror that deletes the last event |
| `EventStatus` mapping | `1/2/3/4` → scheduled/active/completed/cancelled; anything else is `None`, and one unknown status rejects the whole snapshot |
| `SCHEDULED_EVENTS_INTERVAL_MS` | `10 * 60 * 1000` (600 000), the legacy 10-minute cadence |
| Failure sequence | `DiscordReadFailed` then `InvalidResponse` still keeps the snapshot; the next good tick replaces it |

## Verify

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test scheduled_events_skip_acceptance
```
