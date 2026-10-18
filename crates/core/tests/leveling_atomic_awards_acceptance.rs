#![cfg(feature = "db")]

//! Atomicity acceptance for policy eligibility plus seasonal projection.
//!
//! Store proofs restricted to the named disposable agent-testdb service.
//! Create an empty `two_bot_test_local` bootstrap as described in
//! CONTRIBUTING.md, then run:
//! TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_local \
//! python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db --test leveling_atomic_awards_acceptance
//! No DATABASE_URL or inherited application credentials are consulted.
//!
//! Contract under test (test files only; no production code here):
//!
//! - The eligibility decision (daily count/total, ladder rung, burst and
//!   cooldown inputs), the lifetime/source counters, the cooldown row, the
//!   award audit row and the seasonal projection commit as ONE unit for
//!   message and voice awards.
//! - A forced seasonal write failure therefore leaves every participating
//!   table unchanged, and a retry after the fault yields exactly one
//!   complete award. A rejected seasonal ceiling update is a defined
//!   non-partial outcome (an error or a 0-XP award), never silent success
//!   with a diverged seasonal projection.
//! - Concurrent awards whose event timestamps are distinct and can each
//!   satisfy the cooldown never exceed the last daily ladder rung (message)
//!   or the daily cap (voice) and never both spend a stale full rung.
//!
//! Fault injection is a guild-scoped BEFORE trigger on `member_season_xp`
//! installed by the test itself, so no test-only production API is invented:
//! the same store calls run before and after the repair. Concurrency is made
//! deterministic by holding the member's `member_levels` row lock until both
//! racers are queued on a lock (observed in `pg_stat_activity`), which proves
//! both passed their eligibility reads before either could commit; there are
//! no timing-only sleeps.
//!
//! `valid_awards_commit_lifetime_cooldown_audit_and_seasonal_together` is the
//! passing control. The fault and race tests fail against an implementation
//! that reads eligibility before the award transaction or writes the seasonal
//! projection after committing the lifetime award.

use std::time::Duration;

use two_bot_core::leveling::{season_id_for_ms, MAX_STORED_XP};
use two_bot_core::leveling_store as store;
use two_bot_core::parse_iso_millis;
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1545644954272137297";
const OTHER_GUILD: &str = "1545644954272137298";
const MEMBER: &str = "100000000000000001";

type StoreResult<T> = Result<T, two_bot_core::LevelingStoreError>;

fn at(seconds: i64) -> String {
    two_bot_core::format_iso_millis(1_786_771_200_000 + seconds * 1000)
}

fn fixture_season() -> String {
    let ms = parse_iso_millis(&at(0)).expect("fixture timestamp parses");
    let season = season_id_for_ms(ms);
    assert_eq!(season, "2026-Q3", "fixture day must resolve to 2026-Q3");
    season
}

fn channel(index: i64) -> String {
    // A distinct channel per award keeps same-channel burst halving out of
    // these atomicity paths; burst policy has dedicated tests.
    format!("200000000000000{index:03}")
}

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("explicit DB test requires TWO_TEST_DATABASE_URL (test bootstrap only)");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated test database")
}

/// Every row an award may touch for one guild/member, in stable order.
#[derive(Debug, PartialEq, Eq)]
struct AwardSnapshot {
    levels: Vec<(i64, i64, i64, i64, String)>,
    cooldowns: Vec<(String, String)>,
    awards: Vec<(String, i32, Option<String>)>,
    seasonal: Vec<(String, i64, String)>,
}

async fn snapshot(pool: &sqlx::PgPool, guild: &str, member: &str) -> StoreResult<AwardSnapshot> {
    let levels = sqlx::query_as(
        "SELECT xp, message_xp, voice_xp, imported_xp, updated_at::text
           FROM member_levels WHERE guild_id = $1 AND member_id = $2",
    )
    .bind(guild)
    .bind(member)
    .fetch_all(pool)
    .await?;
    let cooldowns = sqlx::query_as(
        "SELECT source, last_awarded_at::text FROM xp_cooldowns
          WHERE guild_id = $1 AND member_id = $2 ORDER BY source",
    )
    .bind(guild)
    .bind(member)
    .fetch_all(pool)
    .await?;
    let awards = sqlx::query_as(
        "SELECT source, xp, decision FROM xp_awards
          WHERE guild_id = $1 AND member_id = $2 ORDER BY id",
    )
    .bind(guild)
    .bind(member)
    .fetch_all(pool)
    .await?;
    let seasonal = sqlx::query_as(
        "SELECT season_id, seasonal_xp, updated_at::text FROM member_season_xp
          WHERE guild_id = $1 AND member_id = $2 ORDER BY season_id",
    )
    .bind(guild)
    .bind(member)
    .fetch_all(pool)
    .await?;
    Ok(AwardSnapshot {
        levels,
        cooldowns,
        awards,
        seasonal,
    })
}

