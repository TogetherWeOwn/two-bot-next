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

JSON is authoritative; each `parity.section` + `parity.row` copies every source table cell except Map. Repeated ClientReady, attendance and member-event surfaces remain separate. Only DROP-prefixed Maps without an S/B/NEW slice or TOG owner are excluded; mixed mapped/DROP rows remain covered in either order, including mapped replacement work. §7 is prose: one explicit catalogue entry covers env_only/cold/hot. Adding a table there also requires entries. §12 copies every cell of its addition and Redirect tables except the disposition, under the same DROP rule. §13 covers each `ported`/`carded`/`gap` ledger row (commit link, area, change and status); `dropped` rows, including every history-rewrite replay, are excluded because their features stay mapped in §§1–8. Each §12/§13 entry lists `owner`: exactly the TOG cards its disposition names, in order (a `ported` row without a card names its owning slice); a changed status, row or owner card is stale until the entry is updated.

Run `PYTHONDONTWRITEBYTECODE=1 python3 scripts/check_soak_checklist.py` and `PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_soak_checklist.py' -v`. CI runs both offline before Cargo. After editing JSON, regenerate Markdown with `PYTHONDONTWRITEBYTECODE=1 python3 scripts/check_soak_checklist.py --render > docs/soak-checklist.md`. Source drift, missing/stale/duplicate rows, missing or stale owner cards, empty steps/evidence, invalid statuses, waivers missing reason or approver, missing automated commands, automated Cargo commands naming an unknown `-p` package, a missing test target or no matching test (read offline from source, never run), missing voice links or Markdown drift fail the gate.

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
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s4-15: systemd `two-bot-guild-config-backup.timer` (sealed Discord config snapshot) — daily 04:31 UTC

- **Method:** `waived` (not an execution verdict).
- **Action:** Use disposable guild-config snapshot fixture at simulated daily 04:31 UTC under TOG-9881.
- **Expected:** Sealed snapshot timer equivalent invokes once; allowlisted metadata only, no secret/log leakage.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s4-16: systemd `two-bot-restore-drill.timer` — monthly

- **Method:** `waived` (not an execution verdict).
- **Action:** Attach TOG-9881 monthly restore-drill receipt from agent-testdb/CI services only.
- **Expected:** Restore verifies integrity and fixture row counts in isolated test DB; never replaces staging or production.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

## 5. DB tables & queries

### s5-01: `events`, `members`, `invite_snapshots` — append-only funnel log + member projection + invite snapshots

- **Method:** `waived` (not an execution verdict).
- **Action:** Replay synthetic joins/messages/leaves and invite snapshots using the authorized test-container funnel store fixture.
- **Expected:** Append-only events and member/invite projections are idempotent and guild-scoped.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s5-02: `internal_nonces`, `internal_clock_high_water`, `internal_idempotency`, `internal_action_log`, `internal_discord_events` — website-callback replay guard (plus its F8 clock high-water mark), idempotency, audit, dedupe

- **Method:** `waived` (not an execution verdict).
- **Action:** Send signed synthetic actions, replay nonce/idempotency key, and repeat Discord callback in CI store fixtures.
- **Expected:** Durable nonce refusal, stable replay response and one action/event audit across restart.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s5-03: `web_contract_meta`, `guild_counters`, `rank_ladder`, `rank_snapshots`, `member_ranks`, `scheduled_events` — website read contract

- **Method:** `waived` (not an execution verdict).
- **Action:** Load website contract fixture on agent-testdb/CI and query every web_v1 read contract view there.
- **Expected:** Contract metadata, counters, ranks and scheduled events agree atomically; no staging database queried.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s5-04: `presence_probe` — hourly presence series

- **Method:** `waived` (not an execution verdict).
- **Action:** Run presence-series fixture ticks at 1h in authorized test DB.
- **Expected:** One bounded presence row per tick with correct series ordering.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s5-05: `counter_snapshots`, `member_exclusions` — live-count audit history, raid exclusions

- **Method:** `waived` (not an execution verdict).
- **Action:** Run counter snapshot and raid-exclusion fixtures in authorized test DB.
- **Expected:** Excluded members do not inflate live counts; history preserves prior snapshots.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s5-06: `invite_campaigns` (`go.two.gg/<slug>`) — tracked short links

- **Method:** `waived` (not an execution verdict).
- **Action:** Use redirect campaign fixture slug against local fixture store, then unknown slug.
- **Expected:** Known slug records invite_click once and redirects; unknown slug uses fallback without invented attribution.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s5-07: `member_levels`, `xp_cooldowns` (60s msg/voice), `xp_awards`, `level_role_rewards`, `level_import_runs` — leveling, MEE6-compat (5 XP/min voice)

- **Method:** `waived` (not an execution verdict).
- **Action:** Use synthetic message XP cooldowns and approved MEE6/reward import fixture in authorized test DB; attach the voice XP portion from TOG-10119 rather than duplicating its voice scenario.
- **Expected:** 60s cooldown, 5 XP/min voice compatibility, reward idempotency and import-run ledger match fixture expectations.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s5-08: `moderation_warnings`, `moderation_scheduled_unbans`, `moderation_audit`, `moderation_lockdowns`, `moderation_idempotency` — moderation ledger, tempban queue, lockdown state, claim table

- **Method:** `waived` (not an execution verdict).
- **Action:** Run synthetic warnings/tempbans/lockdowns and duplicate moderation claim fixtures on test DB.
- **Expected:** Ledger, expiry queue, prior overwrite and idempotency claim persist across replay.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s5-09: `operational_audit_log` (states `none/pending/delivering/delivered/quarantined`, 5-min lease, hourly mirror recheck) — metadata-only Discord-event parity audit

- **Method:** `waived` (not an execution verdict).
- **Action:** Run audit-state fixture transitions with 5m claim expiry and 1h mirror recheck in test DB.
- **Expected:** Only valid none/pending/delivering/delivered/quarantined transitions; expired claim recoverable and duplicates blocked.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s5-10: `automod_violations`, `automod_processed_messages` — sanctions ladder, gateway-retry dedupe

- **Method:** `waived` (not an execution verdict).
- **Action:** Run accepted/rejected/edit/retry automod fixtures on test DB.
- **Expected:** One processed-message claim; violation ladder advances only for distinct qualifying violations.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s5-11: `tickets`, `ticket_transcripts` — ticket lifecycle + transcripts

- **Method:** `waived` (not an execution verdict).
- **Action:** Run open/claim/close/recovery/transcript-expiry fixture in test DB.
- **Expected:** Lifecycle and transcript retention persist through restart; duplicate open does not create second ticket.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s5-12: `containment_events`, `containment_incidents`, `join_risk_flags` — anti-nuke signals/incidents, join-risk (flag-only, never kicks)

- **Method:** `waived` (not an execution verdict).
- **Action:** Run alerts-only anti-nuke and join-burst fixtures on test DB.
- **Expected:** Signals/incidents/risk flags persist; join-risk never kicks.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s5-13: `automation_commands`, `scheduled_messages`, `sticky_messages`, `automation_audit_log` — custom commands, schedule queue, stickies

- **Method:** `waived` (not an execution verdict).
- **Action:** Run create/remove/custom-trigger/schedule/sticky fixtures on test DB.
- **Expected:** Definitions, due queue, last sticky and audit persist with guild isolation and idempotent claims.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s5-14: `self_role_audit`, `self_role_panel_claims` — self-role audit + claim leases

- **Method:** `waived` (not an execution verdict).
- **Action:** Run role add/remove/replace and claim-renewal fixtures on test DB.
- **Expected:** Claim lease prevents duplicate role action; metadata-only audit persists.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s5-15: `community_facts`, `community_stream_heartbeats`, `community_scorecard_runs/alerts` — scorecard facts/runs/alerts

- **Method:** `waived` (not an execution verdict).
- **Action:** Run fact/heartbeat/weekly-scorecard fixtures on test DB, excluding dropped rota extensions.
- **Expected:** Facts and stream liveness yield one weekly run/alert; dropped rota tables are not required.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s5-16: `event_rsvps`, `lfg_posts/roles/signups`, `feed_relays`, `feed_deliveries`, `announcements_audit_log` — RSVP, LFG, feed relay + delivery claims

- **Method:** `waived` (not an execution verdict).
- **Action:** Run RSVP replacement, LFG capacity, feed dedupe and announcement audit fixtures on test DB.
- **Expected:** Latest RSVP, bounded signups, single delivery and idempotent announcement audit persist.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s5-17: `guild_settings`, `guild_settings_audit` — dashboard-writable hot settings

- **Method:** `waived` (not an execution verdict).
- **Action:** Run permitted/forbidden setting changes and version-poll fixtures on test DB.
- **Expected:** Guild setting version and audit advance together for attributed hot/cold writes; wired hot changes apply live, cold changes require restart; env_only and unknown writes are refused before persistence/audit.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s5-18: `audit_kill_switch` (presence = halt; audit fail-open, rota fail-closed) — audit pipeline kill switch

- **Method:** `waived` (not an execution verdict).
- **Action:** Insert/remove the kill-switch fixture only in authorized test DB; simulate pending audit delivery.
- **Expected:** Presence halts the audit pipeline; absence permits resume; failure mode matches audit policy.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

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
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

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
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

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
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s8-04: Rate limits: staging-verifier 3 retries ≤30s; REST pacing §6; internal buckets §6; raid-watch 5 joins/60s + 900s cooldown; ticket cooldown 300s; containment per-executor cooldown — alert cooldowns + sweep/lease bounds (audit 5-min lease, 1-h recheck, claim ≤25)

- **Method:** `manual` (not an execution verdict).
- **Action:** Use mock clocks/REST for verifier retries, REST/internal buckets, raid burst, ticket/containment cooldown and audit sweep leases.
- **Expected:** 3 verifier retries ≤30s; REST/internal §6 bounds; 5 joins/60s +900s raid cooldown; ticket 300s; executor cooldown; 5m audit lease,1h recheck,≤25 claims.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.

### s8-05: Automod (staging-only unless live-approved; `dryRun` unless `ENFORCE=1`; 6 filters; sanctions `1:delete,2:warn,3:timeout:600`; target-protection before delete) — bad-word NFKC matching, invite/link checks, `bat/cmd/…` attachment blocklist; whitespace edge accepted ([TOG-12582](/TOG/issues/TOG-12582)): legacy JS `\s` treats U+FEFF as blank and U+0085 as non-blank while Rust `split_whitespace` does the opposite, but both collapse to empty-vs-nonempty only for these two codepoints and all repeat-expiry blanks stay agreed (NEL: [TOG-10052](/TOG/issues/TOG-10052), format chars: [TOG-10048](/TOG/issues/TOG-10048))

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

