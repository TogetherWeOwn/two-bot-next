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
  Source: `crates/core/src/twitch_eventsub.rs:101-127`,
  `crates/core/src/twitch_eventsub.rs:457-476`.
- The handler never re-reads the environment: the secret is cloned into
  receiver state at bind time. Unarmed, a new value takes effect on container
  restart, not on secret edit alone. Armed (the Operator provisioned
  `TWO_EVENTSUB_RELOAD_KEY` on both sides), the fenced DO attaches the
  Worker's current secret to every delivery with its own HMAC, and the
  receiver swaps its in-memory snapshot before verifying that delivery —
  without dropping the gateway or the receiver socket. Any refresh failure
  answers retryable 503 `unavailable` without verifying against the stale
  snapshot.
  Source: `crates/bot/src/eventsub_http.rs:47-56`,
  `crates/core/src/eventsub_refresh.rs`.
- Logs carry only scalar `event` / `status` / `error_class`. Header values,
  the challenge, body bytes, and query strings are never logged.
  Source: `crates/bot/src/eventsub_http.rs:1-11`,
  `crates/bot/src/eventsub_http.rs:122-148`.

Rotation procedure (needs its own authorization; this runbook does not grant
it): Twitch fixes the transport secret at subscription creation and offers
no update, and `reconcile()` keeps any healthy subscription on our callback
without comparing secrets, so a new secret alone orphans the old
subscription. Generate a fresh value meeting the constraint above, stage it with
`wrangler versions secret put --env staging TWO_TWITCH_WEBHOOK_SECRET --message <sha>`
(stamped with the live `/readyz` `build_revision` of staging, after the
newest-equals-serving precheck in `docs/production-deploy.md`; a plain `wrangler
secret put` publishes an unstamped version that can never form a rollback receipt),
redeploy that Worker version and
transfer ownership per the enable order above, then restart the container.
Restarting first is deliberate: the running container must already carry
the new secret before any subscription is deleted, so no scheduled pass
can re-create a deleted subscription with the old secret. The cost is a
401 window (below) for not-yet-deleted subscriptions signed with the old
secret, so delete promptly after the restart. The delete step stays
manual, the recreate does not: delete each `stream.online` v1
subscription on our callback (Helix DELETE, `delete_subscription_url`),
then let the reconciler (next section) create the missing subscriptions
with the new secret on its next pass — restart the container again to run
the startup pass promptly instead of waiting for the next scheduled pass.
Do not hand-create a duplicate first: Twitch answers `409`, and the plan
already orders deletes before creates. The two sides must
agree: while they differ, every delivery fails closed with `401
unauthorized` (see the matrix below). Confirm agreement by watching for
`notification acknowledged` or `challenge answered` lines after the
reconciler's recreated subscription verifies, not by printing either value.
Never use a test fixture as a runtime credential.

Armed-refresh variant (staging only, needs its own authorization like any
rotation): with `TWO_EVENTSUB_RELOAD_KEY` provisioned and unchanged, the
container needs no restart. Transfer Worker ownership first (as above):
every delivery through the new Worker version then carries refresh headers
with the Worker's current secret, and the armed receiver swaps its live
snapshot from those headers *before* Twitch-signature verification,
independent of the Twitch signature itself — the existing subscriptions
stay signed with the old secret until they are re-created, so their live
deliveries fail closed with `401 unauthorized` from the cutover until the
re-created subscriptions verify. The reconciler creates with the same live
snapshot (`secret_snapshot` in `crates/bot/src/eventsub_http.rs`, cloned
per pass in `crates/bot/src/eventsub_reconcile.rs`), so a re-created
subscription carries whatever the receiver is verifying against.
Source: `crates/core/src/eventsub_refresh.rs:172-261`,
`crates/bot/src/eventsub_http.rs:423-450`,
`wrangler/src/eventsub.ts:99-106,325-346`.

