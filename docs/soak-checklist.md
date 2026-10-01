# Parity soak checklist

<!-- Generated from soak-checklist.json by scripts/check_soak_checklist.py --render. -->

This is the **unexecuted** B4 coverage plan for [TOG-9699](/TOG/issues/TOG-9699), authored under [TOG-10885](/TOG/issues/TOG-10885). Running the soak is out of scope. `manual`/`automated`/`waived` describe the verification method, not PASS. No row has been executed by writing this checklist.

## Safety and execution contract

- Follow [staging-soak.md](staging-soak.md) for lifecycle and rollback mechanics: its S2 acceptance is seven consecutive days with zero missed events and redeploy gap <60s. B4 separately requires its 48h watch log; neither substitutes for the other. [gateway-recovery.md](gateway-recovery.md) is the current session-persistence reference. Verify the deployed SHA, staging guild allowlist, permitted disposable accounts/channels and enabled slice before any Discord action. A missing slice, unapproved destructive scenario or missing fixture is NEEDS WORK, not a silent waiver.
- **No tests, SQL, probes, migrations, imports, backup or restore against staging/production databases.** Only agent-testdb/agent-testredis or ephemeral CI service containers may be used for data assertions. No alternate credential after an auth failure: stop, record the exact error and owner; never log credentials. Discord fixture actions require the separate soak authorization and must never target real members/live guilds.
- Negative/destructive/retry/clock/SSRF scenarios use local mock fixtures. Do not change the host clock, manufacture raids, delete real roles/channels, change live enforcement, or call live internal-action mutation endpoints. Restore approved disposable configuration through its normal UI/API after evidence; no database reset.
- Voice is owned by [TOG-10119](/TOG/issues/TOG-10119): consume its exact-SHA receipt, do not duplicate voice join/leave/restart steps here. Mixed rows still require their non-voice assertions.
- A `waived` entry is a **proposed waiver of staging execution**, not removal of parity or authority to approve it. B4 records the reason and accepting actor plus the owning slice’s isolated fixture evidence; until then the row is unresolved/NEEDS WORK. Backup/config/import evidence belongs to [TOG-9881](/TOG/issues/TOG-9881)/[TOG-9882](/TOG/issues/TOG-9882). No new spend, buckets, secret handling or host jobs is authorized here.
- Evidence table columns: checklist ID; deployed/reviewed SHA; actual command/action and fixture IDs; UTC window; expected vs actual; redacted screenshot/log/CI receipt URL; PASS/NEEDS WORK or accepted waiver with reason/actor; cleanup/rollback result. Attach it to B4 before its verdict. A CI unit result alone does not prove a staging Discord effect.

## Maintaining coverage

JSON is authoritative; each `parity.section` + `parity.row` copies every source table cell except Map. Repeated ClientReady, attendance and member-event surfaces remain separate. Only DROP-prefixed Maps without an S/B/NEW slice or TOG owner are excluded; mixed mapped/DROP rows remain covered in either order, including mapped replacement work. §7 is prose: one explicit catalogue entry covers env_only/cold/hot. Adding a table there also requires entries.

Run `PYTHONDONTWRITEBYTECODE=1 python3 scripts/check_soak_checklist.py` and `PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_soak_checklist.py' -v`. CI runs both offline before Cargo. After editing JSON, regenerate Markdown with `PYTHONDONTWRITEBYTECODE=1 python3 scripts/check_soak_checklist.py --render > docs/soak-checklist.md`. Source drift, missing/stale/duplicate rows, empty steps/evidence, invalid statuses, reasonless waivers, missing automated commands, missing voice links or Markdown drift fail the gate.

## Existing fixture entry points

Run compiling commands on the controller through `python3 scripts/cargo_cache.py run -- test ...` from an isolated workspace; never Cargo directly or another target/cache. Hosted CI retains its own Cargo commands. Attach actual executed commands and results, not this list as proof. An ignored/skipped DB test is not evidence. Configure only the named test-container URL required by each test guard; never inherit a live TWO_DATABASE_URL.

- Command dispatch: `-p two-bot-discord --test interaction_routing`; REST: `-p two-bot-discord --test executor_acceptance` and `--test executor_regressions`; funnel replay: `-p two-bot-discord --test funnel_replay`.
- Leveling: `-p two-bot-core --test leveling_golden`; onboarding: `--test onboarding_effects`; raids/containment: `--test raid_acceptance` and `--test containment_acceptance` (all core targets).
- Test DB receipts: `-p two-bot-core --features db --test leveling_store -- --ignored`; `--test onboarding_store -- --ignored`; `--test audit_store -- --ignored`; LFG `-p two-bot-core --features db lfg_store:: -- --ignored --test-threads=1`. Each needs its own safe container configuration.
- Community/presence/inactivity fixture filters: `-p two-bot-core --features db --lib community_store`, `presence_store`, `inactivity_store` separately; settings classification: `-p two-bot-core --lib settings::tests` and `config::tests` separately.
- Backup/snapshot fixture targets: `-p two-bot-core --test backup_transport`, `--test guild_config_regressions`; backup roundtrip `-p two-bot-core --features db --test backup_roundtrip -- --nocapture` requires TWO_BOT_TEST_DATABASE_URL on test containers. See [backup.md](backup.md). Import parsers: `-p two-bot-cutover mee6_xp::tests` and `mee6_rewards::tests` separately; dry-run import CLI can still enter migrations, so never aim it at staging/production.

## 1. Slash / prefix commands

### s1-01: 1 — `/rank`

- **Method:** `manual` (not an execution verdict).
- **Action:** As a normal fixture member invoke /rank, then /rank member:<other-fixture-member>.
- **Expected:** Both replies identify the selected member with the expected fixture XP/rank; no other guild data.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-02: 2 — `/leaderboard`

- **Method:** `manual` (not an execution verdict).
- **Action:** Invoke /leaderboard as a normal fixture member after two approved fixture XP awards.
- **Expected:** Ordering, tie handling and displayed ranks agree with the fixture awards.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-03: 3 — `/ban`

- **Method:** `manual` (not an execution verdict).
- **Action:** In the disposable guild invoke /ban target:<disposable-member> reason:soak; repeat as unprivileged member and against a protected/higher-role fixture.
- **Expected:** Only the permitted disposable target is banned; denied paths do not mutate membership; one correlated moderation audit.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-04: 4 — `/tempban`