async fn seasonal_xp(pool: &sqlx::PgPool, guild: &str, member: &str) -> StoreResult<Option<i64>> {
    let value: Option<i64> = sqlx::query_scalar(
        "SELECT seasonal_xp FROM member_season_xp
          WHERE guild_id = $1 AND season_id = $2 AND member_id = $3",
    )
    .bind(guild)
    .bind(fixture_season())
    .bind(member)
    .fetch_optional(pool)
    .await?;
    Ok(value)
}

async fn seed_imported(pool: &sqlx::PgPool, guild: &str, member: &str, xp: i64) -> StoreResult<()> {
    sqlx::query(
        "INSERT INTO member_levels
           (guild_id, member_id, xp, message_xp, voice_xp, imported_xp, updated_at)
         VALUES ($1, $2, $3, 0, 0, $3, $4::text::timestamptz)",
    )
    .bind(guild)
    .bind(member)
    .bind(xp)
    .bind(at(0))
    .execute(pool)
    .await?;
    Ok(())
}

/// Install a trigger that rejects every seasonal write for `GUILD` only, so
/// another guild stays a live control while the fault is active.
async fn install_seasonal_failure(pool: &sqlx::PgPool) -> StoreResult<()> {
    // Audit for AssertSqlSafe: the only interpolated value is this file's
    // `GUILD` string literal; a plpgsql body cannot take a bind parameter.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE OR REPLACE FUNCTION reject_seasonal_write_for_atomicity_test()
         RETURNS trigger AS $$
         BEGIN
           IF NEW.guild_id = '{GUILD}' THEN
             RAISE EXCEPTION 'forced seasonal write failure for atomicity test';
           END IF;
           RETURN NEW;
         END;
         $$ LANGUAGE plpgsql"
    )))
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE TRIGGER reject_seasonal_write_for_atomicity_test
         BEFORE INSERT OR UPDATE ON member_season_xp
         FOR EACH ROW EXECUTE FUNCTION reject_seasonal_write_for_atomicity_test()",
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn remove_seasonal_failure(pool: &sqlx::PgPool) -> StoreResult<()> {
    sqlx::query("DROP TRIGGER reject_seasonal_write_for_atomicity_test ON member_season_xp")
        .execute(pool)
        .await?;
    sqlx::query("DROP FUNCTION reject_seasonal_write_for_atomicity_test()")
        .execute(pool)
        .await?;
    Ok(())
}

/// A faulted attempt may surface as an error or as a defined 0-XP award;
/// it must never report granted XP.
fn assert_not_silent_success<T>(result: &StoreResult<T>, awarded: impl Fn(&T) -> u64) {
    if let Ok(value) = result {
        assert_eq!(
            awarded(value),
            0,
            "a rejected seasonal write must not silently report granted XP"
        );
    }
}

/// Wait until at least `expected` other backends in this database are queued
/// on a lock. This is a condition wait on observable lock state, not a sleep
/// raced against the workers: with the `member_levels` row held, a racer can
/// only reach a lock wait after passing its eligibility reads.
async fn wait_for_lock_waiters(pool: &sqlx::PgPool, expected: i64) -> StoreResult<()> {
    for _ in 0..300 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_stat_activity
              WHERE datname = current_database()
                AND wait_event_type = 'Lock'
                AND pid <> pg_backend_pid()",
        )
        .fetch_one(pool)
        .await?;
        if waiting >= expected {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("racers never queued on a lock; the interleaving was not established");
}

