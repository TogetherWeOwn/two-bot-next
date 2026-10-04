# Offline voice health and diagnostic contract

This pure `two-bot-core::health` extension supports the approved
[voice-room specification](voice-rooms.md#v10-logging-health-errors-utilities).
It has no gateway, store, Discord or V1 runtime dependency and performs no I/O.

## Readiness

`VoiceReadiness` requires explicit `ComponentStatus` values for `gateway`,
`store` and `voice_core`. Its `report()` always includes all three under fixed
component names and uses the existing `HealthReport::ready()` rule:

- All three `Ready`: ready, no diagnostics.
- Any `Starting` or `Down`: not ready, one `ComponentNotReady` diagnostic per
  non-ready component, ordered gateway → store → voice core.
- Recompute from each snapshot; recovery leaves no historical failures behind.

This is **readiness**, not process liveness. The current `/health` and `/readyz`
endpoints are unchanged. Guild permission failures are classified separately;
they do not imply that every guild or the entire process is unavailable.

```rust
use two_bot_core::{ComponentStatus, VoiceReadiness};

let report = VoiceReadiness {
    gateway: ComponentStatus::Ready,
    store: ComponentStatus::Ready,
    voice_core: ComponentStatus::Starting,
}.report();
assert!(!report.ready());
```

The serialized report has `health.components` (the existing name/status tuple
format) and `diagnostics`. For this example the diagnostic is:

```json
{"category":"component_not_ready","component":"voice_core","status":"starting"}
```

## Safe classification boundary

`classify_voice_error(kind, untrusted_detail)` drops **all** detail text. The
adapter must select `VoiceFailureKind` from structured failure information, not
by matching an error message. The output contains enums only, with no raw
messages, names, URLs, source chains or credentials. JSON, `Display` and `Debug`
are safe for this diagnostic type; this does not sanitize unrelated log calls.

| Input kind | Output category | Safe context |
| --- | --- | --- |
| `ComponentUnavailable` | `component_not_ready` | gateway/store/voice_core, status down |
| `MissingPermission` | `missing_permission` | permission and guild/category/channel scope |
| `CategoryFull` | `category_full` | fixed suggestion to use a different category |
| `RateLimited` | `rate_limited` | fixed notice; no retry timing or retry permission |
| `Unexpected` | `unexpected` | fixed fallback, never the unknown source message |

Permission identifiers are `manage_channels`, `move_members`, `manage_roles`
and `view_channel`, as specified in V10. A category override remains identifiable
as category scope. Choosing and safely displaying the specific category is an
adapter responsibility, not string interpolation into this diagnostic.

## Residual integration (parent V10)

The parent must collect current gateway/store/voice-core states, translate
structured runtime failures, evaluate effective guild/category/channel
permissions and identify the offending override. It must wire endpoint and
`/setup` consumers, retain current failures, and implement bounded notice repeats
and fallback delivery: guild system channel → setup-person/guild-owner DM →
creator-channel chat. This component neither chooses destinations nor sends or
retries notices. The Discord API notes still govern retry-after handling.

The `/setup` consumer shows findings, store errors and the creator-channel list
to admins only (Manage Channels or Administrator). Every other member gets a
generic running or paused line, plus whether anything needs attention, with no
channel ids, error text or permission gaps.

Logging configuration, guild controls, `/ping`, `/invite`, deployment and live
guild verification remain outside this offline slice. No runtime parity, live
health or notice delivery is established by these tests.

## Hermetic verification

```sh
cargo test -p two-bot-core health::tests --locked
cargo fmt --all -- --check
cargo clippy -p two-bot-core --all-targets --locked -- -D warnings
```

The health suite includes ready, disconnected gateway, failed store, unavailable
voice core, every one of the 27 status combinations, recovery, all four
permissions at all three scopes, stable JSON categories, and synthetic token,
password, query-secret and unknown-secret markers in raw detail text. No
network, database, environment secret or live guild is used.
