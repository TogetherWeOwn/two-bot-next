# Game-activity aggregate report: staff and operator guide

On-demand, read-only per-game aggregates for one guild: total play time,
in-voice overlap and session counts. Staff can use `/game-mix` privately
in Discord; operators can print the existing CLI JSON on stdout.
Both answer "what did this guild play, and how much of it was in voice?"
without exposing per-member data. No scheduling or CSV/file export is added.
For the pipeline behind the numbers see the `game_activity` module docs
(`crates/core/src/game_activity.rs`) and the
[privacy analytics note](privacy.md#what-is-stored).

## In Discord: `/game-mix`

- No options. Published with DMs disabled and **Manage Server** (Manage Guild)
  as its default permission. The runtime also requires Manage Guild or
  Administrator; overriding picker visibility cannot bypass that check.
- Only the invoking, configured guild is read. Missing/foreign guilds are
  ignored before any handler or query. No guild ID can be supplied.
- **The live guild is refused before reading.** This command has no equivalent
  to `--allow-live-guild`: staff permission or a configured production identity
  is not rollout approval. This slice does not activate production reporting.
- Every answer is private (ephemeral), with mentions disabled. It is one JSON
  fenced block, using the CLI keys below, capped at ten games and 2000 UTF-16
  units. Order and numeric totals match the CLI. Hostile labels are made inert;
  labels are shortened to 24 UTF-16 units only when needed to fit the complete
  JSON, with an explicit note. `truncated` is true when rows or labels are cut. No per-member fields appear.
- Empty storage returns `rows: []`, `truncated: false` and
  `no game activity recorded for this guild`. It is a valid result, not an error.
  A store failure returns private **try again** guidance, with no JSON or
  fabricated totals. It does not migrate or create a missing table.
- Reads use the same guild aggregate reader and report ordering as the CLI,
  never live presence/member lookups. There are no DMs, channel posts, audit
  writes, exports or changes to the collector. Totals can lag the hourly flush
  and reflect retention/opt-outs, not a new reporting window.

The remaining invocation, environment and stdout sections describe the CLI;
its 100-row cap, live-guild opt-in and pinned output shape are unchanged.

## When to run it

- A moderator or operator asks for a guild's game mix over the stored
  window (for example, which games dominate voice hangouts).
- Sanity-checking the game-activity pipeline after a deploy: seed or
  production-adjacent aggregates should render with the pinned shape.
- An empty guild is a valid answer, not an error: it prints the header
  shape plus a hint. Do not re-run with different flags to "find" data.

## Invocation

Two report arguments exist (`--guild`, `--allow-live-guild`), plus `--help`.
Anything else is refused. `--help` short-circuits: combined with any other
argument it still prints usage and exits 0 without further validation,
except a malformed safety opt-in (`--allow-live-guild=x` or a repeat),
which is refused at parse with exit 2 before help is reached.

```sh
cargo run -p two-bot-cutover --bin game-activity-report -- --guild <GUILD_ID>
cargo run -p two-bot-cutover --bin game-activity-report -- --guild <GUILD_ID> --allow-live-guild
cargo run -p two-bot-cutover --bin game-activity-report -- --help
```

- `--guild <ID>`: Discord snowflake of the one guild to report. Required
  for a scoped report; without it the bin prints the unscoped hint and
  exits 0 without opening a database.
- `--allow-live-guild`: opens the live-guild fence for guild
  `326474832151838730` only. That guild is otherwise refused. Use it
  solely for an owner-approved rollout, never for routine checks.
- `--help`: prints usage and exits 0 without touching a database.
- `--guild=<ID>` works like `--guild <ID>`. `--allow-live-guild` takes no
  value: `--allow-live-guild=x` (or a space-separated value) is refused with
  `input error: --allow-live-guild is a bare opt-in and does not accept a value`;
  repeating the bare flag is refused with
  `input error: --allow-live-guild must not be repeated`.
- Unknown flags and positional arguments are refused with
  `unknown or malformed argument`.

Exit codes: `0` a report was printed (populated, empty and unscoped
alike); `2` usage error, missing `TWO_DATABASE_URL`, live-guild refusal,
query failure, or stdout write failure; `1` the database could not be
opened (details redacted).

## Environment and role

- `TWO_DATABASE_URL`: required whenever `--guild` is given. The unscoped
  and `--help` paths never read it and never open a connection.
- `TWO_DB_POOL_MAX`: optional pool cap (default 5); `1` is enough for a
  one-shot report.
- `TWO_DATABASE_TLS`: unset means `Required`. Only remote hosts with an
  `sslmode` of `require`, `verify-ca` or `verify-full` in the URL connect
  (`disable`, `allow` and `prefer` are refused); loopback, single-label and
  unix-socket URLs are refused. A refused URL exits 1 with only
  `cannot open database; connection details redacted`. See
  [database TLS](database-tls.md). `local-only` is test-only.
- Run with a read-only database login that holds `SELECT` on
  `game_activity_guild_aggregates` (see [database roles](database-roles.md)).
  For inspection, a login holding `two_bot_migrator_ro` (SELECT on bot
  tables) suffices; the app pool (`two_bot_runtime`) also reads.
  The report applies no migrations and creates no schema: a missing table
  fails loudly instead of emitting an empty report. Never point it at a
  database you would not read from directly.

## Reading the JSON

Top level (keys are exactly these; an empty report adds only `hint`):

| Field      | Meaning |
|------------|---------|
| `guild_id` | Echo of `--guild`; `null` when no `--guild` was given. |
| `rows`     | One object per game, sorted by `total_play_ms` desc, then `in_voice_ms` desc, then title asc (the untitled row sorts first among full ties). At most 100 rows. |
| `truncated`| `true` when more than 100 games were cut; `false` otherwise. |
| `hint`     | Present only when `rows` is empty: `no game activity recorded for this guild` for a scoped run, or the `--guild` usage hint for an unscoped run. |

Each row is exactly `game`, `total_play_ms`, `in_voice_ms`,
`session_count`. There is no member, user or player field anywhere: the
table carries no member column by construction, and the report must not
invent one.

- **Sessions.** One session is one member's contiguous run of Playing (or
  Streaming) observations for one game: observations merge while each step
  stays within the collector's gap timeout (15 minutes); a game change, a
  streaming-flag change, or that member's own non-playing delta closes the
  run. Silence alone does not: the collector fills same-game gaps wider
  than the timeout with synthetic ticks, so a quiet stretch between two
  updates for the same game stays one session, and only the 24h carry TTL
  ends a run with no fresh observation. Other members' observations never
  disturb it. `session_count` is how many such intervals folded into the row. The
  collector flushes hourly and closes each still-playing interval at the
  flush, reopening it for the next window, so one long play can span more
  than one session: it closes once at the first flush after it starts, and
  again in each flush window in which that member's presence updates.
  Windows with no presence update from them persist nothing, and the next
  update bridges the silent stretch into one session. Treat `session_count`
  as a count of persisted intervals, not distinct play sessions.
