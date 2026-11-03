# Bot↔site contract v2 — Ember Hours (two-bot-next ↔ two-web-next)

**Verdict: buildable as designed on the transports that already exist. Two deltas to the design are named in §13; everything else is contract, not new infrastructure.** Settled by the Engineering Lead in the internal decision record. Supersedes the earlier live-voice contract; clauses carried over unchanged are marked *(v1 §n)* and stay binding. Source design: the Ember Hours web design (owner decision: build Ember Hours in full; the Living Lobby plan is retired). This document decides; it does not enable (§15).

## 0. Ground facts (verified in code, 2026-11-03)

- **Two transports already exist and v2 adds no third.**
  1. **Signed actions:** `POST /internal/actions`, HMAC-SHA256 v1 signing (`src/bot/signer.ts` site-side; `crates/core/src/internal_actions.rs` bot-side), durable nonce burn, per-key buckets (20 burst / 1 s⁻¹), closed refusal vocabulary, relayed answers stripped of free text (`docs/internal-actions-receiver.md`). Catalogue on `origin/main`: 20 verbs including `voice.snapshot` (merged). Every verb except `role.assign`, `announcement.post`, `event.upsert` sits behind a default-off `TWO_INTERNAL_ALLOW_*` flag, forwarded deliberately through `wrangler/src/container-env.ts`.
  2. **`web_v1` read views:** bot-owned SQL views in the shared Postgres (`crates/store/sql/web_v1.sql` — "everything the website is allowed to read"), site is a read-only consumer (`docs/web-v1-contract.md` in two-web-next; consumed at `src/counts.ts:111,120`). Views today: `contract_meta`, `live_counts`, `rank_counts`, `rank_ladder`, `members`, `member_milestones`, `upcoming_events`, `next_event`, `funnel_*`. View edits may only append columns; anything else is a `web_v2` schema.
- `needs_reconciliation` appears **nowhere in site code** today; retry decisions ride the wire `retryable` flag with a fallback table (`src/bot/client-retry.ts:7-13`). §11 assigns the site handling.
- The bot stores **no avatar hashes or URLs** anywhere in voice code; display names are transient. The site session already carries `userId`, `username`, `avatar` from Discord OAuth (`src/sessions.ts:52-58`, scopes `identify` + `guilds.join`).
- Bot voice capture exists: gateway `GUILD_VOICE_STATES` (no privileged intent; `GUILD_PRESENCES` stays off), in-memory per-member `channel_id` + `joined_at_ms`, event-window spans in `event_voice_presence` (attendance branch, in flight at the time of decision). There is no REST voice-state list; the gateway cache in the bot process is the only source *(v1 §0)*.
- Bot privacy machinery exists but is **not yet wired into voice**: `privacy_opt_outs` + `guild_analytics_settings.voice_enabled` (migration 0433), `suppressed_in` in `crates/core/src/privacy.rs`. The event-voice writers do not call it. The in-flight notify branch has **no consent check and no per-member bucket**. Both are defects against this contract; §7/§14 route the fixes.
- The site has **no notification-preference storage** today. §7 assigns it.
- `internal_action_log` (bot DB) already records every signed action: `audit_id, intent_id, phase, caller_hash, action, guild_id, actor_id, target_id, response_code, http_status, evidence_code, created_at` (+ `resource_id`, `affected`, `outcome`, `stop_reason`). It is never pruned. The public bot log in §6 is a whitelisted projection of this table plus site-origin lines — never the table itself.
- Both in-flight bot branches add a migration numbered `0471`. Whichever merges second renumbers (§12).

**Transport rule (new, binding):** *member-level facts travel only on signed actions; `web_v1` carries guild aggregates and directory facts the member has already made visible in Discord.* Live who-is-where, attendance and the bot log are member-level → signed actions. The heat grid is a guild aggregate → it may be a `web_v1` view (§5). No new direct table reads; the site never queries bot tables outside `web_v1`, and the bot never gets site DB credentials *(v1 §4)*.

