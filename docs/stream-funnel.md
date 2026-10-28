# Stream funnel: Twitch live alerts + YouTube VOD links

Pure-core slices. No network, secrets, database, Discord client or polling
loops live in `crates/core`. The wiring that touches them (Worker route and
container receiver, Helix client, credential loading, durable dedup ledger,
announcements-channel send) is split into separate reviewed slices; see
[Transport](#transport-webhook).

## Twitch `stream.online` (confirmed 2026-10-12)

- Subscription: `stream.online` version 1, condition `broadcaster_user_id`.
- Authorization scope: none. Token type depends on the transport: webhook
  and Conduit subscriptions use an app access token from the
  client-credentials flow, while WebSocket subscriptions require a user
  access token (no scope for `stream.online`) and fail with an app token.
  A WebSocket transport therefore needs user-token handling and refresh;
  webhook/Conduit do not.
- Transports: webhook, WebSocket, Conduit.
- Cost: EventSub delivery carries no per-event quota units.
- References: `https://dev.twitch.tv/docs/eventsub/`,
  `https://dev.twitch.tv/docs/eventsub/handling-webhook-events/`,
  `https://dev.twitch.tv/docs/authentication/`,
  `https://dev.twitch.tv/docs/eventsub/manage-subscriptions/`.

The bot plans announcements only for `type: stream.online` + `version: 1`,
with a valid broadcaster id/login/`started_at`, matching condition, and an
`event.type` that, when present, is `live`. Other event types (`rerun`, `playlist`,
`watch_party`, `premiere`), subscription types (`stream.offline`,
`channel.update`) and versions are not announced. After signature verification,
a notification that cannot be classified or planned is deterministic and MUST
be acknowledged with 2XX and ignored; do not return an error status that makes
Twitch retry it. Only a transient send or durable-ledger failure is retriable
(5XX); only a send that provably never reached Discord releases the claim —
claim and stamp failures never release it. See
`crates/core/src/stream_funnel.rs`.

## YouTube VOD linking convention (confirmed 2026-10-12)

- Uploads use `videos.insert`, which has its own quota bucket: by default
  100 calls/day at a cost of 1 per call, separate from the default
  10,000 units/day general project quota. Increases go through the
  quota-extension form.
- Every game-night VOD description ends with the footer from
  `vod_description_footer`, and the pinned comment uses
  `vod_pinned_comment`. Both carry the Discord invite link. The pinned-comment
  title is unescaped plain text: YouTube renders backslashes literally, so no
  Discord Markdown escaping is applied there.
- There is no Community-post creation endpoint, so no helper plans one.
- Reference: `https://developers.google.com/youtube/v3/getting-started#quota`.

## No polling loops

- Live alerts consume EventSub push only. No Helix `streams` polling.
- VOD discovery reuses the existing no-quota Atom/RSS polling in `feeds`.
  No `youtube.search` loop (search calls are quota-expensive).
- This slice adds no interval, job, or background task.

## Transport: webhook

Decision: **webhook** (`crates/core/src/twitch_eventsub.rs`).

- Webhook subscriptions authenticate with an app access token from the
  client-credentials flow. There is no user token to store or refresh, which
  a WebSocket transport would need.
- Delivery is an inbound HTTPS push. The Worker can wake the sleeping
  container for it; a WebSocket would need a connection that outlives
  container idle. Conduit adds shard management for no benefit here.
- Twitch requires the callback to be HTTPS on port 443 with no userinfo,
  query or fragment. `CallbackUrl` also requires canonical URL spelling; omit
  explicit `:443` because the URL parser normalizes the default port.

What the core does, in order, for every delivery:

1. Cap the body (`MAX_BODY_BYTES`, 64 KiB) before hashing.
2. Verify `sha256=` HMAC-SHA256 over `message-id + timestamp + raw body`
   with the subscription secret, in constant time, then require a timestamp
   within 10 minutes of now. Nothing is parsed before this passes.
3. Classify `webhook_callback_verification` (echo a valid challenge as
   `text/plain`), `notification` and `revocation`. Only `stream.online`
   version 1 can produce an announcement. Unknown message types are acknowledged
   and ignored. For a verified notification, unsupported or malformed payloads
   and deterministic planning errors are also acknowledged with 2XX and ignored;
   only a transient side-effect failure is retriable.
4. For a go-live: check the broadcaster allowlist, render the post from the
   validated invite, then claim the go-live key. A retry or replay of the same
   `broadcaster + started_at` whose row is started or delivered is skipped
   with 2XX; a fresh never-started row is answered 5XX (`InFlight`), which
   asks Twitch to retry. If the send or
   durable-ledger operation fails transiently, the caller answers 5XX so
   Twitch retries; it releases the claim ONLY when the send provably never
   reached Discord (`SendFailed` in `stream_live::disposition`) — a claim
   failure or a stamp failure after a possibly-successful send never
   releases, or the retry could double-post. Classification or planning
   errors alone must not trigger a retry.

Subscription management is pure request building plus a diff
(`create_subscription_body`, `parse_subscriptions`, `reconcile`). The runtime
wiring (`crates/bot/src/eventsub_reconcile.rs`) runs it once at startup and on
a bounded hourly interval while `TWO_EVENTSUB` is exactly `1`, and it only
touches subscriptions on the bot's own callback (`TWO_TWITCH_CALLBACK_URL`).
Deletes precede creates because Twitch answers 409 to a duplicate create. The
plan runs one action at a time: a rejected action (for example a 400 for a
mistyped broadcaster id) is logged and counted, the later actions and the
final listing still run, and the pass then fails with the failed and missing
counts, so one bad id never starves the broadcasters after it. A
`revocation` delivery is acknowledged by the receiver and healed by the next
pass; the desired set comes from `TWO_TWITCH_BROADCASTERS` (empty parks the
pass without mutations). A successful Helix listing answer counts as
evidence only when it is an object carrying a `data` array: a missing or
non-array `data` envelope on any page fails the whole listing closed, so no
reconcile mutations are planned from invalid evidence and a malformed
confirmation listing can never report the pass healthy.

Secret handling: the signing secret is a `WebhookSecret` (10-100 ASCII, as
Twitch requires) wrapping `Secret<String>`; its `Debug` is `[REDACTED]`. It is
exposed only inside the HMAC and inside the create-subscription body, which is
itself returned as a `Secret`. The Operator provisions the signing secret and
the Twitch client id/secret; they never appear in code, logs or PR text.

The public Worker route (`POST /hooks/twitch`, `wrangler/src/eventsub.ts`)
is staging-only and dark unless `EVENTSUB_INGRESS` and `TWO_EVENTSUB` are
both exactly `"1"`. The private container receiver below lands in this
slice; Operator secret provisioning stays an Operator action. The receiver
calls the ledger below through `deliver_go_live` when delivery
dependencies are injected at bind time. Delivery is provisioned by
`crates/core/src/eventsub_delivery_config.rs`: boot injects the production
dependencies (bot pool ledger plus an admission-backed executor) only
while the receiver is enabled and `TWO_EVENTSUB_DELIVERY` is exactly
`"1"` with a valid announcements channel
(`TWO_EVENTSUB_ANNOUNCEMENTS_CHANNEL_ID`), invite
(`TWO_EVENTSUB_INVITE_URL`) and broadcaster allowlist
(`TWO_TWITCH_BROADCASTERS`, shared with the reconcile). Any other
delivery state stays ack-only if the gate is dark and refuses startup if
the gate is enabled, so posting is dark until the Operator provisions all
four values.

## Durable go-live ledger

- One `stream_live_claims` row per go-live, keyed by `live_dedup_key`.
  `claim_live` inserts with `ON CONFLICT DO NOTHING` and classifies a
  conflicting row in the same call, so its outcome is final: the inserter
  (or the taker-over of a stale never-started claim) owns the post. An
  existing started or delivered claim is acknowledged with 2xx and
  skipped; an existing fresh never-started claim is answered 5xx
  (`InFlight`, see `stream_live::disposition`), which asks Twitch to retry.
- Crash recovery is a bounded lease over never-started claims only
  (migration 0443, `send_started_at`, `STALE_CLAIM_LEASE_SECS = 30 s`).
  The receiver MUST call `mark_live_send_started` before the first Discord
  POST; the takeover inside `claim_live` (also exposed as
  `reclaim_stale_live`) atomically takes over one stale row with
  `delivered_at IS NULL AND send_started_at IS NULL` and restarts its
  `claimed_at` lease, so concurrent retries cannot both win. Lease length
  does not affect safety: the `send_started_at` fence alone decides who
  may post. A crash before send is answered 5xx while the lease is fresh
  and recovers by takeover only if a retry arrives after the lease
  expires. Twitch documents no retry schedule and this slice has no
  sweeper, so a go-live that is never retried is not recovered here.
- Ownership is fenced by a claim token (migration 0443): claiming
  generates one, takeover rotates it, and `mark_live_send_started`,
  `mark_live_delivered`, `release_live` and `release_started_live` act
  only for the token they are given. A timed-out-but-running original
  owner therefore cannot
  start, stamp or release a row a takeover gave to a new owner; exactly
  one owner can post. If the conflicting row disappears before the
  takeover lands, `claim_live` loops back to the insert instead of
  answering 2xx for a go-live nobody posted.
- A Discord send may succeed while `mark_live_delivered` fails. The claim can
  then remain `delivered_at IS NULL` with `send_started_at NOT NULL`;
  `release_live` deletes a row only for its token-holding owner while it
  is still never-started, so it must never be used as stale recovery or
  after a stamp failure; `release_started_live` is the separate
  token-gated path for a started row whose send provably never landed,
  and must likewise never be used on an ambiguous outcome or after a
  stamp failure. Started and delivered rows are never
  reclaimed, even when stale, or a retry can double-post a delivery that may
  already have reached Discord.
- Delivered rows are kept as dedup fences until they age past
  `DELIVERED_DEDUP_RETENTION_DAYS` (90 days), then the retention cadence purges
  them whole (`expire_delivered_live`, full-row deletes, no tombstone). The
  purge only ever matches `delivered_at IS NOT NULL`, so never-started and
  started-but-ambiguous rows stay fenced however old, and a retry inside the
  horizon still dedups instead of double-posting.
- The sender posts with `allowed_mentions: { parse: [] }`: the production
  `LiveSender` is the shared REST executor's `post_message`, whose body
  pins exactly that shape.
- A send that provably never reached Discord deletes its unposted row
  (`release_live` before the send starts, `release_started_live` after
  it starts) and the receiver answers 5xx so Twitch retries; a
  delivered row is stamped and never deleted, so retries after a successful
  stamp cannot double-post. The claim → send → settle table is
  `stream_live::disposition`; its async glue is
  `stream_live_store::deliver_go_live`, called by the receiver for a
  verified notification when delivery dependencies are injected
  (`crates/bot/src/eventsub_delivery.rs`).

## Private container receiver

`crates/bot/src/eventsub_http.rs` plus
`crates/core/src/eventsub_receiver_config.rs`. Operator procedure:
[EventSub operator runbook](eventsub-runbook.md).

- Binds `0.0.0.0:8092` (`EVENTSUB_RECEIVER_PORT` in `wrangler/src/eventsub.ts`)
  when `TWO_EVENTSUB` is exactly `"1"` with a valid `TWO_EVENTSUB_BIND` and
  the Operator-provisioned `TWO_TWITCH_WEBHOOK_SECRET`. Any other state is
  dark; an enabled-but-invalid config stops startup, never downgrades.
- Runs `verify_delivery` plus `classify` with the Operator secret. Nothing
  is parsed before verification passes. When the Operator arms no-restart
  rotation (`TWO_EVENTSUB_RELOAD_KEY` on both sides), the fenced DO attaches
  the Worker's current secret to every delivery with a dedicated refresh HMAC
  (`x-two-eventsub-*` headers, never caller-supplied — the edge ingress
  refuses them), and the receiver authenticates, validates and atomically
  swaps its in-memory snapshot before verifying that delivery
  (`crates/core/src/eventsub_refresh.rs`). Any refresh failure answers
  retryable 503 `unavailable` without verifying against the stale snapshot;
  each request swaps-then-verifies, so concurrent deliveries each use one
  consistent snapshot and a superseded in-flight delivery is a deterministic
  401, never a 500.
- Echoes `Delivery::Challenge` as `200 text/plain`; acknowledges
  `Revocation`, `Ignored` and deterministic classification
  errors (`WrongSubscription`, `InvalidBody`, `DuplicateKeys`) as `200 {"ok":true}`
  so Twitch does not retry what will never succeed. A verified `Notification`
  is acknowledged the same way while no delivery dependencies are injected;
  with them, it runs `deliver_go_live` and answers its disposition (200, or
  a retryable 503 after a transient ledger/send failure).
- Answers `400` for a malformed challenge, `401` for a missing or mismatched
  signature, stale timestamp or malformed delivery headers, `405` for a
  non-POST, `408` for a body slower than the read budget, `413` for an
  oversize body, `415` for a wrong content type, `404` for a wrong path, and
  `503` (`retryable: true`) while the receiver is busy or past the overall
  20 s budget. Only the `408` and `503` paths are retriable; with delivery
  dependencies injected, a transient ledger/send failure also answers the
  retryable 503, and the claim fences make that redelivery safe. No header
  value, challenge, body byte or query is ever logged; only scalar
  `event`/`status`/`error_class`.
- Keeps the callback path out of trace spans: `/hooks/twitch` is not in
  `TRACED_PATHS` (`crates/bot/src/server.rs`) and renders as `[REDACTED]`.
- The DO adds `8092` to `requiredPorts` plus the container bind only while
  `TWO_EVENTSUB` is `"1"` with the signing secret present; the Worker
  forwards the flag, the fixed bind, the container marker, the reconcile
  inputs (`TWO_TWITCH_CALLBACK_URL`, `TWO_TWITCH_BROADCASTERS`) and the Twitch
  credentials (`TWO_TWITCH_CLIENT_ID`, `TWO_TWITCH_CLIENT_SECRET`,
  `TWO_TWITCH_WEBHOOK_SECRET`) on explicit lines, and forwards nothing when
  the secret is missing. The refresh HMAC key (`TWO_EVENTSUB_RELOAD_KEY`)
  travels on its own explicit line inside that forward when set; absent stays
  absent and rotation stays disarmed. The delivery gate and its channel/invite
  inputs travel on their own explicit lines inside that forward, only while
  `TWO_EVENTSUB_DELIVERY` is exactly `"1"`.
  `wrangler/scripts/check-env-bindings.py` rejects any `TWO_EVENTSUB*` or
  `TWO_TWITCH_*` var in `wrangler.toml`.

## Helix client with app-token refresh

`crates/core/src/twitch_helix.rs` drives the plan from `twitch_eventsub`:

- Credentials are Operator-provisioned (`TWO_TWITCH_CLIENT_ID`,
  `TWO_TWITCH_CLIENT_SECRET`, `TWO_TWITCH_WEBHOOK_SECRET`); the Worker
  forwards them on explicit containerEnvVars lines while the EventSub receiver
  is forwarded, and the Container reads them from its process environment.
  Every value travels inside `Secret`, so even a `Debug` dump renders
  `[REDACTED]`.
- The app token is cached and refreshed 60 s before its `expires_in`
  instant, plus once on a 401 with a single retry. The Helix calls have no
  timers of their own; the bot runtime (`crates/bot/src/eventsub_reconcile.rs`)
  invokes them once at startup and on a bounded hourly interval. Each pass
  signs creates with the receiver's live snapshot, never boot env, so a
  rotated secret signs the next creates with no restart.
- `list_subscriptions` follows every `stream.online` page (up to a bounded
  limit); pass its complete result to `plan_boot_actions` for the deletes-first
  plan, then `execute_actions` runs it in order. A create that answers 409
  counts as done only after a follow-up listing confirms a healthy subscription
  for that broadcaster on the configured callback. After the plan executes, a
  final listing confirms every desired broadcaster holds a healthy subscription
  on the bot callback; anything missing or unhealthy logs with an `error_class`
  and fails the pass, never silently.