- **Method:** `manual` (not an execution verdict).
- **Action:** Invoke /tempban target:<disposable-member> duration_seconds:60 reason:soak; wait for the 30s unban sweep after expiry.
- **Expected:** Target is banned once and unbanned after expiry; no duplicate action on retry.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-05: 5 — `/kick`

- **Method:** `manual` (not an execution verdict).
- **Action:** Invoke /kick target:<disposable-member> reason:soak; repeat without KickMembers and with a protected fixture.
- **Expected:** Only the allowed target is kicked; permission/hierarchy protections hold.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-06: 6 — `/timeout`

- **Method:** `manual` (not an execution verdict).
- **Action:** Invoke /timeout target:<disposable-member> duration_seconds:60 reason:soak, then use an unprivileged and protected-target fixture.
- **Expected:** Allowed timeout expires; denied cases do not change the target; correlated audit.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-07: 7 — `/warn`

- **Method:** `manual` (not an execution verdict).
- **Action:** Invoke /warn target:<fixture-member> reason:soak; repeat without ModerateMembers.
- **Expected:** One permitted warning and audit; unauthorized warning refused.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-08: 8 — `/purge`

- **Method:** `manual` (not an execution verdict).
- **Action:** Create three fixture messages; invoke /purge count:3 reason:soak in that disposable channel; exercise boundaries 1 and 100 in fixtures.
- **Expected:** Only requested eligible messages are removed; bounds and permission errors are explicit.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-09: 9 — `/slowmode`

- **Method:** `manual` (not an execution verdict).
- **Action:** Invoke /slowmode seconds:10 reason:soak, then seconds:0; use a member without ManageChannels for the denied case.
- **Expected:** Channel slowmode changes to 10 then 0; denial preserves state; upper bound 21600 validated.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-10: 10 — `/lockdown`

- **Method:** `manual` (not an execution verdict).
- **Action:** Save the disposable channel overwrite through Discord UI, invoke /lockdown reason:soak.
- **Expected:** Send restriction is applied once and prior overwrite retained for unlock; protected actors remain protected.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-11: 11 — `/unlock`

- **Method:** `manual` (not an execution verdict).
- **Action:** After command-10 invoke /unlock reason:soak twice and compare the saved channel overwrite.
- **Expected:** Original overwrite restored exactly; second invocation is harmless.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-12: 12 — `/attendance` (scorecard)

- **Method:** `manual` (not an execution verdict).
- **Action:** Using the published scorecard attendance command name, invoke event-occurrence:<fixture-occurrence> member:<fixture-member> as host and as non-host; record actual registry name.
- **Expected:** Host produces one verified-attendance fact; denial produces none; name is distinct from RSVP totals command-25.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-13: 14 — `/command`

- **Method:** `manual` (not an execution verdict).
- **Action:** Invoke /command name:soak-faq template:"{user} {username} {server} {channel}" description:soak text-trigger:soakfaq as manager; repeat as normal member.
- **Expected:** Template stored and published only for manager; unsupported tokens/names rejected; runtime covered separately.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-14: 15 — `/command-remove`

- **Method:** `manual` (not an execution verdict).
- **Action:** Invoke /command-remove name:soak-faq, then invoke the removed command.
- **Expected:** Definition disappears from registry/list; removed invocation cannot execute.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-15: 16 — `/command-list`

- **Method:** `manual` (not an execution verdict).
- **Action:** Create soak-faq and invoke /command-list as manager and normal member.
- **Expected:** Manager sees the fixture definition without secrets; normal member is denied.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-16: 17 — `/schedule`

- **Method:** `manual` (not an execution verdict).
- **Action:** Invoke /schedule body:soak-once in-minutes:1; also create body:soak-repeat every-minutes:60; try neither option and below-minimum intervals.
- **Expected:** Valid messages are queued for invoking channel, one-shot fires once; invalid cadence rejected; recurring schedule removed after evidence.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-17: 18 — `/schedule-remove`

- **Method:** `manual` (not an execution verdict).
- **Action:** Take the fixture schedule ID from /schedule-list; invoke /schedule-remove id:<unique-prefix>, then an ambiguous/unknown prefix fixture.
- **Expected:** Unique ID is removed; ambiguous or unknown ID is rejected without touching another schedule.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-18: 19 — `/schedule-list`

- **Method:** `manual` (not an execution verdict).
- **Action:** Invoke /schedule-list before and after creation/removal of the fixture schedule, and as an unprivileged actor.
- **Expected:** Only guild-scoped schedules listed to permitted actor, with matching IDs/cadences.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-19: 20 — `/sticky`

- **Method:** `manual` (not an execution verdict).
- **Action:** Invoke /sticky body:soak-sticky debounce:1; send two fixture messages in the disposable channel.
- **Expected:** One latest sticky reappears after debounce, old sticky is replaced, no recursive bot repost storm.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-20: 21 — `/sticky-remove`

- **Method:** `manual` (not an execution verdict).
- **Action:** Invoke /sticky-remove in the fixture channel and send another fixture message.
- **Expected:** No further sticky repost; other channels remain unchanged.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-21: 22 — `/<custom>` (dynamic, DB-backed via `/command`)

- **Method:** `manual` (not an execution verdict).
- **Action:** Create /soak-faq with /command; invoke it as a normal member with automations enabled, then under the approved disabled configuration.
- **Expected:** Enabled invocation renders all four template values; disabled invocation refuses without posting.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-22: 23 — `!<trigger>` (prefix, e.g. `!faq`)

- **Method:** `manual` (not an execution verdict).
- **Action:** With approved TWO_TEXT_COMMANDS=1 send !soakfaq extra words, unknown !name and a builtin-name trigger; repeat with flag off.
- **Expected:** Only configured first-token trigger runs; builtin/unknown/disabled triggers do not run.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-23: 24 — `/rsvp`

- **Method:** `manual` (not an execution verdict).
- **Action:** For a disposable future event invoke /rsvp event-id:<id> status:going, interested, declined; retry the last choice.
- **Expected:** Latest status replaces earlier status once; totals have no duplicate attendee; malformed event-id or unknown-status fixture refused (shape contract only — the port performs no event-time check).
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-24: 25 — `/attendance` (RSVP totals)

- **Method:** `manual` (not an execution verdict).
- **Action:** Invoke the published RSVP totals command for event-id:<fixture-id> after command-24; capture actual published name.
- **Expected:** Totals match the three-status fixture history; command is not overwritten by scorecard attendance command-12.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-25: 26 — `/lfg`

