#![cfg(feature = "db")]

//! First-award serialization: members with no `member_levels` row.
//!
//! Store proofs restricted to the named disposable agent-testdb service.
//! Create an empty `two_bot_test_local` bootstrap as described in
//! CONTRIBUTING.md, then run:
//! TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_local \
//! python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db --test leveling_first_award_race
//! No DATABASE_URL or inherited application credentials are consulted.
//!
//! A `SELECT … FOR UPDATE` on the lifetime row locks nothing when the row
//! is absent (first award, or after erasure), so two concurrent first
//! voice awards 61 s apart could both read empty counters and each spend
//! the 120 XP daily cap. Awards serialize on a transaction-scoped
//! `pg_advisory_xact_lock` keyed on guild+member taken before any
//! eligibility read; this test holds that advisory lock until both racers
//! are queued on it, then releases, and asserts exactly one racer is
//! granted the cap.

use std::time::Duration;

use two_bot_core::leveling_store as store;
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1545644954272137297";
const MEMBER: &str = "100000000000000001";

type StoreResult<T> = Result<T, two_bot_core::LevelingStoreError>;

fn at(seconds: i64) -> String {
    two_bot_core::format_iso_millis(1_786_771_200_000 + seconds * 1000)
}

fn channel(index: i64) -> String {
    format!("200000000000000{index:03}")
}

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("explicit DB test requires TWO_TEST_DATABASE_URL (test bootstrap only)");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated test database")
}

/// Wait until at least `expected` other backends in this database are
/// waiting on any event. Advisory-lock waits do not always surface as
/// `wait_event_type = 'Lock'`, so match any non-null `wait_event`: with the
/// advisory lock held, a racer can only reach a wait after starting its
/// award transaction.
async fn wait_for_waiters(pool: &sqlx::PgPool, expected: i64) -> StoreResult<()> {
    for _ in 0..300 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_stat_activity
              WHERE datname = current_database()
                AND pid <> pg_backend_pid()
                AND wait_event IS NOT NULL",
        )
        .fetch_one(pool)
        .await?;
        if waiting >= expected {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("racers never queued on the advisory lock; the interleaving was not established");
}

#[tokio::test]
async fn concurrent_first_voice_awards_never_exceed_daily_cap() -> StoreResult<()> {
    let fixture = database().await;
    let pool = fixture.pool().clone();
    // No seed: the member starts with no `member_levels` row.
    let empty: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM member_levels WHERE guild_id = $1 AND member_id = $2",
    )
    .bind(GUILD)
    .bind(MEMBER)
    .fetch_one(&pool)
    .await?;
    assert_eq!(empty, 0, "first-award fixture must start with no row");

    // Hold the per-member advisory lock so both racers queue on it before
    // either can read eligibility. Their event instants are 61 s apart, so
    // each can satisfy the cooldown on its own merits; only the 120 XP
    // daily cap separates them.
    let mut lock_tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1 || ':' || $2, 0))")
        .bind(GUILD)
        .bind(MEMBER)
        .execute(&mut *lock_tx)
        .await?;

    let mut tasks = tokio::task::JoinSet::new();
    // 30 minutes × 5 XP/min = 150 base, capped to the 120 XP daily cap.
    for index in [0_i64, 1] {
        let pool = pool.clone();
        let instant = at(index * 61);
        let chan = channel(index);
        tasks.spawn(async move {
            store::award_voice(&pool, GUILD, MEMBER, 1800, &instant, Some(&chan)).await
        });
    }
    wait_for_waiters(&pool, 2).await?;
    lock_tx.commit().await?;

    let mut awarded = Vec::new();
    while let Some(result) = tasks.join_next().await {
        awarded.push(result.expect("award task")?.awarded);
    }
    awarded.sort_unstable();
    assert_eq!(
        awarded,
        vec![0, 120],
        "exactly one first-award racer may spend the 120 XP daily cap"
    );
    let profile = store::profile(&pool, GUILD, MEMBER).await?;
    assert_eq!(profile.voice_xp, 120, "the 120 XP voice daily cap holds");
    assert_eq!(profile.xp, 120);

    fixture.close().await.expect("drop test database");
    Ok(())
}
