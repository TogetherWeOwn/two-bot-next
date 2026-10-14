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
Twitch retry it. Only a transient send or durable-ledger failure is retriable:
release the claim and return 5XX. See `crates/core/src/stream_funnel.rs`.

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
   `broadcaster + started_at` is skipped. If the send or durable-ledger operation
   fails transiently, the caller releases the claim and answers 5XX so Twitch
   retries. Classification or planning errors alone must not trigger a retry.

Subscription management is pure request building plus a diff
(`create_subscription_body`, `parse_subscriptions`, `reconcile`). It runs at
boot and after a `revocation`, never on a timer, and it only touches
subscriptions on the bot's own callback. Deletes precede creates because
Twitch answers 409 to a duplicate create.

Secret handling: the signing secret is a `WebhookSecret` (10-100 ASCII, as
Twitch requires) wrapping `Secret<String>`; its `Debug` is `[REDACTED]`. It is
exposed only inside the HMAC and inside the create-subscription body, which is
itself returned as a `Secret`. The Operator provisions the signing secret and
the Twitch client id/secret; they never appear in code, logs or PR text.

Not in this slice (each its own card): the container receiver and Operator
secret provisioning. The public Worker route is staging-only and
default-dark. The ledger below is present but is not called by a runtime
receiver yet.

## Durable go-live ledger

- One `stream_live_claims` row per go-live, keyed by `live_dedup_key`.
  `claim_live` inserts with `ON CONFLICT DO NOTHING`: the inserter owns
  the post, while any existing claim is acknowledged with 2xx and skipped.
- Crash recovery is a bounded lease over never-started claims only
  (migration 0443, `send_started_at`, `STALE_CLAIM_LEASE_SECS = 300 s`).
  The receiver MUST call `mark_live_send_started` before the first Discord
  POST; `reclaim_stale_live` atomically takes over one stale row with
  `delivered_at IS NULL AND send_started_at IS NULL` and restarts its
  `claimed_at` lease, so concurrent retries cannot both win. A crash before
  send recovers on the next stale retry; the route stays dark until a
  receiver wires this flow.
- A Discord send may succeed while `mark_live_delivered` fails. The claim can
  then remain `delivered_at IS NULL` with `send_started_at NOT NULL`;
  `release_live` deletes the targeted row only when it is unstamped, so it
  must never be used as stale recovery. Started and delivered rows are never
  reclaimed, even when stale, or a retry can double-post a delivery that may
  already have reached Discord.
- Delivered rows are retained indefinitely as dedup fences. This slice adds no
  purge; the retained policy is one row per go-live, kept forever, which never
  weakens the active dedup fence.
- The receiver MUST post with `allowed_mentions: { parse: [] }`. No sender
  exists in this slice, so that no-ping contract has not been runtime-verified.
- A send that provably never reached Discord deletes its unposted row
  (`release_live`) and the receiver answers 5xx so Twitch retries; a
  delivered row is stamped and never deleted, so retries after a successful
  stamp cannot double-post. The claim → send → settle table is
  `stream_live::disposition`; its async glue belongs with the receiver.

## Helix client with app-token refresh

`crates/core/src/twitch_helix.rs` drives the plan from `twitch_eventsub`:

- Credentials are Operator-provisioned (`TWO_TWITCH_CLIENT_ID`,
  `TWO_TWITCH_CLIENT_SECRET`, `TWO_TWITCH_WEBHOOK_SECRET`); the Container
  reads them from its own environment, never from forwarded Worker vars.
  Every value travels inside `Secret`, so even a `Debug` dump renders
  `[REDACTED]`.
- The app token is cached and refreshed 60 s before its `expires_in`
  instant, plus once on a 401 with a single retry. No timers: reconcile
  runs at boot and after a `revocation` only.
- `list_subscriptions` follows every `stream.online` page (up to a bounded
  limit); pass its complete result to `plan_boot_actions` for the deletes-first
  plan, then `execute_actions` runs it in order. A create that answers 409
  counts as done only after a follow-up listing confirms a healthy subscription
  for that broadcaster on the configured callback.