Force and confirm the swap with a crafted challenge, not a Twitch
delivery: no existing subscription can produce a new-signed delivery
before re-creation. POST a `webhook_callback_verification` delivery to
the staging callback URL, signed with the new secret per the Challenge
verification section below (`content-type: application/json`, a fresh
timestamp, a unique `twitch-eventsub-message-id` of at most 128
`[A-Za-z0-9_-]` characters, minimal body `{"challenge": "<printable
ASCII, at most 256 chars>", "subscription": {"type": "stream.online",
"version": "1"}}`; never send `x-two-eventsub-*` — the edge refuses it
and the DO attaches the refresh headers itself). The challenge echo has
no side effects: nothing is persisted or posted. Expect `200` with the
challenge echoed plus a `challenge answered` container line — the HMAC
verifies only against the snapshot the receiver holds, so the echo
proves the running container now holds the new secret. A `401` means it
does not (ownership transfer incomplete, or the wrong secret value):
stop, fix that, do not delete. A `503` means the refresh itself failed:
stop and triage per the `503 unavailable` list below, do not delete.
Source: `crates/core/src/twitch_eventsub.rs:142-159,171-199,282-292`,
`crates/bot/src/eventsub_http.rs:518-534`,
`wrangler/src/eventsub.ts:198-235`.

Drain in-flight passes before deleting: a pass clones the snapshot once
at its start, so a pass that started before the swap still creates with
the old secret. Note the echo time, then wait for the next reconcile
terminal line (`subscriptions reconciled`, `re-created but not all
verified yet`, `refused`, `parked`, `failed`, or `pass interrupted`)
timestamped after it — passes run sequentially, so that line proves
every pass that could hold the old snapshot has finished, and every
later pass clones the new one — then delete promptly. The wait is up to
about an hour plus pass time. If no terminal line appears within about
an hour and a quarter of the echo, stop and work the no-terminal
checklist (next section) instead of deleting.
Source: `crates/bot/src/eventsub_reconcile.rs:609-635`.

The delete step stays manual, the recreate does not: delete each
`stream.online` v1 subscription on our callback (Helix DELETE,
`delete_subscription_url`), then let the reconciler create the missing
subscriptions with the new secret on its next pass — restart the
container to run the startup pass promptly (safe once ownership
transferred, since the new Worker version forwards the new secret to a
fresh container). Confirm agreement by watching for `notification
acknowledged` or `challenge answered` lines after the reconciler's
recreated subscription verifies, not by printing either value.
Source: `crates/core/src/twitch_eventsub.rs:547-550` (deletes first; a
duplicate create answers `409`), `:564-568` (healthy wanted subscriptions
on our callback are kept without comparing secrets), `:486-493`
(`ExistingSubscription` carries no secret),
`crates/core/src/twitch_helix.rs:595-597` (a `409` create counts as
`AlreadyExists` only after a follow-up listing confirms a healthy
subscription on our callback).

The reload key itself is startup-only: arming, rotating, or removing
`TWO_EVENTSUB_RELOAD_KEY` requires an authorized container restart after the
corresponding Worker version takes ownership. Until the running container
loads the matching configuration, deliveries may return retryable 503
`unavailable`: a changed key fails refresh authentication, while removing
the key leaves an armed receiver without refresh headers. Adding the key
does not arm a receiver that was started without it. This restart requirement
does not relax the staging and subscription-rotation restrictions above.
Source: `crates/core/src/eventsub_receiver_config.rs` (`from_lookup`),
`crates/bot/src/eventsub_http.rs` (`ReceiverState::with_delivery`, `receive`).

## Subscription reconciler

The reconciler is wired into bot startup. While `TWO_EVENTSUB` is exactly
`"1"` it runs one pass at startup, then sleeps
`RECONCILE_INTERVAL_SECS = 3_600` after each pass completes before the
next one, until shutdown — so the gap between passes is the pass duration
plus one hour, and no one-hour recovery bound is promised. A failed pass
is logged and the loop still runs the next tick, so a reconcile failure
never exits the process (unlike an enabled-but-invalid receiver bind, above). Dark
spawns no task and logs one parked line (below).
Source: `crates/bot/src/main.rs:399-403`,
`crates/bot/src/eventsub_reconcile.rs:40-42,390-444`.

