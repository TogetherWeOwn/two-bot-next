# Twitch EventSub operator runbook

Staging-only, default-dark Twitch EventSub webhook receiver (`POST /hooks/twitch`).
This is the operator procedure: how to enable it, bind it, provision and rotate
its secret, answer a subscription challenge, and read its refusals. It describes
the shipped source, not proof that any specific deployment or drill passed.

Out of scope: production Twitch application setup, subscription management
outside the bot's reconciler, credential creation or rotation authorization,
and code changes. Secret values never appear in commands, logs, or evidence;
compare binding and flag **names**, never values.

## Enable flag

The container listener exists only when `TWO_EVENTSUB` is exactly `"1"`.
Anything else stays dark, and an enabled-but-invalid configuration stops
startup instead of downgrading:

- `TWO_EVENTSUB` unset or `"0"` disables the receiver without reading the bind
  or the secret. Any other non-`"1"` value is a configuration error.
  Source: `crates/core/src/eventsub_receiver_config.rs:32-51`.
- An invalid enabled configuration logs `eventsub startup refused` and exits
  the process; there is no health-only fallback.
  Source: `crates/bot/src/main.rs:160-168`, `crates/bot/src/eventsub_http.rs:57-62`.
- The public Worker route is a second, independent gate: it answers only when
  the staging-only Worker var `EVENTSUB_INGRESS` **and** the Operator-set
  Worker secret `TWO_EVENTSUB` are both exactly `"1"`. In every other state
  the route does not exist and the container is never contacted.
  Source: `wrangler/src/eventsub.ts:63-69`, `wrangler/src/index.ts:1373-1375`.
- `EVENTSUB_INGRESS` lives only under the staging vars table; a top-level or
  production declaration fails the bindings check.
  Source: `wrangler/wrangler.toml` (staging vars), `wrangler/scripts/check-env-bindings.py`.