## 1. Transport, signing, versioning (carried + pinned)

- **No push, no websocket, no bot→site callback in v2.** All flows are site-initiated polls or calls: synchronous reads (2.5 s budget, `event.read` precedent), synchronous self-move (5 s), queued mutations and cron pulls through `src/jobs/*`. The Discord-button accept/decline cut stands *(v1 §8.1)*.
- **Signing stays v1 HMAC for now.** The designed-but-unimplemented signing v2 (`docs/internal-action-signing-v2.md`, audience/caller/expiry binding) is a separate, already-specified migration. Every verb in this contract is defined scheme-agnostically over the same route and envelope, so the signing flip changes headers only. **Production enablement of any member-affecting verb (§7) requires either signing v2 active or the existing F-gate acceptance of the v1 residual risk on the rollout card** — that call belongs to the rollout, not to this document. (Naming note: "contract v2" here and "signing v2" there are different version lines; neither implies the other.)
- **Versioning *(v1 §9)*:** payloads carry `v`; new fields are additive; parsers ignore unknowns and fail closed on malformed knowns; a breaking change is a new verb or a `v` bump deployed site-first (site tolerates old, then bot emits new). `web_v1` additions follow that file's append-only rule and bump `contract_version`.
- **Envelope:** unchanged relayed-answer contract — `{ok, request_id, result}` or `{ok:false, error:{code,message:"request refused",retryable}}`, `Cache-Control: no-store`, 256 KiB relay ceiling. Domain answers that the UI must distinguish are HTTP 200 `result.outcome` values, never refusals *(v1 §2)*.

## 2. Verb catalogue for Ember Hours

Existing verbs (`role.assign`, `guild.add_member`, `announcement.post`, `event.upsert/cancel/read`, `settings.*`, `automations.*`, `moderation.*`) are unchanged. The Ember Hours surface is:

| Verb | Dir | Gate flag | Idem-Key | Mode | Status |
|---|---|---|---|---|---|
| `voice.snapshot` | bot→site read | `TWO_INTERNAL_ALLOW_VOICE_SNAPSHOT` | no | sync read, site-cached | merged; §3 adds fields |
| `voice.move_member` | site→bot | `TWO_INTERNAL_ALLOW_VOICE_MOVE` | no (natural no-op) | sync | in flight at the time of decision |
| `reminder.send` | site→bot | `TWO_INTERNAL_ALLOW_MEMBER_NOTIFY` | **required** | site job queue | in flight at the time of decision; §7 adds consent + bucket |
| `place_offer.send` | site→bot | same | **required** | site job queue | in flight at the time of decision |
| `event.attendance.read` | bot→site read | `TWO_INTERNAL_ALLOW_ATTENDANCE_READ` | no | site cron pull | in flight at the time of decision |
| `botlog.read` | bot→site read | `TWO_INTERNAL_ALLOW_BOTLOG_READ` | no | site cron pull, cursored | **new in v2** (§6) |
| `web_v1.voice_heat` | bot→site view | n/a (view) | n/a | site cron pull, daily | **new in v2** (§5) |

One bot process serves one configured guild; no verb carries a guild selector *(v1 §0)*.

## 3. Live state — `voice.snapshot` (v1 §1 carried, fields added)

Carried unchanged: request `{"action":"voice.snapshot"}`; result `{v, observed_at, total_in_voice, rooms[{channel_id, name, position, user_limit, member_ids, afk?}]}`; empty rooms included; bot accounts and AFK occupants excluded from counts; ids only — no names, no presence, no activity; fail-closed caps site-side (≤100 rooms, ≤500 ids); never a partial snapshot (`discord_unavailable` when the gateway cache is not READY/complete).

**Additive fields in v2** (same `v:1`; site must tolerate absence — older bot builds omit them):