- **Method:** `manual` (not an execution verdict).
- **Action:** Invoke /lfg title:soak starts-at:<future ISO-8601> roles:tank:Tank:2 as event manager; try invalid dates and actor without ManageEvents.
- **Expected:** One valid role menu appears with capacity 2; malformed/unauthorized creation rejected.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-26: 27 — `/lfg-close`

- **Method:** `manual` (not an execution verdict).
- **Action:** Invoke /lfg-close id:<fixture-lfg-id> and try signing up again.
- **Expected:** Menu closes, future signups refused; repeated close is harmless.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-27: 28 — `/feed-add`

- **Method:** `manual` (not an execution verdict).
- **Action:** Add one approved public fixture source per kind with /feed-add kind:rss|youtube|twitch source:<public-fixture>; use local fixtures for private-IP rejection.
- **Expected:** Valid sources become guild-scoped relays; malformed/SSRF-private sources rejected without network access to private hosts.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-28: 29 — `/feed-remove`

- **Method:** `manual` (not an execution verdict).
- **Action:** Invoke /feed-remove id:<fixture-relay-id>; wait one configured poll interval.
- **Expected:** Relay disappears and no new posts are delivered for the removed source.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s1-29: 30 — `/feed-list`

- **Method:** `manual` (not an execution verdict).
- **Action:** Invoke /feed-list before/after relay changes and as actor without ManageGuild.
- **Expected:** List matches this guild only; unauthorized actor refused; no credentials in output.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

## 2. Non-command interactions

### s2-01: Game picker `two:onboarding:games` — add/remove game roles to match menu, ephemeral reply

- **Method:** `manual` (not an execution verdict).
- **Action:** Select two approved game roles in two:onboarding:games, then deselect one.
- **Expected:** Member roles match selection exactly; ephemeral acknowledgement; unrelated roles preserved.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s2-02: Session picker (`SESSION_SELECT_ID`) — ephemeral ack + routed record, no roles

- **Method:** `manual` (not an execution verdict).
- **Action:** Use SESSION_SELECT_ID twice with the approved two-pick session fixture.
- **Expected:** Ephemeral acknowledgement and routed session record; no role mutation.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s2-03: Self-role buttons/selects — claimed add/remove/replace + validation/hierarchy checks

- **Method:** `manual` (not an execution verdict).
- **Action:** Click configured self-role add/remove buttons, then replace via select; exercise unknown and above-bot-role fixtures.
- **Expected:** Claimed actions are idempotent; unrelated roles preserved; invalid/hierarchy cases refused.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s2-04: `MessageReactionAdd/Remove` — reaction-role grant/revoke (partial fetch, idempotent plan)

- **Method:** `manual` (not an execution verdict).
- **Action:** React then remove reaction on the fixture role panel; replay add/remove using the mock partial-message fixture.
- **Expected:** Role granted then revoked once; partial fetch works and gateway retry does not duplicate audit.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s2-05: Ticket buttons `open/claim/close` — ticket lifecycle + 5m recovery / 60m purge timers

- **Method:** `manual` (not an execution verdict).
- **Action:** Open, claim and close a disposable ticket; restart with an open ticket, then wait 5m recovery and 60m purge.
- **Expected:** Single lifecycle/channel ownership survives restart; transcript retained/purged per policy; no leaked content outside fixture channel.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s2-06: LFG select menus — signup flows

- **Method:** `manual` (not an execution verdict).
- **Action:** Choose Tank on fixture LFG menu as two members, then a third; change/cancel one signup.
- **Expected:** Capacity enforced, duplicate signup not counted, released slot available.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s2-07: `/attendance` host check-in → community attendance store — verified-attendance fact

- **Method:** `manual` (not an execution verdict).
- **Action:** As authorized host check in fixture member for fixture occurrence, then retry and attempt as non-host.
- **Expected:** Exactly one verified-attendance fact; unauthorized/duplicate interactions cannot inflate attendance.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

## 3. Gateway event handlers

### s3-01: `ClientReady` — log `ready`; per-guild invite snapshot for join attribution (skipped in containment)

- **Method:** `manual` (not an execution verdict).
- **Action:** Start the approved contained staging deployment; capture ready log and invite snapshot activity with containment off, then on.
- **Expected:** Ready logged once per session; invite baseline collected only when allowed; containment skips snapshot.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s3-02: `ClientReady` — ticket recovery + panel ensure + transcript purge; command publish (`guild.commands.set`)

- **Method:** `manual` (not an execution verdict).
- **Action:** Restart with a fixture open ticket and existing panel; capture published command registry.
- **Expected:** Recovery does not duplicate tickets/panels; purge runs; unique command names include both attendance variants.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s3-03: `ready` — `audit.retryPending()` once + 30s sweep

- **Method:** `manual` (not an execution verdict).
- **Action:** Restart with mock pending operational audit delivery; advance the fixture clock through 30s.
- **Expected:** Initial retry runs once; bounded sweep recurs and does not duplicate delivered entries.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s3-04: `GuildMemberAdd` — funnel: invite diff vs `expectedJoins`, `onJoin`, instant `onGateCleared` if `!pending`, raid-burst + join-risk scoring

- **Method:** `manual` (not an execution verdict).
- **Action:** Have a disposable account join via one fixture invite; test pending and non-pending variants with mock guild-member events.
- **Expected:** Invite diff/expectedJoins attribution and join funnel occur once; non-pending immediately gate-clears; risk flags are flag-only.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s3-05: `GuildMemberAdd` — legacy / session / anchor welcome (mode switch `TWO_ONBOARDING_MODE`)

- **Method:** `manual` (not an execution verdict).
- **Action:** Join with a disposable member under each approved onboarding mode configuration.
- **Expected:** Correct legacy/session/anchor welcome only; no DM or duplicate prompt.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s3-06: `GuildMemberUpdate` — rules-gate `pending:true→false` → rota `gateCleared` + `onGateCleared`; role/nickname diff → `member_update` audit

- **Method:** `manual` (not an execution verdict).
- **Action:** Clear screening for a pending fixture member; change a fixture nickname and role.
- **Expected:** One gate-clear fact; metadata-only member_update audit; no duplicate on unchanged update.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s3-07: `GuildMemberUpdate` — welcome prompt on gate-clear (per mode)

