# Presence probe, community scorecard, and inactivity flagging

This slice ports the three S5 community jobs as framework-free domain logic in
`two-bot-core` plus sqlx stores and migrations. It does **not** register live
handlers, instantiate a Discord HTTP client, or start a scheduler. It stays
independently testable until the shared S4 interaction router and REST
executor merge.

## Modules

- `core::presence` — hourly probe decision (`decide_probe_cycle`), bot-floor
  re-list rule (`bot_floor_due`, 24 h), and the TOG-469 reopen trigger
  (`evaluate_trigger`: 45-peak threshold, 3 days in 14, 7-day minimum).
- `core::community` — classifier (`classify`, legacy precedence), weekly
  builder (`build_scorecard`), Monday 06:15 UTC schedule (`scorecard_tick`,
  60 s tick, exactly-once per Monday), env gates (`ScorecardGates`).
- `core::inactivity` — hourly quiet-member selection (`flag_inactive`,
  `TWO_INACTIVITY_DAYS ?? 14`). Read-only by construction: the outcome type
  carries no channel/message/DM field, so it cannot feed a send path.
- `core::presence_store` / `community_store` / `inactivity_store` — sqlx row
  moves behind the `db` feature, transliterated from the legacy queries.
- Migrations `0310_presence_probe.sql` / `0311_community_scorecard.sql` in
  `crates/cutover/migrations` (this card's reserved block 0310–0319).
  `community_facts` itself lives in 0160 (TOG-10083, host check-in facts) and
  is not recreated here.

## Integration contract

- Feed the REST guild-counts reading (`GET /guilds/{id}?with_counts=true`)
  into `decide_probe_cycle`; persist `Record`, drop `Skip`. A failed presence
  read writes nothing — not a null row, not a zero. Rescan the bot floor only
  when `bot_floor_due`; a failed listing keeps the presence row with a NULL
  floor. Drive the probe every `PRESENCE_PROBE_INTERVAL_MS` (1 h), unref'd,
  with one reading at startup.
- Drive the scorecard every `SCORECARD_TICK_INTERVAL_MS` (60 s); fire at most
  once per Monday via `scorecard_tick`. Before scoring, persist full-week
  stream coverage (`mark_stream_coverage` for all six streams); a mid-week
  start fails closed (`INGESTION_INCOMPLETE`, human numerators null).
- Drive the inactivity sweep every `INACTIVITY_SWEEP_INTERVAL_MS` (1 h) via
  `run_sweep`. Never DM, ping, or message from this outcome — any outbound
  contact needs CEO sign-off first.
- Implement fact writes on the gateway handlers through the `FactsSink` seam,
  classifying via `classify`. Keep `TWO_COMMUNITY_SCORECARD` off by default
  and `TWO_PRESENCE_PROBE` on (legacy default); restrict enabling to staging.
  No production guild or token was used to verify this slice.
- The presence series is never published: no `web_v1` view may read
  `presence_probe`. The only reader is an operator trend report over
  `read_series` + `evaluate_trigger`.

## Verification

- `cargo test -p two-bot-core --features db` with `TWO_TEST_DATABASE_URL`
  pointing at agent-testdb: 114 tests, including a Monday run replayed against
  a golden scorecard produced by the real legacy build (frozen two-bot @
  `d5d11793`), the week-boundary exactly-once tick, and the inactivity
  never-messages invariant (exactly one `member_inactive` event row across two
  sweeps).
- Legacy table/column names kept verbatim; `CREATE TABLE / INDEX IF NOT EXISTS`
  throughout so S6 re-runs never fail on DDL.