Each pass, from the process environment:

1. Refuse without any Helix call when the configuration is incomplete:
   `TWO_TWITCH_CALLBACK_URL` must be present and a plain `https` callback
   with no port, query, or fragment, and the Operator Twitch client id and
   client secret must load. The webhook secret comes from the receiver's
   live snapshot (seeded at boot from `TWO_TWITCH_WEBHOOK_SECRET`, swapped
   by armed refresh), never boot env directly; an unreadable snapshot
   refuses the same way. Any refusal logs
   `error_class="reconcile_config_invalid"` and mutates nothing. The Worker
   forwards the callback, the broadcaster allowlist, and the Helix
   credentials to the container only on explicit lines while the receiver
   is forwarded, and only when each value is present, so a
   half-provisioned enable fails loudly here instead of running dark.
   Source: `crates/bot/src/eventsub_reconcile.rs:88-104,331-374`,
   `wrangler/src/index.ts:660-680`.
2. Park without mutations when `TWO_TWITCH_BROADCASTERS` (comma-separated
   `broadcaster_user_id` allowlist, trimmed, sorted, deduplicated) is empty:
   `error_class="reconcile_no_broadcasters"`.
   Source: `crates/bot/src/eventsub_reconcile.rs:74-86,343-350`.
3. List existing subscriptions, plan with `plan_boot_actions` /
   `reconcile()` (deletes before creates, **our callback only**: unwanted,
   duplicate, and non-healthy subscriptions on our callback are deleted;
   healthy wanted ones are kept), then — once the plan is built — run it
   one action at a time so one rejected action cannot starve the rest, and
   confirm with a follow-up listing that every desired broadcaster holds
   a healthy (`enabled` or
   `webhook_callback_verification_pending`) subscription on our callback.
   Source: `crates/bot/src/eventsub_reconcile.rs:201-315`,
   `crates/core/src/twitch_eventsub.rs:542-581`,
   `crates/core/src/twitch_helix.rs:233-243`.

Health signal is the container log, scalar fields only
(`event="eventsub_reconcile"`). Terminal line per pass:

- `status="healthy"` — `eventsub subscriptions reconciled` (info, with
  `broadcasters` / `deleted` / `created` / `already_exists` /
  `prior_failed` counts): converged.
- `status="pending"` — `eventsub subscriptions re-created but none verified
  yet` (warn, `reconcile_pending`): every desired subscription exists but
  none is `enabled` yet; the callback has not completed verification.
- `error_class="reconcile_unhealthy"` then the wrapping
  `error_class="reconcile_failed"` (error): the plan ran but an action
  failed or a desired broadcaster still lacks a healthy subscription
  (`failed` / `missing` counts). The pair is one failed pass, not two.
- `error_class="reconcile_failed"` on its own (error, `error` field): the
  pass errored at the first listing, at the plan, or at the follow-up
  listing, so no `reconcile_unhealthy` or `reconcile_action_failed` line
  follows. At the listing or plan stage nothing was mutated; at the
  follow-up listing the plan may already have run but convergence is
  unconfirmed. Read the remediation off the `error` field: an invalid
  broadcaster id (not 1-32 digits) or more than 100 allowlist entries
  fails the plan before any action and blocks **every** broadcaster on
  every pass until the allowlist format is fixed; a token-endpoint
  rejection points at the client id/secret provisioning.
  Source: `crates/bot/src/eventsub_reconcile.rs:253,264,266,378-386`,
  `crates/core/src/twitch_eventsub.rs:551-556`,
  `crates/core/src/stream_funnel.rs:76-77`.
- `error_class="reconcile_config_invalid"` or
  `error_class="reconcile_no_broadcasters"`: refused or parked before any
  mutation, per steps 1-2.
- `status="parked"`, `reason="receiver_dark"` (info, once at spawn time):
  enabled but no shared snapshot exists, so no task spawned and no pass
  ran. (The flag is checked first: a dark flag parks with
  `reason="flag_dark"` at spawn and per pass, even with no snapshot.)

