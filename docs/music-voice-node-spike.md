# Music voice-node spike (P0): external Lavalink node on a Cloudflare Container vs a small VPS

Status: spike in progress, staging guild only, never production. This document
is the spike report: research, architecture, experiment protocol and the
Container-vs-VPS decision record. Numbers land here after the staging run.

## Why an external node

The bot never joins voice today: no voice gateway, no UDP, no Opus
(`docs/adr/0001-discord-framework.md`). Two facts force the shape of any music
work:

- DAVE end-to-end voice encryption is mandatory since 2026-03-02. A voice
  connection without it is closed with code 4017.
- Lavalink 4.2.x ships Discord's official libdave (4.2.0 first, 4.2.2 current
  for this spike). Songbird 0.6 has an open send-stall issue and no DAVE
  story, so in-process Rust audio is out.

So music means a separate voice node speaking DAVE, driven by the bot over
Lavalink's REST/WS API. The node never holds the bot token: the gateway
forwards the per-guild voice handshake (token, endpoint, sessionId,
channelId) and the node uses only that. Track URLs stay short-lived signed
links, and only the `http` source is enabled on the node.

## The one unknown: outbound UDP from a Cloudflare Container

Bandwidth is not the blocker. Opus runs about 0.07 GB per stream-hour, so 10
streams 24/7 is about 500 GB/mo, inside the included 1 TB.

The unknown is whether a Cloudflare Container can send sustained outbound UDP
to Discord voice servers. What the platform docs say today:

- Outbound handlers (`outbound`, `outboundByHost`) intercept HTTP/HTTPS on
  ports 80/443 only. Traffic on other ports is never routed through them.
- With `enableInternet = false` that non-HTTP traffic is denied and only
  ports 80/443 are available. With `enableInternet = true`, other ports
  bypass the host filters — but no doc states that UDP egress works, and no
  example runs voice/RTP from a Container.
- Inbound TCP via Spectrum exists (August 2026); inbound UDP is a different
  feature and not what the node needs (the node dials out).

The spike answers this empirically: run the Lavalink 4.2.2 image as a second
staging Container (`basic`: 1/4 vCPU, 1 GiB, 4 GB disk — the JVM will not fit
`lite` headroom) with internet enabled, drive it from the staging bot, and
listen. If UDP egress fails, the fallback is a small VPS (OVH VPS-1 Vint
Hill, about $5.35/mo, unlimited traffic — buying it needs an owner OK).

Image note: Lavalink documents a glibc floor of 2.35 for DAVE. The musl
(alpine) image variant cannot initialise libdave and fails with 4017 at
position 0. The spike uses the glibc image only.

## Architecture (staging)

```text
staging bot (TwoBotContainer, lite)
  |  gateway: VOICE_STATE_UPDATE (self: sessionId, channelId)
  |           VOICE_SERVER_UPDATE (token, endpoint)
  v
forwarder (TWO_MUSIC=1, staging GUILD_ID fence)
  |  PATCH /v4/sessions/{id}/players/{guild}
  |  {"voice": {token, endpoint, sessionId, channelId}}
  v
Lavalink 4.2.2 (second Container, basic, glibc image, http source only)
  |  DAVE voice (UDP out to Discord)
  v
staging voice channel
```

Placement and secrecy rules:

- The node is never public: no public route reaches it. The bot talks to it
  over the Durable Object binding / `outboundByHost`, and the node's
  password travels as a Worker secret on its own explicit
  `containerEnvVars` line, never through the flag allowlist.
- The client is lavalink-rs 0.15 or a small REST/WS client — whichever the
  spike validates first. Either way the v4 REST shape above is the contract,
  built today by `crates/core/src/music_voice.rs` (pure, unit-tested).
- Staging-only follows the `TWO_VOICE` precedent: `TWO_MUSIC=1` in
  `[env.staging.vars]` only, forwarded via `FORWARDED_FLAGS`, refused
  top-level and in production by `wrangler/scripts/check-env-bindings.py`. Rollback
  is delete-the-line plus redeploy. Every forward additionally checks the
  dispatch guild against the configured staging `GUILD_ID`
  (`music_voice::is_staging_guild`).

