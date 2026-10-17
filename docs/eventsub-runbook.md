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
  Source: `crates/bot/src/eventsub_http.rs:58-71`, `crates/bot/src/main.rs:337-348`.

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
  Source: `crates/bot/src/eventsub_http.rs:47-56`.
- Logs carry only scalar `event` / `status` / `error_class`. Header values,
  the challenge, body bytes, and query strings are never logged.
  Source: `crates/bot/src/eventsub_http.rs:1-11`,
  `crates/bot/src/eventsub_http.rs:122-148`.

Rotation procedure (needs its own authorization; this runbook does not grant
it): Twitch fixes the transport secret at subscription creation and offers
no update, and `reconcile()` keeps any healthy subscription on our callback
without comparing secrets, so a new secret alone orphans the old
subscription. Generate a fresh value meeting the constraint above, set the
Worker secret `TWO_TWITCH_WEBHOOK_SECRET`, redeploy that Worker version and
transfer ownership per the enable order above, then restart the container.
Then delete each `stream.online` v1 subscription on our callback (Helix
DELETE, `delete_subscription_url`) and recreate it with the new secret
(delete first: a duplicate create answers `409`). The two sides must agree:
while they differ, every delivery fails closed with `401 unauthorized` (see
the matrix below). Confirm agreement by watching for `notification
acknowledged` or `challenge answered` lines after the recreate, not by
printing either value. Never use a test fixture as a runtime credential. The
runtime reconciler runs once at startup plus a bounded hourly interval while
`TWO_EVENTSUB` is exactly `1` (dark parks otherwise), so a revocation is
healed by the next pass; it keeps any healthy subscription on our callback
without comparing secrets, so the delete-and-recreate step for secret rotation
stays manual.
Source: `crates/core/src/twitch_eventsub.rs:526-529` (duplicate create answers
`409`), `:543-547` (healthy subscriptions on our callback are kept without
comparing secrets), `:467-472` (`ExistingSubscription` carries no secret),
`wrangler/src/index.ts:535-537,594`, `crates/core/src/twitch_helix.rs:365,591`,
`crates/bot/src/main.rs` (`eventsub_reconcile::spawn_if_enabled`).

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
  `crates/bot/src/eventsub_http.rs:350-366`.
- Only `stream.online` version `1` is accepted; any other subscription type
  or version is a deterministic `WrongSubscription` error.
  Source: `crates/core/src/twitch_eventsub.rs:225-231`.
- An empty, oversized, or non-graphic challenge is `InvalidChallenge` and
  answers `400 malformed`, never an echo.
  Source: `crates/bot/src/eventsub_http.rs:412-425`.

If the challenge echo never arrives at Twitch, check in order: the Worker
route gate (both flags `1`?), the container bind (did startup refuse?), the
secret agreement (401s in the logs?), a `200 delivery ignored` right after
the subscription creation (the challenge was not echoed — delete and
recreate the subscription, see below), and only then the callback URL shape
(plain `https`, no port, query, fragment, or userinfo; canonical form).
Source: `crates/core/src/twitch_eventsub.rs:400-431`,
`crates/bot/src/eventsub_http.rs:403-411`.

## Refusal matrix

Container refusals use the same JSON envelope
(`{"ok": false, "request_id", "error": {"code", "message": "request refused", "retryable"}}`)
and log one scalar warn line (`event="eventsub_receiver"`, `status`,
`error_class`). Worker-ingress refusals use the same envelope except the
404 below, which is plain-text `not found`, and log
`event="eventsub_ingress"`. Retryable refusals are the container
timeout/busy rows, the Worker 429 / 504 / 503 rows, and the container's
unknown/future classification fallback (503 `unavailable`). While the
delivery gate stays dark, ordinary verified deliveries have no side
effects before acknowledgement; with delivery enabled (next section), a
transient ledger/send failure on a verified notification also answers the
retryable 503 `unavailable`, and the durable claim fences keep that
redelivery from double-posting.
Container non-POST answers also carry an `Allow: POST` header.
Source: `wrangler/src/eventsub.ts:101-125`, `crates/bot/src/eventsub_http.rs`.

Worker-ingress layer (`wrangler/src/eventsub.ts`, reached first via the
public URL):