### s8-08: Operator scripts (82 files) — schedule/backfill/migration one-shots → **[TOG-9882](/TOG/issues/TOG-9882)** (MEE6 XP, rewards, history backfill, dedupe, message-milestone scan, join capture); backup/restore/snapshot → [TOG-9881](/TOG/issues/TOG-9881); read-only reports (funnel, gate, attribution, roster, dashboard, scorecard, presence-trend, growth-review) → **DROP** as runtime (query Postgres/`web_v1` on demand); manual raid-list/raid-remove → [TOG-10867](/TOG/issues/TOG-10867) ([runbook](raid-response.md)), not scheduled runtime; staging provision/verify/reset + e2e harness → **DROP** (replaced by mock-discord acceptance + **S6** cutover plan); `reconcile` → **DROP** (absent/broken upstream); guild-config snapshot/restore → [TOG-9881](/TOG/issues/TOG-9881); temp-voice runtime shipped → [TOG-10091](/TOG/issues/TOG-10091) and voice acceptance [TOG-10119](/TOG/issues/TOG-10119); voice/leave-gap integrity reports are retained on [TOG-11152](/TOG/issues/TOG-11152), unlike the other runtime report drops

- **Method:** `waived` (not an execution verdict).
- **Action:** Attach TOG-9882 import/backfill/dedupe and TOG-9881 backup/restore/config fixture receipts; attach [TOG-10867](/TOG/issues/TOG-10867) raid-removal loopback/CLI and disposable raid-list DB fixture receipts; consume full temp-voice runtime acceptance from TOG-10119 and read-only voice/leave-gap integrity receipts from TOG-11152. Do not run dropped scripts or live removals.
- **Expected:** Ported one-shots preserve fixture totals/reward/history idempotency; sealed restore validated in test DB; manual raid tools default to dry-run, fence live guild access, protect staff on every retry and audit each reached target; ordinary report runtime/reconcile/rota stay dropped. Temp-voice runtime and voice/leave-gap integrity reports remain required, not waived by the historical shape-check.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

## 12. Baseline additions and corrections

### s12-01: Temporary voice creator channels, control panels and `/voice` commands

- **Method:** `manual` (not an execution verdict).
- **Action:** Consume the TOG-10119 temp-voice scenario: join the creator channel, use the room control panel and each `/voice` subcommand on the created room, then leave so the sweep removes it.
- **Expected:** Creator, panel and `/voice` routing work end to end in the allowlisted staging guild; config, tables, panel routing and sweeps match TOG-10091. The “never shipped” drop stays withdrawn; per-commit legs are the TOG-10093/TOG-10099/TOG-10101 s13 rows.
- **Evidence:** Link the exact-SHA voice evidence table and verdict from [TOG-10119](/TOG/issues/TOG-10119), including row/scenario ID and time window. Missing/failing evidence leaves this row NEEDS WORK; do not infer PASS from the reference.
- **Owner:** [TOG-10093](/TOG/issues/TOG-10093), [TOG-10099](/TOG/issues/TOG-10099), [TOG-10101](/TOG/issues/TOG-10101), [TOG-10091](/TOG/issues/TOG-10091), [TOG-10119](/TOG/issues/TOG-10119)
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s12-02: Website `event.read`, making the internal-action catalog **19**, not 18

- **Method:** `waived` (not an execution verdict).
- **Action:** Use TOG-10603’s receiver and TOG-10862’s executor fixtures to send signed `event.read` calls for a mapped and an unmapped event key, then replay one signature.
- **Expected:** The catalog lists 19 internal actions; the mapped key returns its Discord event, the unmapped key refuses without Discord writes and the replay is refused. Hashed dedupe is not accepted as a key→ID accessor.
- **Evidence:** Attach the owning slice’s exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no live fault injection, staging/production SQL or credentials.
- **Owner:** [TOG-10603](/TOG/issues/TOG-10603), [TOG-10862](/TOG/issues/TOG-10862)
- **Reason:** Proposed staging-execution waiver: this failure, timing or signed-call path is reproducible only with local mock fixtures under the safety contract (no live fault injection, clock change or credential handling); the owning slice’s exact-head fixture receipt substitutes for a deployed effect. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s12-03: Chronological membership and ordered/idempotent voice capture

- **Method:** `waived` (not an execution verdict).
- **Action:** Run TOG-11149’s gateway/member chronology contract suite on test containers: out-of-order join/leave, same-time ties, replay, channel-scoped voice dedupe, reconnect/session drop, server leave and unknown start.
- **Expected:** Membership projections follow event chronology, not arrival order; voice frames dedupe per channel and close on drop/leave; unknown starts stay unknown. Store coverage alone does not pass this row.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Owner:** [TOG-11149](/TOG/issues/TOG-11149)
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s12-04: Backup v4 coverage and input/publication/restore safety

- **Method:** `waived` (not an execution verdict).
- **Action:** Use TOG-11142/TOG-11186 test-container backup fixtures: dump and restore every bot-owned table, then verify a dump with malformed manifest/end records.
- **Expected:** All bot-owned tables round-trip with owned sequences reset; malformed non-table records fail verification. Completed-output publication and stable-ID restore (s13-90ab4b7, s13-3dc9720) stay green; none of this is deployed nightly-timer evidence.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Owner:** [TOG-11142](/TOG/issues/TOG-11142), [TOG-11186](/TOG/issues/TOG-11186)
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s12-05: Session selection, hidden fallback, rank and self-role bounds

- **Method:** `manual` (not an execution verdict).
- **Action:** On the staging fixture guild, complete onboarding session selection with a disposable member, run `/rank member:` for a fixture member without XP, and attempt over-limit self-role panel configuration through the normal config path; use the onboarding/leveling fixtures for payloads Discord cannot send.
- **Expected:** Valid picks route once in catalog order and invisible fallbacks are withheld (TOG-11180/TOG-10278); a member without XP shows Unranked inside the configured guild fence (TOG-11147/TOG-10343); panels over 20 reactions, 100-character custom_id or 80-character label are refused (TOG-10087/TOG-10292).
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.
- **Owner:** [TOG-11180](/TOG/issues/TOG-11180), [TOG-10278](/TOG/issues/TOG-10278), [TOG-11147](/TOG/issues/TOG-11147), [TOG-10343](/TOG/issues/TOG-10343), [TOG-10087](/TOG/issues/TOG-10087), [TOG-10292](/TOG/issues/TOG-10292)

### s12-06: Presence, scorecard and mirror safety

- **Method:** `waived` (not an execution verdict).
- **Action:** Use the owning slices’ test-container and mock-REST fixtures: an oversized roster scan, a failed Monday scorecard run, a stopped collector with a pending cycle and malformed scheduled-event responses.
- **Expected:** Roster scans stop at the row/page cap and persist truncation/backoff (TOG-11146); scorecards retry only inside the Monday window (TOG-11145); stopped-collector results are discarded and malformed elements never replace the mirror (TOG-11181).
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Owner:** [TOG-11146](/TOG/issues/TOG-11146), [TOG-11145](/TOG/issues/TOG-11145), [TOG-11181](/TOG/issues/TOG-11181)
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s12-07: Feed polling and announcement acknowledgement

- **Method:** `manual` (not an execution verdict).
- **Action:** Poll one approved public feed into a disposable announcement channel and invoke the announcement and LFG commands; take redirect, timeout, per-item, Atom, crashed-claim and over-cap legs from the TOG-11022 s13 fixture rows.
- **Expected:** The managed poller delivers the approved item once (TOG-11022); commands acknowledge/defer before service I/O and the reserved LFG leave-action role key is refused (TOG-10260). Feed CRUD alone does not pass this row.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.
- **Owner:** [TOG-11022](/TOG/issues/TOG-11022), [TOG-10260](/TOG/issues/TOG-10260)

### s12-08: Runtime audit, shutdown release and activation

- **Method:** `manual` (not an execution verdict).
- **Action:** From the staging boot log and one controlled redeploy, capture the activation allowlist decision, requested intents and runtime audit records; attach TOG-10869’s shutdown-preflight fixture output.
- **Expected:** Boot activates only allowlisted identity/capabilities with intents matching enabled capabilities (TOG-11140); audit records boot/timer events (TOG-10346); preflight reports truncation only when rows were cut and names stranded unban claims with release steps (TOG-10869). A legacy staging allowlist never authorizes activation.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.
- **Owner:** [TOG-10346](/TOG/issues/TOG-10346), [TOG-10869](/TOG/issues/TOG-10869), [TOG-11140](/TOG/issues/TOG-11140)

### s12-09: New read-only analytical reports

- **Method:** `waived` (not an execution verdict).
- **Action:** Run TOG-11152’s voice reconcile and member-leave-gap reports against an authorized test-container fixture.
- **Expected:** Reports are read-only, use the events-write heartbeat, retain and exclude unknown starts and compare timestamps by instant, with no auto-repair. Funnel/attribution/anomaly/dashboard runtimes stay dropped under §9.3.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Owner:** [TOG-11152](/TOG/issues/TOG-11152)
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s12-10: Campaign miss cache: null misses cached; default negative TTL ≤2s, hit TTL 30s

- **Method:** `waived` (not an execution verdict).
- **Action:** With TOG-11153’s local redirect-store fixture, look up one unknown slug repeatedly inside and after the negative TTL, then register it and look it up again.
- **Expected:** Repeated misses inside the ≤2s negative TTL cause one store lookup; the slug resolves after the TTL; hits cache for 30s. No long-lived negative entry.
- **Evidence:** Attach the owning slice’s exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no live fault injection, staging/production SQL or credentials.
- **Owner:** [TOG-11153](/TOG/issues/TOG-11153)
- **Reason:** Proposed staging-execution waiver: this failure, timing or signed-call path is reproducible only with local mock fixtures under the safety contract (no live fault injection, clock change or credential handling); the owning slice’s exact-head fixture receipt substitutes for a deployed effect. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s12-11: Trusted Node proxy chain

- **Method:** `manual` (not an execution verdict).
- **Action:** Send one request with a spoofed `X-Forwarded-For` to the approved staging redirect origin for a disposable slug; never production go.two.gg.
- **Expected:** Caller identity and the throttle bucket come from `CF-Connecting-IP`; the spoofed header changes neither admission nor the logged caller. Node hop-walking stays dropped.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.
- **Owner:** [TOG-11153](/TOG/issues/TOG-11153)

### s12-12: Reserved `healthz` campaign input

- **Method:** `manual` (not an execution verdict).
- **Action:** Attempt campaign slug `healthz` through TOG-11183’s campaign input fixture, then request `/healthz` on the approved staging redirect origin.
- **Expected:** The reserved slug is refused at input; the `/healthz` probe still answers. No slug-management UI exists.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.
- **Owner:** [TOG-11183](/TOG/issues/TOG-11183)

### s12-13: Listener port and fallback validation

- **Method:** `waived` (not an execution verdict).
- **Action:** Load TOG-11183’s local fixture with an invalid fallback code and malformed snapshot rows.
- **Expected:** Invalid fallback/snapshot inputs are rejected before any redirect effect; the Worker has no listener-port setting.
- **Evidence:** Attach the owning slice’s exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no live fault injection, staging/production SQL or credentials.
- **Owner:** [TOG-11183](/TOG/issues/TOG-11183)
- **Reason:** Proposed staging-execution waiver: this failure, timing or signed-call path is reproducible only with local mock fixtures under the safety contract (no live fault injection, clock change or credential handling); the owning slice’s exact-head fixture receipt substitutes for a deployed effect. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s12-14: Lookup failure `slug` and bounded `errorClass`