## What this slice lands

- `MusicGates`: exact-`"1"` `TWO_MUSIC` gate (mirrors `VoiceGates`).
- `VoicePairing`: per-guild pairing cache. Server and state halves arrive on
  separate dispatches in either order; a bot-leave (`None` channel) drops
  the guild entry; a server refresh replaces the endpoint. A new session id
  starts a new generation and drops the stale server half, unless a server
  half arrived after the pair completed (server-first rejoin, kept): a
  region move followed by an external move therefore transiently re-pairs
  the region half until the new session's own server half lands and heals
  it. `reset` before each `UpdateVoiceState` send keeps our own rejoins
  exact in either order. The voice token rides `Secret<String>`, redacted
  under `Debug`.
- `PlayerVoiceUpdate::player_path` / `voice_body`: the exact v4 PATCH path
  and JSON body, snowflakes as strings.
- `is_staging_guild`: the never-forward-outside-staging fence.
- `TWO_MUSIC` in `FORWARDED_FLAGS` so the Worker passes the gate through.
  No staging var is committed yet: the Operator enables it when the node
  is up.

Gateway capture (dry-run, staging only) wraps the voice sink
(`crates/bot/src/music_capture.rs`): `VoiceServerUpdate` plus self
`VoiceStateUpdate` (resolved from the cache identity) feed `VoicePairing`
behind `TWO_MUSIC=1` and the staging fence. Each distinct completed
handshake logs one structured line (guild, channel, `has_token` — never
token/endpoint values) and is dropped; a self leave with a live pairing
logs the clear. Redelivered identical halves (mute toggles, duplicate
dispatches) do not re-log; a changed half logs again as a new handshake.
A gateway disconnect silently drops pending/complete pairing and dedup state,
so both fresh halves are required after reconnect, in either arrival order.
No HTTP, no PATCH, no gateway send in the capture slice.

Still to come (follow-up cards): the REST/WS client, the
`UpdateVoiceState` send path (the bot never issues gateway voice commands
today), the second Container / DO binding / JVM image wiring, and the MVP
command surface (`/play`, `/queue`, `/skip`, `/stop`, `/nowplaying`,
auto-leave, metrics).

## Experiment protocol

Run from the staging bot against the staging guild, driven by a human with
ears. Every step records its evidence in the run log linked below.

1. Join reliability: 50 joins, expect at least 49 audible (98%), zero 4017.
2. DAVE rekey: 2 humans join or leave mid-track; playback stays audible
   across the key change.
3. Soak: 2 hours continuous playback, no dropouts.
4. Footprint: sample node CPU, RSS and bytes out hourly; report GB/hour.

Pass criteria (all must hold):

- [ ] no 4017 errors
- [ ] at least 98% of 50 joins audible
- [ ] still audible when 2 humans join or leave mid-track (DAVE key changes)
- [ ] 2 h with no dropouts
- [ ] CPU, RSS and GB per hour recorded

## Decision record: Container vs VPS

| | Cloudflare Container (`basic`) | Small VPS (OVH VPS-1) |
|---|---|---|
| Unit cost | about $6.9/mo (ADR 0001 sizing) | about $5.35/mo, unlimited traffic |
| UDP egress | UNKNOWN — this spike proves it | assumed yes — probe before buying |
| Deploy provenance | staging receipt covers Worker + digest | split: outside the Worker deploy |
| Logs/metrics | needs a Worker mirror (Container stdout is not in Workers Logs) | node-local, needs its own scrape |
| Purchase gate | none (existing account) | owner OK via the host |

Recommendation: Container if the protocol passes fully; VPS if UDP egress
fails or the footprint does not fit `basic`. The numbers below decide.

### Measured numbers (pending the staging run)

| Metric | Container run | VPS run (if needed) |
|---|---|---|
| 4017 count | — | — |
| Audible joins / 50 | — | — |
| Rekey audible | — | — |
| 2 h soak dropouts | — | — |
| CPU (avg/p95) | — | — |
| RSS (avg/max) | — | — |
| GB/hour out | — | — |

Run log: link the staging run record here when the Operator card completes.