| Status | Code | Retryable | Meaning / first action |
|---|---|---|---|
| 404 | plain-text `not found` | false | Non-POST, wrong path, or any query (`isEventsubRequest`). Via the public URL a GET answers 404 here and never reaches the container 405 row; `?x=1` answers 404 here and never reaches the container 400 row. Source: `wrangler/src/eventsub.ts:101-103,133-137,241`. |
| 400 | `malformed` | false | Present-but-badly-shaped delivery header, signature without the `sha256=` prefix, or malformed `content-length` / body framing. Check the request shape, not the secret. Source: `wrangler/src/eventsub.ts:158-169` (header/signature shape), `:181,184,208,212` (content-length and body framing). |
| 401 | `unauthorized` | false | Missing delivery header or signature. Check that the sender forwarded every header. Source: `wrangler/src/eventsub.ts:158-169`. |
| 429 | `rate_limited` | true | Per-isolate burst budget or in-flight cap hit. The sender backs off and redelivers. Source: `wrangler/src/eventsub.ts:243-248`. |
| 504 | `upstream_timeout` | true | The fenced DO did not answer within the Worker deadline. Source: `wrangler/src/eventsub.ts:264`. |
| 503 | `unavailable` | true | DO construction / binding failure, cold container, ownership-fence refusal, or relay failure (see the 503 triage below). Source: `wrangler/src/eventsub.ts:268,299-308,328,338,343` (ingress and relay failures), `wrangler/src/index.ts:905-915` (cold container at `:905`, `containerFetch` failure at `:906-910`, fence refusal at `:912-915`). |

Container layer (`crates/bot/src/eventsub_http.rs`; the 400 / 404 / 405 rows
are reachable via the public URL only when the Worker is bypassed by a
direct `containerFetch`, because the Worker answers those shapes first):

| Status | Code | Retryable | Meaning / first action |
|---|---|---|---|
| 400 | `malformed` | false | Query string present, empty body, or invalid challenge on a direct fetch. The query gate runs before verification. Check the forwarded request shape, not the secret. Source: `crates/bot/src/eventsub_http.rs:223-232,313-322,412-425`. |
| 401 | `unauthorized` | false | Signature, freshness, or timestamp failure, or (on a direct fetch) missing/malformed delivery headers. Via the Worker, a malformed header shape is 400 at ingress (row above); first action is still secret agreement and clock skew (10-minute window). Source: `crates/bot/src/eventsub_http.rs:269-287,325-347`. |
| 404 | `not_found` | false | Wrong path (exact path is `/hooks/twitch`) or any non-callback route via the fallback on a direct fetch. Via the public URL the Worker already answered non-POSTs and bad shapes with its own 404. Source: `crates/bot/src/eventsub_http.rs:35-36,88-89,154-165,213-222`. |
| 405 | `method_not_allowed` | false | Anything but POST on the callback path on a direct fetch. Via the public URL the Worker drops non-POSTs with its own 404 before the container. Source: `crates/bot/src/eventsub_http.rs:233-246`. |
| 408 | `request_timeout` | true | Body slower than the 5 s read budget while holding one of the 32 permits. Retriable: the sender (Twitch) backs off and redelivers. Source: `crates/bot/src/eventsub_http.rs:38-40,290-312`. |
| 413 | `payload_too_large` | false | Body over the 64 KiB cap, checked before verification. Genuine `stream.online` bodies are well under 2 KiB; treat 413 as a malformed or abusive sender. Source: `crates/bot/src/eventsub_http.rs:37,290-301`, `crates/core/src/twitch_eventsub.rs:49-51`. |
| 415 | `unsupported_media_type` | false | Content type is not exactly `application/json` or `application/json; charset=utf-8` (case-insensitive); anything else, including another encoding, is refused. Source: `crates/bot/src/eventsub_http.rs:247-266`. |
| 503 | `busy` / `upstream_timeout` | true | All 32 permits held (`busy`), or the whole request past the 20 s budget (`upstream_timeout`). Both mean the receiver is saturated or stalled, not that the delivery was bad; the sender retries. Check container saturation, not the secret. Source: `crates/bot/src/eventsub_http.rs:38-40,167-199`. |
| 503 | `unavailable` | true | A future/unrecognized post-verification classification error; the receiver fails closed so Twitch retries and counts it as metric `kind="other"`. This is distinct from Worker-ingress `unavailable` above. Inspect the `eventsub delivery refused` warning and the `two_bot_eventsub_classified_total{kind="other"}` counter; the refusal log uses the bounded error class `unavailable`, so it does not identify a specific future variant. Do not assume saturation or a Worker relay failure. Source: `crates/bot/src/eventsub_http.rs:426-439`. |

Verified deliveries that need no operator action answer `200`:

- `notification` and `revocation`: acknowledged as `200 {"ok": true}`.
  While delivery stays dark the receiver only acks a notification; with
  delivery enabled a 200 also covers a posted or deduplicated go-live. The
  reconciler runs at startup and hourly, so a revocation is healed by the
  next pass.
- Unknown message types, and known deterministic classification errors on
  non-verification messages (`WrongSubscription`, `InvalidBody`):
  acknowledged with 2xx so Twitch does not retry what will never succeed.
  Unknown/future `classify()` errors are not included: they return retryable
  503 `unavailable` and count as metric `kind="other"` so an unrecognized
  failure is not silently discarded or conflated with known ignored deliveries.
  This carve-out excludes `webhook_callback_verification`: `classify()`
  returns `WrongSubscription` / `InvalidBody` for a challenge with the wrong
  type or version, or an unparsable signed body, and the handler still acks
  `200 {"ok": true}` with only an info `delivery ignored` line without
  echoing the challenge — so the subscription is never enabled. A `200
  delivery ignored` immediately after a subscription creation means the
  challenge was not echoed: delete and recreate the subscription.