- **Play time.** `total_play_ms` sums session wall-clock spans
  (`last - first`). A session built from a single observation still counts
  one minute (one polling tick, conservative against inflation) instead of
  vanishing as zero-length.
- **Carry.** The gateway sends presence only on change, so the collector
  carries each member's last-seen game forward as their current state until
  the next delta, rather than counting one tick per sighting. A long
  session therefore means sustained presence across the gap timeout, not
  one poll. The carry expires after 24 hours (`GAME_ACTIVITY_CARRY_TTL_MS`)
  with no fresh observation: a long silent stretch stops accruing play
  time. Rows appear only after a flush, so a report run right after play
  can lag by up to an hour. Custom status never creates a session;
  Streaming counts via the same carry with a streaming flag.
- **In-voice overlap.** `in_voice_ms` sums each session's intersection
  with that member's voice spans (overlapping spans merged, never
  double-counted); it never exceeds `total_play_ms`. With voice
  persistence off but presence on, play aggregates still persist while
  `in_voice_ms` stays 0.
- **Untitled.** Sessions with no game title store as `''` and render as
  `"(untitled)"`.
- **Opt-out and toggles.** Members with a guild opt-out row and guilds
  with presence analytics off never persist aggregates: opted-out play
  drains and drops before the write, so a low row may mean opt-outs, not
  missing collection.

## Read-only guarantees

- The only statement is one guild-scoped aggregate read
  (`WHERE guild_id = $1` on `game_activity_guild_aggregates`); rows from
  other guilds are filtered in SQL and again at render.
- The pool opens with migrations off, so no `_sqlx_migrations` ledger
  appears and a missing schema fails instead of self-creating.
- No Discord client, token, gateway call or write of any kind. The
  committed CLI tests pin this: the aggregates table is byte-identical
  after the run (row counts, totals and `updated_at` alike).
- Treat the output as aggregate analytics, not member data, but still do
  not commit it: game mixes can be community-sensitive. Keep one-off
  outputs under your operational retention policy.

## Worked examples

Populated guild (seeded fixture shape from the committed CLI test: Zelda
and Apex tie at 40 minutes of play, Zelda breaks the tie with 2 minutes
in voice; the empty-string title renders as the placeholder; the other
guild's rows never appear):

```sh
cargo run -p two-bot-cutover --bin game-activity-report -- --guild 100000000000000010
```

```json
{"guild_id":"100000000000000010","rows":[{"game":"Zelda","total_play_ms":2400000,"in_voice_ms":120000,"session_count":2},{"game":"Apex","total_play_ms":2400000,"in_voice_ms":60000,"session_count":3},{"game":"(untitled)","total_play_ms":60000,"in_voice_ms":0,"session_count":1}],"truncated":false}
```

Empty guild (header shape plus hint, still exit 0 — this is the
"nothing recorded" answer, not a failure):

```sh
cargo run -p two-bot-cutover --bin game-activity-report -- --guild 100000000000000012
```

```json
{"guild_id":"100000000000000012","rows":[],"truncated":false,"hint":"no game activity recorded for this guild"}
```

Unscoped (no database needed; useful to check the bin builds and to
re-read the flag):

```sh
cargo run -p two-bot-cutover --bin game-activity-report
```

```json
{"guild_id":null,"rows":[],"truncated":false,"hint":"no --guild given; pass --guild <ID> to report one guild"}
```

## Verification boundary

Every command and flag above is verified against the `--help` usage
string in `crates/cutover/src/bin/game_activity_report.rs` and the
committed suites `game_activity_report_cli.rs` (populated/empty/unscoped
contract plus read-only witness), `game_activity_report_db.rs`
(guild isolation and exact pinned rows) and
`game_activity_report_render.rs` (ordering, 100-row cap, key sets). Tests
run only against disposable test databases behind the existing
test-database guard — never staging or production. Shipping the binary is
not evidence of a live report.
