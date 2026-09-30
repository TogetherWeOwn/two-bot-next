# Parity matrix: legacy `two-bot` → `two-bot-next`

- **Source (frozen):** [`TogetherWeOwn/two-bot`](https://github.com/TogetherWeOwn/two-bot) `main` @ `d5d11793` (2026-09-29, maintenance-only: fixes, no new features).
- **Target:** `two-bot-next` (Rust/twilight, one always-on Cloudflare Container, shared Postgres, sqlx) — see `docs/adr/0001-discord-framework.md`.
- **B4 gate:** 0 unmapped rows **plus** a staging-guild soak run against this matrix ([TOG-9699](/TOG/issues/TOG-9699)).
- **Map key:** `S2`=[TOG-9807](/TOG/issues/TOG-9807), `S3`=[TOG-9808](/TOG/issues/TOG-9808), `S4`=[TOG-9809](/TOG/issues/TOG-9809), `S5`=[TOG-9810](/TOG/issues/TOG-9810), `S6`=[TOG-9811](/TOG/issues/TOG-9811), `B2`=[TOG-9695](/TOG/issues/TOG-9695), `B3`=[TOG-9696](/TOG/issues/TOG-9696), `B4`=[TOG-9699](/TOG/issues/TOG-9699). `S1` done. `NEW-n` = new slice card created from this matrix (§10). `DROP` = intentionally not ported, with reason.
- **Status:** every row below is mapped or dropped. Unmapped: **0**.

Conventions: `src/…` paths are legacy `two-bot` files. Permissions are Discord permission flags unless noted.

## 1. Slash / prefix commands (30 rows, including dynamic/prefix and one drop)

Registry: `CORE_COMMAND_DATA` (always published) = leveling only; community/rota/automation/announcement/moderation slices merge via `additionalBuiltins` (`src/index.ts:660-666`, `src/discord/commandNames.ts`). All guild-only, DM off.

| # | Command | Options | Permissions | Map |
|---|---|---|---|---|
| 1 | `/rank` | `member` User opt | everyone | **S4** (`/rank` is also the S2 prototype vehicle) |
| 2 | `/leaderboard` | none | everyone | **S4** |
| 3 | `/ban` | `target` User req, `reason` String req ≤512 | `BanMembers` + hierarchy/protected-role policy | **S4** (moderation LAST) |
| 4 | `/tempban` | `target` req, `duration_seconds` Int req ≥60, `reason` req | `BanMembers` | **S4** |
| 5 | `/kick` | `target` req, `reason` req | `KickMembers` | **S4** |
| 6 | `/timeout` | `target` req, `duration_seconds` ≥60, `reason` req | `ModerateMembers` | **S4** |
| 7 | `/warn` | `target` req, `reason` req | `ModerateMembers` | **S4** |
| 8 | `/purge` | `count` Int 1–100 req, `reason` req | `ManageMessages` | **S4** |
| 9 | `/slowmode` | `seconds` Int 0–21600 req, `reason` req | `ManageChannels` | **S4** |
| 10 | `/lockdown` | `reason` req (current channel) | `ManageChannels` | **S4** |
| 11 | `/unlock` | `reason` req (current channel) | `ManageChannels` | **S4** |
| 12 | `/attendance` (scorecard) | `event-occurrence` String req, `member` User req | `ManageEvents` | **S4**. Known defect to fix in port: name collides with #25 — both land in `additionalBuiltins` and collide on `guild.commands.set` |
| 13 | `/rota-acknowledge` | `message-link` String req | `ManageGuild` + must be configured primary actor | **DROP** — staging-only rota measurement experiment, never enabled in prod; re-enable on demand post-cutover |
| 14 | `/command` | `name` req, `template` req (`{user} {username} {server} {channel}`), `description` opt, `text-trigger` opt | `ManageGuild` (builder + runtime) | **S4** |
| 15 | `/command-remove` | `name` req | `ManageGuild` | **S4** |
| 16 | `/command-list` | none | `ManageGuild` | **S4** |
| 17 | `/schedule` | `body` req; `in-minutes` 1–525600 opt; `every-minutes` 60–525600 opt (one required) | `ManageGuild`, runs in invoking channel | **S4** |
| 18 | `/schedule-remove` | `id` req (prefix-resolved) | `ManageGuild` | **S4** |
| 19 | `/schedule-list` | none | `ManageGuild` | **S4** |
| 20 | `/sticky` | `body` req, `debounce` 1–300 default 5 opt | `ManageGuild`, current channel | **S4** |
| 21 | `/sticky-remove` | none (current channel) | `ManageGuild` | **S4** |
| 22 | `/<custom>` (dynamic, DB-backed via `/command`) | none (template render) | everyone while automations enabled; refused when disabled | **S4** |
| 23 | `!<trigger>` (prefix, e.g. `!faq`) | first token only | everyone; requires `TWO_TEXT_COMMANDS=1`; builtin names excluded | **S4** (only prefix surface; no hardcoded `!` commands) |
| 24 | `/rsvp` | `event-id` String req, `status` req (`going`/`interested`/`declined`) | everyone | **S4** |
| 25 | `/attendance` (RSVP totals) | `event-id` String req | everyone | **S4** (see #12 collision — port must namespace) |
| 26 | `/lfg` | `title` req, `starts-at` ISO-8601 req, `roles` req (`tank:Tank:2,…`) | `ManageEvents` (builder + runtime) | **S4** |
| 27 | `/lfg-close` | `id` req | `ManageEvents` | **S4** |
| 28 | `/feed-add` | `kind` req (`rss`/`youtube`/`twitch`), `source` req | `ManageGuild` | **S4** |
| 29 | `/feed-remove` | `id` req | `ManageGuild` | **S4** |
| 30 | `/feed-list` | none | `ManageGuild` | **S4** |

Runtime permission contract: `crates/core/src/command_permissions.rs` represents
all 30 rows (27 retained builtins, the dropped rota command, custom slash commands,
and prefix triggers). The router checks the invoking interaction's resolved
`member.permissions`, not bot permissions or command-picker defaults, before
returning a builtin handler. Missing restricted bits produce an ephemeral refusal
and a metadata-only `command_permission_denied` tracing audit event (command,
guild, required/resolved bits; no tokens, options or user text). This is a security
log, not a claim of durable operational-audit-store or gateway-dispatch wiring.
Member-target moderation retains its self-target, protected-role/owner/bot and
hierarchy checks in `assert_moderation_allowed`. Dynamic/prefix feature gates and
the dropped rota disposition are unchanged. Tests compare every row with this
section and all published permission bitfields, including Twilight wire JSON.

## Registry golden exceptions

`crates/core/tests/fixtures/legacy_registry.json` captures the frozen source above,
with every builtin enabled (including staging-only rota), no DB custom rows, and
both colliding `attendance` definitions intact. The core router publish set and
the actual Twilight guild bulk-set JSON are checked against that snapshot.

| Intentional difference | Matrix reference | Exact allowance |
|---|---|---|
| `rsvp-attendance` | docs/parity.md §1 #12 / #25 | Rename only the RSVP-totals `attendance` (its option is `event-id`); scorecard keeps `attendance`. No option, choice, description or permission waiver. |
| `rota-acknowledge` | docs/parity.md §1 #13 / §9 drop 1 | Remove the staging-only command; no replacement. |

These are the complete behavioural exceptions, mirrored by the test allowlist.
Only equivalent guild-API representation defaults are canonicalized: omitted
command type = ChatInput (`1`), omitted command options = `[]`, optional
`required` omitted = `false`, permission gate `null` = omitted, guild-only
`dm_permission: false` = omitted (the guild endpoint cannot publish global/DM
commands), and Twilight's server-assigned `version: "1"` placeholder = omitted.
Non-default values and unknown fields are **not** discarded. Array order,
option names/types/bounds, choices, descriptions and permission bitfields remain
strict. Real unlisted drift fails with field paths and legacy/next values; file a
follow-up instead of changing the fixture or expanding the exceptions to hide it.

Regenerate only from a scratch clone (Node 24, no Discord/DB access):

```sh
git clone https://github.com/TogetherWeOwn/two-bot.git "$PAPERCLIP_RUN_SCRATCH_DIR/legacy"
git -C "$PAPERCLIP_RUN_SCRATCH_DIR/legacy" checkout --detach d5d1179348feb9157bcac8c875de9399d4f5c76a
npm ci --prefix "$PAPERCLIP_RUN_SCRATCH_DIR/legacy" --ignore-scripts --no-audit --no-fund
node scripts/export-legacy-registry.mjs "$PAPERCLIP_RUN_SCRATCH_DIR/legacy" crates/core/tests/fixtures/legacy_registry.json
cargo test -p two-bot-core -p two-bot-discord --test registry_golden --locked
```

The export script calls legacy `mergedCommandData` in `src/index.ts:660–666`
feature order, then discord.js `ApplicationCommandManager.transformCommand`, the
same transform used by `guild.commands.set`. It refuses any other legacy SHA.

## 2. Non-command interactions (buttons / selects / reactions)

| Interaction | Behaviour | Map |
|---|---|---|
| Game picker `two:onboarding:games` | add/remove game roles to match menu, ephemeral reply | **S4** |
| Session picker (`SESSION_SELECT_ID`) | ephemeral ack + routed record, no roles | **S4** (session mode is LIVE since TOG-2795) |
| Self-role buttons/selects | claimed add/remove/replace + validation/hierarchy checks | **S4** |
| `MessageReactionAdd/Remove` | reaction-role grant/revoke (partial fetch, idempotent plan) | **S4** |
| Ticket buttons `open/claim/close` | ticket lifecycle + 5m recovery / 60m purge timers | **S4** |
| LFG select menus | signup flows | **S4** |
| `/attendance` host check-in → community attendance store | verified-attendance fact | **S4** |

## 3. Gateway event handlers

Intents (legacy `src/discord/client.ts`): Guilds, GuildMembers*, GuildModeration, GuildVoiceStates, GuildMessages, GuildMessageReactions, GuildInvites, MessageContent* (*privileged, gated). Voice = presence only, never joins/plays audio.

| Event | Behaviour | Map |
|---|---|---|
| `ClientReady` | log `ready`; per-guild invite snapshot for join attribution (skipped in containment) | **S3** |
| `ClientReady` | ticket recovery + panel ensure + transcript purge; command publish (`guild.commands.set`) | **S4** (tickets, registry) |
| `ready` | `audit.retryPending()` once + 30s sweep | **S5** |
| `GuildMemberAdd` | funnel: invite diff vs `expectedJoins`, `onJoin`, instant `onGateCleared` if `!pending`, raid-burst + join-risk scoring | **S3** (+ **S4** raid alerts, **S5** audit) |
| `GuildMemberAdd` | legacy / session / anchor welcome (mode switch `TWO_ONBOARDING_MODE`) | **S4** |
| `GuildMemberUpdate` | rules-gate `pending:true→false` → rota `gateCleared` + `onGateCleared`; role/nickname diff → `member_update` audit | **S3** (+ **S5** audit) |
| `GuildMemberUpdate` | welcome prompt on gate-clear (per mode) | **S4** |
| `GuildMemberRemove` | funnel `onLeave` | **S3** |
| `GuildMemberRemove` | session goodbye post (no ping) | **S4** |
| `MessageCreate` | automod inspect → rota reserve → funnel `onMessage` + level hook; rejected → `captureOnly` row | **S3** funnel, **S4** automod |
| `automationMessageAccepted` (internal) | sticky re-post + `!` text triggers | **S4** |
| `Raw` | message delete/edit audit without cached message | **S5** |
| `MessageUpdate` | fetch partial + automod re-inspect on edit | **S4** |
| `VoiceStateUpdate` | channel-change only; `voice_join/leave/move` audit; `onVoiceLeave`→`onVoiceJoin` + level hook | **S3** (+ **S5** audit) |
| `ShardResume`/`ShardReady` | drop all open voice sessions (no outage-inflated durations) | **S3** (same rule in twilight resume path) |
| `InviteCreate` | re-snapshot invites (attribution freshness) | **S3** |
| `GuildAuditLogEntryCreate` | MAC-verified moderation audit → operational audit | **S5** |
| `GuildAuditLogEntryCreate` | anti-nuke containment filter → `containment.observe` | **S4** |
| `InteractionCreate` | all slices (§1–§2 dispatch) | **S4** |
| `Dispatch/Ready/Closed/Hello/Resumed/…` staging-restart containment filter | allowlisted guild/actor payloads only | **DROP** — Container restarts use persisted session + RESUME (**S5**) and the staging guild (**S6**); no live restart filter needed |
| `Error` | `client_error` log | **S3** (tracing) |

Explicitly unsubscribed (no handler; deletes/edits/moderation surface via `Raw` + audit log): presence, bans, bulk delete, channels/threads, scheduled-event objects, guild/role update, invite delete, webhook update, typing, stage. No collectors in `src`. No rows.

## 4. Scheduled jobs & timers

| Job | Cadence | Map |
|---|---|---|
| presenceProbe (`GET /guilds/{id}?with_counts=true` → `presence_probe`; reader: `presence-trend` only) | 1h, bot-floor re-list 24h | **S5** |
| communitySnapshots counter tick → `guild_counters` + `counter_snapshots` (website `live_counts`) | 60s | **S5** |
| communitySnapshots rank tick → `rank_snapshots` + `member_ranks` (skips raid windows) | 10m | **S5** |
| scheduled-events poller → atomic `scheduled_events` mirror (website feed) | 10m | **S5** |
| communityScorecard Monday 06:15 UTC week run | 60s tick, weekly fire | **S5** |
| inactivity `flagInactive()` (read-only, never DMs) | hourly from `index.ts` | **S5** |
| reengagement list builder | on-demand CLI only, never scheduled | **DROP** as runtime — on-demand query against Postgres/`web_v1` remains available; re-create on demand |
| automation scheduled-message ticker (15s, `next_run_at` queue) | 15s | **S4** |
| rota fallback-notice ticker | 60s | **DROP** with rota (§1 #13) |
| feed poller (RSS/YouTube/Twitch → `feed_deliveries` + relay) | `TWO_FEED_POLL_SECONDS` default 300s | **S4** |
| operational-audit retry sweep (≤25 claims) | 30s | **S5** |
| moderation unban sweep (`moderation_scheduled_unbans`) | 30s | **S4** |
| ticket recovery / transcript purge | 5m / 60m | **S4** |
| settings version poll (`guild_settings` hot reload) | 15s | **S6** |
| self-role claim renewal / automod repeat-tracker expiry / per-request fetch timeouts | lease/timeout driven | **S4** with feature |
| systemd `two-bot-backup.timer` (nightly DB→S3) | daily 04:17 | **[TOG-9881](/TOG/issues/TOG-9881)** |
| systemd `two-bot-guild-config-backup.timer` (sealed Discord config snapshot) | daily 04:31 UTC | **[TOG-9881](/TOG/issues/TOG-9881)** |
| systemd `two-bot-restore-drill.timer` | monthly | **[TOG-9881](/TOG/issues/TOG-9881)** |
| systemd `two-bot-rules-gate-timeout.timer` (gate-stuck report) | daily 04:43 | **DROP** as runtime — on-demand report post-cutover |

## 5. DB tables & queries

Migrations: legacy `migrations/0001–0034` (49 files); numbering reserved bot `0001–0999`, web `1000–1999`. `sql/web_v1.sql` = views only. Store layer ports to sqlx under **S6** throughout; feature queries port with their feature card.

| Tables | Purpose | Map |
|---|---|---|
| `events`, `members`, `invite_snapshots` | append-only funnel log + member projection + invite snapshots | **S3** queries, **S6** migrations |
| `internal_nonces`, `internal_idempotency`, `internal_action_log`, `internal_discord_events` | website-callback replay guard, idempotency, audit, dedupe | **[TOG-9880](/TOG/issues/TOG-9880)** |
| `web_contract_meta`, `guild_counters`, `rank_ladder`, `rank_snapshots`, `member_ranks`, `scheduled_events` | website read contract | **S5/S6**, `web_v1` views preserved (**S6**) |
| `presence_probe` | hourly presence series | **S5** |
| `counter_snapshots`, `member_exclusions` | live-count audit history, raid exclusions | **S5** |
| `invite_campaigns` (`go.two.gg/<slug>`) | tracked short links | **B3** (redirect port owns its slug store) |
| `member_levels`, `xp_cooldowns` (60s msg/voice), `xp_awards`, `level_role_rewards`, `level_import_runs` | leveling, MEE6-compat (5 XP/min voice) | **S4**, import via **[TOG-9882](/TOG/issues/TOG-9882)** |
| `moderation_warnings`, `moderation_scheduled_unbans`, `moderation_audit`, `moderation_lockdowns`, `moderation_idempotency` | moderation ledger, tempban queue, lockdown state, claim table | **S4** |
| `operational_audit_log` (states `none/pending/delivering/delivered/quarantined`, 5-min lease, hourly mirror recheck) | metadata-only Discord-event parity audit | **S5** |
| `automod_violations`, `automod_processed_messages` | sanctions ladder, gateway-retry dedupe | **S4** |
| `tickets`, `ticket_transcripts` | ticket lifecycle + transcripts | **S4** |
| `containment_events`, `containment_incidents`, `join_risk_flags` | anti-nuke signals/incidents, join-risk (flag-only, never kicks) | **S4** |
| `automation_commands`, `scheduled_messages`, `sticky_messages`, `automation_audit_log` | custom commands, schedule queue, stickies | **S4** |
| `self_role_audit`, `self_role_panel_claims` | self-role audit + claim leases | **S4** |
| `community_facts`, `community_stream_heartbeats`, `community_scorecard_runs/alerts` | scorecard facts/runs/alerts | **S5** (rota extensions 0028–0033 **DROP** with rota) |
| `event_rsvps`, `lfg_posts/roles/signups`, `feed_relays`, `feed_deliveries`, `announcements_audit_log` | RSVP, LFG, feed relay + delivery claims | **S4** |
| `guild_settings`, `guild_settings_audit` | dashboard-writable hot settings | **S6** |
| `audit_kill_switch` (presence = halt; audit fail-open, rota fail-closed) | audit pipeline kill switch | **S5** |

## 6. External integrations

| Integration | Detail | Map |
|---|---|---|
| Discord REST (`/api/v10`, `DISCORD_API_BASE` override) | guild/members/scheduled-events reads; 110ms pacing, 429 `retry-after+250ms`, 5xx exp backoff ≤4; kicks 350ms pacing, 4 retries; moderation REST 5s abort, no auto-retry | **S2/S3** adapter (+ **S4** moderation paths) |
| Discord gateway (discord.js → twilight) | intents §3; session persist + RESUME across Container restarts (restart-storage precedent) | **S2/S3** connect, **S5** session persist |
| Discord CDN emoji fetch (guild-config snapshot) | `GUILD_CONFIG_CDN_BASE` override | **[TOG-9881](/TOG/issues/TOG-9881)** |
| Feeds RSS/YouTube/Twitch (UA `Owen/1.0`, SSRF public-IP guard, `MAX_FEED_BYTES`) | poll → `feed_deliveries` → relay | **S4** |
| Website → bot `POST /internal/actions` (HMAC-SHA256 `sha256=` over `POST\npath\nts\nnonce\nsha256(body)`; `TWO_INTERNAL_KEYS`; skew+nonce replay guard; buckets 20 burst/1/s, `guild.add_member` 10/0.5s; 18 actions: `role.assign`, `guild.add_member`, `announcement.post`, `event.upsert/cancel`, `automations.import/export`, `settings.get/set`, `moderation.*` ×9) | web callbacks incl. `identify guilds.join` auto-join path | **[TOG-9880](/TOG/issues/TOG-9880)** |
| `go.two.gg` redirect (`GET /<slug>` → `invite_click` → 302; unknown → fallback code) | ~450-line server + campaigns | **B3** Worker port |
| S3-compatible backup (SigV4 single-PUT, `TWO_BACKUP_S3_*`) | nightly dump upload | **[TOG-9881](/TOG/issues/TOG-9881)** |
| Postgres (`TWO_DATABASE_URL`, staging guard; pool max 5, `statement_timeout`) | direct from Container (no Hyperdrive) | **S6** (shared Neon via TOG-9679) |
| Health endpoint (`TWO_HEALTH_PORT`, `503 gateway_disconnected` pre-ready) | Container probe | **S1** done (`/readyz`) |
| Operational-audit mirror channel (retryable delivery, `allowedMentions:{parse:[]}`, nonce-enforced) | tamper-evident Discord mirror | **S5** |
| Moderation-audit MAC (`[two-audit:v1:token:action:actor:mac]`, HMAC-SHA256 `TWO_MODERATION_AUDIT_SECRET`, `timingSafeEqual`) | bot-action correlation | **S5** |
| Inbound webhook classification only (bot-posts-via-webhook detection for classifier) | no outbound webhook calls exist | **S5** (classifier) |

Agent-events signer: no `agent-events` signer exists in legacy `two-bot` or `two-web` (searched both trees) — the only signers are the internal-actions HMAC ([TOG-9880](/TOG/issues/TOG-9880)), the moderation-audit MAC (**S5**), and the S3 SigV4 PUT ([TOG-9881](/TOG/issues/TOG-9881)). If a separate agent-events signer is expected, file a follow-up row.

## 7. Config / env

Full catalogue: legacy `src/core/settingsCatalog.ts` (~90 keys in `env_only`/`cold`/`hot` classes) + per-feature `config.ts` loaders + `src/core/config.ts` boot `Config`. Classes: `env_only` (~45: tokens, URLs, guild IDs, `TWO_INTERNAL_*`, S3, anti-nuke IDs, restart guards), `cold` (~14, restart-to-apply feature flags: `TWO_AUTOMOD/ANNOUNCEMENTS/AUTOMATIONS/TEXT_COMMANDS/ANTI_NUKE(+DRY_RUN)/COMMUNITY_SCORECARD/PRESENCE_PROBE/FEED_POLL_SECONDS/INACTIVITY_DAYS/TICKET_COOLDOWN/SELF_ROLE_PANELS`), `hot` (~30 dashboard-writable: channel IDs, raid thresholds, automod lists, community actor lists, dry-runs). Secrets via `readSecret` + systemd creds → Worker env in next (**S1** done). Map: whole surface → **S6** (config module + `guild_settings` port); feature flags travel with their feature card.

## 8. Observable behaviours

| Behaviour | Detail | Map |
|---|---|---|
| Structured JSON logs (`{ts,level,msg,…}`, `debug/info/error`, `LOG_LEVEL`) | ~208 line names: lifecycle, funnel, onboarding, audit mirror, alerts, automod/moderation, rota, REST/backoff | **S2+** (tracing equivalents; exact names not frozen) |
| Audit channels (audit/voice/moderation, metadata-only `key=value`, 300/2000 truncation, audit-event identity prefix) |arches fallback voice/moderation → audit | **S5** |
| Audit kill switch (`audit-switch --halt/--resume/--status`) | presence = halt | **S5** |
| Rate limits: staging-verifier 3 retries ≤30s; REST pacing §6; internal buckets §6; raid-watch 5 joins/60s + 900s cooldown; ticket cooldown 300s; containment per-executor cooldown | alert cooldowns + sweep/lease bounds (audit 5-min lease, 1-h recheck, claim ≤25) | with feature (**S3/S4/S5**), internal buckets [TOG-9880](/TOG/issues/TOG-9880) |
| Automod (staging-only unless live-approved; `dryRun` unless `ENFORCE=1`; 6 filters; sanctions `1:delete,2:warn,3:timeout:600`; target-protection before delete) | bad-word NFKC matching, invite/link checks, `bat/cmd/…` attachment blocklist | **S4** (staging gate until soak) |
| Anti-nuke (default alerts-only; weights kick/ban/webhook=1, channel/role delete=3; quarantine strips dangerous perms below bot hierarchy; join-risk flag-only) | `containment_alert` always logged; `**Join burst**` raid alerts, no DMs/pings | **S4** (staging gate until soak) |
| Onboarding modes (`legacy` catalog + hub routing / `session` roleless two-pick LIVE / anchor Sunday-Squad one-message) | no-DM, idempotent `onboarding_prompted`, dry-run aware | **S4** |
| Operator scripts (82 files) | schedule/backfill/migration one-shots → **[TOG-9882](/TOG/issues/TOG-9882)** (MEE6 XP, rewards, history backfill, dedupe, message-milestone scan, join capture); backup/restore/snapshot → [TOG-9881](/TOG/issues/TOG-9881); read-only reports (funnel, gate, attribution, roster, dashboard, scorecard, presence-trend, growth-review, raid-list) → **DROP** as runtime (query Postgres/`web_v1` on demand); staging provision/verify/reset + e2e harness → **DROP** (replaced by mock-discord acceptance + **S6** cutover plan); `reconcile` → **DROP** (absent/broken upstream); guild-config snapshot/restore → [TOG-9881](/TOG/issues/TOG-9881); temp-voice → staging shape-check only, no runtime on legacy `main` (TOG-3471 unmerged) → port the check under **S6**, no runtime row |

## 9. Drops (not ported, with reason)

1. Rota measurement stack (observer, notice delivery, `/rota-acknowledge`, 0028–0033 migrations, 60s ticker) — staging-only experiment, never prod-enabled.
2. Staging restart containment gateway filter — superseded by session persist (**S5**) + staging guild (**S6**).
3. Read-only operator reports (funnel/gate/attribution/roster/dashboard/scorecard/presence/growth/raid-list) — ad-hoc queries, not runtime.
4. Staging provision/verify/reset scripts + e2e harness — replaced by mock-discord acceptance + **S6** cutover.
5. `reconcile` script — absent/broken upstream.
6. Temp-voice runtime — never shipped on legacy `main`; only the staging shape-check ports (**S6**).
7. Reengagement DM-less list as scheduled runtime — on-demand query only.
8. Rules-gate-timeout timer as runtime — on-demand report.

## 10. New slice cards (created from this matrix)

| Card | Slice | Assignee | Rationale |
|---|---|---|---|
| **[TOG-9880](/TOG/issues/TOG-9880)** | Internal-actions HTTP API port (18 actions, HMAC verify, nonce/idempotency stores, rate buckets, `guild.add_member` auto-join path) | Automation Engineer | No S-card owns the website→bot callback surface; web↔bot integration is their domain |
| **[TOG-9881](/TOG/issues/TOG-9881)** | Backup/restore + sealed guild-config snapshot port (dump/restore, S3 upload, snapshot/restore scripts, timer equivalents, restore drill) | DevOps & Reliability Engineer | Owns S1 DB work + backup card TOG-9074; cutover-data domain |
| **[TOG-9882](/TOG/issues/TOG-9882)** | Cutover data imports (MEE6 XP import, level rewards, history backfill, message-milestone scan, join capture, dedupe) | DevOps & Reliability Engineer | Same cutover-data domain as [TOG-9881](/TOG/issues/TOG-9881); one owner for the cutover data path |

Founding Engineer keeps S3–S6 (in flight); the three slices above run in parallel with S4/S6 and merge into the S6 cutover. If S5 stalls, split the audit-sink half to the Automation Engineer — call it in the weekly review, not mid-flight.

## 11. B4 gate checklist

- [ ] 0 unmapped rows (this matrix: **0** — every row mapped or dropped above).
- [ ] [TOG-9880](/TOG/issues/TOG-9880) / [TOG-9881](/TOG/issues/TOG-9881) / [TOG-9882](/TOG/issues/TOG-9882) merged.
- [ ] Staging-guild soak executed against §§1–8 (every mapped row exercised or explicitly waived with reason on [TOG-9699](/TOG/issues/TOG-9699)).
- [ ] 48h watch log + rollback path on [TOG-9699](/TOG/issues/TOG-9699).