- **Method:** `manual` (not an execution verdict).
- **Action:** Clear screening under each approved onboarding mode; replay the same update in mock fixtures.
- **Expected:** Exactly one mode-specific welcome prompt; dry-run honoured.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s3-08: `GuildMemberRemove` — funnel `onLeave`

- **Method:** `manual` (not an execution verdict).
- **Action:** Have disposable member leave the fixture guild after joining.
- **Expected:** One leave funnel fact; member projection reflects leave, retries do not duplicate.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s3-09: `GuildMemberRemove` — session goodbye post (no ping)

- **Method:** `manual` (not an execution verdict).
- **Action:** Have a session-mode disposable member leave.
- **Expected:** Single goodbye post without ping/DM; other modes do not emit session goodbye.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s3-10: `MessageCreate` — automod inspect → rota reserve → funnel `onMessage` + level hook; rejected → `captureOnly` row

- **Method:** `manual` (not an execution verdict).
- **Action:** Send one acceptable and one configured automod-rejected fixture message; retry both as mock gateway events.
- **Expected:** Accepted message reaches funnel/level hook once; rejected message has captureOnly, no accepted XP/sticky/trigger side effects.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s3-11: `automationMessageAccepted` (internal) — sticky re-post + `!` text triggers

- **Method:** `manual` (not an execution verdict).
- **Action:** Send an accepted fixture trigger then an automod-rejected trigger in the sticky fixture channel.
- **Expected:** Only automationMessageAccepted drives prefix/sticky actions; no action on rejected message.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s3-12: `Raw` — message delete/edit audit without cached message

- **Method:** `manual` (not an execution verdict).
- **Action:** Delete and edit fixture messages after clearing mock message cache; dispatch raw payloads.
- **Expected:** Metadata-only delete/edit audit without cached content; no invented actor/content.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s3-13: `MessageUpdate` — fetch partial + automod re-inspect on edit

- **Method:** `manual` (not an execution verdict).
- **Action:** Edit a benign fixture message to a configured automod violation using partial-message mock fixture.
- **Expected:** Partial fetch and reinspection enforce/dry-run according to configuration, retry is idempotent.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s3-14: `VoiceStateUpdate` — channel-change only; `voice_join/leave/move` audit; `onVoiceLeave`→`onVoiceJoin` + level hook

- **Method:** `manual` (not an execution verdict).
- **Action:** Attach the VoiceStateUpdate row evidence from the dedicated voice soak; do not run a second voice scenario here.
- **Expected:** TOG-10119 demonstrates channel-change-only audit, leave/join ordering and XP hook; mute-only update has no session transition.
- **Evidence:** Link the exact-SHA voice evidence table and verdict from [TOG-10119](/TOG/issues/TOG-10119), including row/scenario ID and time window. Missing/failing evidence leaves this row NEEDS WORK; do not infer PASS from the reference.
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s3-15: `ShardResume`/`ShardReady` — drop all open voice sessions (no outage-inflated durations)

- **Method:** `manual` (not an execution verdict).
- **Action:** Attach ShardResume/ShardReady evidence from the dedicated voice soak; do not duplicate its restart procedure.
- **Expected:** TOG-10119 demonstrates all open voice sessions dropped, with no outage-inflated duration or XP.
- **Evidence:** Link the exact-SHA voice evidence table and verdict from [TOG-10119](/TOG/issues/TOG-10119), including row/scenario ID and time window. Missing/failing evidence leaves this row NEEDS WORK; do not infer PASS from the reference.
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s3-16: `InviteCreate` — re-snapshot invites (attribution freshness)

- **Method:** `manual` (not an execution verdict).
- **Action:** Create one disposable staging invite then join via it.
- **Expected:** Invite baseline refreshed; subsequent attribution uses fresh invite counts.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s3-17: `GuildAuditLogEntryCreate` — MAC-verified moderation audit → operational audit

- **Method:** `manual` (not an execution verdict).
- **Action:** Trigger one permitted fixture moderation action; replay a forged/invalid MAC audit-entry payload only in local mock.
- **Expected:** Valid bot-action MAC correlates once into audit; forged entry is not trusted.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s3-18: `GuildAuditLogEntryCreate` — anti-nuke containment filter → `containment.observe`

- **Method:** `manual` (not an execution verdict).
- **Action:** Replay threshold anti-nuke audit entries in local mock with alerts-only configuration; one benign entry in disposable guild.
- **Expected:** Filter calls containment observer for relevant actions; logs alerts without destructive enforcement or pings.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s3-19: `InteractionCreate` — all slices (§1–§2 dispatch)

- **Method:** `manual` (not an execution verdict).
- **Action:** Invoke a slash command, a role button and a select in approved disposable channels.
- **Expected:** InteractionCreate dispatches each to exactly one correct handler; unrelated interaction refused/ignored safely.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s3-20: `Error` — `client_error` log

- **Method:** `manual` (not an execution verdict).
- **Action:** Inject mock gateway error (do not break the live connection or credentials).
- **Expected:** Structured client_error tracing is present without tokens or message content.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s3-21: `Dispatch/Ready/Closed/Hello/Resumed/…` staging-restart containment filter — allowlisted guild/actor payloads only

- **Method:** `manual` (not an execution verdict).
- **Action:** Attach the approved staging lifecycle receipt for persisted session plus RESUME across a Container restart (see gateway-recovery.md), including staging guild/actor allowlist checks. Use local fixtures for rejected guild/actor payloads; do not reintroduce the dropped legacy restart filter.
- **Expected:** S5 session persistence and S6 staging containment replace the legacy filter: the approved staging session resumes and disallowed fixture payloads have no side effects. The legacy runtime filter remains dropped; voice reset evidence comes only from TOG-10119.
- **Evidence:** Record the deployed SHA, staging identity/allowlist receipt, restart and RESUME UTC timestamps, correlated sanitized logs and local rejection-fixture result. Cross-link the existing lifecycle receipt and TOG-10119 voice receipt; no second voice procedure or production restart.
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — consume its voice reset receipt; non-voice replacement assertions remain on this row.

## 4. Scheduled jobs & timers

### s4-01: presenceProbe (`GET /guilds/{id}?with_counts=true` → `presence_probe`; reader: `presence-trend` only) — 1h, bot-floor re-list 24h

- **Method:** `manual` (not an execution verdict).
- **Action:** Observe one approved staging presence tick at 1h; replay 24h bot-floor relist using fixture clock.
- **Expected:** Count probe contains expected human/bot separation; bot-floor refreshed at 24h.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s4-02: communitySnapshots counter tick → `guild_counters` + `counter_snapshots` (website `live_counts`) — 60s

