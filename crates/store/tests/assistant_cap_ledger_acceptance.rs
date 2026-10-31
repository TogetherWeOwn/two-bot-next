//! Acceptance tests: persistent per-guild monthly-cap ledger for the
//! template assistant (spec `docs/voice-rooms.md` §V12: "200 builds per
//! guild per month, resetting on the 1st").
//!
//! Protected test file: the build card implements against these tests and
//! must not edit them. If a pinned name below cannot be implemented as
//! written, stop and escalate instead of changing the tests.
//!
//! Contract pinned here (build card implements exactly this):
//!
//! - A new migration in `crates/store/migrations/` (unused bot number,
//!   checksum entry in `migrations.lock`) creates one row per guild in a
//!   table named `assistant_monthly_builds` with at least the columns
//!   `guild_id`, `month_key` and `build_count`. Column types are the
//!   builder's choice; behavior below is pinned through the store API.
//! - `two_bot_store::PgAssistantCapLedger`, constructed with
//!   `PgAssistantCapLedger::new(pool: Pool<Postgres>)`, re-exported from
//!   the `two-bot-store` crate root like the other `Pg*` stores.
//! - `record_build(&self, guild_id: Snowflake, now_secs: i64)
//!   -> Result<CapDecision, sqlx::Error>` atomically counts one build
//!   attempt against the guild's row for the UTC month containing
//!   `now_secs`, using the V12a core rules
//!   (`two_bot_core::voice_assistant_cap`, limit
//!   `MONTHLY_BUILD_LIMIT` = 200): under the cap it increments the
//!   persisted count and returns `Allow { remaining }` (builds left after
//!   this one); at the cap it counts nothing and returns
//!   `Deny { limit, resets_at }` with `resets_at` the next 1st 00:00 UTC.
//!   A stored row from an older month resets to zero for the current
//!   month. Concurrent attempts for one guild never exceed the cap.
//!   `guild_id == 0` is an error and writes nothing; a `now_secs` in an
//!   earlier month than the stored row (clock rollback) is an error and
//!   writes nothing.
//! - `builds_used(&self, guild_id: Snowflake, now_secs: i64)
//!   -> Result<u32, sqlx::Error>` is the read-only current-month view:
//!   `0` for an unknown guild and `0` when the stored row belongs to an
//!   older month. It never writes. `guild_id == 0` is an error.
//! - Counts live in the database, not in memory: a new ledger instance
//!   over the same pool sees the same counts (restart survival).
//!
//! Opt-in database tests, disposable database only (the same admission
//! rules as `postgres.rs`): agent-testdb or a CI service container, the
//! `agent_test` identity, one throwaway schema per test. No live
//! provider call, no real credential, `DATABASE_URL` never consulted.

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Pool, Postgres, Row};
use two_bot_core::voice_assistant_cap::{CapDecision, MONTHLY_BUILD_LIMIT};
use two_bot_core::Snowflake;
use two_bot_store::{migrate, PgAssistantCapLedger, DB_POOL_MAX};

static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

const GUILD: Snowflake = 100_000_000_000_000_001;
const OTHER_GUILD: Snowflake = 100_000_000_000_000_002;

// Pinned UTC timestamps (`date -u -d "<utc>" +%s`).
const DEC_2025_START: i64 = 1_764_547_200; // 2025-12-01 00:00:00 UTC
const DEC_31_2025_END: i64 = 1_767_225_599; // 2025-12-31 23:59:59 UTC
const JAN_2026_START: i64 = 1_767_225_600; // 2026-01-01 00:00:00 UTC
const JAN_15_2026_NOON: i64 = 1_768_478_400; // 2026-01-15 12:00:00 UTC
const FEB_2026_START: i64 = 1_769_904_000; // 2026-02-01 00:00:00 UTC
const FEB_2024_START: i64 = 1_706_745_600; // 2024-02-01 00:00:00 UTC (leap year)
const FEB_29_2024_NOON: i64 = 1_709_208_000; // 2024-02-29 12:00:00 UTC
const MAR_2024_START: i64 = 1_709_251_200; // 2024-03-01 00:00:00 UTC

