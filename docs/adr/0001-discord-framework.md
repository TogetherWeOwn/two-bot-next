# ADR 0001: Discord framework for two-bot-next — twilight

- **Status:** Accepted (merge of this PR records the decision)
- **Date:** 2026-09-29
- **Card:** TOG-9787
- **Deciders:** CTO & Chief AI Officer (manager: President & COO); owner directive 2026-09-29 mandates the Rust rewrite

## Context

two-bot-next is a Rust rewrite of
[two-bot](https://github.com/TogetherWeOwn/two-bot) (TypeScript/discord.js,
now maintenance-only), targeting one always-on Cloudflare Container (`lite`
if RSS stays under ~200 MiB, else `basic`) with shared Neon Postgres
([TOG-9679](/TOG/issues/TOG-9679)).

[TOG-9408](/TOG/issues/TOG-9408) (research, done 2026-09-29) recommended
*keeping* discord.js: Rust only converts `basic` ($7/mo) → `lite` ($1.7/mo),
≈ $5/mo saving against a ~150–250-run rewrite. The owner overrode that
recommendation with an explicit directive: rewrite in Rust if it saves up to
~$5/month. That language decision is therefore **out of scope here** — this
ADR decides *which* Rust Discord framework, within the directive.

### Real two-bot workload (two-bot `main`, verified this run)

- **Gateway intents:** Guilds, GuildMembers*, GuildModeration,
  GuildVoiceStates, GuildMessages, GuildMessageReactions, GuildInvites,
  MessageContent* (*privileged, requested only when automod/tickets justify
  it — `src/discord/client.ts`).
- **Gateway events consumed:** GuildMemberAdd/Update/Remove,
  VoiceStateUpdate, MessageCreate/Update, ReactionAdd/Remove, InviteCreate,
  GuildAuditLogEntryCreate, InteractionCreate, ShardResume/ShardReady, Raw
  (11 interaction handlers: onboarding, self-roles, tickets, moderation,
  leveling, announcements, automations, temp voice).
- **Voice:** presence/session tracking only (`voice_session_start/end`,
  temp-voice channel management). The bot **never joins a voice channel and
  never plays audio** — no voice gateway, no UDP, no Opus.
- **Scheduled in-process jobs:** inactivity sweep, community scorecard,
  presence probe, re-engagement, scheduled-events poller, rota scheduler.
- **State:** Postgres append-only `events` + projections (pool max 5);
  open voice sessions held **in memory** and deliberately dropped on
  reconnect (`ShardResume`/`ShardReady` handlers).
- **Scale:** one guild (~84 humans / 107 members); low thousands of gateway
  dispatches/day (estimated, no prod telemetry — see Consequences).

## Options considered

| Crate | Version (checked 2026-09-29) | License | MSRV | Activity |
|---|---|---|---|---|
| twilight-gateway / -http / -cache-inmemory | 0.17.1 (2025-12-13) | ISC | 1.89 | pushed 2026-09-26, not archived |
| serenity | 0.12.5 (2025-12-20) | ISC | 1.74 | pushed 2026-09-22, 5.6k stars, not archived |
| poise (on serenity) | 0.7.0 (2026-09-06) | MIT | 1.82 | pushed 2026-09-20, not archived |
| songbird | 0.6.0 (2026-04-05) | ISC | 1.83 | pushed 2026-04-08 |
| sqlx (DB, framework-independent) | 0.9.0 (2026-05-21) | MIT/Apache-2.0 | **1.94** | active (17.5k stars) |

**songbird is excluded:** it adds voice-connection audio (Opus, ffmpeg,
youtube-dl deps, ~100+ MiB). Our workload never touches voice audio; voice
presence arrives over the normal gateway (`VoiceStateUpdate`), which both
candidate frameworks deliver. Revisit only if voice playback becomes a
feature — it is not on any roadmap.

## Comparison

### Idle and peak RSS on the two-bot workload

Estimates (ranges, not measured — measurement is a gated follow-up):