- **Method:** `manual` (not an execution verdict).
- **Action:** Observe two 60s counter ticks with one disposable member/message change.
- **Expected:** Published live_counts progresses consistently; counter snapshot captured once per tick.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s4-03: communitySnapshots rank tick → `rank_snapshots` + `member_ranks` (skips raid windows) — 10m

- **Method:** `manual` (not an execution verdict).
- **Action:** Observe 10m rank tick after fixture XP awards; replay a raid window in local fixture.
- **Expected:** Rank output agrees with awards; raid-window update skipped.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s4-04: scheduled-events poller → atomic `scheduled_events` mirror (website feed) — 10m

- **Method:** `manual` (not an execution verdict).
- **Action:** Create/change/cancel a disposable scheduled event and observe 10m poll.
- **Expected:** Website event feed shows atomic replacement with no partial/inconsistent event set.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s4-05: communityScorecard Monday 06:15 UTC week run — 60s tick, weekly fire

- **Method:** `manual` (not an execution verdict).
- **Action:** Use local fixture clock for Monday 06:15 UTC; replay adjacent 60s ticks. Do not change host clock.
- **Expected:** One weekly scorecard run/alert, not one per tick; rerun does not duplicate.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s4-06: inactivity `flagInactive()` (read-only, never DMs) — hourly from `index.ts`

- **Method:** `manual` (not an execution verdict).
- **Action:** Observe hourly inactivity job against approved synthetic membership fixture, never real-member outreach.
- **Expected:** Flags/report only; no DMs or membership mutation.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s4-07: automation scheduled-message ticker (15s, `next_run_at` queue) — 15s

- **Method:** `manual` (not an execution verdict).
- **Action:** Queue a fixture one-shot due message and observe 15s ticker; retry/restart through mock fixture.
- **Expected:** One post per due item; next_run_at advances or one-shot retires, no duplicate on claim/restart.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s4-08: feed poller (RSS/YouTube/Twitch → `feed_deliveries` + relay) — `TWO_FEED_POLL_SECONDS` default 300s

- **Method:** `manual` (not an execution verdict).
- **Action:** Configure approved public fixture feeds and observe TWO_FEED_POLL_SECONDS (default 300s); replay same item.
- **Expected:** One feed_deliveries claim/relay per item; unchanged item not reposted.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s4-09: operational-audit retry sweep (≤25 claims) — 30s

- **Method:** `manual` (not an execution verdict).
- **Action:** Queue 26 mock pending audit deliveries; advance 30s clock across retries.
- **Expected:** Sweep claims at most 25; retryable failures stay pending; successful mirrors delivered once.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s4-10: moderation unban sweep (`moderation_scheduled_unbans`) — 30s

- **Method:** `manual` (not an execution verdict).
- **Action:** After command-04 observe successive 30s unban sweeps across expiry.
- **Expected:** Only expired tempban is unbanned once; unexpired item retained.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s4-11: ticket recovery / transcript purge — 5m / 60m

- **Method:** `manual` (not an execution verdict).
- **Action:** Restart with open/closed disposable tickets, observe recovery at 5m and purge at 60m.
- **Expected:** Recovery is idempotent, only policy-expired fixture transcripts purged.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s4-12: settings version poll (`guild_settings` hot reload) — 15s

- **Method:** `manual` (not an execution verdict).
- **Action:** Change one permitted hot guild setting via dashboard, then wait at least 15s.
- **Expected:** Version advances and runtime uses new setting without restart; unchanged version not reloaded.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s4-13: self-role claim renewal / automod repeat-tracker expiry / per-request fetch timeouts — lease/timeout driven

- **Method:** `manual` (not an execution verdict).
- **Action:** In local fixtures advance self-role lease, automod repeat TTL and fetch timeout clocks; retry expired claim.
- **Expected:** Lease renewal and expiry bounded; repeat tracker cleaned; timed-out request aborts without duplicate effect.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s4-14: systemd `two-bot-backup.timer` (nightly DB→S3) — daily 04:17

- **Method:** `waived` (not an execution verdict).
- **Action:** Use an authorized test-container backup fixture at simulated daily 04:17; capture its TOG-9881 receipt.
- **Expected:** Dump/upload timer equivalent invokes once with integrity receipt; no staging/production DB connection.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.

### s4-15: systemd `two-bot-guild-config-backup.timer` (sealed Discord config snapshot) — daily 04:31 UTC

- **Method:** `waived` (not an execution verdict).
- **Action:** Use disposable guild-config snapshot fixture at simulated daily 04:31 UTC under TOG-9881.
- **Expected:** Sealed snapshot timer equivalent invokes once; allowlisted metadata only, no secret/log leakage.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.

### s4-16: systemd `two-bot-restore-drill.timer` — monthly

- **Method:** `waived` (not an execution verdict).
- **Action:** Attach TOG-9881 monthly restore-drill receipt from agent-testdb/CI services only.
- **Expected:** Restore verifies integrity and fixture row counts in isolated test DB; never replaces staging or production.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.

## 5. DB tables & queries

### s5-01: `events`, `members`, `invite_snapshots` — append-only funnel log + member projection + invite snapshots

- **Method:** `waived` (not an execution verdict).
- **Action:** Replay synthetic joins/messages/leaves and invite snapshots using the authorized test-container funnel store fixture.
- **Expected:** Append-only events and member/invite projections are idempotent and guild-scoped.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.

### s5-02: `internal_nonces`, `internal_idempotency`, `internal_action_log`, `internal_discord_events` — website-callback replay guard, idempotency, audit, dedupe

- **Method:** `waived` (not an execution verdict).
- **Action:** Send signed synthetic actions, replay nonce/idempotency key, and repeat Discord callback in CI store fixtures.
- **Expected:** Durable nonce refusal, stable replay response and one action/event audit across restart.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.

### s5-03: `web_contract_meta`, `guild_counters`, `rank_ladder`, `rank_snapshots`, `member_ranks`, `scheduled_events` — website read contract

- **Method:** `waived` (not an execution verdict).
- **Action:** Load website contract fixture on agent-testdb/CI and query every web_v1 read contract view there.
- **Expected:** Contract metadata, counters, ranks and scheduled events agree atomically; no staging database queried.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.

### s5-04: `presence_probe` — hourly presence series