Detail lines inside a pass: `reconcile_prior_failed` (pre-pass
subscriptions on our callback already in `webhook_callback_verification_failed`,
`notification_failures_exceeded`, or `authorization_revoked`; this pass
deletes them and re-creates the ones still wanted) and `reconcile_action_failed` (one `delete` or
`create` rejected; the rest of the plan and the follow-up listing still
ran). Source: `crates/bot/src/eventsub_reconcile.rs:106-122,224-315,378-387`;
the same lines are catalogued in
[the observability event catalog](observability-event-catalog.md), and
[the subscription health queries](eventsub-subscription-health-queries.md)
give the staging log-read procedure (Q2), including which fields the read
script drops and the dashboard keeps.

Operator verification and remediation (staging only; never run a test
against production Twitch, and never print a secret value to check one):

- Verify a pass by its terminal log line, not by calling Helix by hand.
  No terminal line for the startup pass, or none for well over one
  interval (plus pass time) after the previous one, means check, in order:
  the flag (parked?), the refusal lines (config invalid or empty
  allowlist?), and only then container liveness.
- `reconcile_config_invalid`: compare binding and flag **names** — callback
  shape, client id/secret, webhook secret — fix the provisioning, redeploy
  and restart per the enable order; the next pass retries with the new
  environment. Do not substitute another credential to make the error go
  away.
- `reconcile_no_broadcasters`: provision `TWO_TWITCH_BROADCASTERS` and
  redeploy/restart; until then the reconciler correctly mutates nothing.
- Repeated `reconcile_prior_failed` or a stuck `pending`: the callback is
  not verifying. Work the challenge checklist below (route gate, bind,
  secret agreement, callback URL shape). The reconciler already deletes and
  re-creates failed subscriptions every pass; manually looping deletes
  adds nothing.
- `reconcile_action_failed` / `reconcile_unhealthy`: read the `action` and
  `error` fields (dashboard; the log-read script drops them). A
  well-formed numeric broadcaster id that Twitch rejects with 400 during a
  create is fixed in the allowlist, not at Twitch; transient 429/5xx
  answers are retried by the next pass, and later broadcasters in the same
  pass were not starved. That no-starvation note covers rejected actions
  only — a malformed allowlist entry never reaches an action (see the lone
  `reconcile_failed` above).
- Lone `reconcile_failed`: read its `error` field first. An invalid
  broadcaster id or a more-than-100-entry allowlist means fix the
  `TWO_TWITCH_BROADCASTERS` format and redeploy/restart — no broadcaster
  is reconciled until then. A token-endpoint rejection means
  check the client id/secret provisioning (names, never values); do not
  substitute another credential. A listing rejection is not always a
  credentials problem: read its `error` field before touching
  provisioning.
- A `revocation` delivery is acknowledged by the receiver and healed by
  the next scheduled pass (the revoked entry is absent or
  `authorization_revoked`), not by an immediate revocation-triggered
  pass. Because the interval is counted from the previous pass's
  completion, the wait can exceed one hour — restart the container to run
  the startup pass sooner.
  Source: `crates/bot/src/eventsub_reconcile.rs:400-425`,
  `docs/stream-funnel.md:106-108`.

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
`crates/core/src/twitch_eventsub.rs:168-199`.

Classification after verification:

- A well-formed challenge (nonempty, at most 256 printable ASCII characters)
  on a `stream.online` v1 subscription is echoed as the `200 text/plain`
  body with `Cache-Control: no-store`.
  Source: `crates/core/src/twitch_eventsub.rs:243-249,263-292`,
  `crates/bot/src/eventsub_http.rs:350-366`.
- Only `stream.online` version `1` is accepted; any other subscription type
  or version is a deterministic `WrongSubscription` error.
  Source: `crates/core/src/twitch_eventsub.rs:243-249,275-280`.
- An empty, oversized, or non-graphic challenge is `InvalidChallenge` and
  answers `400 malformed`, never an echo.
  Source: `crates/bot/src/eventsub_http.rs:412-425`.

