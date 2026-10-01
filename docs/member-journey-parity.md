# Database-backed member journey

This fixture tests funnel persistence, not onboarding replies, role awards,
leveling, a Discord websocket, or analytics report rendering. All database work
uses the existing gateway test-database guard and an isolated disposable schema.
No staging or production database is a valid test target.

## Legacy derivation

The reference is `TogetherWeOwn/two-bot` at
`96777468472f23a02a1e97a43ffab3912fe5df2a`. Source links below are pinned so a
later legacy change cannot silently redefine the expectation. The golden is a
hand-computed journey, **not** an export of the Rust implementation's output.
The legacy report tests seed aggregates; we adapt their individual row contracts
rather than pretend their report totals describe this single member.

- [e2e.onboarding.test.ts:130–173](https://github.com/TogetherWeOwn/two-bot/blob/96777468472f23a02a1e97a43ffab3912fe5df2a/test/e2e.onboarding.test.ts#L130-L173):
  a pending join already produces `member_join`, but not `gate_cleared`.
- [e2e.onboarding.test.ts:206–227](https://github.com/TogetherWeOwn/two-bot/blob/96777468472f23a02a1e97a43ffab3912fe5df2a/test/e2e.onboarding.test.ts#L206-L227):
  the true-to-false pending transition records source `gateway` and projects
  `members.gate_cleared_at`, independently of welcome configuration.
- [e2e.funnel-accuracy.test.ts:113–156](https://github.com/TogetherWeOwn/two-bot/blob/96777468472f23a02a1e97a43ffab3912fe5df2a/test/e2e.funnel-accuracy.test.ts#L113-L156):
  joins credit `invite:<code>`; message and voice firsts credit `channel:<id>`;
  each milestone is a separate event with a corresponding member timestamp.
  [Lines 245–254](https://github.com/TogetherWeOwn/two-bot/blob/96777468472f23a02a1e97a43ffab3912fe5df2a/test/e2e.funnel-accuracy.test.ts#L245-L254)
  exclude unknown-start ends from measured duration averages, not from event counts.
- [e2e.attribution-guild-scope.test.ts:99–129](https://github.com/TogetherWeOwn/two-bot/blob/96777468472f23a02a1e97a43ffab3912fe5df2a/test/e2e.attribution-guild-scope.test.ts#L99-L129):
  the third message has its own `third_message_at`, invite snapshots retain
  `(guild_id, code, uses, channel_id, updated_at)`.
  [Lines 152–205](https://github.com/TogetherWeOwn/two-bot/blob/96777468472f23a02a1e97a43ffab3912fe5df2a/test/e2e.attribution-guild-scope.test.ts#L152-L205)
  motivate a foreign-guild control with the same member/invite identifiers.

The seeded report tests do not specify all runtime fields. These are derived
explicitly from the legacy producer/projection instead:

| Field | Legacy rule |
| --- | --- |
| `events.guild_id`, `member_id` | Event emitter identities; never inferred from the DB. [events.ts:84–100](https://github.com/TogetherWeOwn/two-bot/blob/96777468472f23a02a1e97a43ffab3912fe5df2a/src/core/events.ts#L84-L100). |
| `events.occurred_at` | Emitter UTC timestamp, frozen by the script instead of wall-clock now. Same reference. |
| `events.source` | Invite for joins, gateway for gate/leave, channel for messages/voice. [handlers.ts:87–149](https://github.com/TogetherWeOwn/two-bot/blob/96777468472f23a02a1e97a43ffab3912fe5df2a/src/core/handlers.ts#L87-L149), [367–397](https://github.com/TogetherWeOwn/two-bot/blob/96777468472f23a02a1e97a43ffab3912fe5df2a/src/core/handlers.ts#L367-L397). |
| `events.metadata` | SQL NULL without extra data; join carries `inviterId`; voice ends carry `startKnown`, `startedAt`, `durationSeconds`. [handlers.ts:101–108](https://github.com/TogetherWeOwn/two-bot/blob/96777468472f23a02a1e97a43ffab3912fe5df2a/src/core/handlers.ts#L101-L108), [311–325](https://github.com/TogetherWeOwn/two-bot/blob/96777468472f23a02a1e97a43ffab3912fe5df2a/src/core/handlers.ts#L311-L325). |
| `events.idempotency_key` | Guild/member/type for lifetime milestones; repeatable join/leave/boundaries add timestamp; voice boundaries also add channel source. [events.ts:120–169](https://github.com/TogetherWeOwn/two-bot/blob/96777468472f23a02a1e97a43ffab3912fe5df2a/src/core/events.ts#L120-L169). |
| Message milestones | First, second, third messages each fill one rung; further messages update activity only. [handlers.ts:152–204](https://github.com/TogetherWeOwn/two-bot/blob/96777468472f23a02a1e97a43ffab3912fe5df2a/src/core/handlers.ts#L152-L204). |
| Voice boundary order | On a move, end the old channel then start the new channel at the same timestamp. [client.ts:579–621](https://github.com/TogetherWeOwn/two-bot/blob/96777468472f23a02a1e97a43ffab3912fe5df2a/src/discord/client.ts#L579-L621). |
| Known voice end | Credit the open channel and measure seconds from start to end. Unknown end has `startedAt: null`, `durationSeconds: null`, `startKnown: false`. [handlers.ts:265–348](https://github.com/TogetherWeOwn/two-bot/blob/96777468472f23a02a1e97a43ffab3912fe5df2a/src/core/handlers.ts#L265-L348). |
| Resume | Clear every unproven open session, without inventing end rows; a subsequent leave is unknown-start. [client.ts:625–642](https://github.com/TogetherWeOwn/two-bot/blob/96777468472f23a02a1e97a43ffab3912fe5df2a/src/discord/client.ts#L625-L642), parity §3. |
| `members.joined_at`, `join_source`, `left_at` | Join projects its timestamp/source and clears leave; final leave sets `left_at`. [eventStore.ts:111–147](https://github.com/TogetherWeOwn/two-bot/blob/96777468472f23a02a1e97a43ffab3912fe5df2a/src/store/eventStore.ts#L111-L147). |
| `members.gate_cleared_at`, `first_message_at`, `third_message_at`, `first_voice_at` | First milestone timestamp wins. Same projection reference. |
| `members.last_active_at` | Messages/voice update recency; server leave alone does not. [handlers.ts:159–175](https://github.com/TogetherWeOwn/two-bot/blob/96777468472f23a02a1e97a43ffab3912fe5df2a/src/core/handlers.ts#L159-L175), [337–341](https://github.com/TogetherWeOwn/two-bot/blob/96777468472f23a02a1e97a43ffab3912fe5df2a/src/core/handlers.ts#L337-L341). |
| `members.is_bot`, `inactive_flagged_at` | Human fixture (`0`); no inactivity job, so NULL. [e2e.funnel-accuracy.test.ts:147–156](https://github.com/TogetherWeOwn/two-bot/blob/96777468472f23a02a1e97a43ffab3912fe5df2a/test/e2e.funnel-accuracy.test.ts#L147-L156). |
| Invite snapshot fields | Code/use count/channel come from the REST snapshot; timestamp is capture time. The same code in another guild is a separate row. [e2e.attribution-guild-scope.test.ts:124–129](https://github.com/TogetherWeOwn/two-bot/blob/96777468472f23a02a1e97a43ffab3912fe5df2a/test/e2e.attribution-guild-scope.test.ts#L124-L129). |

Serial event IDs and `recorded_at` are database-owned and deliberately excluded;
insertion order is checked by reading the rows in ID order. No other semantic
row field is normalized away. Timestamp rendering uses explicit UTC millisecond
strings, and JSON metadata is compared structurally, preserving SQL NULL vs JSON
objects. The Rust schema represents legacy's human flag as BOOLEAN (`false`),
not the report fixture's numeric `0`.

## Script and hand-computed totals

The expectation lives in
`crates/bot/src/gateway_tests/member_journey.golden.json`; the executable script
is its sibling `member_journey.rs`. All stamps below are on 2026-09-30 UTC.

| Time | Dispatch / expected consequence |
| --- | --- |
| 11:59:00–30 | Foreign guild `3333` creates `journey`, then member `77` joins pending. Counter becomes 8, inviter is 88. This is the same member/code as the main guild, not a manually seeded control. |
| 11:59:40–50 | Main guild `2222` creates `side` and `journey`. Both snapshots must survive; inviter 99 and channel 4444 are retained. No funnel event is emitted for creation. |
| 12:00:00 | Member 77 joins pending; REST counter `journey` increases 0→1 while `side` stays 0. One attributed join, no gate clear. Observation is at 12:00:05, so snapshot time differs from join time. |
| 12:01:00 | Pending true→false: one gate event and member gate timestamp. A second cleared update at 12:01:30 adds nothing. |
| 12:02, 03, 04, 05 | Four messages: first/second/third events only. The fourth updates activity; the observation clock is intentionally 12:06 while message payload times remain authoritative. |
| 12:10 | Voice join A (5555): start then lifetime first-voice event. A same-channel mute update at 12:11 adds nothing. |
| 12:20 | Move A→B (6666): end A (600 seconds), then start B at the same instant. A conflicting duplicate dispatch sequence is rejected before touching the cache. |
| 12:25:30 | Leave B: known end, 330 seconds. |
| 12:30 | Join A again: a start, not a second lifetime first-voice milestone. |
| 12:40 | RESUMED: open count 1→0; no synthetic end event. The cached channel remains available. |
| 12:41 | Leave A after resume: unknown-start end with both duration and start timestamp NULL. Never claim 660 seconds spanning an unproven outage. |
| 13:00 | Leave guild: one gateway leave, member left timestamp; last activity stays 12:41. |

Total: **14 events** (13 main + 1 foreign), **2 member projections**, **3 invite
snapshots**. Main has 1 join + 1 gate + 3 message milestones + 3 voice starts +
1 first-voice + 3 voice ends + 1 leave. The snapshot assertions run both after
invite creation and at the end, catching accidental full-replacement deletion.
The entire table is compared, not just selected rows or positive existence.

The separate transactional regression distinguishes an unavailable snapshot
(`None`, preserve rows) from a successful empty snapshot (`Some([])`, remove
missing codes). A bad capture timestamp fails after funnel writes and proves
snapshot/event/projection/checkpoint rollback. A foreign-guild snapshot is
rejected by the same writer.

## Minimal implementation seam and limits

`Pipeline::handle_at` injects observation time without replacing gateway payload
timestamps. Normal `handle` still observes the real clock. The store's optional
snapshot hook stages successful REST replacements and invite-create upserts in
`FunnelBatch`; `GatewaySessionStore::commit_dispatch` writes them with events,
activity and the dispatch checkpoint in one transaction. Failed commits retain
the existing fail-stop runner contract; do not continue a mutated pipeline after
an SQL failure.

This test uses `ScriptedInvites`, not live Discord REST. It proves the real
pipeline-to-Postgres persistence path, **not** deployment, live invite fetching,
or invite-baseline hydration on a cold process restart. Resume here means a
RESUMED dispatch on the existing cache; the separate gateway recovery suite owns
cold restart. Onboarding/leveling runtime effects remain out of scope.

## Run and database containment

```sh
TWO_GATEWAY_TEST_DATABASE_URL=postgres://agent_test@agent-testdb:5432/agent_test \
  cargo test -p two-bot --locked gateway_tests::member_journey \
  -- --ignored --test-threads=1
```

`gateway_tests::TestDb::new` checks the dedicated URL before opening a connection,
accepts only the test host/CI loopback and `agent_test` user/database, and creates
an isolated schema. It never falls back to runtime `DATABASE_URL`. Both journey
tests drop their own schema even when their assertions panic, then propagate the
failure. The existing `Gateway restart and transaction tests`
CI step includes this module, so these are executed tests, not silently skipped
coverage on `check`. Build output belongs in a run-owned `CARGO_TARGET_DIR` and
is removed at handoff; no shared cache should be deleted.