- `rooms[].occupied_since` — ISO-8601 UTC of when the room's current continuous occupancy began (bot derives from its `joined_at_ms` cache), or `null` when unknown (e.g. occupancy predates bot start). Powers the design's "how long" on room rows. Never fabricated: unknown is `null` and the site omits the duration.
- `rooms[].event_key` — present only when the room is the mapped voice channel of a currently-windowed event (the bot already holds `event.upsert` mappings + `event_voice_channels`). Lets event surfaces and the Lifeline name "the Raid 1 room" without guessing. Absent ⇒ unlinked room.

**Freshness & staleness *(v1 §1 bands, retained)*:** per-isolate in-memory cache only, ≤1 bot fetch / 10 s / isolate, 2.5 s budget, pages never block; ≤30 s "Updated Xs ago", 31 s–2 min honest age with Pull me in still offered (the move re-validates live bot-side), >2 min or failing fetches ⇒ freeze with "Last synced X min ago", **Pull me in withdrawn** for the Discord deep link; no snapshot ever ⇒ pre-contract surfaces, never a rendered "0 in voice" from absent data. Snapshot data is **never written to the site database** *(v1 hard rule)*. The design's "stale > 5 min: Last seen 18:40" line is copy for the same frozen state; the *bands* above govern behaviour (delta §13.3).

**Live Sentence & Now Line inputs** — each rendered word traces to one input with its own fallback; no input borrows another's label *(v1 §1)*:

| Input | Source | When missing/stale |
|---|---|---|
| In-voice total, room names/counts, "how long" | `voice.snapshot` (fresh bands above) | Sentence drops live claims; leads with next event |
| Online-ish count where still shown | Discord widget `presenceCount`, labelled as itself | Omit the fragment (as `src/counts.ts` already does) |
| Next / live event, RSVP counts, "yours" | Site DB (`events`, RSVPs) | Always available; this is the fallback spine |
| Guild totals, rank counts | `web_v1.live_counts` / `rank_counts` (10-min freshness, null ≠ 0) | Numerals suppressed, copy intact (existing rule) |
| Now Line shading | `web_v1.voice_heat` (§5) with its `source` label | Seeded estimate, labelled "estimate" |
| Now marker, event placement | Site clock + site DB | — |

## 4. Member avatars in live rooms — consent and privacy

- **The bot never sends avatar URLs, hashes, usernames or nicknames** — it has none stored (§0) and v2 adds none. The signed payload stays snowflake-only.
- **The site resolves identity from its own data:** display name + avatar come from the site profile created through Discord OAuth (`identify` scope — the member already consented to the site holding these). Avatar rendering uses the existing CDN URL construction and `src/image-policy.ts` allowlist. An occupant with **no site profile is counted, never named or pictured** ("and K more") *(v1 §6)*.
- **Audience projections are server-side at serialization** *(v1 §6)*: guests receive anonymous discs and aggregate counts only; signed-in members receive names/avatars for rooms they may see; restricted rooms additionally require the `src/roles.ts` moderator bit (blank allowlist ⇒ nobody). Client-side hiding of a full payload is forbidden.
- **Consent toggles (site-owned profile settings; defaults binding):**
  - `show_in_live_rooms` — default **on** (parity: voice occupancy is already visible to every guild member inside Discord). Off ⇒ the member is counted but rendered unnamed/anonymous on every site surface, including members directory "In voice now".
  - `show_in_bot_log` — default **on**; off ⇒ never named in the public bot log (§6).
  - `pull_in_with_me` — governs only whether a member's profile advertises "pull in with me" to others *(v1 §2)*; display consent, site-side.
  - `usual_hours` — opt-**in** (default off); derived from **Echo attendance only**, never from snapshots or raw sessions *(v1 §6)*.
  - Leaderboard opt-out (exists) is honoured by every public list, per design row 34.
- **Privacy floor *(v1 §6, retained)*:** logged-out surfaces show a live in-voice count only when the guild total (AFK- and bot-excluded) is **≥ 5**; below that, no live count at all — the surface leads with the next event. The Ember copy grammar's "< 3 = quiet" threshold applies to **signed-in** surfaces only (§13.3). One site-side constant, next to the projection code.
- `privacy_opt_outs` on the bot is the **backstop**, not the product toggle: a member suppressed there must be excluded at capture (voice-session writers, §9) regardless of site settings. Bot ops set it on moderator request; the site toggles above are the member-facing control. The two never merge into one store in v2.

