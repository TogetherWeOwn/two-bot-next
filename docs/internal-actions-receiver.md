# Private internal-actions receiver

## Implementation and deployment boundary

Receiver integration is in progress. The durable store, announcement executor,
nonce-commit authentication capability and strict receiver configuration exist.
The container does **not** yet load this configuration or expose an action route.
The public health router continues to serve only health/readiness endpoints.
Configuration parsing is not evidence of a deployed or reachable receiver.

No endpoint, signing key, secret custodian or export authority is supplied by
this code. Deployment and transfer into the website's staging secret store stay
on HOLD until separately approved and recorded. Never use the signing test
fixture as a runtime credential. The Worker ingress and secret bindings are
unchanged.

## Authentication across a durable nonce commit

`AuthenticatedRequest::verify` authenticates the four v1 headers and exact body
bytes before evaluating the process-clock `ClockGuard` or checking freshness.
It neither parses JSON nor consumes a bucket. The returned capability is opaque,
non-cloneable and not printable. It keeps those same bytes, header values and
guarded process time across the async database wait.

`burn_durably(&InternalActionStore)` consumes that capability and grants a
`NonceBurnedRequest` only after a successful new nonce commit. The store checks
freshness again against its independent, persisted DB-clock high-water mark.
A replay, DB failure, incompatible skew or excessive DB rollback cannot grant
authorization. There is no volatile fallback. Do not restore a process-clock
guard from the DB-clock mark: they are different clock domains.

Only the burned capability exposes post-replay `authorize`: per-key bucket,
unique-key JSON parsing and action/feature checks, in that order. Cancellation
before commit never authorizes; cancellation after commit may burn an attempt
without executing it. The existing in-memory `authorize` uses the same stages
with its cache burn, preserving its ClockGuard and TTL-coverage checks.

This is still the legacy v1 signing contract. The [v2 specification](internal-action-signing-v2.md)
is not an implemented verifier. No unsigned caller/audience header is trusted.
Stable caller mapping, idempotency claim, action validation, outbound admission,
execution and durable finalization remain receiver responsibilities; this
capability does not itself permit a Discord effect.

## Configuration contract

`two_bot_core::internal_action_config::InternalActionConfig::from_env` returns
`None` when `TWO_INTERNAL_ACTIONS` is absent or exactly `0`. In that case it
never reads receiver keys or bind settings. Exactly `1` requests a receiver;
other values, including an empty or non-Unicode flag, are errors.

An enabled receiver requires all of these settings, without defaults:

| Setting | Meaning |
| --- | --- |
| `TWO_INTERNAL_BIND` | Literal private IP plus explicit nonzero port. IPv6 uses brackets. No hostname, URL, wildcard, public address or ephemeral port. |
| `TWO_INTERNAL_KEYS` | Existing comma-separated `key-id:secret` signing specification; each secret is at least 32 bytes. At most 64 keys, with unique IDs and distinct secrets. |
| `TWO_INTERNAL_CALLERS` | Comma-separated `key-id:caller` mappings. Exactly one entry for each signing key, no unknown entries. The caller is a stable logical identity, not a key-rotation version. |
| `TWO_INTERNAL_CHANNEL_KEYS` | Explicit nonempty comma-separated channel-key/Discord-ID map. Names are unique; IDs are canonical, nonzero, u64-representable snowflakes. |

Key IDs, caller names and channel-key names are 1–128 ASCII alphanumeric,
period, underscore or hyphen characters. Multiple rotating keys may identify the
same logical caller, but every key ID must have a distinct signing secret, even
for that same caller. The shared parser refuses secret aliases: the key ID
itself is not authenticated by the legacy signature format.

Runtime integration must load this separately from the gateway's health-only
configuration fallback. Invalid enabled configuration is a fatal startup error,
not permission to continue with a different bind or missing authentication.
The private listener must never be merged into the health router. Binding a
private address does not itself approve an ingress path or replace TLS/network
access controls.

Resolve `caller_for(key_id)` only after verifying the signature. Pass the stable
caller to `RequestIdentity::new`, together with the client's original idempotency
key and the exact authenticated bytes. Do not use the signing-key ID as that
identity: rotation must not grant a second execution of an existing intent.
Changing a caller mapping or target-channel configuration is an operational
identity/policy change, not an automatic migration of stored requests.

All configuration errors use fixed messages and setting names. Parser failures
are not forwarded because those errors can quote malformed input. Debug prints
only the bind address and counts, never keys, secrets or caller mappings.

## Remaining outbound safety integration

The announcement adapter's cooldown obligation is not satisfied by core inbound
`TokenBuckets`, the existing executor's pacing mutexes, or sharing a Twilight
client. A shared durable admission gate
([TOG-11045](/TOG/issues/TOG-11045)) must cover the announcement transport
before execution is enabled. See
[the executor contract](internal-action-executor.md#single-attempt-and-safe-results).

## Tests

The configuration unit tests use only the existing public signing vectors. They
exercise dark defaults, strict enable values, missing/non-Unicode settings,
private literal binds, distinct-key rotation with caller continuity,
duplicate/ambiguous mappings, same-secret aliases for the same or different
callers, canonical channels and redaction.
They do not read runtime credentials, open sockets, access databases or send to
Discord.

On the persistent controller, run compiling checks only through the approved
bounded-cache wrapper, for example:

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --lib internal_action_config
```

If cache admission is refused, retain that result and use the existing hosted CI
path; do not substitute a target directory or invoke Cargo compilation directly.