- **Method:** `waived` (not an execution verdict).
- **Action:** Run presence-series fixture ticks at 1h in authorized test DB.
- **Expected:** One bounded presence row per tick with correct series ordering.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.

### s5-05: `counter_snapshots`, `member_exclusions` — live-count audit history, raid exclusions

- **Method:** `waived` (not an execution verdict).
- **Action:** Run counter snapshot and raid-exclusion fixtures in authorized test DB.
- **Expected:** Excluded members do not inflate live counts; history preserves prior snapshots.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.

### s5-06: `invite_campaigns` (`go.two.gg/<slug>`) — tracked short links

- **Method:** `waived` (not an execution verdict).
- **Action:** Use redirect campaign fixture slug against local fixture store, then unknown slug.
- **Expected:** Known slug records invite_click once and redirects; unknown slug uses fallback without invented attribution.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.

### s5-07: `member_levels`, `xp_cooldowns` (60s msg/voice), `xp_awards`, `level_role_rewards`, `level_import_runs` — leveling, MEE6-compat (5 XP/min voice)

- **Method:** `waived` (not an execution verdict).
- **Action:** Use synthetic message XP cooldowns and approved MEE6/reward import fixture in authorized test DB; attach the voice XP portion from TOG-10119 rather than duplicating its voice scenario.
- **Expected:** 60s cooldown, 5 XP/min voice compatibility, reward idempotency and import-run ledger match fixture expectations.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s5-08: `moderation_warnings`, `moderation_scheduled_unbans`, `moderation_audit`, `moderation_lockdowns`, `moderation_idempotency` — moderation ledger, tempban queue, lockdown state, claim table

- **Method:** `waived` (not an execution verdict).
- **Action:** Run synthetic warnings/tempbans/lockdowns and duplicate moderation claim fixtures on test DB.
- **Expected:** Ledger, expiry queue, prior overwrite and idempotency claim persist across replay.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.

### s5-09: `operational_audit_log` (states `none/pending/delivering/delivered/quarantined`, 5-min lease, hourly mirror recheck) — metadata-only Discord-event parity audit

- **Method:** `waived` (not an execution verdict).
- **Action:** Run audit-state fixture transitions with 5m claim expiry and 1h mirror recheck in test DB.
- **Expected:** Only valid none/pending/delivering/delivered/quarantined transitions; expired claim recoverable and duplicates blocked.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.

### s5-10: `automod_violations`, `automod_processed_messages` — sanctions ladder, gateway-retry dedupe

- **Method:** `waived` (not an execution verdict).
- **Action:** Run accepted/rejected/edit/retry automod fixtures on test DB.
- **Expected:** One processed-message claim; violation ladder advances only for distinct qualifying violations.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.

### s5-11: `tickets`, `ticket_transcripts` — ticket lifecycle + transcripts

- **Method:** `waived` (not an execution verdict).
- **Action:** Run open/claim/close/recovery/transcript-expiry fixture in test DB.
- **Expected:** Lifecycle and transcript retention persist through restart; duplicate open does not create second ticket.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.

### s5-12: `containment_events`, `containment_incidents`, `join_risk_flags` — anti-nuke signals/incidents, join-risk (flag-only, never kicks)

- **Method:** `waived` (not an execution verdict).
- **Action:** Run alerts-only anti-nuke and join-burst fixtures on test DB.
- **Expected:** Signals/incidents/risk flags persist; join-risk never kicks.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.

### s5-13: `automation_commands`, `scheduled_messages`, `sticky_messages`, `automation_audit_log` — custom commands, schedule queue, stickies

- **Method:** `waived` (not an execution verdict).
- **Action:** Run create/remove/custom-trigger/schedule/sticky fixtures on test DB.
- **Expected:** Definitions, due queue, last sticky and audit persist with guild isolation and idempotent claims.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.

### s5-14: `self_role_audit`, `self_role_panel_claims` — self-role audit + claim leases

- **Method:** `waived` (not an execution verdict).
- **Action:** Run role add/remove/replace and claim-renewal fixtures on test DB.
- **Expected:** Claim lease prevents duplicate role action; metadata-only audit persists.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.

### s5-15: `community_facts`, `community_stream_heartbeats`, `community_scorecard_runs/alerts` — scorecard facts/runs/alerts

- **Method:** `waived` (not an execution verdict).
- **Action:** Run fact/heartbeat/weekly-scorecard fixtures on test DB, excluding dropped rota extensions.
- **Expected:** Facts and stream liveness yield one weekly run/alert; dropped rota tables are not required.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.

### s5-16: `event_rsvps`, `lfg_posts/roles/signups`, `feed_relays`, `feed_deliveries`, `announcements_audit_log` — RSVP, LFG, feed relay + delivery claims

- **Method:** `waived` (not an execution verdict).
- **Action:** Run RSVP replacement, LFG capacity, feed dedupe and announcement audit fixtures on test DB.
- **Expected:** Latest RSVP, bounded signups, single delivery and idempotent announcement audit persist.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.

### s5-17: `guild_settings`, `guild_settings_audit` — dashboard-writable hot settings

- **Method:** `waived` (not an execution verdict).
- **Action:** Run permitted/forbidden setting changes and version-poll fixtures on test DB.
- **Expected:** Guild setting version and audit advance together for attributed hot/cold writes; wired hot changes apply live, cold changes require restart; env_only and unknown writes are refused before persistence/audit.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.

### s5-18: `audit_kill_switch` (presence = halt; audit fail-open, rota fail-closed) — audit pipeline kill switch

- **Method:** `waived` (not an execution verdict).
- **Action:** Insert/remove the kill-switch fixture only in authorized test DB; simulate pending audit delivery.
- **Expected:** Presence halts the audit pipeline; absence permits resume; failure mode matches audit policy.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.

## 6. External integrations

### s6-01: Discord REST (`/api/v10`, `DISCORD_API_BASE` override) — guild/members/scheduled-events reads; 110ms pacing, 429 `retry-after+250ms`, 5xx exp backoff ≤4; kicks 350ms pacing, 4 retries; moderation REST 5s abort, no auto-retry