| Framework | Idle RSS (1 guild, 1 shard) | Fits `lite` (256 MiB)? |
|---|---|---|
| twilight (gateway + http + selective cache) | ~15–40 MiB | Yes, large headroom |
| serenity + poise (default cache) | ~20–50 MiB | Yes, comfortable headroom |

Both fit `lite`. The difference is **control, not tier**: twilight's cache
is opt-in per resource type (`twilight-cache-inmemory` `ResourceType`
flags), so RSS is a deterministic function of what we subscribe to;
serenity caches broadly by default and trims opt-out. Since the *entire*
economic justification for Rust is RSS headroom on `lite`, the framework
that makes RSS a deliberate choice wins on the decision's own terms.

### Gateway resume/reconnect on Container restart

A Container restart (deploy, rollout, host move) kills the process: any
in-memory `session_id` + sequence is lost under **either** framework, so a
restart always risks a fresh IDENTIFY unless the app persists the session.

- **twilight:** the shard exposes its session (`session_id` + last sequence)
  for application-level persistence, and accepts a stored session at
  startup to attempt RESUME. This matches the existing two-bot precedent
  (`test:restart-storage`, restart-storage gate): session bytes live in
  Postgres, the gateway resumes when Discord still holds the session.
  Missed events during the gap are handled the way two-bot already does —
  voice sessions are dropped on `ShardResume`/fresh session rather than
  reported with outage-inflated durations.
- **serenity/poise:** resume after a transient disconnect is automatic and
  well-tested *within a process lifetime*; session state is managed
  internally, so persisting it across a process restart is not a
  first-class path. Fresh IDENTIFY on every Container restart is the
  practical outcome — acceptable (one guild, fast IDENTIFY), but strictly
  worse than twilight's resumability.

Neither framework changes the keepalive requirement below.

### Cloudflare Containers fit (`lite` vs `basic`, keepalive)

Framework-neutral, recorded so the choice is auditable:

- Both candidates idle at <20% of `lite`'s 256 MiB; **target `lite`, fall
  back to `basic` only if measured RSS exceeds ~200 MiB** (same gate as
  TOG-9408). CPU at this load is noise (<2% of 1/16 vCPU — inside the
  included 375 vCPU-min).
- **Keepalive risk ([TOG-9408](/TOG/issues/TOG-9408) caveat 1):** Container
  billing/sleep is driven by *inbound* requests; the gateway is an
  *outbound* WebSocket that generates no inbound traffic. An always-on bot
  needs a DO-alarm keepalive (or `sleepAfter` renewal) regardless of
  framework, or the container sleeps and silently drops join/voice events.
  twilight's explicit shard handle makes health-gating (`/readyz` on shard
  state) marginally easier; not decisive.

### sqlx / Postgres (Neon via S1 [TOG-9679](/TOG/issues/TOG-9679))

Identical for both candidates — a point in favour of deciding on gateway
merits rather than DB ergonomics:

- `sqlx` 0.9, compile-time-checked queries, `Pool<Postgres>` (max 5,
  matching today's `TWO_DB_POOL_MAX`), `statement_timeout` preserved.
- The bot connects **directly** from the Container (Hyperdrive is a Workers
  binding; not needed). Migration numbering stays: bot `0001–0999`, web
  `1000–1999`; `web_v1` read-only views + HMAC contract unchanged.
- **Toolchain consequence:** sqlx's MSRV (1.94) exceeds both frameworks',
  so CI pins stable ≥ 1.94 regardless of this decision.

### Maintenance, MSRV, license

Both are healthy: twilight (ISC, MSRV 1.89, commits days old),
serenity (ISC, MSRV 1.74, 5.6k stars, active), poise (MIT, MSRV 1.82,
released Sept 2026). poise adds a second maintainer surface (serenity-rs
org, effectively one active maintainer) on the command path; twilight is a
single org/repo across all crates. Licenses are all permissive and
BUSL-compatible.

### Monthly $ vs the current Node bot

Cloudflare Container pricing (per-10-ms active; memory/disk on
*provisioned*, CPU on *active*; Workers Paid $5 base shared account-wide,
excluded as marginal):

| Setup | Memory | Disk | CPU | **Total/mo** |
|---|---|---|---|---|
| Rust on **`lite`** (256 MiB·2 GB) | $1.40 | $0.31 | $0 | **≈ $1.7** |
| Node discord.js on **`basic`** (1 GiB·4 GB) | $6.26 | $0.68 | $0 | **≈ $6.9** |
| Node discord.js on `lite` (if RSS <200 MiB) | $1.40 | $0.31 | $0 | ≈ $1.7 |

Saving vs the realistic Node placement: **≈ $5.2/mo (≈ $63/yr)** — meets
the owner's ~$5/mo bar nominally. Honest caveats (from TOG-9408, still
true): the current bot runs on the Coolify VPS at $0 marginal cost, so the
company bill delta is **≈ $0 until the VPS is retired** (bot + web + DB all
off it); at 10x load both placements stay under ~$10/mo.

## Decision

**Use twilight** (`twilight-gateway` + `twilight-http` +
`twilight-cache-inmemory` with a minimal `ResourceType` set; `twilight-util`
and `twilight-standby` as needed). No songbird. No poise/serenity.

Reasons:

1. **The cost lever stays deliberate.** The rewrite exists to buy RSS
   headroom; twilight's opt-in cache makes RSS a code-reviewed choice
   instead of a default to trim later.
2. **Architectural fit.** two-bot's core/adapter split (`src/core/` has no
   discord.js import) ports 1:1 onto twilight's model/http/gateway split:
   port the funnel rules as framework-free Rust, write a thin twilight
   adapter. serenity's `Context`-threaded handlers would collapse that
   seam.
3. **Resume across Container restarts** is a first-class application path
   (persist session, attempt RESUME), matching the existing
   restart-storage precedent.
4. **Workload shape.** The bot is gateway-event-heavy (funnel recording,
   moderation, voice presence); slash commands are ~a dozen handlers, not
   hundreds. poise's strength (command ergonomics) addresses the minority
   of the workload, while twilight's explicit event stream addresses the
   majority.
5. **Smaller compile surface** (only the crates we use) keeps CI fast and
   the binary small.

Accepted trade-off: hand-rolled slash-command routing (a small module;
the `commandRegistry` pattern ports directly) instead of poise's derive
macros. Mitigation: the command surface is small and the prototype below
proves the pattern once.

## Feature-port checklist (sliced into TWO Bot Next cards)

- [ ] S1 scaffold: Cargo workspace, `check` CI (fmt/clippy/test), Dockerfile, wrangler Container + DO-alarm keepalive, secrets via Worker env
- [ ] S2 measured prototype: gateway connect + one slash command against `tools/mock-discord`, RSS/CPU recorded; **gate: RSS <200 MiB confirms `lite`**
- [ ] S3 gateway + intents + funnel pipeline (join/leave/gate-clear, message milestones, voice sessions, invite attribution)
- [ ] S4 commands: leveling/rank, onboarding welcome, self-roles, tickets, moderation/containment/anti-nuke, announcements/automations/temp-voice
- [ ] S5 jobs + audit sink + moderation-audit MAC + restart session persistence
- [ ] S6 store: sqlx port, migrations 0001–0999 reserved, `web_v1` views preserved, staging cutover plan

## Consequences

- Prototype must run against **mock Discord + agent-testdb only**; never
  the production guild or tokens.
- RSS/CPU/10x-load figures above are estimates; S2 measurement confirms or
  reopens the `lite` placement (not the framework choice — both fit).
- Revisit triggers: twilight archived/inactive >6 months; Discord API
  change twilight doesn't cover; measured RSS forcing `basic` *and* a
  serenity port proving materially smaller (unlikely); voice-audio feature
  (reopens songbird).