- **Method:** `waived` (not an execution verdict).
- **Action:** Force a store lookup failure for a known slug in TOG-11183’s local redirect fixture.
- **Expected:** The log names the slug and a bounded `errorClass` from the fixed classifier; no credentials or raw error text.
- **Evidence:** Attach the owning slice’s exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no live fault injection, staging/production SQL or credentials.
- **Owner:** [TOG-11183](/TOG/issues/TOG-11183)
- **Reason:** Proposed staging-execution waiver: this failure, timing or signed-call path is reproducible only with local mock fixtures under the safety contract (no live fault injection, clock change or credential handling); the owning slice’s exact-head fixture receipt substitutes for a deployed effect. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s12-15: Staging rollout gate receipt: intended Worker version at 100% traffic, `/readyz` 200 with compiled revision/build ID, allowlisted receipt fields, plus Neon `channel_binding` acceptance

- **Method:** `manual` (not an execution verdict).
- **Action:** Attach the staging rollout-gate receipt for the deployed soak build: require the intended Worker version at 100% traffic, `/readyz` 200 with the exact compiled revision/build ID and only allowlisted receipt fields, then a control-plane re-check; a 503 or a previous-instance response never passes.
- **Expected:** The deployed soak build serves the intended Worker version at 100% traffic with `/readyz` 200 carrying the compiled revision/build ID; the receipt carries only allowlisted fields. The gate refuses stale-instance responses.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.
- **Owner:** [TOG-9699](/TOG/issues/TOG-9699)

### s12-16: Bootstrap migration accepts the `Security` release-notes section in `migrate-release-notes`

- **Method:** `waived` (not an execution verdict).
- **Action:** Run the `migrate-release-notes` bootstrap fixture with a hand-written `Security` section in Unreleased: confirm the migration preserves the Security tail, removes Unreleased, and stays idempotent; confirm any other section still fails closed.
- **Expected:** `Security` migrates alongside Added/Fixed/Changed/Notes; unknown sections fail closed; later releases do not repeat the bootstrap tail.
- **Evidence:** Attach the owning slice’s exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no live fault injection, staging/production SQL or credentials.
- **Owner:** [TOG-12801](/TOG/issues/TOG-12801)
- **Reason:** Proposed staging-execution waiver: this failure, timing or signed-call path is reproducible only with local mock fixtures under the safety contract (no live fault injection, clock change or credential handling); the owning slice’s exact-head fixture receipt substitutes for a deployed effect. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s12-17: Guild room access-controls store (0227 migration, whole-row upsert with fail-closed validation)

- **Method:** `waived` (not an execution verdict).
- **Action:** Run the #360 store round-trip fixture on authorized test containers: defaults with no row, empty role list surviving reload, per-guild isolation, lifting restrictions, adapter refusals and raw-SQL CHECK rejection of unknown commands and zero roles.
- **Expected:** Whole-row upsert persists and reloads validated controls per guild; unknown commands and zero roles fail closed on save and load.
- **Evidence:** Attach the owning slice's exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no live fault injection, staging/production SQL or credentials.
- **Owner:** [TOG-10119](/TOG/issues/TOG-10119), [TOG-13123](/TOG/issues/TOG-13123)
- **Reason:** Proposed staging-execution waiver: this store, gate-diagnostic, failure-path, test-only, standards-only or CI-only slice is reproducible only with local mock fixtures or isolated test containers under the safety contract (no live fault injection, clock change, staging/production SQL or credential handling); the owning slice's exact-head fixture receipt substitutes for a deployed effect. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s12-18: Durable join-risk event claim store over the existing flags table (unwired, no new migration)

- **Method:** `waived` (not an execution verdict).
- **Action:** Run the #362 join-risk claim fixture on authorized test containers: 10-way concurrent exactly-once claims, serialized burst scoring with threshold-equality bonus, old-account evidence, cross-guild isolation, out-of-window exclusion, bulk-suppression persistence, duplicate replay and the legacy-0015 upgrade shape.
- **Expected:** Event-ID dedupe returns Duplicate on replay with never a second alert; per-guild advisory locking serializes bursts; legacy IDs stay claimed with rows untouched.
- **Evidence:** Attach the owning slice's exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no live fault injection, staging/production SQL or credentials.
- **Owner:** [TOG-13123](/TOG/issues/TOG-13123)
- **Reason:** Proposed staging-execution waiver: this store, gate-diagnostic, failure-path, test-only, standards-only or CI-only slice is reproducible only with local mock fixtures or isolated test containers under the safety contract (no live fault injection, clock change, staging/production SQL or credential handling); the owning slice's exact-head fixture receipt substitutes for a deployed effect. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s12-19: Staging rollout gate timeout report (last observed stage)

- **Method:** `waived` (not an execution verdict).
- **Action:** Run the #367 gate timeout fixture: no-new-rollout, unconverged rollout with instance counts, converged rollout with readyz/components/identity states, hostile component names and unparseable bodies.
- **Expected:** Timeout output names the last observed stage from the fixed vocabulary; hostile values are dropped and unparseable bodies read body=unreadable; the gate never loosens.
- **Evidence:** Attach the owning slice's exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no live fault injection, staging/production SQL or credentials.
- **Owner:** [TOG-13123](/TOG/issues/TOG-13123)
- **Reason:** Proposed staging-execution waiver: this store, gate-diagnostic, failure-path, test-only, standards-only or CI-only slice is reproducible only with local mock fixtures or isolated test containers under the safety contract (no live fault injection, clock change, staging/production SQL or credential handling); the owning slice's exact-head fixture receipt substitutes for a deployed effect. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s12-20: `/access` admin command for guild voice room controls with live-actor refresh

- **Method:** `manual` (not an execution verdict).
- **Action:** As an admin invoke /access show, creation, role, restrict and unrestrict in the disposable guild, then repeat as a non-admin; exercise unknown commands, unrestricting an unrestricted command and malformed shapes.
- **Expected:** Admin changes persist, show reflects them and the live actor gates creation without a restart; non-admin use is refused; bad input is answered without changing anything.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.
- **Owner:** [TOG-10119](/TOG/issues/TOG-10119), [TOG-13123](/TOG/issues/TOG-13123)
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s12-21: Staging-only default-dark ingress for the internal-actions receiver

- **Method:** `waived` (not an execution verdict).
- **Action:** Run the #370 ingress fixture: route absent without both the staging-only Worker var and the exact operator secret; header allowlist, body bounds, per-IP bucket and in-flight cap enforced; only the receiver envelope leaves the relay.
- **Expected:** The route stays absent unless both staging-only var and exact secret are present; production has no var, route or secret; no container contact on refusal.
- **Evidence:** Attach the owning slice's exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no live fault injection, staging/production SQL or credentials.
- **Owner:** [TOG-13123](/TOG/issues/TOG-13123)
- **Reason:** Proposed staging-execution waiver: this store, gate-diagnostic, failure-path, test-only, standards-only or CI-only slice is reproducible only with local mock fixtures or isolated test containers under the safety contract (no live fault injection, clock change, staging/production SQL or credential handling); the owning slice's exact-head fixture receipt substitutes for a deployed effect. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s12-22: Gateway checkpoint advisory-lock test serialization (tests only)

- **Method:** `waived` (not an execution verdict).
- **Action:** Run the #371 gateway lock-fence fixture: exclusive lock holders run alone while shared TestDb lifetimes hold the fence shared; sibling checkpoint commits never queue behind a held gateway key.
- **Expected:** No checkpoint wait exceeds its IO bound due to test-parallel lock contention; no timeout raised and no step skipped; production code untouched.
- **Evidence:** Attach the owning slice's exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no live fault injection, staging/production SQL or credentials.
- **Owner:** [TOG-13123](/TOG/issues/TOG-13123)
- **Reason:** Proposed staging-execution waiver: this store, gate-diagnostic, failure-path, test-only, standards-only or CI-only slice is reproducible only with local mock fixtures or isolated test containers under the safety contract (no live fault injection, clock change, staging/production SQL or credential handling); the owning slice's exact-head fixture receipt substitutes for a deployed effect. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s12-23: `/logging` admin command for guild voice notice level, channel and mention role

- **Method:** `manual` (not an execution verdict).
- **Action:** As an admin invoke /logging show and set notice level, channel and mention role in the disposable guild, then repeat as a non-admin; exercise bad input and store-failure fixtures.
- **Expected:** Admin changes persist, show reflects them and the live actor applies them without a restart; non-admin use is refused; failures change nothing and say so.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.
- **Owner:** [TOG-10119](/TOG/issues/TOG-10119), [TOG-13123](/TOG/issues/TOG-13123)
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s12-24: `/setup` missing-permission surfacing with category-override attribution and health line

- **Method:** `manual` (not an execution verdict).
- **Action:** Invoke /setup in the disposable guild with clean permissions, then with an approved category-override and channel-override gap; repeat with incomplete cache data.
- **Expected:** /setup lists each missing permission once with the causing category or channel named by mention only; clean and incomplete-data guilds report no false failure.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.
- **Owner:** [TOG-10119](/TOG/issues/TOG-10119), [TOG-13123](/TOG/issues/TOG-13123)
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s12-25: Gateway start-failure class on `/readyz` plus unconverged-rollout warm-up read

- **Method:** `waived` (not an execution verdict).
- **Action:** Run the #374 failure-class fixture: every fallible gateway step maps to its fixed-vocabulary class, the 503 readyz body carries phase and class with no error text, the linger serves it until shutdown cuts it short, and the gate warm-up reads an unconverged rollout only from the expected Worker version and build identity.
- **Expected:** Failure class is an enum token, never text, URL or secret; stale Worker or image answers are ignored; no probe runs before a new rollout exists.
- **Evidence:** Attach the owning slice's exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no live fault injection, staging/production SQL or credentials.
- **Owner:** [TOG-13123](/TOG/issues/TOG-13123)
- **Reason:** Proposed staging-execution waiver: this store, gate-diagnostic, failure-path, test-only, standards-only or CI-only slice is reproducible only with local mock fixtures or isolated test containers under the safety contract (no live fault injection, clock change, staging/production SQL or credential handling); the owning slice's exact-head fixture receipt substitutes for a deployed effect. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s12-26: Repo standards (PR template, issue forms, public-safe pr-lint)

- **Method:** `waived` (not an execution verdict).
- **Action:** Run the #375 lint fixture: standards-only headers, template sections and public-safe body rules pass while private URLs, tracker IDs and secrets fail.
- **Expected:** Standards-only change with no runtime, staging, deploy or CI-selection behavior; the lint contract holds.
- **Evidence:** Attach the owning slice's exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no live fault injection, staging/production SQL or credentials.
- **Owner:** [TOG-13123](/TOG/issues/TOG-13123)
- **Reason:** Proposed staging-execution waiver: this store, gate-diagnostic, failure-path, test-only, standards-only or CI-only slice is reproducible only with local mock fixtures or isolated test containers under the safety contract (no live fault injection, clock change, staging/production SQL or credential handling); the owning slice's exact-head fixture receipt substitutes for a deployed effect. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s12-27: Required-check fail-closed job selection with pinned contract