If the challenge echo never arrives at Twitch, check in order: the Worker
route gate (both flags `1`?), the container bind (did startup refuse?), the
secret agreement (401s in the logs?), a `200` with an `eventsub delivery
classification failed` warn (`error_variant = "wrong_subscription"`,
`"invalid_body"`, or `"duplicate_keys"`) right after the subscription creation (the challenge was
not echoed — delete and recreate the subscription, see below), and only
then the callback URL shape
(plain `https`, no port, query, fragment, or userinfo; canonical form).
Source: `crates/core/src/twitch_eventsub.rs:428-446`,
`crates/bot/src/eventsub_http.rs:403-411`.

## Refusal matrix

Container refusals use the same JSON envelope
(`{"ok": false, "request_id", "error": {"code", "message": "request refused", "retryable"}}`)
and log one sampled scalar warn line (`event="eventsub_receiver"`,
`status`, `error_class`): the first warn, then every 64th process-wide, so
hostile bursts count in counters, never per-request logs. Classify errors
warn through the same sampler with an added bounded `error_variant`.
Worker-ingress refusals use the same envelope except the
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
| 400 | `malformed` | false | Present-but-badly-shaped delivery header, signature without the `sha256=` prefix, malformed `content-length` / body framing, or any caller-supplied `x-two-eventsub-*` refresh header (a privileged DO-set field, never Twitch input). Check the request shape, not the secret. Source: `wrangler/src/eventsub.ts:158-169` (header/signature shape), `:181,184,208,212` (content-length and body framing), refresh-header refusal in `allowlistedHeaders`. |
| 401 | `unauthorized` | false | Missing delivery header or signature. Check that the sender forwarded every header. Source: `wrangler/src/eventsub.ts:158-169`. |
| 429 | `rate_limited` | true | Per-isolate burst budget or in-flight cap hit. The sender backs off and redelivers. Source: `wrangler/src/eventsub.ts:243-248`. |
| 504 | `upstream_timeout` | true | The fenced DO did not answer within the Worker deadline. Source: `wrangler/src/eventsub.ts:264`. |
| 503 | `unavailable` | true | DO construction / binding failure, cold container, ownership-fence refusal, relay failure (see the 503 triage below), or — with rotation armed — a missing signing secret or refresh-signing failure before the container is contacted. Source: `wrangler/src/eventsub.ts:268,299-308,328,338,343` (ingress and relay failures), `wrangler/src/index.ts:905-915` (cold container at `:905`, `containerFetch` failure at `:906-910`, fence refusal at `:912-915`), refresh attach in `internalEventsub`. |

The DO relays receiver answers through `relayEventsubResponse`, which
projects them to the supported contract before they leave: JSON
acknowledgements become exactly `{ok: true, request_id}` (status 200) and
refusals become exactly
`{ok: false, request_id, error: {code, message: "request refused", retryable}}`
with their valid container-layer status, code and retry policy from the table
below; extra fields and upstream message text are dropped. A `200 text/plain`
body echoes byte for byte only when it is 1..=256 ASCII-graphic bytes. Empty,
malformed, scalar or contradictory JSON, unknown codes, foreign or mismatched
correlation ids, and invalid challenge bytes all become the fixed 503
`unavailable` envelope carrying the forwarded request id. Relayed and refusal
responses carry `no-store` plus the decided security headers; only a numeric
`retry-after` passes through. This is schema/outflow enforcement, not proof of
upstream authenticity.

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
| 503 | `unavailable` | true | A future/unrecognized post-verification classification error, or — with rotation armed — a missing, malformed, unauthenticated, stale, replayed, or invalid secret refresh. Both fail closed without verifying against a stale snapshot; the classification case counts its own variant metric label, the refresh case never reaches verification or classification. This is distinct from Worker-ingress `unavailable` above. The sampled classify warn carries the bounded `error_variant`, so the classification case names its variant; the refresh case identifies no specific cause: check Worker secret/version and reload-key agreement, not the secret value. Do not assume saturation or a Worker relay failure. Source: `crates/bot/src/eventsub_http.rs`, `crates/core/src/eventsub_refresh.rs`. |

Verified deliveries that need no operator action answer `200`:

