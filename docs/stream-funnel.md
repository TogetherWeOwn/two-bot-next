# Stream funnel: Twitch live alerts + YouTube VOD links

Pure-helper slice. No network, secrets, database, Discord client or polling
loops. Transport wiring (EventSub receiver, secret handling,
announcements-channel posting) is a separate reviewed slice.

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