## 5. Voice heat-map — the Now Line's measured truth

**What the bot measures (aggregates only — this is the one narrow amendment to the v1 "nothing recorded outside event windows" wall):** every 5 minutes the bot samples `total_in_voice` (guild-wide, AFK- and bot-excluded, the §3 definition) from the gateway cache into an in-memory accumulator; hourly it flushes `{hour_start_utc, samples, occupancy_sum}` to a new bot table `voice_heat_hours`. **No member id, channel id or per-room split is ever recorded.** A stopped gateway records nothing (a gap is a gap, never a zero). Retention: rolling **35 days**, pruned by the bot retention job.

**Publication:** new view `web_v1.voice_heat` (aggregate ⇒ view-legal under the §0 transport rule), one row per hour-of-week cell over the trailing 28 complete days:

| Column | Meaning |
|---|---|
| `dow` (0–6, Mon=0), `hour` (0–23, UTC) | the 7×24 cell the site's hot zones already render |
| `avg_in_voice` | mean occupancy for that cell |
| `sample_days` | distinct days contributing samples to the window |
| `coverage` | samples present / samples expected in the window |
| `measured_through` | ISO-8601 UTC text, last flushed hour |
| `source` | `measured` when `sample_days ≥ 14` **and** `coverage ≥ 0.8`, else `insufficient` |

**The honest label, as a state machine the site must render verbatim:**
- `source = insufficient` (or view absent) ⇒ the site keeps today's seeded grid (`src/hotzones/aggregates.ts`) and labels it **"estimate"**. Bot calibration has not happened; no copy may imply otherwise.
- `source = measured` ⇒ the site stores the grid (site DB, overwritten daily by cron pull) and labels it **"measured · last 4 weeks"** with `measured_through` available on the freshness line.
- Measured and estimated cells are **never mixed in one grid**, and a measured grid that goes stale (>48 h without a successful pull) drops back to "estimate" labelling with the last measured values greyed, never silent freshness.
- Joins data in hot zones is already real and stays separately labelled *(design §1)*; games stay seeded estimates until something measures them — nothing in v2 does, and the label must keep saying so.

## 6. Public bot log — `botlog.read`

**Shape:** the site owns publication; the bot owns facts. Site cron pulls new entries every minute: request `{"action":"botlog.read","after":"<cursor>","limit":200}` (keyless read, `event.read` class). Result:

```json
{"v":1,"entries":[{"seq":"<opaque cursor>","at":"2026-11-03T19:04:11Z","kind":"event.opened",
"subject_id":"234567890123456789","event_key":"test-raid-night-1","detail":"prospect"}],
"next":"<opaque cursor>"}
```

`seq` is opaque to the site (bot encodes `(created_at, audit_id)`); ordering is strictly ascending bot-side; the site stores `next` and resumes — a missed minute loses nothing, a replayed pull dedupes on `seq`. Entries are **terminal successes only** (the audit `phase`/`response_code` say so); failures, refusals and in-flight intents never publish.

**Publishable kinds — a closed allowlist in bot code** (the `member_milestones` whitelist precedent, enforced where the audit is written). Anything not listed does not exist as far as the public log is concerned:

| `kind` | Source fact | Subject named? |
|---|---|---|
| `event.opened` / `event.updated` / `event.cancelled` | `event.upsert` (created vs updated from its outcome) / `event.cancel` | n/a (event named) |
| `announcement.posted` | `announcement.post` | channel-key name only, never body text |
| `rank.assigned` | `role.assign` whose resolved role is a rank rung | per §4 projection + `show_in_bot_log` |
| `member.joined` | `guild.add_member` success | guests: "a new member", always; members: per projection |
| `echo.frozen` | **site-origin line** when the site writes an Echo (§9) | n/a (event named) |