- `notification` and `revocation`: acknowledged as `200 {"ok": true, "request_id"}`.
  While delivery stays dark the receiver only acks a notification; with
  delivery enabled a 200 also covers a posted or deduplicated go-live. The
  reconciler (above) re-creates a revoked subscription on its next
  scheduled pass when the broadcaster is still in the allowlist, so
  re-subscription after a revocation is automatic, not manual and not
  immediate — and not bounded by one hour (see above).
- Unknown message types, and known deterministic classification errors on
  non-verification messages (`WrongSubscription`, `InvalidBody`,
  `DuplicateKeys`): acknowledged with 2xx so Twitch does not retry what
  will never succeed.
  Unknown/future `classify()` errors are not included: they return retryable
  503 `unavailable` and count under their own variant metric label so an
  unrecognized failure is not silently discarded or conflated with known
  ignored deliveries.
  This carve-out excludes `webhook_callback_verification`: `classify()`
  returns `WrongSubscription` / `InvalidBody` / `DuplicateKeys` for a challenge
  with the wrong type or version, an unparsable signed body, or a signed body
  that repeats a key, and the handler still acks
  `200 {"ok": true, "request_id"}` with only a sampled `eventsub delivery
  classification failed` warn (`error_variant = "wrong_subscription"`,
  `"invalid_body"`, or `"duplicate_keys"`) without echoing the challenge — so
  the subscription is never enabled. A `200` with that warn immediately after
  a subscription creation means the challenge was not echoed: delete and
  recreate the subscription.

Source: `crates/bot/src/eventsub_http.rs:350-411`.

Log lines to expect (scalar fields only):

- `eventsub delivery refused` (sampled warn: `status`, `error_class`)
- `eventsub delivery classification failed` (sampled warn: `status`,
  `error_class`, bounded `error_variant`, `message_type`); for unknown/future
  classification errors expect `status = 503` and
  `error_class = "unavailable"`, plus the variant metric increment
- `challenge answered`, `notification acknowledged`, `revocation acknowledged`,
  `delivery ignored` (info: `status = 200`)

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
degrades to ack-only. An enabled delivery on a boot identity where
Announcements is not cleared (live or unknown identity) refuses startup
the same way, like every other bot-identity posting job. Because a bad value
crash-loops the container, set
the three inputs first, redeploy, and flip the gate last. Enable on
staging first and keep the posted go-live as enablement evidence;
production enablement is its own rollout decision. To stop posting
without touching the receiver, unset the gate (or set it to `"0"`) and
redeploy: verified deliveries return to ack-only.

## Lost go-live announcements

Outcome reason `send_failed_unreleased` means the Discord send provably
never landed, but releasing the started claim row failed (database error
or noop), so the row stays fenced: the Twitch retry is answered
`Duplicate` (2xx) and the announcement never posts. There is no
automatic re-post or release retry; the row is never reclaimed. It
counts as attempted+failed in `eventsub_post_failed_share`, like every
other sender-reaching failure, and it logs one fixed-class warn naming
the reason with no delivery data, safe to alert on as-is.

Operator action: confirm the stream is (or was) actually live on
Twitch, post a manual announcement if the moment still matters, and
investigate the release failure in the container logs and database
health around the warn. Do not hand-delete the ledger row and do not
redeploy to repair one lost announcement.
Source: `crates/core/src/stream_live_store.rs:614-642`,
`crates/bot/src/eventsub_delivery.rs:86-98`.

## Dark-unless-enabled behavior

Dark is the normal state, not an outage:

- `TWO_EVENTSUB_DELIVERY` unset or `"0"`: the receiver (when enabled)
  acknowledges verified notifications without posting; no delivery input
  is read.

- `TWO_EVENTSUB` unset or `"0"`: no listener, no bind attempt, no secret
  read, and no reconcile task — the reconciler logs one parked line
  (`status="parked"`, `reason="flag_dark"`) and spawns nothing. Health and
  readiness are unchanged.
  Source: `crates/core/src/eventsub_receiver_config.rs:32-51`,
  `crates/bot/src/eventsub_reconcile.rs:427-444`.
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