Enable order: deploy the receiver code dark first, then set the Operator
secret `TWO_EVENTSUB` to `1` **last**, redeploy that version, and transfer
ownership per the [persisted ownership control](runbook.md#persisted-ownership-control).
Every Worker secret change publishes a new Worker version; the fence keeps it
from serving until takeover. Rollback is the reverse: remove the secret,
redeploy, transfer ownership. The route is absent again and the next container
start carries no receiver.

## Bind address

The receiver listens on a fixed private port, distinct from the public
listener and the internal-actions receiver:

- Fixed receiver port **8092**; the Worker bind constant is `0.0.0.0:8092`
  inside the private container network. `BOT_PORT` must differ from it.
  Source: `wrangler/src/eventsub.ts:30-45`, `wrangler/src/index.ts:588-597`.
- The container requires an explicit `TWO_EVENTSUB_BIND` (for example the
  loopback bind used in tests, `127.0.0.1:8092`). It must parse as a literal
  `IP:port`, name a specific private IP, and use a nonzero port. A wildcard
  bind is accepted only with the Worker-set container marker
  `TWO_INTERNAL_CONTAINER=1`, which marks the process as running inside the
  container network; everywhere else a wildcard bind is refused.
  Source: `crates/core/src/eventsub_receiver_config.rs:53-79`.
- The Worker forwards the flag, the fixed bind, the container marker, and the
  signing secret on explicit lines, and only while the secret is present;
  with no secret it forwards nothing, so a typo or missing secret cannot
  crash-loop the gateway.
  Source: `wrangler/src/index.ts:582-597`.
- Binding completes before any gateway or job task starts. A bind failure logs
  `eventsub startup refused` and exits the process.
  Source: `crates/bot/src/eventsub_http.rs:57-70`, `crates/bot/src/main.rs:337-348`.

## Secret provisioning and rotation

Three Twitch values are Operator-provisioned and never appear in code, logs,
or PR text: the application client id, the client secret, and the webhook
signing secret (`TWO_TWITCH_CLIENT_ID`, `TWO_TWITCH_CLIENT_SECRET`,
`TWO_TWITCH_WEBHOOK_SECRET`).
Source: `docs/configuration.md:255-257`, `crates/core/src/settings.rs:127-134`.

Rules for the signing secret:

- It must be 10-100 printable ASCII characters (Twitch's constraint, enforced
  at load). A shorter, longer, or non-graphic value refuses startup.
  Source: `crates/core/src/twitch_eventsub.rs:59-62`,
  `crates/core/src/eventsub_receiver_config.rs:81-87`.
- The value travels only inside `WebhookSecret` / `Secret`. Its `Debug` form
  renders `[REDACTED]`, and it leaves that wrapper only at the HMAC boundary
  and inside the create-subscription body, which is itself a `Secret`.
  Source: `crates/core/src/twitch_eventsub.rs:99-125`,
  `crates/core/src/twitch_eventsub.rs:436-455`.
- The handler never re-reads the environment: the secret is cloned into
  receiver state at bind time. A new value takes effect on container restart,
  not on secret edit alone.
  Source: `crates/bot/src/eventsub_http.rs:46-55`.
- Logs carry only scalar `event` / `status` / `error_class`. Header values,
  the challenge, body bytes, and query strings are never logged.
  Source: `crates/bot/src/eventsub_http.rs:1-11`,
  `crates/bot/src/eventsub_http.rs:117-132`.

Rotation procedure (needs its own authorization; this runbook does not grant
it): generate a fresh value meeting the constraint above, update the Twitch
subscription secret (the reconciler embeds the secret in the Helix
create-subscription body) **and** the container's
`TWO_TWITCH_WEBHOOK_SECRET` together, then restart. The two sides must agree:
while they differ, every delivery fails closed with `401 unauthorized` (see
the matrix below). Confirm agreement by watching for `notification
acknowledged` or `challenge answered` lines after the restart, not by
printing either value. Never use a test fixture as a runtime credential.

## Challenge verification

When Twitch creates a subscription it first posts a
`webhook_callback_verification` delivery carrying a `challenge` token. The
receiver must echo that token back for the subscription to become enabled.

Verification runs **before any JSON is parsed**, in this order, so an
unauthenticated sender learns nothing about the clock:

1. Size cap: bodies over 64 KiB (`MAX_BODY_BYTES`) are refused before hashing.
2. Signature: HMAC-SHA256 over `message-id + timestamp + raw body`, expected
   as a `sha256=`-prefixed hex header, compared in constant time.
3. Freshness: the delivery timestamp must be within 10 minutes of now
   (`MAX_TIMESTAMP_SKEW_MS`), past or future.

Source: `crates/core/src/twitch_eventsub.rs:51-53`,
`crates/core/src/twitch_eventsub.rs:166-197`.

Classification after verification:

- A well-formed challenge (nonempty, at most 256 printable ASCII characters)
  on a `stream.online` v1 subscription is echoed as the `200 text/plain`
  body with `Cache-Control: no-store`.
  Source: `crates/core/src/twitch_eventsub.rs:245-272`,
  `crates/bot/src/eventsub_http.rs:270-286`.
- Only `stream.online` version `1` is accepted; any other subscription type
  or version is a deterministic `WrongSubscription` error.
  Source: `crates/core/src/twitch_eventsub.rs:225-231`.
- An empty, oversized, or non-graphic challenge is `InvalidChallenge` and
  answers `400 malformed`, never an echo.
  Source: `crates/bot/src/eventsub_http.rs:326-328`.

If the challenge echo never arrives at Twitch, check in order: the Worker
route gate (both flags `1`?), the container bind (did startup refuse?), the
secret agreement (401s in the logs?), and only then the callback URL shape
(plain `https`, no port, query, fragment, or userinfo; canonical form).
Source: `crates/core/src/twitch_eventsub.rs:400-431`.

## Refusal matrix

Every refusal uses the same JSON envelope
(`{"ok": false, "request_id", "error": {"code", "message": "request refused", "retryable"}}`)
and logs one scalar warn line (`event="eventsub_receiver"`, `status`,
`error_class`). Only the two timeout/busy rows are `retryable: true`; this
slice has no send and no durable ledger, so a verified delivery retries only
on a pathological scheduling stall, and the retry re-acks with no side
effects. Non-POST answers also carry an `Allow: POST` header.

| Status | Code | Retryable | Meaning / first action |
|---|---|---|---|
| 400 | `malformed` | false | Query string present, empty body, or invalid challenge. The query gate runs before verification, so even a correctly signed delivery with `?x=1` answers 400. Check the forwarded request shape, not the secret. Source: `crates/bot/src/eventsub_http.rs:180-182,251-253,326-328`. |
| 401 | `unauthorized` | false | Missing/malformed delivery headers, or signature, freshness, or timestamp failure. First action: confirm secret agreement and clock skew (10-minute window), not the request shape. Source: `crates/bot/src/eventsub_http.rs:223-227,255-268`. |
| 404 | `not_found` | false | Wrong path (exact path is `/hooks/twitch`) or any non-callback route via the fallback. Wrong method on the right path is 405, not 404. Source: `crates/bot/src/eventsub_http.rs:35,138-146,176-179`. |
| 405 | `method_not_allowed` | false | Anything but POST on the callback path. Check the sender's method; the Worker already drops non-POSTs before the container. Source: `crates/bot/src/eventsub_http.rs:183-195`. |
| 408 | `request_timeout` | true | Body slower than the 5 s read budget while holding one of the 32 permits. Retriable: the sender (Twitch) backs off and redelivers. Source: `crates/bot/src/eventsub_http.rs:38,241-250`. |
| 413 | `payload_too_large` | false | Body over the 64 KiB cap, checked before verification. Genuine `stream.online` bodies are well under 2 KiB; treat 413 as a malformed or abusive sender. Source: `crates/bot/src/eventsub_http.rs:36,230-240,258-265`, `crates/core/src/twitch_eventsub.rs:49-51`. |
| 415 | `unsupported_media_type` | false | Content type is not exactly `application/json` or `application/json; charset=utf-8` (case-insensitive); anything else, including another encoding, is refused. Source: `crates/bot/src/eventsub_http.rs:198-214`. |
| 503 | `busy` / `upstream_timeout` | true | All 32 permits held (`busy`), or the whole request past the 20 s budget (`upstream_timeout`). Both mean the receiver is saturated or stalled, not that the delivery was bad; the sender retries. Check container saturation, not the secret. Source: `crates/bot/src/eventsub_http.rs:37-39,150-171`. |

Verified deliveries that need no operator action answer `200`:

- `notification` and `revocation`: acknowledged as `200 {"ok": true}`. This
  slice has no send and no ledger, so the receiver only acks; reconcile runs
  at boot and after a revocation in the Helix slice.
- Unknown message types, and deterministic classification errors
  (`WrongSubscription`, `InvalidBody`): acknowledged with 2xx so Twitch does
  not retry what will never succeed.

Source: `crates/bot/src/eventsub_http.rs:287-325`.

Log lines to expect (scalar fields only):

- `eventsub delivery refused` (warn: `status`, `error_class`)
- `challenge answered`, `notification acknowledged`, `revocation acknowledged`,
  `delivery ignored` (info: `status = 200`)

Source: `crates/bot/src/eventsub_http.rs:117-136,270-325`.

## Dark-unless-enabled behavior

Dark is the normal state, not an outage:

- `TWO_EVENTSUB` unset or `"0"`: no listener, no bind attempt, no secret
  read. Health and readiness are unchanged.
  Source: `crates/core/src/eventsub_receiver_config.rs:32-51`,
  `crates/bot/src/main.rs:337-348`.
- Worker route without both flags: the path keeps today's behavior (404) and
  the container is never contacted.
  Source: `wrangler/src/eventsub.ts:63-69`.
- Enabled-but-invalid (bad flag value, bad bind, missing or short secret):
  the process exits at startup with `eventsub startup refused`. Fix the
  configuration and restart; do not work around it by enabling anything else.
  Source: `crates/bot/src/main.rs:160-168`.
- `POST /hooks/twitch` 404 on staging therefore means one of: the Worker
  route is dark (check both flags), the container is cold (public ingress
  never starts it), or the path/method/query shape is wrong (see the matrix).
  It never means "retry with another credential": an authentication failure
  is a stop — report it, do not substitute credentials.

## Operator do-not list

- Do not dump the environment or request headers to troubleshoot; tokens,
  connection strings, and member data stay out of evidence.
- Do not enable the receiver to repair an unrelated outage. An unwired
  feature is not a fix.
- Do not infer production state from staging observations, and do not test
  against production. Staging end-to-end checks stay on the staging Worker
  origin and the staging container.