- **Method:** `automated` (not an execution verdict).
- **Action:** Against mock-discord REST double, script success/429/5xx/timeout/kick/moderation responses; attach fixture timestamps. Use only benign permitted staging REST during command scenarios.
- **Expected:** 110ms pacing, retry-after+250ms, ≤4 exponential retries; kick 350ms/4 retries; moderation aborts at 5s without automatic retry.
- **Evidence:** Attach exact-head fixture commands and PASS/NEEDS WORK result with sanitized request/response timestamps or signature/integrity assertion; fixture proof only, not a deployed-network soak receipt.
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot-discord --test executor_acceptance && python3 scripts/cargo_cache.py run -- test -p two-bot-discord --test executor_regressions

### s6-02: Discord gateway (discord.js → twilight) — intents §3; session persist + RESUME across Container restarts (restart-storage precedent)

- **Method:** `manual` (not an execution verdict).
- **Action:** Attach approved lifecycle deployment receipt for ready→restart→RESUME with persisted session; cross-link voice reset evidence.
- **Expected:** Required intents gated; persisted session resumes instead of reconnect loop; voice effects proven by TOG-10119.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s6-03: Discord CDN emoji fetch (guild-config snapshot) — `GUILD_CONFIG_CDN_BASE` override

- **Method:** `automated` (not an execution verdict).
- **Action:** Run backup_transport::guild_config_capture_plan_apply_round_trip, which supplies loopback Discord/CDN doubles and passes the CDN override to GuildConfigDiscordApi; no external CDN or real guild call.
- **Expected:** The loopback CDN override is honoured: captured unmanaged emoji image starts with data:image/png;base64, and the snapshot seal verifies. This fixture does not prove oversize, timeout, invalid-content-type or deployed-network behaviour; do not claim those from its PASS.
- **Evidence:** Attach the exact-head filtered command, executed test count (one, not zero), result and assertions at crates/core/tests/backup_transport.rs:394–420. Fixture capture/override and seal proof only, not a deployed-network or negative-validation soak receipt.
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot-core --test backup_transport guild_config_capture_plan_apply_round_trip -- --exact

### s6-04: Feeds RSS/YouTube/Twitch (UA `Owen/1.0`, SSRF public-IP guard, `MAX_FEED_BYTES`) — poll → `feed_deliveries` → relay

- **Method:** `manual` (not an execution verdict).
- **Action:** Poll approved public RSS/YouTube/Twitch fixture; use local mocks for redirect/private-IP/oversize cases.
- **Expected:** Owen/1.0 UA, public-IP SSRF guard and MAX_FEED_BYTES enforced; one relay per item.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s6-05: Website → bot `POST /internal/actions` (HMAC-SHA256 `sha256=` over `POST\npath\nts\nnonce\nsha256(body)`; `TWO_INTERNAL_KEYS`; skew+nonce replay guard; buckets 20 burst/1/s, `guild.add_member` 10/0.5s; 18 actions: `role.assign`, `guild.add_member`, `announcement.post`, `event.upsert/cancel`, `automations.import/export`, `settings.get/set`, `moderation.*` ×9) — web callbacks incl. `identify guilds.join` auto-join path

- **Method:** `waived` (not an execution verdict).
- **Action:** In local mock/CI replay all 18 signed internal actions plus bad signature, stale timestamp, reused nonce and rate-bound fixtures; do not call live mutation endpoints.
- **Expected:** POST canonical HMAC, replay/skew rejection, durable idempotency; 20 burst/1s and add_member 10/0.5s buckets; nine moderation actions keep protection.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.

### s6-06: `go.two.gg` redirect (`GET /<slug>` → `invite_click` → 302; unknown → fallback code) — ~450-line server + campaigns

- **Method:** `manual` (not an execution verdict).
- **Action:** Use approved staging redirect origin with a disposable registered slug and unknown slug; never send traffic to production go.two.gg for this check.
- **Expected:** 302 destination and fallback match fixture; invite_click is attributable without leaking personal tokens.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s6-07: S3-compatible backup (SigV4 single-PUT, `TWO_BACKUP_S3_*`) — nightly dump upload

- **Method:** `automated` (not an execution verdict).
- **Action:** Attach TOG-9881 SigV4 single-PUT fixture receipt from an approved local S3 double, not a real bucket.
- **Expected:** Request canonicalization/signature and integrity checks pass; no production object or credential changes.
- **Evidence:** Attach exact-head fixture commands and PASS/NEEDS WORK result with sanitized request/response timestamps or signature/integrity assertion; fixture proof only, not a deployed-network soak receipt.
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot-core --test backup_transport

### s6-08: Postgres (`TWO_DATABASE_URL`, staging guard; pool max 5, `statement_timeout`) — direct from Container (no Hyperdrive)

- **Method:** `waived` (not an execution verdict).
- **Action:** Run pool/timeout/staging-guard negative fixtures with no live URLs; test connectivity only to agent-testdb/CI.
- **Expected:** Wrong stage/guild guard refuses before connect; configured pool ≤5 and statement timeout bounded.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.

### s6-09: Health endpoint (`TWO_HEALTH_PORT`, `503 gateway_disconnected` pre-ready) — Container probe

- **Method:** `manual` (not an execution verdict).
- **Action:** In contained staging observe /readyz before gateway ready, after ready and during approved restart; pair with local pre-ready fixture.
- **Expected:** 503 with per-component JSON pre-ready (process ready, gateway starting/down); 200 only when every component reports ready; no secret/session data in body. `gateway_disconnected` is the legacy matrix label, not the deployed body — see docs/staging-soak.md.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s6-10: Operational-audit mirror channel (retryable delivery, `allowedMentions:{parse:[]}`, nonce-enforced) — tamper-evident Discord mirror

- **Method:** `manual` (not an execution verdict).
- **Action:** Cause retryable mirror failure only in local REST mock; deliver one metadata-only fixture audit message to approved staging audit channel.
- **Expected:** Retry durable, nonce-enforced delivery once, allowedMentions parse empty and no message content.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s6-11: Moderation-audit MAC (`[two-audit:v1:token:action:actor:mac]`, HMAC-SHA256 `TWO_MODERATION_AUDIT_SECRET`, `timingSafeEqual`) — bot-action correlation

- **Method:** `manual` (not an execution verdict).
- **Action:** Run valid/tampered/wrong-actor MAC fixtures locally; correlate one permitted disposable moderation action.
- **Expected:** Exact two-audit:v1 envelope validates constant-time; forged audit marker never trusted.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s6-12: Inbound webhook classification only (bot-posts-via-webhook detection for classifier) — no outbound webhook calls exist

- **Method:** `manual` (not an execution verdict).
- **Action:** Replay fixture webhook-authored and regular bot messages in local classifier.
- **Expected:** Webhook bot-post classification correct; no outbound webhook request introduced.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