- **Method:** `waived` (not an execution verdict).
- **Action:** Run the #376 selector fixture: job-selection failure fails the required check closed and the pinned contract matches the workflow.
- **Expected:** No silent green on selection failure; contract drift fails the gate.
- **Evidence:** Attach the owning slice's exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no live fault injection, staging/production SQL or credentials.
- **Owner:** [TOG-13123](/TOG/issues/TOG-13123)
- **Reason:** Proposed staging-execution waiver: this store, gate-diagnostic, failure-path, test-only, standards-only or CI-only slice is reproducible only with local mock fixtures or isolated test containers under the safety contract (no live fault injection, clock change, staging/production SQL or credential handling); the owning slice's exact-head fixture receipt substitutes for a deployed effect. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s12-28: Read-only `report voice-ghosts` ghost-channel count (tracked-present, tracked-gone, untracked-present plus clean flag)

- **Method:** `manual` (not an execution verdict).
- **Action:** Run `report voice-ghosts --seed` for the demo counts, then poll the live command against the disposable staging guild during a cutover rehearsal until both gaps read empty; confirm the clean flag.
- **Expected:** Seed demo reports the fixture gaps; live staging polling shows tracked-present, tracked-gone and untracked-present counts with no deletes or writes; the clean flag is set only when both gaps are empty.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel IDs (no tokens), sanitized command output and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.
- **Owner:** [TOG-10119](/TOG/issues/TOG-10119), [TOG-13123](/TOG/issues/TOG-13123)
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s12-29: Actionable denied-path interaction copy with next steps

- **Method:** `manual` (not an execution verdict).
- **Action:** On the disposable staging guild as a non-privileged member, trigger each denial path: a permission-gated command, a disabled-feature command, an unknown slash name, a stale button/control and a forced handler failure.
- **Expected:** Every denial names the Discord permission and who grants it (or the admin-only enable path), unknown names and expired controls are distinguishable, and generic failures keep the ref correlation id with a retry hint; all replies stay ephemeral and under the 2000-character cap.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized reply text or screenshots and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.
- **Owner:** [TOG-13123](/TOG/issues/TOG-13123)

## 13. Post-freeze ledger obligations (non-dropped rows)

### s13-f114c44: f114c44 — TOG-3052: temp-voice generator (join-to-create), staging only

- **Method:** `automated` (not an execution verdict).
- **Action:** Attach the TOG-10119 temp-voice scenario for this leg on the staging fixture guild; do not run a second voice scenario here. Join the configured creator channel as a disposable member, then leave.
- **Expected:** Exactly one owned room is created and the member moved; the empty room is removed; outside the allowlisted guild nothing is created.
- **Evidence:** Link the exact-SHA voice evidence table and verdict from [TOG-10119](/TOG/issues/TOG-10119), including row/scenario ID and time window. Missing/failing evidence leaves this row NEEDS WORK; do not infer PASS from the reference. Fixture half: the offline `verification` command at the exact head; the staging half above is unchanged.
- **Owner:** [TOG-10119](/TOG/issues/TOG-10119)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot --lib -- joining_a_channel_that_is_not_a_creator_never_creates_a_room a_guild_without_creator_channels_never_creates_a_room
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s13-681dc29: 681dc29 — TOG-3052: mutation harness proving the delete guards are real

- **Method:** `waived` (not an execution verdict).
- **Action:** Run TOG-10093’s delete-guard mutation fixture against the sweep.
- **Expected:** Mutating or removing temp-voice provenance makes the guard refuse deletion; a channel without provenance is never deleted.
- **Evidence:** Attach the owning slice’s exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no live fault injection, staging/production SQL or credentials.
- **Owner:** [TOG-10093](/TOG/issues/TOG-10093), [TOG-10119](/TOG/issues/TOG-10119)
- **Reason:** Proposed staging-execution waiver: this failure, timing or signed-call path is reproducible only with local mock fixtures under the safety contract (no live fault injection, clock change or credential handling); the owning slice’s exact-head fixture receipt substitutes for a deployed effect. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s13-287d00e: 287d00e — TOG-3052: name the missing permission instead of asking for a retry

- **Method:** `automated` (not an execution verdict).
- **Action:** Run the worker-level refused-create and refused-move fixtures in `crates/bot/src/voice_rooms_tests.rs`: deny the bot Manage Channels or Move Members (guild, category or creator-channel scope) and trigger a join-time create; also a 403 the permission cache cannot explain.
- **Expected:** The recorded failure, the error notice and the `/setup` failure line name the missing permission and, when known, the category or channel override that removes it (or, with a clean cache, the permission the refused write needs); no partial room remains.
- **Evidence:** Attach the exact-head fixture command and PASS/NEEDS WORK result; fixture proof only (the worker fixtures run against mock Discord/store doubles), not a deployed-network soak receipt. Staging acceptance stays on TOG-10119.
- **Owner:** [TOG-10093](/TOG/issues/TOG-10093), [TOG-10119](/TOG/issues/TOG-10119)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot --lib voice_rooms::tests::missing_permission_
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s13-847fbf8: 847fbf8 — TOG-3471: serialize guild reservations and enforce overwrite preflight

- **Method:** `manual` (not an execution verdict).
- **Action:** Attach the TOG-10119 temp-voice scenario for this leg on the staging fixture guild; do not run a second voice scenario here. Have two disposable members join the creator channel together; take the overwrite-preflight refusal from TOG-10093’s fixture.
- **Expected:** Each member gets one distinct room with no duplicate reservation; a create whose overwrites exceed bot permissions is refused before any channel exists.
- **Evidence:** Link the exact-SHA voice evidence table and verdict from [TOG-10119](/TOG/issues/TOG-10119), including row/scenario ID and time window. Missing/failing evidence leaves this row NEEDS WORK; do not infer PASS from the reference. Partial offline fixture evidence, not a PASS: the `verification` command covers only the two refusal cases (missing Manage Channels; private default without Manage Roles) and sequential distinct-room dispatch. The capped durable guild reservation and the general overwrite-bit permission preflight are unverified.
- **Owner:** [TOG-10093](/TOG/issues/TOG-10093), [TOG-15336](/TOG/issues/TOG-15336), [TOG-10119](/TOG/issues/TOG-10119)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot --lib -- refused_creation_plan_records_a_failure_before_any_channel_is_created two_simultaneous_members_get_distinct_persisted_rooms
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s13-a891063: a891063 — TOG-3471: reconcile settings, command registry, and source citations

- **Method:** `manual` (not an execution verdict).
- **Action:** Attach the TOG-10119 temp-voice scenario for this leg on the staging fixture guild; do not run a second voice scenario here. List the deployed `/voice` subcommands and temp-voice settings in the staging guild.
- **Expected:** Registry and settings match TOG-10091’s config/registry golden; no orphan or missing subcommand.
- **Evidence:** Link the exact-SHA voice evidence table and verdict from [TOG-10119](/TOG/issues/TOG-10119), including row/scenario ID and time window. Missing/failing evidence leaves this row NEEDS WORK; do not infer PASS from the reference.
- **Owner:** [TOG-10093](/TOG/issues/TOG-10093), [TOG-10119](/TOG/issues/TOG-10119)
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s13-50c3e9f: 50c3e9f — TOG-3471: retain temp-voice provenance after rollback failure

- **Method:** `manual` (not an execution verdict).
- **Action:** In TOG-10093’s mock-Discord fixture, fail a create and then its rollback.
- **Expected:** The room keeps its provenance so a later sweep removes it; no unmanaged orphan channel.
- **Evidence:** Attach the owning slice’s exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. No live fault injection, staging/production SQL or credentials. Partial offline fixture evidence, not a PASS: the `verification` command covers same-worker, in-memory retention only. Durable provenance, fresh-worker restart recovery, attachment retry, missing-channel cleanup and cap accounting retained until a guarded delete or 404 are unverified (the `#[ignore]`d restart test states the missing behaviour; it is skipped in CI until a durable witness exists).
- **Owner:** [TOG-10093](/TOG/issues/TOG-10093), [TOG-15336](/TOG/issues/TOG-15336), [TOG-10119](/TOG/issues/TOG-10119)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot --lib -- failed_rollback_keeps_the_room_tracked_until_a_later_sweep_removes_it
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s13-984175d: 984175d — TOG-3471: serialize and journal temp-voice ownership changes

- **Method:** `manual` (not an execution verdict).
- **Action:** Attach the TOG-10119 temp-voice scenario for this leg on the staging fixture guild; do not run a second voice scenario here. Transfer room ownership with the panel/`/voice` controls, then let the owner leave; take a failed transfer from TOG-10099’s fixture.
- **Expected:** Ownership changes are serialized and journaled; a failed transfer rolls back to the prior owner.
- **Evidence:** Link the exact-SHA voice evidence table and verdict from [TOG-10119](/TOG/issues/TOG-10119), including row/scenario ID and time window. Missing/failing evidence leaves this row NEEDS WORK; do not infer PASS from the reference.
- **Owner:** [TOG-10099](/TOG/issues/TOG-10099)
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s13-0495e7d: 0495e7d — TOG-3186: one live-activation allowlist replacing five divergent staging fences

- **Method:** `automated` (not an execution verdict).
- **Action:** Run the activation matrix and boot fixtures: identity x capability decisions, runtime composition for the staging, live, mixed and third-party pairs, and the env-driven boot child processes.
- **Expected:** One allowlist decides activation: the staging pair permits every capability, the live pair only the cleared list, and every other identity, a missing token and a mixed pair are refused; a refused capability publishes no command, handler or privileged intent and does not abort boot.
- **Evidence:** Attach exact-head fixture commands and PASS/NEEDS WORK result with sanitized request/response timestamps or signature/integrity assertion; fixture proof only, not a deployed-network soak receipt.
- **Owner:** [TOG-11140](/TOG/issues/TOG-11140)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot-core --lib activation::tests && python3 scripts/cargo_cache.py run -- test -p two-bot activation_

### s13-a7f16b9: a7f16b9 — TOG-3314: hot-wire goodbye channels so a stored value reaches the live handler

- **Method:** `automated` (not an execution verdict).
- **Action:** Run the runtime goodbye hot-setting fixture and the gateway configured-empty goodbye fixture on a disposable test database with local mock Discord.
- **Expected:** A stored goodbye channel list reaches a runtime built before the write with no restart; deleting it restores the deployment value; a stored empty list posts nothing even though the deployment names a channel.
- **Evidence:** Attach exact-head fixture commands and PASS/NEEDS WORK result with sanitized request/response timestamps or signature/integrity assertion; the `#[ignore]` DB scenarios need the disposable agent-testdb URL and run in the `onboarding_` and `gateway_tests` steps of `check.yml`. Fixture proof only, not a deployed-network soak receipt.
- **Owner:** [TOG-10278](/TOG/issues/TOG-10278)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot onboarding_runtime_goodbye_hot_setting_reaches_live_handler -- --include-ignored && python3 scripts/cargo_cache.py run -- test -p two-bot onboarding_gateway_configured_empty_destinations_are_terminal_noops -- --ignored --test-threads=1

