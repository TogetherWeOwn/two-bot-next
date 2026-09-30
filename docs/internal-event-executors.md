# Internal scheduled-event executors

`ActionExecutor::execute_event` handles the Discord/mirror portion of
`event.upsert`, `event.cancel` and `event.read`, independently of the HTTP
receiver. It uses Twilight routes and the existing single-attempt transport.
There is no gateway, live Discord test, timer, or RSVP change in this slice.

## Receiver boundary

The receiver must authenticate and validate requests, enforce action flags,
acquire the durable mutation claim, and resolve `event_key` in its configured
guild before constructing `EventCall`. `Upsert { event_id: None, .. }` creates;
a trusted mapped ID updates. Cancel/read take the trusted mapped ID, never a
raw website `event_id`. The receiver retains/records the key mapping and owns
terminal result replay; these executors neither add an HTTP route nor implement
that mapping/replay policy. A replay must not invoke the executor again.

Pass the caller's UTC-millisecond clock value as `observed_at`. After each valid
Discord response the executor awaits `ScheduledEventMirror::upsert` before
acknowledging. The `PgPool` implementation writes one `(guild_id, event_id)` row
via `website_store::upsert_event`; it never runs the poller's full-guild swap.
Null channel/description values replace older values. Unrelated rows survive.

## Legacy parity and strict mirror handling

Source comparison uses legacy two-bot revision
`e099be0382d640b6b714f3a397ef1421bb03615a`:

- `src/internal/discordActions.ts:86–102`: privacy level 2, voice entity 2 or
  external entity 3, omitted description stays omitted, external clears the
  channel and supplies location metadata.
- `src/internal/actions.ts:385–428`: created/updated/cancelled results contain
  only `outcome` and `event_id`. Cancellation is PATCH `{ "status": 4 }`, not
  DELETE; mapping is retained.
- `src/internal/actions.ts:446–473` and `discordActions.ts:243–273`: read returns
  exactly `outcome`, `event_id`, `name`, `starts_at`, `location`, `status`, and
  `observed_at`, without creator, attendees, end time, or channel ID. Lifecycle
  status uses `SCHEDULED`, `ACTIVE`, `COMPLETED`, `CANCELED`; the DB uses
  lowercase `cancelled`. The read result retains the upstream start string;
  the DB mirror normalizes it to UTC milliseconds.
- `test/e2e.internalactions.test.ts:252–260,1159–1248,1333–1415` provides the
  Launch Night request vector, mutation/replay and narrow read contracts.
  `test/unit.internalallowlist.test.ts:536–564` pins the placement body shape;
  `test/unit.eventcancel.test.ts:21–29` pins error classification.

Fresh-key cancellation of an already-cancelled event is still a Discord
rejection (typically 400). A 404 is also `discord_rejected`, not a fabricated
successful cancellation. Same-key idempotency is durable replay of the first
result. No 429, 5xx or uncertain mutation is automatically retried here.

Synchronous persistence adds stricter response checks than legacy mutations,
which ignored PATCH bodies: require a complete normalizable row, known status,
parseable start time, valid event ID, matching requested ID and matching guild
when present. A cancellation response must actually be cancelled. Unknown
numeric status names remain supported by `event_status_name`, but cannot be
persisted as a valid mirror row. Unreadable responses leave the prior row intact.

`EventActionError::is_safe_pre_mutation()` is true only for a proven Discord/local
refusal. Unreadable success and mirror failure are **not** safe claim-release
signals: preserve the execution fence and require reconciliation, never create
a second event to recover a missing acknowledgement. Mirror failures map to
`internal`; unreadable upstream rows map to `discord_unavailable`.

## Verification

Golden request JSON: `crates/discord/tests/fixtures/internal_event_bodies.json`.
The scripted REST tests assert create/update/cancel/read wires and results,
four status names, already-cancelled/404/error classification, malformed or
mismatched responses, synchronous acknowledgement, and write failure behavior.

Controller compilation must use the bounded cache wrapper:

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-discord --test internal_events
python3 scripts/cargo_cache.py run -- clippy -p two-bot-discord --all-targets --all-features -- -D warnings
TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/agent_test \
  python3 scripts/cargo_cache.py run -- test -p two-bot-discord --features db \
  --test internal_events -- --ignored
```

The opt-in DB test validates effective connection options **before** connecting,
pins the empty test password, rejects URL/socket/startup overrides and all
non-test targets, owns a unique schema, applies the real website-table migration,
and asserts mirror rows after each action and failed requests. It preserves
same-guild unrelated events and another guild's same-ID row. CI explicitly runs
it on its ephemeral `agent-testdb` service; production/staging are forbidden.

A missing/refused controller pool is not permission to build directly or use an
external target. Hosted CI keeps its existing compiling commands on ephemeral
runners, as required by the build-cache runbook.