struct Fixture {
    admin: Pool<Postgres>,
    pool: Pool<Postgres>,
    schema: String,
}

impl Fixture {
    async fn new() -> Self {
        let url = std::env::var("TEST_DATABASE_URL").expect("set TEST_DATABASE_URL for DB tests");
        let options = PgConnectOptions::from_str(&url).expect("invalid TEST_DATABASE_URL");
        let ci_container = std::env::var("CI").as_deref() == Ok("true")
            && matches!(options.get_host(), "localhost" | "127.0.0.1" | "postgres");
        assert!(
            options.get_host() == "agent-testdb" || ci_container,
            "test containers only"
        );
        assert_eq!(options.get_username(), "agent_test", "test identity only");
        let admin = two_bot_store::connect_pool_with_tls(
            &url,
            two_bot_core::database_tls::TlsPolicy::LocalOnly,
        )
        .await
        .expect("test DB connect failed");
        let schema = format!(
            "acap_test_{}_{}",
            std::process::id(),
            NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed)
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(DB_POOL_MAX)
            .connect_with(options.options([
                ("search_path", schema.clone()),
                ("statement_timeout", "15000ms".to_owned()),
            ]))
            .await
            .unwrap();
        migrate(&pool).await.expect("migrations must apply cleanly");
        Self {
            admin,
            pool,
            schema,
        }
    }

    fn ledger(&self) -> PgAssistantCapLedger {
        PgAssistantCapLedger::new(self.pool.clone())
    }

    async fn finish(self) {
        self.pool.close().await;
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA IF EXISTS {}_web_v1 CASCADE; DROP SCHEMA {} CASCADE",
            self.schema, self.schema
        )))
        .execute(&self.admin)
        .await
        .unwrap();
        self.admin.close().await;
    }
}

/// Record builds until the guild sits exactly at the monthly cap.
async fn fill_to_cap(ledger: &PgAssistantCapLedger, guild: Snowflake, now: i64) {
    for used in 1..=MONTHLY_BUILD_LIMIT {
        let decision = ledger
            .record_build(guild, now)
            .await
            .expect("record_build under the cap must succeed");
        assert_eq!(
            decision,
            CapDecision::Allow {
                remaining: MONTHLY_BUILD_LIMIT - used
            },
            "build {used} of the month"
        );
    }
    assert_eq!(
        ledger.builds_used(guild, now).await.unwrap(),
        MONTHLY_BUILD_LIMIT
    );
}

#[tokio::test]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn migration_creates_monthly_builds_table_and_is_idempotent() {
    let fx = Fixture::new().await;
    // Round-trip: applying the whole chain again on the migrated schema
    // is a no-op, never an error.
    migrate(&fx.pool)
        .await
        .expect("re-running migrations must be idempotent");
    let rows = sqlx::query(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = 'assistant_monthly_builds'",
    )
    .fetch_all(&fx.pool)
    .await
    .unwrap();
    let columns: HashSet<String> = rows
        .iter()
        .map(|row| row.try_get::<String, _>("column_name").unwrap())
        .collect();
    for required in ["guild_id", "month_key", "build_count"] {
        assert!(
            columns.contains(required),
            "assistant_monthly_builds must carry {required}; got {columns:?}"
        );
    }
    fx.finish().await;
}

#[tokio::test]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn first_builds_increment_and_report_remaining() {
    let fx = Fixture::new().await;
    let ledger = fx.ledger();
    assert_eq!(
        ledger.builds_used(GUILD, JAN_15_2026_NOON).await.unwrap(),
        0
    );
    assert_eq!(
        ledger.record_build(GUILD, JAN_15_2026_NOON).await.unwrap(),
        CapDecision::Allow { remaining: 199 }
    );
    assert_eq!(
        ledger.builds_used(GUILD, JAN_15_2026_NOON).await.unwrap(),
        1
    );
    assert_eq!(
        ledger.record_build(GUILD, JAN_15_2026_NOON).await.unwrap(),
        CapDecision::Allow { remaining: 198 }
    );
    assert_eq!(
        ledger.builds_used(GUILD, JAN_15_2026_NOON).await.unwrap(),
        2
    );
    fx.finish().await;
}