### s13-78e7c2d: 78e7c2d — TOG-3471: renumber temp-voice migrations to 0036/0037; filter create-path names through automod

- **Method:** `automated` (not an execution verdict).
- **Action:** Run the worker-level create-path name-filter fixtures in `crates/bot/src/voice_rooms_tests.rs` with a fixture automod policy (invented blocked term, no real word list): a display name containing the term, an invite link, and a template that is itself blocked; also the `/create` name.
- **Expected:** A blocked name is retried without the username; when even the bare template is blocked the join is refused with the `name_blocked` reason and no Discord create call is made. A blocked `/create` name is refused before any REST call.
- **Evidence:** Attach the exact-head fixture command and PASS/NEEDS WORK result; fixture proof only (mock Discord/store doubles), not a deployed-network soak receipt. Temp-voice migrations come from the test-container receipt and staging acceptance stays on TOG-10119.
- **Owner:** [TOG-10093](/TOG/issues/TOG-10093), [TOG-10119](/TOG/issues/TOG-10119)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot --lib voice_rooms::tests::name_filter_
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s13-e2a3f37: e2a3f37 — TOG-5356: clear self_roles in LIVE_CLEARED_CAPABILITIES

- **Method:** `automated` (not an execution verdict).
- **Action:** Run the clearance pin and the boot composition fixture for the live pair with a self-role component and every other capability.
- **Expected:** The live pair routes self-role components and refuses every other capability; other identity pairs route no self-role component.
- **Evidence:** Attach exact-head fixture commands and PASS/NEEDS WORK result with sanitized request/response timestamps or signature/integrity assertion; fixture proof only, not a deployed-network soak receipt.
- **Owner:** [TOG-11140](/TOG/issues/TOG-11140)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot-core --lib activation_shipped_clearance_is_self_roles_only && python3 scripts/cargo_cache.py run -- test -p two-bot activation_boot_publishes_only_permitted_capabilities

### s13-e2082e4: e2082e4 — TOG-5357: gate MessageContent intent on automod/tickets

- **Method:** `automated` (not an execution verdict).
- **Action:** Run the intent bitfield pins, the permission matrix and the env-driven boot and preflight fixtures with automod and tickets enabled and disabled.
- **Expected:** MessageContent is requested only when automod or tickets is enabled and permitted; the gated set is 1735 and the full set 34503; a refused identity requests no privileged intent.
- **Evidence:** Attach exact-head fixture commands and PASS/NEEDS WORK result with sanitized request/response timestamps or signature/integrity assertion; fixture proof only, not a deployed-network soak receipt.
- **Owner:** [TOG-11140](/TOG/issues/TOG-11140)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot-discord --lib intents::tests && python3 scripts/cargo_cache.py run -- test -p two-bot activation_intents_refuse_uncleared_automod_and_tickets && python3 scripts/cargo_cache.py run -- test -p two-bot activation_boot_from_env_isolates_denied_moderation_validation && python3 scripts/cargo_cache.py run -- test -p two-bot --test preflight refused_activation_requests_no_privileged_message_content_intent

### s13-720419b: 720419b — TOG-5683: voice blind-window reconcile report (count startKnown:false per gap)

- **Method:** `automated` (not an execution verdict).
- **Action:** Run the voice blind-window unit fixtures and the DB-backed `report_cli` scenario (events-write gaps with startKnown:false ends across two blind windows).
- **Expected:** Per-gap startKnown:false counts match the fixture (2 and 1, one end unattributed); known-start sessions are not counted; the report writes nothing and no `_sqlx_migrations` table appears.
- **Evidence:** Attach exact-head fixture commands and PASS/NEEDS WORK result with fixture counts; the DB-backed scenario skips without `TWO_TEST_DATABASE_URL` and runs in CI on disposable agent-testdb databases. Fixture proof only, not a deployed-network soak receipt.
- **Owner:** [TOG-11152](/TOG/issues/TOG-11152)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --test report_cli voice_reconcile_reports_events_write_gaps_with_unknown_start_counts_read_only && python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --lib voice_reconcile::tests
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s13-40e266e: 40e266e — TOG-5683: reconcile heartbeat from events write series, not the contained probe table

- **Method:** `automated` (not an execution verdict).
- **Action:** Run the DB-backed `report_cli` scenario: an events-write gap with a healthy probe table seeded through it.
- **Expected:** The heartbeat comes from `events.recorded_at`, so the gap is reported despite healthy probes; a backfilled row (old `occurred_at`) does not close it.
- **Evidence:** Attach exact-head fixture commands and PASS/NEEDS WORK result with fixture counts; the DB-backed scenario skips without `TWO_TEST_DATABASE_URL` and runs in CI on disposable agent-testdb databases. Fixture proof only, not a deployed-network soak receipt.
- **Owner:** [TOG-11152](/TOG/issues/TOG-11152)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --test report_cli voice_reconcile_reports_events_write_gaps_with_unknown_start_counts_read_only

### s13-edaf2dd: edaf2dd — TOG-5684: enforce startKnown:false exclusion from duration averages

- **Method:** `automated` (not an execution verdict).
- **Action:** Run the duration summary fixtures, the core metadata-parsing fixtures in `crates/core/src/voice.rs` and the DB-backed `report_cli` scenario mixing known and startKnown:false sessions.
- **Expected:** `durations` averages known-start sessions only (the unknown start carrying a number is excluded) and counts the excluded unknown starts. Known 60 / unknown 600 averages to 60 (removing the start-known filter yields 330); mixed numeric/numeric-string metadata averages to 60 with measured=2 and excluded_unknown_starts=4; invalid-only metadata yields no average, not fabricated zeroes. Finite decimal numeric strings are measured; legacy JavaScript coercions of empty strings, booleans, containers and radix-prefixed strings are deliberately rejected.
- **Evidence:** Attach exact-head fixture commands and PASS/NEEDS WORK result with fixture counts; the DB-backed scenario skips without `TWO_TEST_DATABASE_URL` and runs in CI on disposable agent-testdb databases. Include the core `voice::tests` results (`averages_filter_on_flag_not_null`, `metadata_average_excludes_unknown_numbers_and_numeric_strings`, `metadata_numbers_and_numeric_strings_enter_known_average`, `metadata_invalid_durations_never_become_measured_zeroes`). Fixture proof only, not a deployed-network soak receipt; the separate voice receipt gate is unchanged.
- **Owner:** [TOG-11152](/TOG/issues/TOG-11152)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --test report_cli voice_reconcile_reports_events_write_gaps_with_unknown_start_counts_read_only && python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --lib voice_reconcile::tests && python3 scripts/cargo_cache.py run -- test -p two-bot-core --lib voice::tests::

### s13-59965d0: 59965d0 — TOG-5981: serialize same-member voice frames, scope voice idempotency keys by channel

- **Method:** `waived` (not an execution verdict).
- **Action:** Replay a same-tick burst of same-member voice frames across two channels through the fixture added by TOG-15277.
- **Expected:** Frames apply serially per member and idempotency keys are channel-scoped: one session per channel, no cross-channel dedupe.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Owner:** [TOG-15277](/TOG/issues/TOG-15277)
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s13-7da2c15: 7da2c15 — TOG-6123: drop open voice sessions on fresh session (ShardReady) as well as resume

- **Method:** `automated` (not an execution verdict).
- **Action:** Run the READY and RESUMED reconnect-drop fixtures in `crates/discord/tests/funnel_replay.rs`.
- **Expected:** Open voice sessions are dropped on both a fresh session (READY) and a resume; a first READY with nothing open writes nothing and the next leave ends unknown-start.
- **Evidence:** Attach exact-head fixture commands and PASS/NEEDS WORK result with sanitized request/response timestamps or signature/integrity assertion; fixture proof only, not a deployed-network soak receipt.
- **Owner:** [TOG-11149](/TOG/issues/TOG-11149)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot-discord --test funnel_replay -- pipeline_fresh_session_ready_drops_open_voice_sessions pipeline_voice_boundaries_and_reconnect_drop
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s13-448cf1d: 448cf1d — TOG-6122: close open voice session on server-leave (end to open channel, duration to leave time)

- **Method:** `automated` (not an execution verdict).
- **Action:** Run the server-leave-mid-voice fixtures in `crates/discord/tests/funnel_replay.rs`, `crates/core/src/handlers.rs` and `crates/core/src/voice.rs`.
- **Expected:** The open session closes on its open channel with duration ending at the leave time; the tracker is emptied and a repeated leave writes no second end row.
- **Evidence:** Attach exact-head fixture commands and PASS/NEEDS WORK result with sanitized request/response timestamps or signature/integrity assertion; fixture proof only, not a deployed-network soak receipt.
- **Owner:** [TOG-11149](/TOG/issues/TOG-11149)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot-discord --test funnel_replay -- pipeline_leave_closes_voice_first pipeline_voice_move_and_server_leave_share_receipt_boundaries && python3 scripts/cargo_cache.py run -- test -p two-bot-core --lib -- handlers::tests::leave_closes_open_session_then_records voice::tests::resolve_measured_and_unknown
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s13-6928f11: 6928f11 — TOG-6817: guard so backfill gate_cleared never feeds time-to-clear

