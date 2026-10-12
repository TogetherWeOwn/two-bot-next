# Stream funnel: Twitch live alerts + YouTube VOD links

Pure-helper slice. No network, secrets, database, Discord client or polling
loops. Transport wiring (EventSub receiver, secret handling,
announcements-channel posting) is a separate reviewed slice.

## Twitch `stream.online` (confirmed 2026-10-12)

- Subscription: `stream.online` version 1, condition `broadcaster_user_id`.
- Authorization scope: none. The subscription call uses an app access token
  from the client-credentials flow.
- Transports: webhook, WebSocket, Conduit.
- Cost: EventSub delivery carries no per-event quota units.
- References: `https://dev.twitch.tv/docs/eventsub/`,
  `https://dev.twitch.tv/docs/authentication/`.

The bot accepts only `type: stream.online` + `version: 1`, validates the
broadcaster id/login/`started_at`, checks the condition against the event,
and keys redelivery dedup on broadcaster + instant. Anything else
(`stream.offline`, `channel.update`, wrong version) is rejected before any
post is planned. See `crates/core/src/stream_funnel.rs`.

## YouTube VOD linking convention (confirmed 2026-10-12)

- Uploads use `videos.insert`: 1,600 quota units per call against the
  default 10,000 units/day project quota (about six uploads a day).
  Increases go through the quota-extension form.
- Every game-night VOD description ends with the footer from
  `vod_description_footer`, and the pinned comment uses
  `vod_pinned_comment`. Both carry the Discord invite link.
- There is no Community-post creation endpoint, so no helper plans one.
- Reference: `https://developers.google.com/youtube/v3/getting-started#quota`.

## No polling loops

- Live alerts consume EventSub push only. No Helix `streams` polling.
- VOD discovery reuses the existing no-quota Atom/RSS polling in `feeds`.
  No `youtube.search` loop (search calls are quota-expensive).
- This slice adds no interval, job, or background task.