#[tokio::test]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn cap_refusal_at_the_limit_counts_nothing() {
    let fx = Fixture::new().await;
    let ledger = fx.ledger();
    fill_to_cap(&ledger, GUILD, JAN_15_2026_NOON).await;
    for _ in 0..3 {
        assert_eq!(
            ledger.record_build(GUILD, JAN_15_2026_NOON).await.unwrap(),
            CapDecision::Deny {
                limit: MONTHLY_BUILD_LIMIT,
                resets_at: FEB_2026_START,
            }
        );
    }
    assert_eq!(
        ledger.builds_used(GUILD, JAN_15_2026_NOON).await.unwrap(),
        MONTHLY_BUILD_LIMIT,
        "denied builds must never increment the persisted count"
    );
    fx.finish().await;
}

#[tokio::test]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn month_rollover_after_cap_december_to_january() {
    let fx = Fixture::new().await;
    let ledger = fx.ledger();
    fill_to_cap(&ledger, GUILD, DEC_2025_START).await;
    assert_eq!(
        ledger.record_build(GUILD, DEC_31_2025_END).await.unwrap(),
        CapDecision::Deny {
            limit: MONTHLY_BUILD_LIMIT,
            resets_at: JAN_2026_START,
        }
    );
    // The first instant of the new month opens a fresh quota.
    assert_eq!(ledger.builds_used(GUILD, JAN_2026_START).await.unwrap(), 0);
    assert_eq!(
        ledger.record_build(GUILD, JAN_2026_START).await.unwrap(),
        CapDecision::Allow { remaining: 199 }
    );
    assert_eq!(ledger.builds_used(GUILD, JAN_2026_START).await.unwrap(), 1);
    fx.finish().await;
}

#[tokio::test]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn stale_month_reads_as_zero_and_the_read_never_writes() {
    let fx = Fixture::new().await;
    let ledger = fx.ledger();
    for expected_remaining in [199, 198, 197] {
        assert_eq!(
            ledger.record_build(GUILD, JAN_15_2026_NOON).await.unwrap(),
            CapDecision::Allow {
                remaining: expected_remaining
            }
        );
    }
    assert_eq!(ledger.builds_used(GUILD, FEB_2026_START).await.unwrap(), 0);
    // The rollover view is read-only: January's persisted count is intact.
    assert_eq!(
        ledger.builds_used(GUILD, JAN_15_2026_NOON).await.unwrap(),
        3
    );
    fx.finish().await;
}

#[tokio::test]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn leap_day_build_counts_toward_february() {
    let fx = Fixture::new().await;
    let ledger = fx.ledger();
    assert_eq!(
        ledger.record_build(GUILD, FEB_29_2024_NOON).await.unwrap(),
        CapDecision::Allow { remaining: 199 }
    );
    assert_eq!(ledger.builds_used(GUILD, FEB_2024_START).await.unwrap(), 1);
    assert_eq!(
        ledger.builds_used(GUILD, FEB_29_2024_NOON).await.unwrap(),
        1
    );
    assert_eq!(ledger.builds_used(GUILD, MAR_2024_START).await.unwrap(), 0);
    assert_eq!(
        ledger.record_build(GUILD, MAR_2024_START).await.unwrap(),
        CapDecision::Allow { remaining: 199 }
    );
    fx.finish().await;
}

#[tokio::test]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn guilds_are_isolated() {
    let fx = Fixture::new().await;
    let ledger = fx.ledger();
    fill_to_cap(&ledger, GUILD, JAN_15_2026_NOON).await;
    assert_eq!(
        ledger
            .record_build(OTHER_GUILD, JAN_15_2026_NOON)
            .await
            .unwrap(),
        CapDecision::Allow { remaining: 199 },
        "one guild at its cap must never consume another guild's quota"
    );
    assert_eq!(
        ledger.builds_used(GUILD, JAN_15_2026_NOON).await.unwrap(),
        MONTHLY_BUILD_LIMIT
    );
    assert_eq!(
        ledger
            .builds_used(OTHER_GUILD, JAN_15_2026_NOON)
            .await
            .unwrap(),
        1
    );
    fx.finish().await;
}

