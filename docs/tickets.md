# Tickets domain and persistence

This slice supplies `two_bot_core::tickets` and `two_bot_cutover::tickets::TicketStore`, plus migration `0210_tickets.sql` in the S6 migration directory. It does **not** enable live ticket interactions, Ready hooks, or timers. Wiring must use the shared S4 interaction router and REST action executor once they merge; there is no private dispatcher or HTTP client here. Temporary voice rooms are unrelated and unchanged.

## Verified legacy sources

The ticket implementation retrieved at legacy `main` `b0a26a5e3882dd0784d208079f309893e2ede7e8` has the **same Git blob** (`76e484cb84dcd5b911abb725836f9b6251fec275`) as the parity matrix's frozen `d5d11793`:

- [Ticket implementation](https://github.com/TogetherWeOwn/two-bot/blob/d5d11793/src/discord/tickets.ts): button IDs, staff policy, five states, channel naming, transcripts, panel ensure, recovery and timers.
- [Initial schema](https://github.com/TogetherWeOwn/two-bot/blob/d5d11793/migrations/0013_tickets.sql) and [safety migration](https://github.com/TogetherWeOwn/two-bot/blob/d5d11793/migrations/0014_ticket_safety.sql): legacy table/column names and TEXT timestamps preserved.
- [Legacy tests](https://github.com/TogetherWeOwn/two-bot/blob/d5d11793/test/unit.tickets.test.ts).

Framework-specific references: [sqlx 0.9 transactions](https://docs.rs/sqlx/0.9.0/sqlx/struct.Transaction.html), [sqlx query builder](https://docs.rs/sqlx/0.9.0/sqlx/struct.QueryBuilder.html), and [Postgres advisory/row locks](https://www.postgresql.org/docs/current/explicit-locking.html#ADVISORY-LOCKS).

## Lifecycle and privacy invariants

- Stable button IDs: `two:tickets:open`, `two:tickets:claim`, `two:tickets:close`. Only configured-guild interactions are accepted; claim/close require the configured staff role, ManageChannels, or Administrator. The invoking member supplies roles/permissions, never command options.
- Reserve `creating` **before** channel creation. One active ticket per `(guild_id, opener_id)` spans `creating`, `open`, `closing`, and `cleanup_pending`. Both the active check and the configurable cooldown (default 300 seconds, measured from creation) serialize inside one per-member advisory-lock transaction; the partial unique index remains the final arbiter.
- Record the channel immediately, then activate. A crash between Discord creation and recording is discoverable by exact topic `two-ticket:<reservation-id>`. Recovery must successfully query the guild before treating a missing topic as absence. Access/transport failures are not evidence of absence.
- Claim is first-winner only. Begin-close records `closing_started_at`; subsequent snapshot/recovery writes must match it. After restoring opener permissions, an interrupted unsaved close may reopen. A late snapshot from that close cannot replace the current one.
- Freeze opener SendMessages, collect **all** history pages (100 messages/page), sort chronologically, preserve attachment URLs, format the snapshot, and save it. A partial/failed fetch must not be represented as a completed snapshot. Transcript message count is the total fetched count even when the 200,000 UTF-16-unit body cap truncates content. Rust avoids splitting a Unicode scalar at that cap.
- **Snapshot INSERT and transition to `cleanup_pending` commit together.** The store derives guild/channel/opener/claimant metadata from the locked ticket, not from caller input. Delete the channel only after this commit. A failed DELETE leaves cleanup recoverable; only successful deletion or Discord error code 10003 counts as gone, never 50013/403 or a textual error match.
- Transcript bodies live only in `ticket_transcripts`, not in funnel/audit events. Retention is exactly 90 days from capture; `purge_after <= now` is inclusive. Purge still applies when cleanup is pending: retention is a privacy ceiling. All store lookups, transitions, recovery, purge and member-erasure calls are guild-scoped. Member erasure uses the FK cascade to delete transcript bodies atomically.
- Recovery can reconcile legacy `closing` rows that already have a transcript, without replacing its body. A saved snapshot never reopens the ticket.

## Shared-router / executor integration checklist

1. Register all three component IDs through the shared router. Guild-fence first; Open defers an ephemeral reply, and Claim/Close additionally run `authorize`. Claim/Close resolve `by_channel` in the same guild; never trust an interaction-supplied ticket ID.
2. The Open executor uses the legacy `ticket-<safe username>` name, configured category, and topic above. Overwrites deny @everyone ViewChannel, grant the bot ViewChannel/SendMessages/ReadMessageHistory/ManageChannels, and grant opener + staff role ViewChannel/SendMessages/ReadMessageHistory. Suppress uncontrolled mentions. Controls are Claim/secondary and Close/danger; panel button is Open a ticket/primary.
3. After activation, post controls and the 90-day retention notice, mentioning only the opener. On failure, delete/clean up the channel; failed cleanup must remain recorded. `queue_open_rollback` requires a recorded channel. Uncertain create responses remain `creating` for topic recovery, never blindly retry channel creation.
4. On Ready load `recoverable()` from Postgres, apply `recovery_action`, ensure controls on each existing open ticket without duplicating them, then ensure the panel in the configured guild text channel. `panel_needed` examines the last 50 messages and only trusts a panel authored by this bot. Send `PANEL_TEXT` with no parsed mentions if absent.
5. Recovery ticks every 300 seconds, uses the 15-minute stale-work cutoff, and retries cleanup. Purge runs on Ready and every 3,600 seconds. Timers and overlapping-execution controls belong to the runtime layer, not this pure/store slice.
6. Permission changes, deletion, Ready hooks, and history pagination must be exercised against the shared mock Discord double before runtime enablement. No production guild/token or production/staging database is used by these tests.

## Verification

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo test -p two-bot-cutover --locked --test tickets_store -- --ignored
```

The last command opts into **agent-testdb** (`agent_test` user/database, empty password), not `DATABASE_URL`. Only `agent-testdb` is accepted locally. GitHub Actions may use `127.0.0.1` with its dedicated CI service container. Tests create and drop their own numeric-pid/counter schemas, never existing application tables. `tickets postgres` runs these tests in CI so they are not silently omitted by the default ignored-test behavior.

Integration proofs cover concurrent reservation/claim/close winners, the cooldown boundary, restart loading, stale-close fencing, transactional rollback on rejected transcript INSERT, interrupted create, legacy saved-close recovery, inclusive retention, guild isolation, erasure cascade, and upgrading the original legacy schema without dropping transcripts.
