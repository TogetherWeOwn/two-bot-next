# Private internal-actions receiver

## Implementation and deployment boundary

The container source integrates an opt-in private `POST /internal/actions`
listener with the durable store, announcement executor, nonce-commit
authentication capability and strict receiver configuration. It supports only
`announcement.post`, regardless of the core action catalogue's broader defaults.
The public health/readiness/metrics router has no action route. This source
checkpoint is not evidence of a merged, deployed or reachable receiver.

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

Runtime loads this separately from the gateway's health-only configuration
fallback. Invalid enabled configuration, missing gateway credentials/database,
a non-staging guild or private bind failure is fatal, not permission to continue
with a different bind or missing authentication. The authoritative runtime pool
backs the receiver; no volatile nonce or idempotency fallback exists. The private
listener binds before gateway/job task startup and shares their sticky shutdown
and bounded HTTP drain. Unexpected listener exit stops its sibling and jobs.
Binding privately does not approve ingress or replace TLS/network access controls.

Resolve `caller_for(key_id)` only after verifying the signature. Pass the stable
caller to `RequestIdentity::new`, together with the client's original idempotency
key and the exact authenticated bytes. Do not use the signing-key ID as that
identity: rotation must not grant a second execution of an existing intent.
Changing a caller mapping or target-channel configuration is an operational
identity/policy change, not an automatic migration of stored requests.

All configuration errors use fixed messages and setting names. Parser failures
are not forwarded because those errors can quote malformed input. Debug prints
only the bind address and counts, never keys, secrets or caller mappings.

## HTTP boundary and execution ownership

Only exact `POST /internal/actions` is accepted; query strings and alternate
paths are refused. The boundary requires singleton v1 signing headers and
`application/json` (optionally `charset=utf-8`), rejects content encoding, and
caps header count/bytes (64/8192), body bytes (2 MiB), body collection (5 seconds),
active requests (32) and total request duration (20 seconds). These application
bounds are **not** transport connection/header-read limits; approved ingress
must supply those controls. No generic request tracing middleware is attached.
Rejections use bounded closed scalar telemetry with periodic/shutdown flushes,
never raw paths, bodies, signatures, secrets or unknown caller labels.

After authentication, durable nonce burn and per-key admission, the receiver
validates the announcement and original `Idempotency-Key`, then atomically
claims its intent and audit record. Only a committed `Claimed` result permits
an effect. Replay returns the stored terminal response; mismatched payloads,
in-flight or reconciliation-required intents never send. The stable caller
mapping preserves this ownership across distinct-key rotation and restart.

Runtime constructs the single-attempt announcement adapter with
`PgSendAdmission` over the same database/token authority as other governed
outbound paths, plus its local `CooldownGovernor`. Core inbound buckets and a
shared Twilight client are not substitutes for that durable gate. A Discord
429 installs the adapter's cooldown and stores a terminal refusal, never an
automatic resend. Unknown transport outcomes, cancellation or failed receipt
finalization retain durable ownership and require reconciliation. Success is
returned only after the receipt/audit transaction commits. See
[the executor contract](internal-action-executor.md#single-attempt-and-safe-results).

The website-compatible envelopes contain `ok`, `request_id`, and either
`result.message_id` or `error.{code,message,retryable}`. Durable replay adds
`Idempotent-Replay: true`; inbound bucket refusal carries `Retry-After`.
Messages are fixed/redacted and all envelopes use `Cache-Control: no-store`.

## Tests

The configuration unit tests use only the existing public signing vectors. They
exercise dark defaults, strict enable values, missing/non-Unicode settings,
private literal binds, distinct-key rotation with caller continuity,
duplicate/ambiguous mappings, same-secret aliases for the same or different
callers, canonical channels and redaction.
They do not read runtime credentials, open sockets, access databases or send to
Discord.

Receiver tests use a module-private injected effect and guarded, migrated
`TestDatabase` instances. They cover protocol caps and body deadlines, request
capacity, redacted authentication failures, nonce-before-parse ordering,
concurrent/restarted/key-rotated replay, byte mismatch, unsupported actions,
cancellation/stale ownership, unknown/no-effect outcomes, unavailable stores,
failed receipt finalization and listener supervision. Nonces are generated
fresh for each attempt; the nonce-replay test intentionally reuses one generated
value. They do not send live Discord actions.

Combined receiver/real-adapter acceptance uses an ephemeral loopback HTTP double
and independent migrated test pools. It covers successful durable receipts,
rotation/restart replay, single-attempt 429 refusal, token-wide holds blocking
new intents and other transports, and invalid receipts retaining both intent and
send-lane ownership. The Discord crate's `test-support` origin seam is enabled
only as a bot dev-dependency: it refuses non-loopback origins and executors without
admission. Runtime construction has no origin override. Added test source is not
evidence of execution; record exact-head compiler/test results separately.

On the persistent controller, run compiling checks only through the approved
bounded-cache wrapper, for example:

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --lib internal_action_config
```

If cache admission is refused, retain that result and use the existing hosted CI
path; do not substitute a target directory or invoke Cargo compilation directly.