#[tokio::test]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn counts_survive_a_new_ledger_instance() {
    let fx = Fixture::new().await;
    {
        let ledger = fx.ledger();
        ledger.record_build(GUILD, JAN_15_2026_NOON).await.unwrap();
        ledger.record_build(GUILD, JAN_15_2026_NOON).await.unwrap();
    }
    // A fresh instance (a restarted process) reads the persisted row, not
    // an in-memory mirror.
    let reopened = fx.ledger();
    assert_eq!(
        reopened.builds_used(GUILD, JAN_15_2026_NOON).await.unwrap(),
        2
    );
    assert_eq!(
        reopened
            .record_build(GUILD, JAN_15_2026_NOON)
            .await
            .unwrap(),
        CapDecision::Allow { remaining: 197 }
    );
    fx.finish().await;
}

#[tokio::test]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn concurrent_attempts_never_exceed_the_cap() {
    let fx = Fixture::new().await;
    let ledger = fx.ledger();
    // Seed to one below the cap, then race eight attempts at the last slot.
    for _ in 1..MONTHLY_BUILD_LIMIT {
        ledger.record_build(GUILD, JAN_15_2026_NOON).await.unwrap();
    }
    assert_eq!(
        ledger.builds_used(GUILD, JAN_15_2026_NOON).await.unwrap(),
        MONTHLY_BUILD_LIMIT - 1
    );
    let mut handles = Vec::new();
    for _ in 0..8 {
        let racer = fx.ledger();
        handles.push(tokio::spawn(async move {
            racer.record_build(GUILD, JAN_15_2026_NOON).await
        }));
    }
    let mut allowed = 0;
    let mut denied = 0;
    for handle in handles {
        match handle
            .await
            .unwrap()
            .expect("raced record_build must not error")
        {
            CapDecision::Allow { remaining: 0 } => allowed += 1,
            CapDecision::Deny { limit, resets_at } => {
                assert_eq!(limit, MONTHLY_BUILD_LIMIT);
                assert_eq!(resets_at, FEB_2026_START);
                denied += 1;
            }
            other => panic!("unexpected raced decision: {other:?}"),
        }
    }
    assert_eq!(allowed, 1, "exactly one racer may take the last slot");
    assert_eq!(denied, 7);
    assert_eq!(
        ledger.builds_used(GUILD, JAN_15_2026_NOON).await.unwrap(),
        MONTHLY_BUILD_LIMIT
    );
    fx.finish().await;
}

#[tokio::test]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn zero_guild_is_refused_and_writes_nothing() {
    let fx = Fixture::new().await;
    let ledger = fx.ledger();
    assert!(
        ledger.record_build(0, JAN_15_2026_NOON).await.is_err(),
        "guild 0 is not a real guild and must be refused"
    );
    assert!(ledger.builds_used(0, JAN_15_2026_NOON).await.is_err());
    let rows: i64 = sqlx::query("SELECT count(*) AS n FROM assistant_monthly_builds")
        .fetch_one(&fx.pool)
        .await
        .unwrap()
        .try_get("n")
        .unwrap();
    assert_eq!(rows, 0, "a refused attempt must not write a row");
    fx.finish().await;
}

#[tokio::test]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn clock_before_the_stored_month_is_refused_and_writes_nothing() {
    let fx = Fixture::new().await;
    let ledger = fx.ledger();
    ledger.record_build(GUILD, FEB_2026_START).await.unwrap();
    assert!(
        ledger.record_build(GUILD, JAN_15_2026_NOON).await.is_err(),
        "a clock before the stored month must not grant a fresh quota"
    );
    assert_eq!(ledger.builds_used(GUILD, FEB_2026_START).await.unwrap(), 1);
    fx.finish().await;
}