**Never published, structurally:** every `moderation.*` verb and all discipline (design rows 36/38), `settings.*`, `automations.*`, `reminder.send`, `place_offer.send`, `voice.move_member` (a member's whereabouts and DM metadata are not a public ledger — they stay in the internal audits both sides; delta §13.2), reasons, message bodies, caller hashes, request ids, IP/session data, free text of any kind. `detail` is a closed per-kind scalar (rank key, channel key), validated bot-side against per-kind grammars.

**Redaction is two-layer:** the bot emits scalars only (ids, keys, timestamps) and additionally suppresses any subject present in `privacy_opt_outs` (entry still counts, `subject_id` omitted); the site resolves names through the §4 projection (guest ⇒ anonymised line; no profile or opt-out ⇒ "a member"). The bot log payload itself is never logged site-side beyond counts/ages *(v1 §6 discipline)*.

**Retention:** site keeps the public feed **90 days** rolling (older entries deleted by cron; the internal bot audit retention is unchanged and is not the site's business). Member erasure (§10) nulls subjects immediately, ahead of the 90-day sweep.

## 7. Site actions → bot (v1 §2/§3 carried, gaps closed)

`voice.move_member`, `reminder.send`, `place_offer.send` keep their v1 wire shapes, outcome enums, validation orders, sync/queued split and audit duties exactly as decided in the move-member and member-notify branch scopes; if this section and v1 ever disagree, stop and say so). V2 closes four gaps found in the in-flight code:

1. **Consent for DMs (new requirement, both ends).** The site owns member notification preferences (new site storage; there is none today): `notify_reminders` and `notify_place_offers`, default **on** for members who RSVP (the RSVP is the ask), one-click off from profile and from the DM's linked page. The site must not enqueue a DM to a member who has opted out. **Backstop bot-side:** before sending, the bot checks `suppressed_in` (`privacy_opt_outs`); suppressed ⇒ outcome `dm_unavailable` (same 200 class; the audit scalar records the closed reason `suppressed`, never why). A preference is not a permission the bot can see being abused silently — both checks exist because either side can be wrong.
2. **Per-member notify bucket (new):** bot-side, keyed `notify:<discord_id>`, **burst 3, refill to at most 10/day**, on top of the per-caller bucket; over-limit ⇒ `rate_limited` (site queue backs off with the same idempotency key). The move bucket (6 / 5 min, in flight as burst 6 @ 0.02 s⁻¹) stands *(v1 §2)*.
3. **Subject derivation (restated as the auth rule for every member-affecting verb):** the site derives `discord_id` from the authenticated session (`web_sessions.userId`) — the browser never supplies it, for any verb, ever. The bot authenticates the *site* (signing), then independently re-derives every Discord fact it acts on (member's Connect, bot's Move/View/Connect, room limit) from live Discord state, fail-closed *(v1 §2/§5)*. Authorization is therefore: site session ⇒ subject; signed caller ⇒ channel; Discord state ⇒ truth. No verb accepts an actor field; actor ≠ subject variants remain out of scope and are not flags *(v1 §2)*.
4. **Idempotency & unknown outcomes:** keyed verbs (both notify verbs, `announcement.post`, event mutations) keep the durable claim/replay machinery; the site persists one UUID per logical intent and retries **only** with that key. `needs_reconciliation` must be implemented site-side for keyed mutations before any of them enable in production: stop, surface to moderators in admin, never re-key *(receiver doc's next-action map is normative)*. Keyless naturally-idempotent verbs (`voice.move_member`, `role.assign`, all reads) retry freely within their buckets.

Rate-limit summary (bot-side, all after signature verification): per-caller 20/1 s⁻¹; `guild.add_member` 10/0.5 s⁻¹; per-member move 6/5 min; per-member notify burst 3, 10/day; Worker ingress per-IP bucket unchanged. Site-side: route throttle at write tier + 2 s per-session debounce on Pull me in *(v1 §2)*; queue backoff honours `Retry-After`.

**Deferred, not promised:** site RSVP → Discord scheduled-event "interested" mirroring (design card 11). Whether a bot token may add a user to a scheduled event's interested list is unproven; v2 specifies **no verb** for it. The bot roadmap proves or kills the capability on staging; only a proven capability gets a contract amendment (§13.1).

## 8. Lifeline — what the bot has done and will do, per event

**Definition:** the Lifeline is a *site-composed* timeline. Past steps come from facts; future steps are computed by the site from the schedule and this contract's constants. **The bot never emits promises** — no payload contains a future action (the bot has no timers *(v1 §3)*; the site's scheduler is the only clock).

| Step | Done when (fact source) | Planned when |
|---|---|---|
| Sign-ups opened | `botlog` `event.opened` for the `event_key` | — (first step; absent ⇒ event not yet published to Discord) |
| You said you're in / seat saved | Site RSVP row (viewer-specific; the only per-viewer step) | while RSVP open |
| Reminder ping | Site records its own `reminder.send` job's terminal outcomes (sent count, `dm_unavailable` count) — never per-member lines | `starts_at − 30 min` |
| Room opens · Pull me in on | Fresh `voice.snapshot` shows the event-linked room (`rooms[].event_key`) at/after `starts_at` | `starts_at`; hollow "next" until then |
| Bot counts who's in | Event window open (`starts_at − 15 min … ends_at + 15 min` *(v1 §4)*) — shown as the current step, ticked only in retrospect | from window open |
| Echo frozen | Site Echo row exists (`echo.frozen` bot-log line) | after window close + grace |

**Honesty rules:** a step is ticked only by its fact. If a planned step's time passes with no fact within a 10-minute grace, it renders as **"waiting on the bot"** (or "waiting on the site" for site facts) — a named stale state, never a silent tick and never backdated. If the snapshot is in a frozen band (§3), the room step shows the frozen freshness text and Pull me in stays withdrawn. Cancelled events end the Lifeline at the `event.cancelled` fact with the reason the site already holds *(design seat-card states)*.

## 9. Echoes — attendance record (v1 §4 carried, two additions)

Carried unchanged: bot owns the raw session log **only inside event windows** `[starts_at − 15 min, ends_at + 15 min]`; `event.attendance.read` returns `{v, event_key, window, threshold_minutes: 10, peak_concurrent, rooms[], members[{discord_id, minutes, sessions, first_joined_at}], final}`; site writes the Echo only from `final:true`; ≥10 cumulative minutes = attended (threshold travels in the payload); positive lists only — **no public no-show data, structurally**; no backfill ever (pre-enable events stay "sign-up record"); counts frozen at write (correction = superseding record with audit, never a silent edit); Echo posts to Discord via existing `announcement.post`; bot raw sessions delete after **90 days**.

Additions in v2:

1. **Opt-out at capture:** the event-voice writers must consult `suppressed_in` before opening a span; a suppressed member produces no span, no minutes, no payload row (they are absent from the record, not anonymised in it — an anonymised attendance row is still a record of presence). `guild_analytics_settings.voice_enabled = off` stops capture guild-wide. This is a fix for the attendance branch before its flag ever enables (§14).
2. **Echo display bounds:** public surfaces show counts, rooms, peak, hosts, first-timer *counts*; per-member minutes appear only on that member's own profile and to moderators *(v1 §4)*. First-timers/hosts/welcomers are site derivations from site history. The ladder counts Echoes (design row 25) — going-counts remain labelled sign-ups wherever they are still used, never rebranded.

## 10. Consent, retention, erasure — the one table

| Data | Owner | Retention | Erasure path |
|---|---|---|---|
| Voice snapshots | site RAM only | seconds (cache) | nothing persists |
| Raw event-window sessions | bot DB | 90 days | bot ops on moderator request *(v1 §4)* |
| `voice_heat_hours` aggregates | bot DB | rolling 35 days | no member data to erase |
| Heat grid (site copy) | site DB | overwritten daily | n/a (aggregate) |
| Echo records incl. per-member rows | site DB | the permanent record | `src/member-erasure.ts` covers Echo rows (site card, *(v1 §4)*) |
| Public bot log | site DB | 90 days rolling | erasure nulls `subject_id` immediately |
| Bot `internal_action_log` / site `activity_log` | respective DBs | existing policies | existing; scalars only |
| Consent toggles (§4, §7) | site DB | with the profile | deleted with the profile |
| DM bodies | nowhere | bot renders from templates, stores audit scalars only *(v1 §3)* | n/a |

## 11. Failure modes the site must render (normative)

| Condition | Wire signal | Site renders / does |
|---|---|---|
| Bot down or gateway not ready | fetch failure / `discord_unavailable` | Freeze last real values with honest age; sign-up/RSVP/waitlist unaffected; Pull me in withdrawn for deep link |
| Verb flag off (pre-rollout) | `action_not_allowed` | Identical to bot-down rendering — members never see rollout plumbing |
| Signing/credential wrong | `unauthorized`, `stale_request` | Same member-facing freeze; operator-visible alert (this is our bug, not a state to explain to members) |
| Rate limited | `rate_limited` + `Retry-After` | Move: "Slow down a moment" + deep link. Queued: back off, same idempotency key |
| Move domain answers | 200 outcomes *(v1 §2 table)* | Per-outcome copy + Discord deep link `https://discord.com/channels/{guild}/{channel}`; never pretend a move happened |
| DM undeliverable | `dm_unavailable` | Offer/reminder stands on the site; shown on event page + profile; place never consumed by notification failure *(v1 §3)* |
| Unknown outcome (504, abort) | `upstream_timeout` / Worker `unavailable` | Retry same idempotency key only; "we're confirming" state where member-visible |
| `in_progress` | 409 retryable | Bounded queue retries; then operator surface, not member error |
| `needs_reconciliation` | 409 non-retryable | Stop; moderator/admin surface; reconcile by hand; never re-key (§7.4) |
| Stale everywhere (>2 min) | §3 bands | "Last synced X min ago"; Live Sentence drops to next-event grammar; Now Line shows events on an unshaded axis with its label intact |
| Heat stale/insufficient | §5 state machine | "estimate" label; never borrowed freshness |
| Bot log pull failing | cron failure | Feed shows its last entry's age ("Bot's log · updated 2 h ago"); nothing fabricated, nothing hidden |

## 12. Rollout, flags and collisions

Per verb, in order: merge behind default-off flag → staging verification (voice verbs: the `docs/staging-voice-event-verification.md` pattern from the move-member branch) → operator forwards the flag deliberately through `container-env` (adding a secret is never enough) → production enable under the receiver doc's enable order and the §1 signing gate. Site degrades honestly at every step (§11), so bot and site releases are never coupled.

- **Migration collision:** both in-flight branches (the move-member and member-notify branches) add `0471_*.sql` over main's `0470`. The second merger renumbers; reviewers check the ledger, not the card text.
- **Production schema/data changes** (new bot tables in §5/§6, site bot-log/Echo/prefs tables) ride their own PRs and the Release Manager merge path per repo governance, as every default-branch merge already does. Nothing in this document pre-approves a migration.
- **Move Members permission** on the production bot role is an operator rollout step with the §5-of-v1 preflight; Manage Roles is never requested.

## 13. Deltas to the Ember Hours design (the only ones)

1. **RSVP → Discord-interest mirroring is deferred** (§7): capability unproven under a bot token. Site RSVP remains the one source of truth; the Lifeline's "seat saved" step is a site fact, exactly as designed — only the Discord-side mirror waits on proof.
2. **Pull-me-in and DM deliveries are not lines in the *public* bot log** (§6): design P2 says moves are "logged to the bot's log" — they are, in the internal audits on both sides, moderator-visible. Publishing a member's room movements or DM receipt to a public feed would turn the transparency ledger into a whereabouts feed. The public log keeps event-scoped facts only.
3. **Freshness bands and the guest floor come from v1, not the mock grammar** (§3/§4): >2 min freezes and withdraws Pull me in (design copy "stale > 5 min" described the same state later than is safe for offering a move into a room that may be gone); guests see a live count only at ≥5 in voice (design's <3 "quiet" switch governs signed-in copy). Copy can adopt these numbers without redesign.

## 14. Follow-up work this contract produces (for the web and bot roadmaps to cut)

**New bot work (two-bot-next, same flag-off pattern as the move-member branch):**
- **B5** — voice heat aggregates: sampler + `voice_heat_hours` + 35 d prune + `web_v1.voice_heat` view + `contract_version` bump (§5). Size M. Unblocks Now Line calibration; site ships on "estimate" without it.
- **B6** — `botlog.read`: publishable-kind projection over `internal_action_log`, cursor, `privacy_opt_outs` suppression, flag `TWO_INTERNAL_ALLOW_BOTLOG_READ` (§6). Size M. Unblocks the bot's log + Lifeline facts.

**Amendments to the in-flight branches:**
- Member-notify branch: add `suppressed_in` pre-send check → `dm_unavailable`, and the per-member notify bucket (§7.1–7.2), before its flag enables anywhere.
- Attendance branch: add opt-out at capture + `voice_enabled` kill switch (§9.1), and the 90-day raw-session prune if not already scoped.
- Move-member branch: no functional change; the `0471` renumber rule (§12) and the §3 additive snapshot fields (`occupied_since`, `event_key`) ride B6-era work or a small follow-up — snapshot consumers must tolerate their absence meanwhile.

**Site-side (rides the web roadmap re-cut, named here so they cannot be forgotten):** consent toggles + notify preferences (§4/§7.1); `needs_reconciliation` handling (§7.4); bot-log store + 90-day sweep + erasure hook (§6/§10); heat pull + label state machine (§5); Lifeline composer with the §8 honesty rules; member-erasure coverage for Echo rows.

## 15. Contract artefacts, conformance, and what this does not do

- **Canonical file:** `docs/bot-site-contract.md` in **two-bot-next** (the receiver is the enforcing side; its docs already carry the receiver + signing contracts), holding this text minus card references, plus machine fixtures `crates/core/tests/fixtures/bot-site-contract-v2.json` (golden request/result pairs per verb in §2, test-only secrets). Its SHA-256 is pinned by a conformance test in **two-web-next** against a byte-identical fixture copy — the signing-vectors precedent (`test/bot-hmac-vectors.test.ts`); drift fails either CI. `web_v1` changes keep following `crates/store/sql/web_v1.sql` + `docs/web-v1-contract.md`, which stay the executable truth for views.
- **Not the design kit:** `two-design-kit` owns design tokens/components and how these states *look*; a wire contract with rate buckets and retention belongs with its two implementers. The kit may link to the canonical file; it must not fork it.
- **Amendment process:** a change is a PR to the canonical file + fixtures; additive fields need site tolerance first (§1); a new publishable bot-log kind or a new DM `kind` is an amendment to §6/§3-of-v1, never a free string; a breaking change is a new verb or `v` bump.
- **This document does not** flip a flag, grant a Discord permission, create a credential, approve a migration, or weaken any check. Staging verifications, operator rollouts, Release Manager merge confirmations and the Security review of this contract do that work in their own lanes.

**REA angle:** Twitch EventSub's per-subscription-type versioning + revocation states are the model for our per-verb `v` + flag-gated kill switches (a version you can revoke per consumer beats a global cutover); Steam's presence privacy defaults (rich presence visible to friends, aggregates to everyone else) are the model for §4's member-only who-is-where with a guest floor; Discord's own `widget.json` (names-free presence for the public) is the precedent our guest projection already follows. We borrowed their versioning and privacy defaults, and deliberately not their push transport: at one guild and one consumer, signed polling with honest staleness beats a subscription fleet — EventSub's operational tax (callback verification, subscription decay, duplicate delivery) buys nothing the 10-second cache doesn't.
