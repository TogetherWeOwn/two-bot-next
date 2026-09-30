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

Serial event IDs are database-owned and deliberately excluded; insertion order
is checked by reading the rows in ID order. No other semantic row field should
be normalized away. Timestamp rendering uses explicit UTC millisecond strings,
and JSON metadata is compared structurally, preserving SQL NULL vs JSON objects.