## 7. Config / env

### s7-01: Config / env catalogue

- **Method:** `manual` (not an execution verdict).
- **Action:** With approved synthetic configs, attempt an attributed dashboard write to a wired hot key (TWO_RAID_JOIN_THRESHOLD), a cold key (TWO_FEED_POLL_SECONDS), an env_only key and an unknown key. Observe the hot change after the 15s poll; cold and hot-but-unwired changes wait for an approved restart. Never display secret values.
- **Expected:** Catalogued hot and cold keys are writable, versioned and audited; wired hot keys apply live, cold and hot-but-unwired keys are marked next-restart and do not apply live. Env_only and unknown writes are refused before persistence/audit; env-only values remain private.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

## 8. Observable behaviours

### s8-01: Structured JSON logs (`{ts,level,msg,…}`, `debug/info/error`, `LOG_LEVEL`) — ~208 line names: lifecycle, funnel, onboarding, audit mirror, alerts, automod/moderation, rota, REST/backoff

- **Method:** `manual` (not an execution verdict).
- **Action:** Capture structured logs for startup, one command, one denied request and mock retry with each approved LOG_LEVEL.
- **Expected:** JSON has timestamp/level/message fields; lifecycle/funnel/audit/backoff equivalents observable; no secrets or message content.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s8-02: Audit channels (audit/voice/moderation, metadata-only `key=value`, 300/2000 truncation, audit-event identity prefix) — arches fallback voice/moderation → audit

- **Method:** `manual` (not an execution verdict).
- **Action:** Deliver a fixture audit message with long metadata; use local fixture for fallback/truncation, attach voice channel portion from TOG-10119.
- **Expected:** Metadata-only key=value, 300/2000 truncation, event identity prefix; voice/moderation fallback to audit; no mentions.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s8-03: Audit kill switch (`audit-switch --halt/--resume/--status`) — presence = halt

- **Method:** `waived` (not an execution verdict).
- **Action:** Run audit-switch halt/status/resume against local authorized test-container fixture only, never staging/production DB.
- **Expected:** Switch presence halts, status reports accurately, resume clears only the fixture switch and pending audit resumes safely.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.

### s8-04: Rate limits: staging-verifier 3 retries ≤30s; REST pacing §6; internal buckets §6; raid-watch 5 joins/60s + 900s cooldown; ticket cooldown 300s; containment per-executor cooldown — alert cooldowns + sweep/lease bounds (audit 5-min lease, 1-h recheck, claim ≤25)

- **Method:** `manual` (not an execution verdict).
- **Action:** Use mock clocks/REST for verifier retries, REST/internal buckets, raid burst, ticket/containment cooldown and audit sweep leases.
- **Expected:** 3 verifier retries ≤30s; REST/internal §6 bounds; 5 joins/60s +900s raid cooldown; ticket 300s; executor cooldown; 5m audit lease,1h recheck,≤25 claims.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s8-05: Automod (staging-only unless live-approved; `dryRun` unless `ENFORCE=1`; 6 filters; sanctions `1:delete,2:warn,3:timeout:600`; target-protection before delete) — bad-word NFKC matching, invite/link checks, `bat/cmd/…` attachment blocklist

- **Method:** `manual` (not an execution verdict).
- **Action:** In disposable approved staging channel with dryRun enabled, send one benign fixture per six configured filters; use local mocks for enforced ladder and protected-target deletion.
- **Expected:** NFKC words/invite/link/attachment checks classify; default no sanctions; ENFORCE=1 requires separate live approval, ladder delete/warn/600s timeout and protection tested locally.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s8-06: Anti-nuke (default alerts-only; weights kick/ban/webhook=1, channel/role delete=3; quarantine strips dangerous perms below bot hierarchy; join-risk flag-only) — `containment_alert` always logged; `**Join burst**` raid alerts, no DMs/pings

- **Method:** `manual` (not an execution verdict).
- **Action:** Replay kick/ban/webhook/channel-delete/role-delete threshold fixtures locally with alerts-only setting; one benign staging join-burst fixture only if authorized.
- **Expected:** Weights 1/3, containment_alert always present; Join burst no ping/DM; quarantine hierarchy tested locally; never kicks join-risk member.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s8-07: Onboarding modes (`legacy` catalog + hub routing / `session` roleless two-pick LIVE / anchor Sunday-Squad one-message) — no-DM, idempotent `onboarding_prompted`, dry-run aware

- **Method:** `manual` (not an execution verdict).
- **Action:** Under separately approved mode configuration, join and gate-clear disposable member for legacy/session/anchor; repeat through mock fixture.
- **Expected:** Correct hub/two-pick/Sunday-Squad one-message prompt; no DM, idempotent onboarding_prompted and dry-run awareness.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s8-08: Operator scripts (82 files) — schedule/backfill/migration one-shots → **[TOG-9882](/TOG/issues/TOG-9882)** (MEE6 XP, rewards, history backfill, dedupe, message-milestone scan, join capture); backup/restore/snapshot → [TOG-9881](/TOG/issues/TOG-9881); read-only reports (funnel, gate, attribution, roster, dashboard, scorecard, presence-trend, growth-review, raid-list) → **DROP** as runtime (query Postgres/`web_v1` on demand); staging provision/verify/reset + e2e harness → **DROP** (replaced by mock-discord acceptance + **S6** cutover plan); `reconcile` → **DROP** (absent/broken upstream); guild-config snapshot/restore → [TOG-9881](/TOG/issues/TOG-9881); temp-voice runtime shipped → [TOG-10091](/TOG/issues/TOG-10091) and voice acceptance [TOG-10119](/TOG/issues/TOG-10119); voice/leave-gap integrity reports are retained on [TOG-11152](/TOG/issues/TOG-11152), unlike the other runtime report drops

- **Method:** `waived` (not an execution verdict).
- **Action:** Attach TOG-9882 import/backfill/dedupe and TOG-9881 backup/restore/config fixture receipts; consume full temp-voice runtime acceptance from TOG-10119 and read-only voice/leave-gap integrity receipts from TOG-11152. Do not run dropped scripts.
- **Expected:** Ported one-shots preserve fixture totals/reward/history idempotency; sealed restore validated in test DB; ordinary report runtime/rota stay dropped. Temp-voice runtime and voice/leave-gap integrity reports remain required, not waived by the historical shape-check.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.