- **Method:** `waived` (not an execution verdict).
- **Action:** Import a history backfill fixture containing gate_cleared events with TOG-9882’s importer on test containers.
- **Expected:** Backfilled gate_cleared events never produce time-to-clear durations.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Owner:** [TOG-9882](/TOG/issues/TOG-9882)
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s13-37afd80: 37afd80 — feat(presence): cap probe scan cost and window trend reads (#236)

- **Method:** `waived` (not an execution verdict).
- **Action:** Run the presence probe with TOG-11146’s oversized mock roster and a long trend window.
- **Expected:** The scan stops at its cost cap and trend reads are windowed; truncation is persisted.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Owner:** [TOG-11146](/TOG/issues/TOG-11146)
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s13-3f513ed: 3f513ed — test(qa): fixture-driven acceptance for roster, moderation, backfill, automod, dashboard (#218)

- **Method:** `waived` (not an execution verdict).
- **Action:** Run TOG-9882’s roster, moderation, backfill, automod and dashboard acceptance fixtures on test containers.
- **Expected:** Each fixture contract passes; the automod export runs on demand only, not as a scheduled job.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Owner:** [TOG-15759](/TOG/issues/TOG-15759)
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s13-b948b89: b948b89 — fix(onboarding): dedupe repeated keys in legacy planSelection (#269)

- **Method:** `automated` (not an execution verdict).
- **Action:** Submit repeated and unknown game keys through the pure planner and the runtime picker fixture.
- **Expected:** Repeated keys are deduped (first occurrence wins) for known and unknown keys alike; each valid pick grants and records once.
- **Evidence:** Attach exact-head fixture commands and PASS/NEEDS WORK result with sanitized request/response timestamps or signature/integrity assertion; the `#[ignore]` DB scenarios need the disposable agent-testdb URL and run in the `onboarding_` and `gateway_tests` steps of `check.yml`. Fixture proof only, not a deployed-network soak receipt.
- **Owner:** [TOG-11180](/TOG/issues/TOG-11180), [TOG-10278](/TOG/issues/TOG-10278)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot-core --lib game_plan_dedupes_known_and_unknown_keys_in_first_submission_order && python3 scripts/cargo_cache.py run -- test -p two-bot-core --test onboarding_effects game_finalization_keeps_reply_plan_and_recording_in_sync_when_visibility_changes && python3 scripts/cargo_cache.py run -- test -p two-bot onboarding_runtime_game_picker_withholds_invisible_destinations_and_dedupes_keys -- --include-ignored

### s13-7f90492: 7f90492 — feat(events): add gated event.read mapped-event verifier (#266)

- **Method:** `automated` (not an execution verdict).
- **Action:** Send signed keyless `event.read` calls for mapped and unmapped event keys, with the flag on and off, through the receiver fixtures with a loopback Discord double.
- **Expected:** The gated verifier returns the mapped event with the 7-field read result and refuses unmapped keys, a disabled flag, replayed nonces and forged signatures without Discord writes.
- **Evidence:** Attach exact-head fixture commands and PASS/NEEDS WORK result with sanitized request/response timestamps or signature/integrity assertion; fixture proof only, not a deployed-network soak receipt.
- **Owner:** [TOG-15757](/TOG/issues/TOG-15757)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot-core --lib event_key_names_a_key_never_a_snowflake && python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db --test internal_action_store event_key_map && python3 scripts/cargo_cache.py run -- test -p two-bot event_read

### s13-dc2b507: dc2b507 — feat(voice): reconcile open-half sessions with explicit reasons (#273)

- **Method:** `automated` (not an execution verdict).
- **Action:** Run the open-half reconcile fixtures (every reason), the timestamp-parser boundary fixtures in `crates/core/src/funnel.rs`, the `Number()` duration-string fixtures and the DB-backed `report_cli` legacy-fixture scenario.
- **Expected:** Every open-half session is reported with its explicit reason (restart-gap, server-leave and metadata-recompute resolve; still-open, superseded, no-start-on-file and bad-end-row are flagged) and nothing is repaired. A timestamp with a multi-byte zone (`+1é1`) or an absurd year is unparseable (legacy `Date.parse` NaN): the row is skipped or falls through to the bad-end-row or earlier-start path, and the sweep never panics. A `durationSeconds` string reads as legacy `Number()` does (`"0x3c"` is 60, `"6e1"` is 60, whitespace is trimmed, `""` is 0; `"nan"`, `"inf"` and `"Infinity"` are unmeasured); negative and non-finite values never become a measured time. Accepted divergence: a JSON array or object is unmeasured (legacy `Number([])` is 0).
- **Evidence:** Attach exact-head fixture commands and PASS/NEEDS WORK result with fixture counts; the DB-backed scenario skips without `TWO_TEST_DATABASE_URL` and runs in CI on disposable agent-testdb databases. Include the core `funnel::tests` results (`multibyte_zone_is_unparseable_not_a_panic`, `non_ascii_anywhere_in_a_timestamp_never_panics`, `absurd_years_are_unparseable_not_an_overflow`) and the cutover `voice_reconcile::tests` results. Fixture proof only, not a deployed-network soak receipt; the separate voice receipt gate is unchanged.
- **Owner:** [TOG-11152](/TOG/issues/TOG-11152)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --test report_cli voice_reconcile_matches_legacy_fixtures_read_only && python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --lib voice_reconcile::tests && python3 scripts/cargo_cache.py run -- test -p two-bot-core --lib funnel::tests::
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s13-860557f: 860557f — test(backfill): refuse malformed export rows without throwing (#289)

- **Method:** `waived` (not an execution verdict).
- **Action:** Feed malformed export rows to TOG-9882’s backfill parser fixture.
- **Expected:** Malformed rows are refused and counted without aborting the run.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Owner:** [TOG-9882](/TOG/issues/TOG-9882)
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s13-b3747a8: b3747a8 — fix(session): route valid picks alongside stale keys (#290)

- **Method:** `automated` (not an execution verdict).
- **Action:** Submit a session selection holding one valid and one stale key through the pure planner and the runtime picker fixture.
- **Expected:** The valid pick still routes and the ack carries a retry note for the stale key; a wholly unknown selection routes nowhere and records nothing.
- **Evidence:** Attach exact-head fixture commands and PASS/NEEDS WORK result with sanitized request/response timestamps or signature/integrity assertion; the `#[ignore]` DB scenarios need the disposable agent-testdb URL and run in the `onboarding_` and `gateway_tests` steps of `check.yml`. Fixture proof only, not a deployed-network soak receipt.
- **Owner:** [TOG-11180](/TOG/issues/TOG-11180), [TOG-10278](/TOG/issues/TOG-10278)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot-core --lib session_valid_picks_survive_stale_keys_and_offer_a_partial_retry && python3 scripts/cargo_cache.py run -- test -p two-bot-core --lib session_entirely_unknown_or_empty_submissions_route_nothing && python3 scripts/cargo_cache.py run -- test -p two-bot onboarding_runtime_roleless_session_reselection_stale_menu_and_goodbye_mentions -- --include-ignored

### s13-8371181: 8371181 — feat(feeds): follow same-host redirects, refuse cross-host distinctly (#299)

- **Method:** `waived` (not an execution verdict).
- **Action:** Serve a same-host and a cross-host redirect from TOG-11022’s local mock feed server.
- **Expected:** The same-host redirect is followed; the cross-host redirect is refused with a distinct reason.
- **Evidence:** Attach the owning slice’s exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no live fault injection, staging/production SQL or credentials.
- **Owner:** [TOG-11022](/TOG/issues/TOG-11022)
- **Reason:** Proposed staging-execution waiver: this failure, timing or signed-call path is reproducible only with local mock fixtures under the safety contract (no live fault injection, clock change or credential handling); the owning slice’s exact-head fixture receipt substitutes for a deployed effect. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s13-b5038fb: b5038fb — fix(feeds): bound each pollFeeds read with a timeout (#298)

- **Method:** `waived` (not an execution verdict).
- **Action:** Serve a stalled feed body from TOG-11022’s local mock feed server.
- **Expected:** Each feed read ends at its timeout and the poll continues with the next feed.
- **Evidence:** Attach the owning slice’s exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no live fault injection, staging/production SQL or credentials.
- **Owner:** [TOG-11022](/TOG/issues/TOG-11022)
- **Reason:** Proposed staging-execution waiver: this failure, timing or signed-call path is reproducible only with local mock fixtures under the safety contract (no live fault injection, clock change or credential handling); the owning slice’s exact-head fixture receipt substitutes for a deployed effect. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s13-a50e747: a50e747 — fix(feeds): isolate per-item failures in pollFeeds batch (#302)

- **Method:** `waived` (not an execution verdict).
- **Action:** Include one item that fails delivery in a TOG-11022 mock feed batch.
- **Expected:** The failing item is isolated; the other items are delivered.
- **Evidence:** Attach the owning slice’s exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no live fault injection, staging/production SQL or credentials.
- **Owner:** [TOG-11022](/TOG/issues/TOG-11022)
- **Reason:** Proposed staging-execution waiver: this failure, timing or signed-call path is reproducible only with local mock fixtures under the safety contract (no live fault injection, clock change or credential handling); the owning slice’s exact-head fixture receipt substitutes for a deployed effect. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s13-673cd13: 673cd13 — fix(moderation): report truncated only when rows were actually cut (#307)

- **Method:** `automated` (not an execution verdict).
- **Action:** Run the disable-preflight report fixtures in `crates/core/src/disable_preflight.rs` with exactly 10 and exactly 11 owed ids in each of the unban, running-claim, lockdown and scheduled-message sections.
- **Expected:** Exactly 10 ids are all named with the exact count and no `+N more`; 11 ids name the first 10, end in `+1 more` and keep the exact count 11. Truncation is reported only when ids were actually cut.
- **Evidence:** Attach exact-head fixture commands and PASS/NEEDS WORK result with the sanitized report lines for 10 and 11 ids; fixture proof only, not a deployed-network soak receipt.
- **Owner:** [TOG-15281](/TOG/issues/TOG-15281)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot-core --lib disable_preflight::tests

### s13-df990c9: df990c9 — fix(announcements): prefer alternate link for multi-link Atom entries (#306)

- **Method:** `waived` (not an execution verdict).
- **Action:** Serve an Atom entry with several links from TOG-11022’s mock feed server.
- **Expected:** The delivered link is the alternate link.
- **Evidence:** Attach the owning slice’s exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no live fault injection, staging/production SQL or credentials.
- **Owner:** [TOG-11022](/TOG/issues/TOG-11022)
- **Reason:** Proposed staging-execution waiver: this failure, timing or signed-call path is reproducible only with local mock fixtures under the safety contract (no live fault injection, clock change or credential handling); the owning slice’s exact-head fixture receipt substitutes for a deployed effect. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s13-e2ffdf0: e2ffdf0 — feat(tempvoice): enforce durable rolling create burst limits (#308)

- **Method:** `manual` (not an execution verdict).
- **Action:** Attach the TOG-10119 temp-voice scenario for this leg on the staging fixture guild; do not run a second voice scenario here. Create rooms past the configured rolling burst limit with disposable members, across the TOG-10119 restart.
- **Expected:** Creates beyond the limit are refused with a named reason, and the count survives restart.
- **Evidence:** Link the exact-SHA voice evidence table and verdict from [TOG-10119](/TOG/issues/TOG-10119), including row/scenario ID and time window. Missing/failing evidence leaves this row NEEDS WORK; do not infer PASS from the reference.
- **Owner:** [TOG-10093](/TOG/issues/TOG-10093), [TOG-10119](/TOG/issues/TOG-10119)
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s13-3c3e7e8: 3c3e7e8 — fix(voice): treat malformed leave timestamp as unknown-start (#265)

- **Method:** `waived` (not an execution verdict).
- **Action:** Deliver a voice leave with a malformed timestamp, with and without an open session, through the fixture added by TOG-15277.
- **Expected:** The session is treated as unknown-start, not a fabricated duration, and no leveling XP is awarded.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Owner:** [TOG-15277](/TOG/issues/TOG-15277)
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s13-c99fec7: c99fec7 — fix(announcements): reclaim crashed feed-delivery claims after 60s lease (#312)

- **Method:** `waived` (not an execution verdict).
- **Action:** Leave a feed-delivery claim from a simulated crash in TOG-11022’s fixture and advance its clock past 60s.
- **Expected:** The claim is reclaimed after the 60s lease and the item is delivered once.
- **Evidence:** Attach the owning slice’s exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no live fault injection, staging/production SQL or credentials.
- **Owner:** [TOG-11022](/TOG/issues/TOG-11022)
- **Reason:** Proposed staging-execution waiver: this failure, timing or signed-call path is reproducible only with local mock fixtures under the safety contract (no live fault injection, clock change or credential handling); the owning slice’s exact-head fixture receipt substitutes for a deployed effect. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s13-7587037: 7587037 — fix(temp-voice): stop conferring ManageRoles on channel create (#314)

- **Method:** `manual` (not an execution verdict).
- **Action:** Attach the TOG-10119 temp-voice scenario for this leg on the staging fixture guild; do not run a second voice scenario here. Inspect the permission overwrites on a created room.
- **Expected:** The owner overwrite grants room controls but never ManageRoles.
- **Evidence:** Link the exact-SHA voice evidence table and verdict from [TOG-10119](/TOG/issues/TOG-10119), including row/scenario ID and time window. Missing/failing evidence leaves this row NEEDS WORK; do not infer PASS from the reference.
- **Owner:** [TOG-10093](/TOG/issues/TOG-10093), [TOG-10119](/TOG/issues/TOG-10119)
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s13-cad94b6: cad94b6 — fix(moderation): name stranded running unban claims with hand-release steps (#320)

- **Method:** `automated` (not an execution verdict).
- **Action:** With `TWO_TEST_DATABASE_URL` set to the disposable agent-testdb or CI service (an unset URL skips the DB cases), run the disable-preflight fixtures: a `running` unban left by a stopped worker, and a pending-only refusal.
- **Expected:** The refusal and `two-bot moderation preflight [--json]` list the stranded claim in a `[running]` section with the recovery pointer, and add `running_unbans` beside the unchanged `pending_unbans`; a pending-only refusal has neither the tag nor the pointer. Exit codes are unchanged and the claim token is never reported. The close is the documented claim-token-fenced store call; no operator command for it exists.
- **Evidence:** Attach exact-head fixture commands and PASS/NEEDS WORK result with the sanitized report lines and `running_unbans` array; fixture proof only, not a deployed-network soak receipt or a recovery rehearsal.
- **Owner:** [TOG-15281](/TOG/issues/TOG-15281)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot-core --lib disable_preflight::tests && python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db --test disable_preflight_db && python3 scripts/cargo_cache.py run -- test -p two-bot --test moderation_preflight_cli

### s13-db84b22: db84b22 — fix(feeds): advance poll window past 20-post cap across polls (#318)

- **Method:** `waived` (not an execution verdict).
- **Action:** Serve more than 20 new posts across two polls from TOG-11022’s mock feed server.
- **Expected:** The poll window advances past the 20-post cap so no post is skipped or repeated.
- **Evidence:** Attach the owning slice’s exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no live fault injection, staging/production SQL or credentials.
- **Owner:** [TOG-11022](/TOG/issues/TOG-11022)
- **Reason:** Proposed staging-execution waiver: this failure, timing or signed-call path is reproducible only with local mock fixtures under the safety contract (no live fault injection, clock change or credential handling); the owning slice’s exact-head fixture receipt substitutes for a deployed effect. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s13-9ee2e89: 9ee2e89 — feat(backup): cover all bot-owned tables in dump and restore (#330)

- **Method:** `waived` (not an execution verdict).
- **Action:** Dump and restore every bot-owned table with TOG-11142’s test-container fixture.
- **Expected:** All bot-owned tables round-trip and every owned serial sequence is reset.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Owner:** [TOG-11142](/TOG/issues/TOG-11142)
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s13-2f386e8: 2f386e8 — test(scheduler): prove disabled scheduler fires zero jobs (#335)

- **Method:** `waived` (not an execution verdict).
- **Action:** Run TOG-10081’s scheduler fixture with the scheduled-message scheduler disabled and due jobs present.
- **Expected:** Zero jobs fire.
- **Evidence:** Attach the owning slice’s exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no live fault injection, staging/production SQL or credentials.
- **Owner:** [TOG-10081](/TOG/issues/TOG-10081)
- **Reason:** Proposed staging-execution waiver: this failure, timing or signed-call path is reproducible only with local mock fixtures under the safety contract (no live fault injection, clock change or credential handling); the owning slice’s exact-head fixture receipt substitutes for a deployed effect. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s13-298f3a4: 298f3a4 — fix(selfrole): reject reaction panels over Discord's 20-reaction cap (#336)

- **Method:** `manual` (not an execution verdict).
- **Action:** Attempt to configure a disposable reaction self-role panel with 21 reactions through the normal config path.
- **Expected:** Configuration is refused at the 20-reaction cap; nothing is posted. Runtime stays TOG-10292.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.
- **Owner:** [TOG-10087](/TOG/issues/TOG-10087), [TOG-10292](/TOG/issues/TOG-10292)

### s13-4931aec: 4931aec — fix(redirect): cache campaign misses at short negative TTL (#339)

- **Method:** `waived` (not an execution verdict).
- **Action:** Look up one unknown slug repeatedly within the negative TTL in TOG-11153’s redirect fixture.
- **Expected:** One store lookup serves the repeated misses; the cache entry expires within ≤2s.
- **Evidence:** Attach the owning slice’s exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no live fault injection, staging/production SQL or credentials.
- **Owner:** [TOG-11153](/TOG/issues/TOG-11153)
- **Reason:** Proposed staging-execution waiver: this failure, timing or signed-call path is reproducible only with local mock fixtures under the safety contract (no live fault injection, clock change or credential handling); the owning slice’s exact-head fixture receipt substitutes for a deployed effect. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s13-f9fbf5c: f9fbf5c — fix(redirect): refuse reserved healthz slug at campaign add time (#340)

- **Method:** `manual` (not an execution verdict).
- **Action:** Add campaign slug `healthz` through TOG-11183’s campaign input fixture, then request `/healthz` on the approved staging redirect origin.
- **Expected:** The slug is refused at add time and the health probe still answers.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.
- **Owner:** [TOG-11183](/TOG/issues/TOG-11183)

### s13-6433578: 6433578 — fix(redirect): validate port and fallback code at startup (#341)

- **Method:** `waived` (not an execution verdict).
- **Action:** Start TOG-11183’s redirect fixture with an invalid fallback code.
- **Expected:** Startup rejects the fallback before any redirect effect; Node listener-port configuration stays dropped.
- **Evidence:** Attach the owning slice’s exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no live fault injection, staging/production SQL or credentials.
- **Owner:** [TOG-11183](/TOG/issues/TOG-11183)
- **Reason:** Proposed staging-execution waiver: this failure, timing or signed-call path is reproducible only with local mock fixtures under the safety contract (no live fault injection, clock change or credential handling); the owning slice’s exact-head fixture receipt substitutes for a deployed effect. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s13-b3d8d97: b3d8d97 — fix(selfrole): reject button custom_ids over Discord's 100-char limit (#338)

- **Method:** `manual` (not an execution verdict).
- **Action:** Attempt to configure a disposable button self-role panel with a 101-character custom_id through the normal config path.
- **Expected:** Configuration is refused at Discord’s 100-character limit.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.
- **Owner:** [TOG-10087](/TOG/issues/TOG-10087), [TOG-10292](/TOG/issues/TOG-10292)

### s13-3400bb2: 3400bb2 — feat(analytics): add member_leave backfill gap sweep (#343)

- **Method:** `waived` (not an execution verdict).
- **Action:** Run TOG-11152’s member_leave backfill gap sweep on a test-container fixture with leave gaps.
- **Expected:** Each gap is reported read-only; nothing is backfilled.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Owner:** [TOG-11152](/TOG/issues/TOG-11152)
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s13-f5e62b7: f5e62b7 — fix(redirect): resolve throttle bucket through trusted-proxy chain (#342)

- **Method:** `manual` (not an execution verdict).
- **Action:** Send one request with a spoofed `X-Forwarded-For` to the approved staging redirect origin.
- **Expected:** The throttle bucket is keyed on `CF-Connecting-IP`, never the user-supplied header.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.
- **Owner:** [TOG-11153](/TOG/issues/TOG-11153)

### s13-ce3c0e2: ce3c0e2 — test(backfill): extend malformed-row coverage, fix 3 probe-found bugs (#356)

- **Method:** `waived` (not an execution verdict).
- **Action:** Run TOG-9882’s extended malformed-row backfill fixtures on test containers.
- **Expected:** Every malformed-row case is refused without throwing, including the three probe-found cases.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Owner:** [TOG-9882](/TOG/issues/TOG-9882)
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s13-b62ff28: b62ff28 — fix(redirect): log slug and error class on lookup failure (#355)

- **Method:** `waived` (not an execution verdict).
- **Action:** Force a lookup failure for a known slug in TOG-11183’s redirect fixture.
- **Expected:** The log names the slug and a bounded errorClass; no raw error text or credentials.
- **Evidence:** Attach the owning slice’s exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no live fault injection, staging/production SQL or credentials.
- **Owner:** [TOG-11183](/TOG/issues/TOG-11183)
- **Reason:** Proposed staging-execution waiver: this failure, timing or signed-call path is reproducible only with local mock fixtures under the safety contract (no live fault injection, clock change or credential handling); the owning slice’s exact-head fixture receipt substitutes for a deployed effect. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s13-38041a1: 38041a1 — fix(temp-voice): refuse renames that collide with a sibling channel name (#361)

- **Method:** `manual` (not an execution verdict).
- **Action:** Attach the TOG-10119 temp-voice scenario for this leg on the staging fixture guild; do not run a second voice scenario here. Rename a room to the name of a sibling channel in the same category, then to a unique name.
- **Expected:** The colliding rename is refused; the unique name is applied.
- **Evidence:** Link the exact-SHA voice evidence table and verdict from [TOG-10119](/TOG/issues/TOG-10119), including row/scenario ID and time window. Missing/failing evidence leaves this row NEEDS WORK; do not infer PASS from the reference.
- **Owner:** [TOG-10101](/TOG/issues/TOG-10101)
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s13-a40d4a5: a40d4a5 — fix(onboarding): plan session picks in catalog order (#258)

- **Method:** `automated` (not an execution verdict).
- **Action:** Submit session picks in reverse catalog order, with a repeat, through the pure planner and the runtime picker fixture.
- **Expected:** Picks are planned and routed in catalog order, deduped, and the ack links follow that order.
- **Evidence:** Attach exact-head fixture commands and PASS/NEEDS WORK result with sanitized request/response timestamps or signature/integrity assertion; the `#[ignore]` DB scenarios need the disposable agent-testdb URL and run in the `onboarding_` and `gateway_tests` steps of `check.yml`. Fixture proof only, not a deployed-network soak receipt.
- **Owner:** [TOG-11180](/TOG/issues/TOG-11180), [TOG-10278](/TOG/issues/TOG-10278)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot-core --lib session_reordered_duplicate_submissions_follow_the_supplied_catalog && python3 scripts/cargo_cache.py run -- test -p two-bot onboarding_runtime_session_picks_route_in_catalog_order -- --include-ignored

### s13-90ab4b7: 90ab4b7 — fix(backup): publish dumps only after completed output (#386)

- **Method:** `automated` (not an execution verdict).
- **Action:** Run the completed-output publication fixtures in `crates/core/src/backup/dump_file.rs`.
- **Expected:** Dumps publish only after completed, count-validated output; short writes retry, failures propagate and abandoned writers leave no backup. Not deployed timer evidence.
- **Evidence:** Attach exact-head fixture commands and PASS/NEEDS WORK result with sanitized request/response timestamps or signature/integrity assertion; fixture proof only, not a deployed-network soak receipt.
- **Owner:** [TOG-9881](/TOG/issues/TOG-9881)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot-core --lib backup::dump_file

### s13-7df2a95: 7df2a95 — fix(leveling): honor configured command guild fence (#394)

- **Method:** `manual` (not an execution verdict).
- **Action:** Run `/rank` in the configured staging guild; take an unconfigured-guild call from the leveling fixture.
- **Expected:** The configured guild fence is checked before any leveling read, write or reply.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.
- **Owner:** [TOG-10343](/TOG/issues/TOG-10343)

### s13-cc27351: cc27351 — fix(tempvoice): isolate per-channel sweep failures (#393)

- **Method:** `waived` (not an execution verdict).
- **Action:** In TOG-10093’s mock-Discord fixture, fail one room’s delete during a sweep.
- **Expected:** The other empty rooms are still removed and the single failure is logged.
- **Evidence:** Attach the owning slice’s exact-head local-fixture command and sanitized PASS/NEEDS WORK result, including the mock request/response, timing or log assertion. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no live fault injection, staging/production SQL or credentials.
- **Owner:** [TOG-10093](/TOG/issues/TOG-10093), [TOG-10119](/TOG/issues/TOG-10119)
- **Reason:** Proposed staging-execution waiver: this failure, timing or signed-call path is reproducible only with local mock fixtures under the safety contract (no live fault injection, clock change or credential handling); the owning slice’s exact-head fixture receipt substitutes for a deployed effect. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)
- **Reference:** [TOG-10119](/TOG/issues/TOG-10119) — attach its exact-SHA evidence; shared non-voice assertions remain on this row.

### s13-d3d9afe: d3d9afe — fix(onboarding): withhold invisible fallback destinations (#398)

- **Method:** `automated` (not an execution verdict).
- **Action:** Deny a disposable member the dedicated rooms and the hub, then submit game picks; repeat with only the hub denied.
- **Expected:** The roles are kept, no link to a room the member cannot open is shown, and no `channel_routed` row is written when nothing is reachable; a mixed submission links the reachable room and records the other pick as unavailable.
- **Evidence:** Attach exact-head fixture commands and PASS/NEEDS WORK result with sanitized request/response timestamps or signature/integrity assertion; the `#[ignore]` DB scenarios need the disposable agent-testdb URL and run in the `onboarding_` and `gateway_tests` steps of `check.yml`. Fixture proof only, not a deployed-network soak receipt.
- **Owner:** [TOG-11180](/TOG/issues/TOG-11180), [TOG-10278](/TOG/issues/TOG-10278)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot-core --lib invisible_primary_and_fallback_retain_roles_without_a_route && python3 scripts/cargo_cache.py run -- test -p two-bot-core --lib routed_row_names_unavailable_picks_only_when_a_pick_has_no_route && python3 scripts/cargo_cache.py run -- test -p two-bot-core --test onboarding_effects && python3 scripts/cargo_cache.py run -- test -p two-bot onboarding_runtime_game_picker_withholds_invisible_destinations_and_dedupes_keys -- --include-ignored && python3 scripts/cargo_cache.py run -- test -p two-bot onboarding_runtime_game_role_match_post_grant_routing_clear_and_dry_run -- --include-ignored

### s13-1e64035: 1e64035 — fix(presence): bound repeated truncated roster scans (#400)

- **Method:** `waived` (not an execution verdict).
- **Action:** Run repeated presence scans against TOG-11146’s truncating mock roster.
- **Expected:** Repeated truncated scans back off within bounds instead of rescanning every cycle.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Owner:** [TOG-11146](/TOG/issues/TOG-11146)
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s13-35aac83: 35aac83 — fix(analytics): compare leave-gap timestamps by instant (#411)

- **Method:** `waived` (not an execution verdict).
- **Action:** Run TOG-11152’s leave-gap comparison on fixture timestamps with different offsets for one instant.
- **Expected:** Timestamps compare by instant, not text.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Owner:** [TOG-11152](/TOG/issues/TOG-11152)
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s13-8b5d1e1: 8b5d1e1 — fix(lfg): reject reserved leave-action role keys (#414)

- **Method:** `automated` (not an execution verdict).
- **Action:** Run the reserved leave-action role-key refusals in `crates/core/tests/lfg_acceptance.rs` and `crates/core/src/lfg.rs`.
- **Expected:** The reserved `__leave__` key is refused wherever it appears in a role spec and the leave action keeps routing.
- **Evidence:** Attach exact-head fixture commands and PASS/NEEDS WORK result with sanitized request/response timestamps or signature/integrity assertion; fixture proof only, not a deployed-network soak receipt.
- **Owner:** [TOG-10260](/TOG/issues/TOG-10260)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot-core --test lfg_acceptance && python3 scripts/cargo_cache.py run -- test -p two-bot-core --lib -- lfg::tests::role_spec_refuses_reserved_leave_key lfg::tests::select_parsing_routes_signup_and_leave

### s13-218e469: 218e469 — fix(dump): validate non-table record shapes before declaring a backup verified (#416)

- **Method:** `waived` (not an execution verdict).
- **Action:** Verify a test-container dump whose manifest/end records are malformed with TOG-11186’s fixture.
- **Expected:** Verification fails before declaring success.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Owner:** [TOG-11186](/TOG/issues/TOG-11186)
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s13-1d64196: 1d64196 — fix(scorecard): retry failed weekly runs within bounded schedule (#413)

- **Method:** `waived` (not an execution verdict).
- **Action:** Fail a weekly scorecard run in TOG-11145’s fixture inside and after the Monday window.
- **Expected:** Retries happen only inside the bounded Monday schedule window.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Owner:** [TOG-11145](/TOG/issues/TOG-11145)
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s13-287a181: 287a181 — fix(snapshots): discard pending collector cycles after stop (#421)

- **Method:** `waived` (not an execution verdict).
- **Action:** Stop a snapshot collector with a pending cycle in TOG-11181’s fixture.
- **Expected:** Pending cycle results are discarded after stop; the mirror is unchanged.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Owner:** [TOG-11181](/TOG/issues/TOG-11181)
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s13-a8d9f53: a8d9f53 — fix(announcements): defer interactions before service I/O (#424)

- **Method:** `manual` (not an execution verdict).
- **Action:** Invoke the LFG commands and select in the staging guild with a slow-service leg from a local mock; invoke /rsvp once TOG-10293 wires it.
- **Expected:** Each interaction is deferred before service I/O, so no interaction-failed reply appears.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.
- **Owner:** [TOG-10293](/TOG/issues/TOG-10293)

### s13-f5edc70: f5edc70 — fix(leveling): show absent members as unranked (#429)

- **Method:** `manual` (not an execution verdict).
- **Action:** Run `/rank member:` for a fixture member with no XP row.
- **Expected:** The reply shows Unranked, not zero XP with a numeric rank.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.
- **Owner:** [TOG-11147](/TOG/issues/TOG-11147)

### s13-3dc9720: 3dc9720 — fix(restore): prefer stable IDs for renamed guild resources (#426)

- **Method:** `automated` (not an execution verdict).
- **Action:** Run the renamed/swapped role and moved-channel regressions in `crates/core/tests/guild_config_regressions.rs`.
- **Expected:** Restore matches surviving resources by stable ID before name, for renamed/swapped roles and moved channels.
- **Evidence:** Attach exact-head fixture commands and PASS/NEEDS WORK result with sanitized request/response timestamps or signature/integrity assertion; fixture proof only, not a deployed-network soak receipt.
- **Owner:** [TOG-9881](/TOG/issues/TOG-9881)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot-core --test guild_config_regressions

### s13-3f3f969: 3f3f969 — fix(events): reject malformed scheduled-event response elements (#436)

- **Method:** `waived` (not an execution verdict).
- **Action:** Return malformed scheduled-event elements from TOG-11181’s mock REST fixture.
- **Expected:** Malformed elements are rejected before the atomic mirror replacement.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Owner:** [TOG-11181](/TOG/issues/TOG-11181)
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s13-881d21d: 881d21d — fix(events): reject malformed optional snapshot fields (#432)

- **Method:** `waived` (not an execution verdict).
- **Action:** Return scheduled events with malformed optional fields from TOG-11181’s mock REST fixture.
- **Expected:** Malformed optional fields are rejected; the mirror keeps the last good snapshot.
- **Evidence:** Attach the owning slice’s exact-head CI/local-fixture command, sanitized result, expected/actual fixture counts or signature digest and test-container guard receipt. Record waiver decision/reason on [TOG-9699](/TOG/issues/TOG-9699); no staging/production SQL or credentials.
- **Owner:** [TOG-11181](/TOG/issues/TOG-11181)
- **Reason:** Proposed staging-execution waiver: agent tests/probes may use only agent-testdb/agent-testredis or CI services, never staging/production databases; this data-plane/operator path needs an isolated fixture receipt from its owning slice. B4 must record acceptance with receipt or keep NEEDS WORK; this checklist is not approval or completed evidence.
- **Approver:** pending — CEO/DoE acceptance on [TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)

### s13-3676c1d: 3676c1d — fix(self-role): reject button labels over Discord's 80-char limit (#337)

- **Method:** `manual` (not an execution verdict).
- **Action:** Attempt to configure a disposable button self-role panel with an 81-character label through the normal config path.
- **Expected:** Configuration is refused at Discord’s 80-character label limit.
- **Evidence:** Record exact deployed head SHA, UTC start/end, fixture guild/channel/actor IDs (no tokens), sanitized request/result or screenshot and correlated log IDs; attach per-row PASS/NEEDS WORK and cleanup receipt to the B4 evidence table.
- **Owner:** [TOG-10087](/TOG/issues/TOG-10087), [TOG-10292](/TOG/issues/TOG-10292)

### s13-bffccf3: bffccf3 — fix(events): keep membership projections chronological (#387)

- **Method:** `automated` (not an execution verdict).
- **Action:** Run the membership chronology contract in `crates/core/tests/membership_contract.rs` (out-of-order and same-time join/leave events, replay).
- **Expected:** Membership projections follow event chronology and tie rules, not arrival order; replay is idempotent. Store contract only; the Postgres run is the `--ignored` step in the check workflow.
- **Evidence:** Attach exact-head fixture commands and PASS/NEEDS WORK result with sanitized request/response timestamps or signature/integrity assertion; fixture proof only, not a deployed-network soak receipt.
- **Owner:** [TOG-11149](/TOG/issues/TOG-11149)
- **Verification:** python3 scripts/cargo_cache.py run -- test -p two-bot-core --test membership_contract
