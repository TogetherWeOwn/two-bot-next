# Rejection telemetry (threat-model F4)

Bounded, scalar rejection logging for `POST /internal/actions`.
Module: `crates/core/src/rejection_telemetry.rs`.
Tests: `crates/core/tests/rejection_telemetry.rs`.

## Why

F4 asks for a structured rejection logger with bounded labels and
suppression: no token, body or SQL marker escapes into logs, and a rejection
flood stays bounded. This is the pure core only. Wiring it into the receiver
(feeding one `Rejection` per refused request and logging the returned
`RejectionRecord`s) stays with the receiver slice (TOG-10603).

## Classes

`RejectionClass::classify` maps every `ErrorCode` to one of 11 classes, with
no wildcard arm, so a new refusal variant fails to compile until it has a
class here:

| Class | Wire codes |
|---|---|
| `auth_failure` | `unauthorized` from a configured key id |
| `unknown_key` | `unauthorized` from any other key id |
| `clock_skew` | `stale_request` |
| `nonce_replay` | `replayed` |
| `rate_limit` | `rate_limited` |
| `unknown_action` | `action_not_allowed` for a non-catalog action (or none) |
| `action_disabled` | `action_not_allowed` for a catalog action |
| `malformed_body` | `malformed` |
| `conflict` | `version_conflict`, `in_progress` |
| `upstream` | `discord_rejected`, `discord_unavailable`, `upstream_timeout` |
| `internal` | `internal` |

Caller-visible indistinguishability is preserved: unknown key id and bad
signature are one refusal on the wire. Only the log tells them apart.

## Scalar labels

- `key`: the raw `X-TWO-Key-Id` header when its shape is valid
  (1–64 ASCII letters, digits, `.`, `_`, `-`) **and** the ring holds it
  (`KeyRing::contains`); `invalid` when the shape is wrong, `unknown` when
  the shape is right but the ring does not hold it, `other` when folded by
  the tracking cap. Configured ids are operator config, not caller input;
  well-shaped but unknown ids are caller-chosen and never printed.
- `action`: the catalog name when the action is one of the 19 in
  `IMPLEMENTED_ACTIONS`, else `unknown` (or `other` when folded). The label
  is the `&'static str` catalog constant, never the caller's bytes.
- Never logged: the body, header values other than a configured key id, the
  `ActionError` message, `log_reason`, SQL text, or the raw path. Records
  have nowhere to put them: every field is a closed label or a count.

`Rejection::new` takes the refusal's `ActionError::code` plus labels built
with `KeyLabel::new` (raw key id + `KeyRing::contains`) and either
`AuthDecision::action` when the request got that far, `ActionLabel::from_body`
(which refuses oversized bodies and unparseable shapes), or `new(None)`.
Call `from_body` only after the signature verified: parsing unauthenticated
bodies hands callers free JSON work.

## Suppression and the bound

Suppression runs per `(class, key, action)` bucket inside a tumbling window
(default 60 s). At most `max_tracked` buckets are tracked (default 32,
ceiling 1024); overflow folds into one `other` bucket per class, so memory is
at most `max_tracked` keyed buckets plus 11 fixed overflow buckets. Each
bucket emits at most `samples_per_window` samples (default 1, ceiling 100)
plus one summary when the window closes.

One window emits at most

```
(max_tracked + 11) * (samples_per_window + 1)
```

records however large the flood — 86 with defaults. Across windows each
window re-arms samples and closes with its own summaries. `close_window`
(and the rollover inside `record`) emits one summary per bucket that
suppressed anything; call `flush` from a periodic tick so a flood that stops
still reports its summary.

## For the receiver slice

Feed one `Rejection` per refused request, log each returned record (the
`Display` form is `kind=… class=… key=… action=… count=… suppressed=…`).
`RejectionTelemetry` is not `Sync`-safe for sharing: keep one per route
worker or guard it with a mutex at the call site.
