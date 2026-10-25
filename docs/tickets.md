# Tickets domain, persistence and runtime integration

The merged domain/store supplies `two_bot_core::tickets`, `two_bot_cutover::tickets::TicketStore`, and migration `0210_tickets.sql`. The runtime integration adds `bot::ticket_runtime` to the **existing** S4 `CommandRuntime` router/executor composition: exact button IDs, guild/member authorization, deferred ephemeral replies, lifecycle orchestration, panel/control ensure, and Ready recovery/purge. All ticket REST verbs live on the shared `ActionExecutor`; there is no private dispatcher or HTTP client. Temporary voice rooms are unrelated and unchanged.

**Source repair checkpoint, not accepted runtime parity:** periodic 300-second recovery / 3,600-second purge supervision and managed shutdown are implemented. Review repairs add cold RESUMED startup using authenticated bot-user identity, bounded button admission/work, rotating per-row recovery budgets, a reserved panel slot, bounded chronological transcript retention, effective history-permission proof, and fenced missing-open-channel retirement. Combined PostgreSQL/shared-mock regression definitions include cold RESUMED recovery/purge, typed absence/reservation release, stale-state/channel/saved-capture retirement fences, and cancelled unsaved close recovery alongside the existing lifecycle/retention proofs. Mock, pure-helper and paused-clock definitions cover the deadlines, Unicode/attachment cap, all-stored-page validation/counting, equal-millisecond ordering, unreadable-history no-repost, identity failures and shutdown. **Runtime Rust acceptance remains unverified:** the bounded Cargo wrapper refused the missing pool. CI run `36882135361` passed all 11 ticket-store tests, but runtime acceptance, Clippy and the runtime image build stopped on the same Twilight overwrite-model type mismatch before runtime tests executed. Channel creation now uses the required channel-model overwrite while permission updates retain the HTTP overwrite model; the existing exact-wire acceptance asserts all four least-privilege overwrites. The CI acceptance command's YAML colon-space parse defect is repaired without changing its command, runner, service isolation or gates; offline parsing and formatting are not green Rust CI. Prior independent security/code verdicts were CHANGES; renewed exact-head approval and actual green acceptance are required before reviewer squash merge. No deployment, staging/live execution, ownership transfer or soak acceptance is claimed; those holds remain separate.

## Verified legacy sources

The ticket implementation retrieved at legacy `main` `b0a26a5e3882dd0784d208079f309893e2ede7e8` has the **same Git blob** (`76e484cb84dcd5b911abb725836f9b6251fec275`) as the parity matrix's frozen `d5d11793`:

- [Ticket implementation](https://github.com/TogetherWeOwn/two-bot/blob/d5d11793/src/discord/tickets.ts): button IDs, staff policy, five states, channel naming, transcripts, panel ensure, recovery and timers.
- [Initial schema](https://github.com/TogetherWeOwn/two-bot/blob/d5d11793/migrations/0013_tickets.sql) and [safety migration](https://github.com/TogetherWeOwn/two-bot/blob/d5d11793/migrations/0014_ticket_safety.sql): legacy table/column names and TEXT timestamps preserved.
- [Legacy tests](https://github.com/TogetherWeOwn/two-bot/blob/d5d11793/test/unit.tickets.test.ts).

Framework-specific references: [sqlx 0.9 transactions](https://docs.rs/sqlx/0.9.0/sqlx/struct.Transaction.html), [sqlx query builder](https://docs.rs/sqlx/0.9.0/sqlx/struct.QueryBuilder.html), and [Postgres advisory/row locks](https://www.postgresql.org/docs/current/explicit-locking.html#ADVISORY-LOCKS).

## Lifecycle and privacy invariants

- Stable button IDs: `two:tickets:open`, `two:tickets:claim`, `two:tickets:close`. Only configured-guild interactions are accepted; claim/close require the configured staff role, ManageChannels, or Administrator. The invoking member supplies roles/permissions, never command options.
- Reserve `creating` **before** channel creation. One active ticket per `(guild_id, opener_id)` spans `creating`, `open`, `closing`, and `cleanup_pending`. Both the active check and the configurable cooldown (default 300 seconds, measured from creation) serialize inside one per-member advisory-lock transaction; the partial unique index remains the final arbiter.
- Prove the create receipt **before** recording, activating or posting opener controls: canonical nonzero channel ID, requested guild, GuildText type, requested category and exact topic `two-ticket:<reservation-id>`. Require the four exact requested permission overwrites (order-independent): @everyone denies ViewChannel; opener/staff retain ViewChannel, SendMessages and ReadMessageHistory; bot additionally retains ManageChannels. Missing, duplicate, malformed, contradictory or unrequested overwrites are not private-channel proof. These channel overwrites do **not** promise privacy from Discord administrators.
- A 2xx status or parseable ID alone is uncertain, not a safe-pre-mutation rejection. Incomplete/contradictory receipts are refused without a fallback fetch or permission repair. Keep the creating reservation; repeated Open cannot issue a fresh create. Members get a short retry/recovery message, never a ready link. The bounded `ticket creation receipt unproven` diagnostic contains a fixed `reason`, `next_action=inspect_ticket_creation` and `retryable=false`, never remote names, topics, message content or tokens. Inspect the reservation and configured permission contract; do not blindly retry creation or delete an unproven channel.
- A crash between Discord creation and recording is discoverable by exact topic. Recovery must successfully query the guild before treating a missing topic as absence; access/transport failures are not evidence of absence. A matching listing or recorded ID selects a candidate only. Fetch and prove its ID, guild, type, category, reservation topic and private overwrites **before** recording or advancing rollback state/deletion. Failed proof leaves `creating` unchanged and never deletes. Proven private candidates keep the existing cleanup flow; failed deletion remains recoverable and open-rollback cleanup re-proves the contract on retry. Captured-close cleanup retains its existing close-token/commit fences and does not require opener SendMessages to be restored.
- Live destination moves apply to newly reserved tickets. `DISCORD_TICKET_CATEGORY_ID` and `DISCORD_TICKET_STAFF_ROLE_ID` are re-read live, but in-flight rows are verified against the live values, so a ticket created under previous destinations stays uncertain while the values differ: `creating` rows are retained, `cleanup_pending` open rollbacks are not deleted, and `Open` rows do not reattach controls, until the destinations are restored or the channel is removed. Persisting creation-time destinations per ticket needs a store migration and is future work, not this change.
- Claim is first-winner only. Begin-close records `closing_started_at`; subsequent snapshot/recovery writes must match it. After restoring opener permissions, an interrupted unsaved close may reopen. A late snapshot from that close cannot replace the current one.
- Freeze opener SendMessages, collect up to 10 history pages (100 messages/page, newest 1000) plus one boundary probe, preserve attachment URLs, and save a chronological snapshot. The runtime retains only the oldest-first 200,000 UTF-16-unit prefix plus the truncation marker while fetching, validating and counting every page up to the cap. Strict descending snowflake/cursor and nonincreasing timestamp evidence rejects duplicates or inconsistent history without a history-sized ID set. Equal-millisecond messages use older snowflakes first. Formatting never splits a Unicode scalar. A partial/failed fetch or cancelled capture must not become a completed snapshot; message count covers every stored message (at most 1000) even after truncation, and deeper histories complete with the truncation marker while their oldest messages stay unstored.
- **Snapshot INSERT and transition to `cleanup_pending` commit together.** The store derives guild/channel/opener/claimant metadata from the locked ticket, not from caller input. Delete the channel only after this commit. A failed DELETE leaves cleanup recoverable; only successful deletion or Discord error code 10003 counts as gone, never 50013/403 or a textual error match.
- Transcript bodies live only in `ticket_transcripts`, not in funnel/audit events. Retention is exactly 90 days from capture; `purge_after <= now` is inclusive. Purge still applies when cleanup is pending: retention is a privacy ceiling. All store lookups, transitions, recovery, purge and member-erasure calls are guild-scoped. Member erasure uses the FK cascade to delete transcript bodies atomically.
- Recovery can reconcile legacy `closing` rows that already have a transcript, without replacing its body. A saved snapshot never reopens the ticket.

## Open-button state and next-try guidance

Pressing Open again explains the invoking member's existing ticket in this guild, using the status and channel from the same reservation transaction:

- **Creating / recovery pending:** the channel is not yet proven ready, even if its ID is recorded. Wait for creation/recovery; ask staff if it stays stuck. No channel link is offered.
- **Open with a channel:** the reply links the existing ticket. If Discord will not let you view it, ask staff to check access; the reply does not recheck or restore permissions. Open without a channel instead asks staff for help.
- **Closing:** wait while the transcript is captured; ask staff if it stays stuck. The channel is not offered as usable.
- **Cleanup pending:** wait for cleanup before opening another ticket; ask staff if it stays stuck. This state can also represent an opening rollback, so it does not promise a saved transcript.
- **Cooldown:** the private reply gives the next time to **try**, in Discord-localized and relative form. It is not a booking or a promise of availability. Display rounds upward to whole seconds, but admission keeps its exact millisecond boundary. Future stored creation times remain future deadlines; the configured duration keeps its existing saturating arithmetic. A date outside the conservative 1970–9999 display range is explained as outside that range, not clipped to an earlier false deadline. Invalid stored timestamps remain store errors, never cooldown or absence.

These explanations create no ticket, channel, controls, transcript, repair or retry job. Active-ticket precedence, the guild/opener lock, cooldown duration, authorization and ephemeral acknowledgement/mention suppression remain unchanged. Existing recovery supervision, not a repeated button press, owns lifecycle progress.

## Private ticket lists

`/mytickets` shows only the invoking member's non-closed tickets in the configured
guild: ticket ID, lifecycle status, claim state and an Open row's stored channel
link. An empty list says there are no open tickets. The reply stays within Discord's
2,000 UTF-16-unit limit and states how many rows are shown when truncated.

`/ticket-queue [page]` shows the same guild's non-closed tickets, five per page,
ordered by creation instant then ticket ID. Pages start at 1 and include the opener,
status, Discord-localized creation time and claim state. Queue access uses the
existing ticket staff role, Manage Channels or Administrator policy; it adds no new
role. Invalid and unavailable pages ask staff to start again at page 1.

Both commands publish only when the boot ticket configuration is valid. Replies
are private and suppress mentions; persisted display values are single-line,
escaped and bounded. Creating, Closing and Cleanup Pending rows never offer a
channel link. An Open row's channel link is a stored convenience, not fresh proof
of channel liveness or permissions: Discord may render a deleted channel, and the
list never promises readiness or recovery. Closed rows are omitted.

These paths only read ticket rows. They do not fetch Discord channels, reserve,
claim, close, repair, capture transcripts, write audit records or schedule recovery.
A database failure returns private retry guidance without lifecycle work. Existing
button receipts, reserve policy and supervisor retries remain unchanged.

## Shared-router / executor integration checklist

1. Register all three component IDs through the shared router. Guild-fence first; Open defers an ephemeral reply, and Claim/Close additionally run `authorize`. Claim/Close resolve `by_channel` in the same guild; never trust an interaction-supplied ticket ID.
2. The Open executor uses the legacy `ticket-<safe username>` name, configured category, and topic above. Overwrites deny @everyone ViewChannel, grant the bot ViewChannel/SendMessages/ReadMessageHistory/ManageChannels, and grant opener + staff role ViewChannel/SendMessages/ReadMessageHistory. Suppress uncontrolled mentions. Controls are Claim/secondary and Close/danger; panel button is Open a ticket/primary.
3. After activation, post controls and the 90-day retention notice, mentioning only the opener. On failure, delete/clean up the channel; failed cleanup must remain recorded. `queue_open_rollback` requires a recorded channel. Uncertain create responses remain `creating` for topic recovery, never blindly retry channel creation.
4. On READY initialize the bot-user ID; on RESUMED resolve it using the shared executor's authenticated `GET /users/@me`, independently of application registry synchronization. Both signal recovery and purge. If the cold RESUMED lookup fails, periodic recovery retries authenticated identity within six seconds instead of waiting indefinitely for another gateway event. Load `recoverable()` from Postgres, apply `recovery_action`, ensure open-ticket controls and the configured guild text-channel panel. Before interpreting empty history, prove effective ViewChannel + ReadMessageHistory from explicit bot-member grants or guild/member roles with Discord overwrite precedence. Unreadable or malformed evidence never proves message absence. `panel_needed` examines the last 50 messages and trusts only this bot's authorship; send `PANEL_TEXT` with no parsed mentions if absent. Typed code 10003 for an open ticket retires its reservation under a guild/channel/state/saved-capture row-lock fence without fabricating a transcript.
5. Recovery ticks every 300 seconds and uses the 15-minute stale-work cutoff. Each pass skips settled Open rows the cursor has passed without attempting them, while rows in an in-flight status (creating, closing, cleanup_pending) are always re-attempted even behind the cursor — they turn actionable only after the 15-minute stale cutoff or move status across passes — and the cursor never moves backwards. Failed, cancelled or timed-out Open work keeps bounded retry eligibility in the existing supervisor without a restart: the row is retained before its work is awaited, becomes due after a doubling backoff (300 s up to hourly), and is re-attempted oldest-due first after each forward scan inside the same 180-second row budget (at most 64 rows per pass from at most 128 retained rows). Retries never move the forward cursor, re-read the row before any Discord I/O so terminal or removed rows shed their entry, and re-prove the full creation receipt and readable history before posting; uncertain evidence refuses instead of posting a possible duplicate. Successful unchanged Open rows are never retained and stay zero-work, while budget-truncated rows ahead of the cursor resume on the next pass. Purge runs on startup notification and every 3,600 seconds. The gateway-owned supervisor coalesces notifications, skips elapsed busy ticks, and bounds recovery/purge at 240/60 seconds. Recovery loads rows within 20 seconds, uses 20-second item budgets within a 180-second row budget, sorts rows by the decoded millisecond/id cursor key, skips settled Open rows at or behind the persisted cursor without attempting them, advances the cursor before each await without ever moving it backwards, and reserves a separate 30-second panel slot. Steady-state passes with no newer row and no due retry attempt zero tickets. Skip and truncation outcomes are observable log lines with stable identifiers: `ticket recovery skipped behind-cursor rows` (info, `skipped=<n>` drained behind-cursor Open rows) and `ticket recovery budget exhausted` (warn, `dropped=<n>` post-skip rows never attempted). This permits five slow successful role-backed panel calls and still bounds identity + row load + row work + panel at 236 seconds within the 240-second supervisor deadline. Slow prefixes cannot permanently starve later rows or the panel. Recovery/buttons share one lane; button admission is bounded at 10 seconds and total button work at 120 seconds. Purge has its own lane so slow Discord work cannot postpone retention cleanup. Shared shutdown cancels and joins ticket buttons and maintenance before shard exit; an aborted shard scope also cancels work. Cancellation leaves durable creating/closing/cleanup state for restart recovery.
6. Permission changes, deletion, Ready hooks, and history pagination must be exercised against the shared mock Discord double before runtime enablement. No production guild/token or production/staging database is used by these tests.

## Observability

Every retire, abandon, purge and successful staff-button claim/close
executed through the button path emits one `ticket_lifecycle` log line and
one `two_bot_ticket_lifecycle_total{event,outcome}` sample with a
fixed-vocabulary cause; the `ticket recovery deferred` and `ticket action
incomplete` warns cite the action, ticket and status. Recovery completions
and the open-path controls-post rollback reach their terminal states
without emitting. The series, labels and causes are listed in
[ticket-lifecycle-observability.md](ticket-lifecycle-observability.md).

Open retries report one line per attempt outside the lifecycle series:
`ticket open retry deferred` (warn, `retryable=true`), `ticket open retry
repaired` and `ticket open retry cancelled` (info, `retryable=false`), each
with `ticket_id`, a bounded `reason` token (the shared `db_cause`/executor
`cause()` classes, `invalid_evidence`, `interrupted`, `status_changed`,
`missing_row`, `retry_succeeded`), a fixed `outcome`
(`deferred`/`repaired`/`cancelled`) and a `next_action`
(`await_retry`/`none`). No line carries channel, topic, message or member
content. A pass cut short by the row budget logs `ticket open retry budget
exhausted` (warn, `dropped=<n>`) for retries never attempted, mirroring the
forward-scan truncation line.

## Component admission and overload

Ticket button presses never enter the shared user-slot/lane admission, so the ticket runtime admits them itself, synchronously, before spawning any handler task:

- At most `MAX_TICKET_COMPONENT_IN_FLIGHT` (16) component executions run per runtime, and one member holds at most the shared per-member budget (3). Both checks and the spawn are atomic: beyond either bound the dispatch refuses immediately and queues no waiter.
- A refused but otherwise valid component gets exactly one ephemeral busy acknowledgement (immediate type-4 reply: the bot is busy, try again) on the shared busy lane (capacity 8). When the busy lane is also full the refusal stays silent. Refused work creates no ticket row, sends no other Discord effect, defers nothing, and promises no recovery or automatic retry.
- Admission is held until the handler actually settles: the permit releases on success, failure, cancellation and shutdown, so later work and other members keep making progress. Authorization, guild fencing, durable close/recovery semantics and the existing button deadlines (10-second lane wait, 120-second total) are unchanged.

Overload evidence is machine-identifiable and label-free: every refusal increments `two_bot_ticket_component_drops_total` (no member, token or interaction labels), and the log samples `ticket_component_admission_saturated` at most once per 60 seconds per runtime with the interaction id only.

**Next action:** when `ticket_component_admission_saturated` appears or the drops counter climbs, the runtime is shedding ticket bursts as designed — no restart or config change is needed. If saturation persists outside bursts, look for stuck handlers holding admission (deferred components awaiting slow Discord or store calls) before considering bound changes; do not retune lane sizes outside tickets from this signal.

## Verification

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- clippy -p two-bot-cutover --all-targets --locked -- -D warnings
python3 scripts/cargo_cache.py run -- test -p two-bot-core --locked tickets::
python3 scripts/cargo_cache.py run -- test -p two-bot-discord --locked --test ticket_executor
python3 scripts/cargo_cache.py run -- test -p two-bot --locked --bin two-bot ticket_runtime:: -- --include-ignored
python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --locked --test tickets_store -- --ignored
```

The focused Open-retry lifecycle proof (no live Discord: the shared
loopback double plus a disposable `agent-testdb` database) is one command:

```sh
TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_local \
python3 scripts/cargo_cache.py run -- test -p two-bot --bin two-bot \
open_retry_acceptance_tests -- --test-threads=1
```

On the persistent controller, all compiling commands use the [bounded cache wrapper](build-cache.md); do not bypass a refused lease. The last command opts into **agent-testdb** (`agent_test` user/database, empty password), not `DATABASE_URL`. Only the `agent-testdb` hostname is accepted, locally and in CI. The `tickets postgres` job uses `[self-hosted, two-selfhosted]`, a pinned Rust job container and an `agent-testdb` service without published host ports. Tests create and drop their own numeric-pid/counter schemas, never existing application tables, and CI executes the opt-in proofs so they are not silently omitted by the default ignored-test behavior.

Integration proofs cover concurrent reservation/claim/close winners, the cooldown boundary, restart loading, stale-close fencing, transactional rollback on rejected transcript INSERT, interrupted create, legacy saved-close recovery, inclusive retention, guild isolation, erasure cascade, and legacy upgrades without dropping transcripts. The frozen `0013 → 0014 → 0210` path is tested under UTC, Asia/Tokyo and America/New_York: populated overlong deadlines are clamped to capture + 2160 elapsed hours, offset-bearing deadlines are compared by instant, and shorter deadlines are never extended. The shared database-role matrix grants runtime CRUD on both ticket tables and excludes the website reader; role acceptance tests exercise those rights and denials.
