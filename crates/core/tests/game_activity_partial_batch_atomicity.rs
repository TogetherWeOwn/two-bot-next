//! Partial-batch aggregate atomicity acceptance.
//!
//! Retry contract: `upsert_guild_aggregates` must persist a flush
//! atomically. The flush loop requeues the whole drained batch when any row
//! fails, so a batch where an early row commits and a later row fails would
//! double-count the early row on retry. A failing batch must leave every
//! covered row exactly as it was, and the retry after the transient failure
//! lifts must add exactly one batch of counters.
//!
//! Fixture-only failure: a disposable `CHECK` constraint rejects one sentinel
//! game key. It lives in the test-owned database only, is dropped before the
//! retry, and never touches migrations, roles, or production code. Empty
//! input still writes nothing.
//!
//! Disposable database only (`TestDatabase` over the guarded agent-testdb
//! service). Skips without `TWO_TEST_DATABASE_URL`; never staging or
//! production.

#![cfg(feature = "db")]

use two_bot_core::game_activity::GuildGameAggregate;
use two_bot_core::game_activity_store::{read_guild_aggregates, upsert_guild_aggregates};
use two_bot_testsupport::TestDatabase;

const MIN_MS: i64 = 60_000;
const GUILD: &str = "guild-partial-batch";
/// Sentinel game rejected by the fixture-only `CHECK`; never a real title.
const POISON_GAME: &str = "poison-game-partial-batch";
const FIXTURE_CONSTRAINT: &str = "partial_batch_fixture_no_poison";

fn row(
    game: Option<&str>,
    total_play_ms: i64,
    in_voice_ms: i64,
    session_count: u64,
) -> GuildGameAggregate {
    GuildGameAggregate {
        guild_id: GUILD.to_owned(),
        game: game.map(str::to_owned),
        total_play_ms,
        in_voice_ms,
        session_count,
    }
}

async fn fixture() -> Option<TestDatabase> {
    match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => Some(
            TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
                .await
                .expect("create migrated test database"),
        ),
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure partial-batch atomicity acceptance"
            );
            eprintln!("SKIP game_activity_partial_batch_atomicity: TWO_TEST_DATABASE_URL not set");
            None
        }
    }
}

#[tokio::test]
async fn partial_batch_failure_leaves_no_rows_committed_and_retry_counts_once() {
    let Some(db) = fixture().await else { return };
    let pool = db.pool();

    // Empty input writes nothing; preserved while the retry contract lands.
    assert_eq!(
        upsert_guild_aggregates(pool, &[])
            .await
            .expect("empty write"),
        0,
        "empty flush writes nothing"
    );

    // Seed baseline: one titled row plus one untitled row for the guild.
    assert_eq!(
        upsert_guild_aggregates(
            pool,
            &[
                row(Some("Apex"), 30 * MIN_MS, MIN_MS, 1),
                row(None, MIN_MS, 0, 1),
            ],
        )
        .await
        .expect("seed baseline"),
        2,
        "seed writes two rows"
    );
    let baseline = read_guild_aggregates(pool, GUILD)
        .await
        .expect("read baseline");
    assert_eq!(baseline.len(), 2, "baseline holds both seeded rows");

    // Fixture-only failure: the later row is rejected after the earlier row
    // would have committed under a row-at-a-time write.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "ALTER TABLE game_activity_guild_aggregates \
         ADD CONSTRAINT {FIXTURE_CONSTRAINT} CHECK (game <> '{POISON_GAME}')"
    )))
    .execute(pool)
    .await
    .expect("install fixture-only failure");
    let failing_batch = vec![
        row(Some("Apex"), 10 * MIN_MS, 0, 2),
        row(Some(POISON_GAME), MIN_MS, 0, 1),
    ];
    assert!(
        upsert_guild_aggregates(pool, &failing_batch).await.is_err(),
        "batch with a failing later row must error"
    );
    assert_eq!(
        read_guild_aggregates(pool, GUILD)
            .await
            .expect("read after failed batch"),
        baseline,
        "failed batch must leave no earlier row added or changed, \
         or the whole-batch retry double-counts it"
    );

    // Lift the transient failure and retry the identical batch: exactly one
    // batch of counters lands, never the partial row plus the retry.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "ALTER TABLE game_activity_guild_aggregates DROP CONSTRAINT {FIXTURE_CONSTRAINT}"
    )))
    .execute(pool)
    .await
    .expect("remove fixture-only failure");
    assert_eq!(
        upsert_guild_aggregates(pool, &failing_batch)
            .await
            .expect("retry after transient failure"),
        2,
        "retry writes both rows"
    );
    let retried = read_guild_aggregates(pool, GUILD)
        .await
        .expect("read after retry");
    assert_eq!(retried.len(), 3, "retry adds only the new sentinel row");
    let apex = retried
        .iter()
        .find(|row| row.game.as_deref() == Some("Apex"))
        .expect("Apex row present");
    assert_eq!(
        (apex.total_play_ms, apex.in_voice_ms, apex.session_count),
        (40 * MIN_MS, MIN_MS, 3),
        "Apex accumulates exactly one batch (30+10 play, 1+2 sessions)"
    );
    let poison = retried
        .iter()
        .find(|row| row.game.as_deref() == Some(POISON_GAME))
        .expect("sentinel row present after retry");
    assert_eq!(
        (
            poison.total_play_ms,
            poison.in_voice_ms,
            poison.session_count
        ),
        (MIN_MS, 0, 1),
        "retried sentinel row carries exactly one batch of counters"
    );
    let untitled = retried
        .iter()
        .find(|row| row.game.is_none())
        .expect("untitled row untouched");
    assert_eq!(
        (untitled.total_play_ms, untitled.session_count),
        (MIN_MS, 1),
        "rows outside the batch never move"
    );

    db.close().await.expect("drops fixture database");
}