Source: `crates/bot/src/eventsub_http.rs:350-411`.

Log lines to expect (scalar fields only):

- `eventsub delivery refused` (warn: `status`, `error_class`)
- `challenge answered`, `notification acknowledged`, `revocation acknowledged`,
  `delivery ignored` (info: `status = 200`)
- `eventsub delivery refused` (warn: `status`, bounded `error_class`); for
  unknown/future classification errors expect `status = 503` and
  `error_class = "unavailable"`, plus the `kind="other"` metric increment

Source: `crates/bot/src/eventsub_http.rs:131-148,324-439`.

## Go-live delivery enablement

Posting a go-live to Discord is a second, independent gate on top of the
receiver (`crates/core/src/eventsub_delivery_config.rs`). It stays dark
until the Operator provisions four Worker secrets, all forwarded to the
container on explicit lines only while the receiver forward is active:

1. `TWO_EVENTSUB_ANNOUNCEMENTS_CHANNEL_ID`: the announcements channel
   snowflake (canonical decimal) the post lands in.
2. `TWO_EVENTSUB_INVITE_URL`: the Discord invite rendered into the post;
   it must pass the renderer's own validation (postable https invite).
3. `TWO_TWITCH_BROADCASTERS`: the broadcaster allowlist, shared with the
   subscription reconcile, so the subscribed set and the announceable set
   are one list. Entries must be canonical numeric Twitch user ids.
4. `TWO_EVENTSUB_DELIVERY` set to exactly `"1"` **last**.

An enabled-but-invalid delivery state — the gate at `"1"` with any input
missing or invalid, or delivery requested without the receiver —
refuses container startup (`eventsub startup refused`); it never
degrades to ack-only. Because a bad value crash-loops the container, set
the three inputs first, redeploy, and flip the gate last. Enable on
staging first and keep the posted go-live as enablement evidence;
production enablement is its own rollout decision. To stop posting
without touching the receiver, unset the gate (or set it to `"0"`) and
redeploy: verified deliveries return to ack-only.

## Dark-unless-enabled behavior

Dark is the normal state, not an outage:

- `TWO_EVENTSUB_DELIVERY` unset or `"0"`: the receiver (when enabled)
  acknowledges verified notifications without posting; no delivery input
  is read.

- `TWO_EVENTSUB` unset or `"0"`: no listener, no bind attempt, no secret
  read. Health and readiness are unchanged.
  Source: `crates/core/src/eventsub_receiver_config.rs:32-51`,
  `crates/bot/src/main.rs:337-348`.
- Worker route without both flags: the path keeps today's behavior (404) and
  the container is never contacted.
  Source: `wrangler/src/eventsub.ts:63-69`.
- Enabled-but-invalid on a direct container start (bad flag value, bad bind,
  missing or short secret): the process exits at startup with `eventsub
  startup refused`. Through the Worker the shape differs: a missing
  `TWO_TWITCH_WEBHOOK_SECRET` forwards nothing while the route stays live
  (the ingress gate never checks the secret), so the result is 503
  `unavailable` below with no startup refusal; only a present-but-short
  secret crash-loops the container. Fix the configuration and restart; do
  not work around it by enabling anything else.
  Source: `crates/bot/src/main.rs:160-168`,
  `wrangler/src/index.ts:535-537,583-585`, `wrangler/src/eventsub.ts:63-69`.
- `POST /hooks/twitch` 404 on staging therefore means one of: the Worker
  route is dark (check both flags), or the path/method/query shape is wrong
  (see the matrix). A cold container never answers 404.
  It never means "retry with another credential": an authentication failure
  is a stop — report it, do not substitute credentials.
- `503 unavailable` triage (the matrix `busy` row covers saturation only):
  cold container (`!this.ctx.container?.running`), ownership-fence refusal,
  `containerFetch` failure, or the Worker running with `EVENTSUB_INGRESS=1`
  but without `TWO_TWITCH_WEBHOOK_SECRET` so no listener was forwarded.
  Source: `wrangler/src/index.ts:905-915` (cold container at `:905`,
  `containerFetch` failure at `:906-910`, fence refusal at `:912-915`),
  `wrangler/src/eventsub.ts:21-25,268,299-308,328,338,343`.

## Operator do-not list

- Do not dump the environment or request headers to troubleshoot; tokens,
  connection strings, and member data stay out of evidence.
- Do not enable the receiver to repair an unrelated outage. An unwired
  feature is not a fix.
- Do not infer production state from staging observations, and do not test
  against production. Staging end-to-end checks stay on the staging Worker
  origin and the staging container.