#[tokio::test]
async fn valid_awards_commit_lifetime_cooldown_audit_and_seasonal_together() -> StoreResult<()> {
    let fixture = database().await;
    let pool = fixture.pool().clone();
    // Imported lifetime XP is preserved and is not backfilled into the
    // seasonal projection: seasonal mirrors new awards only.
    seed_imported(&pool, GUILD, MEMBER, 99).await?;

    let message =
        store::award_message(&pool, GUILD, MEMBER, &at(0), Some(&channel(0)), None, false).await?;
    assert_eq!(message.awarded, 15);
    assert_eq!(message.total_xp, 114);
    assert!(message.leveled_up, "crossing 100 XP still levels up");
    assert_eq!((message.previous_level, message.level), (0, 1));

    let voice = store::award_voice(&pool, GUILD, MEMBER, 120, &at(61), Some(&channel(1))).await?;
    assert_eq!(voice.awarded, 10);
    assert_eq!(voice.total_xp, 124);

    let after = snapshot(&pool, GUILD, MEMBER).await?;
    assert_eq!(after.levels.len(), 1);
    assert_eq!(
        (
            after.levels[0].0,
            after.levels[0].1,
            after.levels[0].2,
            after.levels[0].3
        ),
        (124, 15, 10, 99),
        "lifetime = message + voice + imported"
    );
    assert_eq!(
        after
            .awards
            .iter()
            .map(|(source, xp, _)| (source.as_str(), *xp))
            .collect::<Vec<_>>(),
        vec![("message", 15), ("voice", 10)]
    );
    assert_eq!(after.cooldowns.len(), 2, "one cooldown row per source");
    assert_eq!(
        seasonal_xp(&pool, GUILD, MEMBER).await?,
        Some(25),
        "seasonal mirrors the 15 + 10 new awards, never the imported 99"
    );

    // Guild isolation: the same member in another guild starts empty, and
    // awarding there leaves this guild's snapshot untouched.
    assert_eq!(store::profile(&pool, OTHER_GUILD, MEMBER).await?.xp, 0);
    let other = store::award_message(&pool, OTHER_GUILD, MEMBER, &at(0), None, None, false).await?;
    assert_eq!(other.awarded, 15);
    assert_eq!(snapshot(&pool, GUILD, MEMBER).await?, after);
    assert_eq!(seasonal_xp(&pool, OTHER_GUILD, MEMBER).await?, Some(15));

    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn seasonal_write_failure_rolls_back_message_award_and_retry_awards_once() -> StoreResult<()>
{
    let fixture = database().await;
    let pool = fixture.pool().clone();
    seed_imported(&pool, GUILD, MEMBER, 99).await?;
    let before = snapshot(&pool, GUILD, MEMBER).await?;
    assert_eq!(before.levels[0].0, 99);
    assert!(before.cooldowns.is_empty() && before.awards.is_empty() && before.seasonal.is_empty());

    install_seasonal_failure(&pool).await?;
    let failed =
        store::award_message(&pool, GUILD, MEMBER, &at(0), Some(&channel(0)), None, false).await;
    assert_not_silent_success(&failed, |award| award.awarded);
    assert_eq!(
        snapshot(&pool, GUILD, MEMBER).await?,
        before,
        "a seasonal failure must leave lifetime, cooldown, audit and seasonal rows unchanged"
    );
    // Control while the fault is active: another guild still awards fully.
    let control =
        store::award_message(&pool, OTHER_GUILD, MEMBER, &at(0), None, None, false).await?;
    assert_eq!(control.awarded, 15);
    assert_eq!(seasonal_xp(&pool, OTHER_GUILD, MEMBER).await?, Some(15));
    remove_seasonal_failure(&pool).await?;

    // Retry at the same event instant: exactly one complete award. If the
    // faulted attempt had committed a cooldown, this would wrongly read 0;
    // if it had committed lifetime XP, the totals below would double-count.
    let retry =
        store::award_message(&pool, GUILD, MEMBER, &at(0), Some(&channel(0)), None, false).await?;
    assert_eq!(retry.awarded, 15);
    assert_eq!(retry.total_xp, 114);
    let after = snapshot(&pool, GUILD, MEMBER).await?;
    assert_eq!(
        (
            after.levels[0].0,
            after.levels[0].1,
            after.levels[0].2,
            after.levels[0].3
        ),
        (114, 15, 0, 99)
    );
    assert_eq!(
        after
            .awards
            .iter()
            .map(|(source, xp, _)| (source.as_str(), *xp))
            .collect::<Vec<_>>(),
        vec![("message", 15)],
        "exactly one audit row after fault plus retry"
    );
    assert_eq!(after.cooldowns.len(), 1);
    assert_eq!(seasonal_xp(&pool, GUILD, MEMBER).await?, Some(15));

    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn seasonal_write_failure_rolls_back_voice_award_and_retry_awards_once() -> StoreResult<()> {
    let fixture = database().await;
    let pool = fixture.pool().clone();
    let before = snapshot(&pool, GUILD, MEMBER).await?;
    assert!(
        before.levels.is_empty()
            && before.cooldowns.is_empty()
            && before.awards.is_empty()
            && before.seasonal.is_empty()
    );

    install_seasonal_failure(&pool).await?;
    let failed = store::award_voice(&pool, GUILD, MEMBER, 60, &at(0), Some(&channel(0))).await;
    assert_not_silent_success(&failed, |award| award.awarded);
    assert_eq!(
        snapshot(&pool, GUILD, MEMBER).await?,
        before,
        "a seasonal failure must leave lifetime, cooldown, audit and seasonal rows unchanged"
    );
    let control = store::award_voice(&pool, OTHER_GUILD, MEMBER, 60, &at(0), None).await?;
    assert_eq!(control.awarded, 5);
    assert_eq!(seasonal_xp(&pool, OTHER_GUILD, MEMBER).await?, Some(5));
    remove_seasonal_failure(&pool).await?;

    let retry = store::award_voice(&pool, GUILD, MEMBER, 60, &at(0), Some(&channel(0))).await?;
    assert_eq!(retry.awarded, 5);
    assert_eq!(retry.total_xp, 5);
    let after = snapshot(&pool, GUILD, MEMBER).await?;
    assert_eq!(
        (
            after.levels[0].0,
            after.levels[0].1,
            after.levels[0].2,
            after.levels[0].3
        ),
        (5, 0, 5, 0)
    );
    assert_eq!(
        after
            .awards
            .iter()
            .map(|(source, xp, _)| (source.as_str(), *xp))
            .collect::<Vec<_>>(),
        vec![("voice", 5)],
        "exactly one audit row after fault plus retry"
    );
    assert_eq!(after.cooldowns.len(), 1);
    assert_eq!(seasonal_xp(&pool, GUILD, MEMBER).await?, Some(5));

    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn seasonal_ceiling_rejection_is_non_partial_and_retry_awards_once() -> StoreResult<()> {
    let fixture = database().await;
    let pool = fixture.pool().clone();
    // Fault: the seasonal projection already sits at its stored ceiling, so
    // the next seasonal update must be rejected by the table CHECK.
    sqlx::query(
        "INSERT INTO member_season_xp (guild_id, season_id, member_id, seasonal_xp, updated_at)
         VALUES ($1, $2, $3, $4, $5::text::timestamptz)",
    )
    .bind(GUILD)
    .bind(fixture_season())
    .bind(MEMBER)
    .bind(MAX_STORED_XP as i64)
    .bind(at(0))
    .execute(&pool)
    .await?;
    let before = snapshot(&pool, GUILD, MEMBER).await?;

    let rejected =
        store::award_message(&pool, GUILD, MEMBER, &at(0), Some(&channel(0)), None, false).await;
    assert_not_silent_success(&rejected, |award| award.awarded);
    assert_eq!(
        snapshot(&pool, GUILD, MEMBER).await?,
        before,
        "a rejected seasonal ceiling update must not commit lifetime, cooldown or audit rows"
    );

    // Clear the fault; the same event then awards exactly once.
    sqlx::query(
        "UPDATE member_season_xp SET seasonal_xp = 0
          WHERE guild_id = $1 AND season_id = $2 AND member_id = $3",
    )
    .bind(GUILD)
    .bind(fixture_season())
    .bind(MEMBER)
    .execute(&pool)
    .await?;
    let retry =
        store::award_message(&pool, GUILD, MEMBER, &at(0), Some(&channel(0)), None, false).await?;
    assert_eq!(retry.awarded, 15);
    let after = snapshot(&pool, GUILD, MEMBER).await?;
    assert_eq!(after.awards.len(), 1, "exactly one audit row after retry");
    assert_eq!(after.cooldowns.len(), 1);
    assert_eq!(seasonal_xp(&pool, GUILD, MEMBER).await?, Some(15));
    assert_eq!(store::profile(&pool, GUILD, MEMBER).await?.xp, 15);

    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn concurrent_message_awards_at_distinct_instants_never_exceed_last_ladder_rung(
) -> StoreResult<()> {
    let fixture = database().await;
    let pool = fixture.pool().clone();
    // Seed nine same-day awards through the real store: ladder rungs
    // 15 x5 then 5 x4 = 95. One 5-XP rung remains for the day.
    for index in 0..9_i64 {
        let award = store::award_message(
            &pool,
            GUILD,
            MEMBER,
            &at(index * 61),
            Some(&channel(index)),
            None,
            false,
        )
        .await?;
        assert_eq!(
            award.awarded,
            if index < 5 { 15 } else { 5 },
            "seed #{index}"
        );
    }
    assert_eq!(store::profile(&pool, GUILD, MEMBER).await?.message_xp, 95);

    // Hold the lifetime row so both racers pass eligibility, queue on a
    // lock, and only then race to commit. Their event instants are distinct
    // and 61 s apart (and 61 s after the last seed), so each can satisfy
    // the cooldown on its own merits; only the last rung separates them.
    let mut lock_tx = pool.begin().await?;
    let held: i32 = sqlx::query_scalar(
        "SELECT 1 FROM member_levels WHERE guild_id = $1 AND member_id = $2 FOR UPDATE",
    )
    .bind(GUILD)
    .bind(MEMBER)
    .fetch_one(&mut *lock_tx)
    .await?;
    assert_eq!(held, 1);

    let mut tasks = tokio::task::JoinSet::new();
    for index in 9..11_i64 {
        let pool = pool.clone();
        let instant = at(index * 61);
        let chan = channel(index);
        tasks.spawn(async move {
            store::award_message(&pool, GUILD, MEMBER, &instant, Some(&chan), None, false).await
        });
    }
    wait_for_lock_waiters(&pool, 2).await?;
    lock_tx.commit().await?;

    let mut awarded = Vec::new();
    while let Some(result) = tasks.join_next().await {
        awarded.push(result.expect("award task")?.awarded);
    }
    awarded.sort_unstable();
    assert_eq!(
        awarded,
        vec![0, 5],
        "exactly one racer may spend the last 5-XP rung; the other must cap to 0"
    );
    let profile = store::profile(&pool, GUILD, MEMBER).await?;
    assert_eq!(profile.message_xp, 100, "95 seeded + one last rung");
    assert_eq!(profile.xp, 100);
    let positive_audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM xp_awards
          WHERE guild_id = $1 AND member_id = $2 AND source = 'message' AND xp > 0",
    )
    .bind(GUILD)
    .bind(MEMBER)
    .fetch_one(&pool)
    .await?;
    assert_eq!(positive_audits, 10, "no eleventh positive message award");
    assert_eq!(seasonal_xp(&pool, GUILD, MEMBER).await?, Some(100));

    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn concurrent_voice_awards_at_distinct_instants_never_exceed_daily_cap() -> StoreResult<()> {
    let fixture = database().await;
    let pool = fixture.pool().clone();
    // Seed 23 one-minute voice awards through the real store: 23 x 5 = 115.
    // Exactly 5 XP of the 120 daily cap remains.
    for index in 0..23_i64 {
        let award = store::award_voice(
            &pool,
            GUILD,
            MEMBER,
            60,
            &at(index * 61),
            Some(&channel(index)),
        )
        .await?;
        assert_eq!(award.awarded, 5, "seed #{index}");
    }
    assert_eq!(store::profile(&pool, GUILD, MEMBER).await?.voice_xp, 115);

    let mut lock_tx = pool.begin().await?;
    let held: i32 = sqlx::query_scalar(
        "SELECT 1 FROM member_levels WHERE guild_id = $1 AND member_id = $2 FOR UPDATE",
    )
    .bind(GUILD)
    .bind(MEMBER)
    .fetch_one(&mut *lock_tx)
    .await?;
    assert_eq!(held, 1);

    let mut tasks = tokio::task::JoinSet::new();
    for index in 23..25_i64 {
        let pool = pool.clone();
        let instant = at(index * 61);
        let chan = channel(index);
        tasks.spawn(async move {
            store::award_voice(&pool, GUILD, MEMBER, 60, &instant, Some(&chan)).await
        });
    }
    wait_for_lock_waiters(&pool, 2).await?;
    lock_tx.commit().await?;

    let mut awarded = Vec::new();
    while let Some(result) = tasks.join_next().await {
        awarded.push(result.expect("award task")?.awarded);
    }
    awarded.sort_unstable();
    assert_eq!(
        awarded,
        vec![0, 5],
        "exactly one racer may spend the last 5 XP of the voice daily cap"
    );
    let profile = store::profile(&pool, GUILD, MEMBER).await?;
    assert_eq!(profile.voice_xp, 120, "the 120 XP voice daily cap holds");
    assert_eq!(profile.xp, 120);
    let positive_audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM xp_awards
          WHERE guild_id = $1 AND member_id = $2 AND source = 'voice' AND xp > 0",
    )
    .bind(GUILD)
    .bind(MEMBER)
    .fetch_one(&pool)
    .await?;
    assert_eq!(positive_audits, 24, "no twenty-fifth positive voice award");
    assert_eq!(seasonal_xp(&pool, GUILD, MEMBER).await?, Some(120));

    fixture.close().await.expect("drop test database");
    Ok(())
}
