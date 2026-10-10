# Private internal-actions receiver

## Implementation and deployment boundary

The container source integrates an opt-in private `POST /internal/actions`
listener with the durable store, announcement executor, event-read, settings
and moderation executors, nonce-commit authentication capability and strict
receiver configuration. It supports `announcement.post`, `event.read`,
`settings.get`, `settings.set` and the restrictive member verbs
`moderation.ban`, `moderation.tempban`, `moderation.kick`, `moderation.warn`
and `moderation.timeout`, regardless of the core action catalogue's broader
defaults. Every other verb stays refused by the per-effect fences.
The public health/readiness/metrics router has no action route. A merged,
deployed receiver is dark until the Operator enables it, and it is reachable
only through the staging-only Worker ingress described in
[Staging ingress](#staging-ingress-default-dark).

No signing key, secret custodian or export authority is supplied by this code.
Key creation and transfer into the website's staging secret store stay on HOLD
until separately approved and recorded. Never use the signing test fixture as a
runtime credential.

## Staging ingress (default dark)

TOG-12980, with the security conditions C1-C10 of the CISO verdict on TOG-12979.
Production has no ingress, route, variable or secret for this receiver.

| Layer | Behavior |
| --- | --- |
| Gate | Worker var `INTERNAL_ACTIONS_INGRESS = "1"`, declared only under `[env.staging.vars]`, **and** the Operator's Worker secret `TWO_INTERNAL_ACTIONS` exactly `1`. Either one missing, or any other value, leaves the route absent: today's response, no container contact. `scripts/check-env-bindings.py` fails a top-level or production declaration and any `TWO_INTERNAL_*` var in `wrangler.toml`. |
| Route | Exactly `POST /internal/actions`, no query string, no trailing slash. Everything else, including other methods, is today's behavior (404). |
| Request bounds | `application/json` (optionally `; charset=utf-8`), no `Content-Encoding`, body 1 byte to 2 MiB (413 over, declared or streamed), body read within 5 s (408), whole request within 20 s (504). Per-IP token bucket (burst 20, 1/s) and 8 concurrent requests per isolate answer 429 before the container is touched. |
| Headers | Only `X-TWO-Key-Id`, `X-TWO-Timestamp`, `X-TWO-Nonce`, `X-TWO-Signature` (required), `Idempotency-Key` and `Content-Type` reach the container, each a single printable token within the receiver's own length caps. `Authorization`, `Cookie`, `X-Forwarded-*` and any caller-supplied deployment header are dropped. The Worker stamps the deployment id itself. |
| Body | Forwarded byte for byte; the signature covers the raw bytes. Never re-serialized. |
| Fence | Forwarded only inside the Durable Object's ownership gate. A stale, fenced or missing deployment answers 503 `unavailable` and never reaches the container. |
| Startup | Public ingress never starts the container; probes and the keepalive do. While the container is not running the answer is 503 `unavailable`. |
| Answers | The receiver's own JSON envelope is relayed with `Content-Type`, `Idempotent-Replay` and `Retry-After` only, `Cache-Control: no-store`. Any non-JSON, oversize or transport-failure answer (including SDK startup text that embeds error messages) becomes the fixed `unavailable` envelope. Worker refusals use the same envelope with a fixed message. |
| Logs | Scalar `internal_actions_ingress` events: `status` and `error_class` only. Never header values, nonce, signature, body or query. |

Authentication is unchanged: legacy v1 HMAC and a durable nonce burn at the
receiver. The ingress adds no bearer, key or second identity.

### Container wiring

The three plain settings and the signing secret are **not** in
`FORWARDED_FLAGS`. They reach the container through explicit `containerEnvVars`
lines, and only while `TWO_INTERNAL_ACTIONS` is exactly `1`:

| Name | Source |
| --- | --- |
| `TWO_INTERNAL_ACTIONS` | Worker secret, forwarded as `1` |
| `TWO_INTERNAL_BIND` | Worker constant `0.0.0.0:8091` (TOG-16851: the Containers port check and `containerFetch` cannot reach a loopback-only socket). An Operator-supplied value is ignored, so the bind can never become a public address. |
| `TWO_INTERNAL_CALLERS` | Worker secret, e.g. `web-staging:website-staging` |
| `TWO_INTERNAL_CONTAINER` | Worker constant `1`: the Worker sets it to mark the process as running inside the container network, where the wildcard bind is reachable only via `containerFetch` and the startup port check. That network is presumed private but unverified. The marker is a deployment claim, not a proof: the bot trusts it only because the Worker (not the Operator) sets it. A wildcard bind without exactly this marker is refused. |
| `TWO_INTERNAL_CHANNEL_KEYS` | Worker secret, e.g. `smoke-throwaway:<channel snowflake in the TWO Staging guild>` |
| `TWO_INTERNAL_KEYS` | Worker secret `web-staging:<64 hex>`; never a var, never logged |

Fixed receiver port: **8091** (health stays on `BOT_PORT` 8080, and the two may
not be equal while the receiver is enabled). While enabled, 8091 joins the
container's startup port checks, so a sidecar that cannot reach the receiver
fails the start instead of reporting a half-working bot. TOG-16851: a
loopback-only socket is unreachable from the Containers port check and
`containerFetch`, so the Worker binds the wildcard and the bot accepts it only
with the `TWO_INTERNAL_CONTAINER` marker; other binds still pass
`assert_private_bind`.

### Enable order and rollback

Every Worker secret change publishes a new Worker version, and the ownership
fence keeps a new version from serving until the Operator transfers ownership
(see [persisted ownership control](runbook.md#persisted-ownership-control)).
Stage the secrets with `wrangler versions secret put --env staging` (a version
that is not yet deployed), deploy that one version, then run the takeover once.

1. Merge this change. `deploy-staging` deploys it dark; `/health` and `/readyz` are unchanged.
2. Operator stages `TWO_INTERNAL_CALLERS` and `TWO_INTERNAL_CHANNEL_KEYS` (plain values).
3. Operator generates the key on the Operator host and stages `TWO_INTERNAL_KEYS`, and sets the website's `staging` environment secret `BOT_SHARED_SECRET` to the same value, without printing it.
4. Operator stages `TWO_INTERNAL_ACTIONS` as `1` **last**, deploys the version, and transfers ownership. The container restart applies the settings: an invalid combination exits the process (the receiver boots all-or-nothing) and keeps staging red until step 5.
5. Rollback: stage deletion of `TWO_INTERNAL_ACTIONS` (`wrangler versions secret delete`), deploy and transfer ownership; or use the existing Worker-version rollback. The route is absent again and the next container start carries no receiver setting.

TOG-16851 proved a loopback-only receiver never becomes healthy: the Containers
port check and `containerFetch` cannot reach it, so the container waited on 8091
forever. The fix binds the wildcard behind the Worker-set container marker (the
container network is presumed private — no private listener or network ACL was
verified; HMAC/caller checks are unchanged). If the startup
port check for 8091 fails after this change, roll back and hand the question to
the CTO.

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
| `TWO_INTERNAL_BIND` | Literal IP plus explicit nonzero port. IPv6 uses brackets. No hostname, URL, public address or ephemeral port. A specific private IP always passes; the wildcard (`0.0.0.0`, `::`) passes only with `TWO_INTERNAL_CONTAINER` exactly `1`. |
| `TWO_INTERNAL_CONTAINER` | Required only with a wildcard bind: exactly `1`, Worker-set. Ignored for a specific private bind. |
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
private literal binds, wildcard binds only with the container marker,
distinct-key rotation with caller continuity,
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
