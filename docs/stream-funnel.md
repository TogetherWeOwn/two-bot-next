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
  `https://dev.twitch.tv/docs/authentication/`,
  `https://dev.twitch.tv/docs/eventsub/manage-subscriptions/`.

The bot accepts only `type: stream.online` + `version: 1`, validates the
broadcaster id/login/`started_at`, requires a present `event.type` to be
`live` (`rerun`, `playlist`, `watch_party`, `premiere` are rejected),
checks the condition against the event,
and keys redelivery dedup on broadcaster + instant. Anything else
(`stream.offline`, `channel.update`, wrong version) is rejected before any
post is planned. See `crates/core/src/stream_funnel.rs`.

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
  query or fragment. `CallbackUrl` refuses anything else.

What the core does, in order, for every delivery:

1. Cap the body (`MAX_BODY_BYTES`, 64 KiB) before hashing.
2. Verify `sha256=` HMAC-SHA256 over `message-id + timestamp + raw body`
   with the subscription secret, in constant time, then require a timestamp
   within 10 minutes of now. Nothing is parsed before this passes.
3. Classify `webhook_callback_verification` (echo the challenge as
   `text/plain`), `notification` and `revocation`. Only `stream.online`
   version 1 is accepted; unknown message types are acknowledged and ignored.
4. For a go-live: check the broadcaster allowlist, render the post from the
   validated invite, then claim the go-live key. A retry or replay of the same
   `broadcaster + started_at` is skipped. If the send fails the caller
   releases the claim and answers 5xx so Twitch retries.

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

Not in this slice (each its own card): the public Worker route and container
receiver, the Helix client with app-token refresh, Operator secret
provisioning, and a durable dedup ledger. Until the ledger lands, dedup is
in-memory (`RecentKeys`), so a crash between the post and the 2xx can post
once more after restart.
