-- 0446_assistant_monthly_builds: per-guild template-assistant monthly-cap ledger.
--
-- Durable half of the V12a monthly cap (spec `docs/voice-rooms.md` §V12:
-- "200 builds per guild per month, resetting on the 1st"). One row per
-- guild: the UTC month the count belongs to plus the builds used in it. The
-- `PgAssistantCapLedger` store (`crates/store/src/assistant_cap.rs`)
-- reconciles this row against the processing clock through the pure V12a
-- core (`two_bot_core::voice_assistant_cap`, limit `MONTHLY_BUILD_LIMIT`):
-- a row from an older month resets to zero for the current month, an
-- attempt at the cap counts nothing and reports the next 1st 00:00 UTC,
-- and concurrent attempts serialize on the guild row so the cap is never
-- exceeded. Guild-keyed only: no member column, no per-member row and no
-- public-surface view.
--
-- `month_key` is the UTC month start (the 1st at 00:00:00) as Unix seconds,
-- matching the core `MonthKey::start_unix_secs` the store writes and reads.
-- Snowflakes persist as decimal TEXT, matching the funnel tables. The writer
-- inserts a zero-count row if absent, then locks it to decide and update the
-- count atomically. Each committed allowed call counts once; retrying after
-- an unacknowledged commit counts again (there is no idempotency key).
--
-- Additive only: one new table, no changes to existing tables. `IF NOT
-- EXISTS` throughout, matching the S6 convention, so cross-running the store
-- and cutover chains against one schema stays safe in either order. Rollback
-- is `DROP TABLE IF EXISTS assistant_monthly_builds`.
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS assistant_monthly_builds (
  guild_id      TEXT NOT NULL,
  month_key     BIGINT NOT NULL,
  build_count   BIGINT NOT NULL DEFAULT 0,
  updated_at    TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (guild_id),
  CONSTRAINT assistant_builds_count_nonneg CHECK (build_count >= 0)
);
