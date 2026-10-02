# V10 ping/invite utility core integration seam

`two_bot_core::voice_utilities` is an original, pure implementation derived only
from [the approved voice-room specification](voice-rooms.md#v10-logging-health-errors-utilities).
It requires no `db` feature, Discord wire types, clock, or external I/O.

## Renders

- `ping_render(rtt_ms)`: bounded human-readable latency line for a measured
  round-trip time. Under one second it names milliseconds (`Pong! 42ms`); at
  or above it names seconds with one decimal (`Pong! 1.5s`). Inputs above
  `MAX_PING_DISPLAY_MS` (30,000) saturate there and gain a `+` suffix
  (`Pong! 30.0s+`), so even `u64::MAX` renders a short line and the function
  cannot panic. The runtime supplies the measurement; freshness and clock
  handling stay outside this slice.
- `invite_render(guild_invite_code)`: invite line for the configured guild
  code, or a fixed notice when none is configured. `None` (or an empty
  string) renders `NO_INVITE_CONFIGURED`. A code passing
  `is_valid_invite_code` renders as a `discord.gg` link line with the code
  embedded as an opaque token. Anything else renders `INVALID_INVITE_CODE`
  verbatim, without echoing the rejected value (a stale code may be a rotated
  secret).
- `is_valid_invite_code`: ASCII alphanumeric plus `-`/`_`, 2–32 characters
  (`MIN_INVITE_CODE_CHARS`..=`MAX_INVITE_CODE_CHARS`). Anything URL-shaped
  (`://`, `/`, `.`, whitespace, query/fragment markers) fails the charset,
  so a pasted link is refused rather than embedded or fetched. Codes are
  never fetched as URLs anywhere in this core; the runtime performs the
  Discord reply.

## Residual parent work

Guild invite-code configuration storage, `/ping` measurement wiring,
`/invite` command registration, ephemeral-reply sending, and runtime wiring
remain outside this slice. The V10 parent (TOG-10091) must persist the code,
pass it through unchanged, and never log rejected values. Unit tests
establish domain behavior only, not runtime parity or staging readiness.

## Hermetic verification

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_utilities
python3 scripts/cargo_cache.py run -- clippy -p two-bot-core --all-targets -- -D warnings
```

The acceptance fixture covers the millisecond/second boundary, the
saturation boundary and `u64::MAX`, unconfigured/empty/valid codes, length
bounds, overlong and URL/malformed refusal without echo, plus property tests
for bounded ping output and never-echoed invalid codes. No tests use a
database, Redis, Discord, or a staging identity.
