use super::*;

async fn database() -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP scorecard retry integration: TWO_TEST_DATABASE_URL is not set");
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated agent-testdb fixture"),
    )
}

async fn saved(pool: &PgPool, guild: &str, week: &str) -> (i32, i64, bool) {
    sqlx::query_as(
        "SELECT attempts, next_attempt_at, completed FROM community_scorecard_attempts
                   WHERE guild_id=$1 AND week_key=$2",
    )
    .bind(guild)
    .bind(week)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn runs(pool: &PgPool, guild: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM community_scorecard_runs WHERE guild_id=$1")
        .bind(guild)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn scorecard_database_failure_then_restart_success_publishes_once() {
    let Some(fixture) = database().await else {
        return;
    };
    let pool = fixture.pool();
    let monday = at("2026-09-28T06:15:00.000Z");
    // Missing relations simulate transient coverage and scoring failures in
    // this unique disposable database only; no production/staging store.
    for (guild, table) in [
        ("coverage", "community_stream_heartbeats"),
        ("scoring", "community_facts"),
    ] {
        sqlx::query(&format!("ALTER TABLE {table} RENAME TO retry_hidden"))
            .execute(pool)
            .await
            .unwrap();
        let state = fresh_state(enabled_gates(), 14);
        assert_eq!(
            scorecard_once(pool, guild, &state, monday).await,
            Err(ErrorClass::Database)
        );
        assert_eq!(
            saved(pool, guild, "2026-09-28").await,
            (1, monday + RETRY_DELAY_MS, false)
        );
        sqlx::query(&format!("ALTER TABLE retry_hidden RENAME TO {table}"))
            .execute(pool)
            .await
            .unwrap();
        let restarted = fresh_state(enabled_gates(), 14);
        scorecard_once(pool, guild, &restarted, monday + RETRY_DELAY_MS - 1)
            .await
            .unwrap();
        assert_eq!(runs(pool, guild).await, 0);
        scorecard_once(pool, guild, &restarted, monday + RETRY_DELAY_MS)
            .await
            .unwrap();
        assert_eq!(runs(pool, guild).await, 1);
        assert_eq!(
            saved(pool, guild, "2026-09-28").await,
            (2, monday + 2 * RETRY_DELAY_MS, true)
        );
        // Simulate output committed but completion acknowledgement lost. A
        // changed classifier would defeat the old run-key reuse mechanism.
        sqlx::query("UPDATE community_scorecard_attempts SET completed=FALSE WHERE guild_id=$1")
            .bind(guild)
            .execute(pool)
            .await
            .unwrap();
        let mut restarted = fresh_state(enabled_gates(), 14);
        restarted.classifier_version = "community-after-restart".to_owned();
        restarted.capture_started_at = format_iso_millis(monday + 2 * RETRY_DELAY_MS);
        scorecard_once(pool, guild, &restarted, monday + 2 * RETRY_DELAY_MS)
            .await
            .unwrap();
        scorecard_once(pool, guild, &restarted, at("2026-09-28T06:59:59.999Z"))
            .await
            .unwrap();
        assert_eq!(runs(pool, guild).await, 1);
        assert_eq!(saved(pool, guild, "2026-09-28").await.0, 2);
        assert!(saved(pool, guild, "2026-09-28").await.2);
    }
    fixture.close().await.unwrap();
}

#[tokio::test]
async fn scorecard_three_database_failures_survive_restarts_and_reset_next_week() {
    let Some(fixture) = database().await else {
        return;
    };
    let pool = fixture.pool();
    let monday = at("2026-09-28T06:15:00.000Z");
    sqlx::query("ALTER TABLE community_stream_heartbeats RENAME TO retry_hidden")
        .execute(pool)
        .await
        .unwrap();
    for attempt in 1..=3 {
        let state = fresh_state(enabled_gates(), 14);
        assert_eq!(
            scorecard_once(
                pool,
                "failures",
                &state,
                monday + (i64::from(attempt) - 1) * RETRY_DELAY_MS
            )
            .await,
            Err(ErrorClass::Database)
        );
        assert_eq!(saved(pool, "failures", "2026-09-28").await.0, attempt);
    }
    sqlx::query("ALTER TABLE retry_hidden RENAME TO community_stream_heartbeats")
        .execute(pool)
        .await
        .unwrap();
    let restarted = fresh_state(enabled_gates(), 14);
    for now in [
        monday + 3 * RETRY_DELAY_MS,
        at("2026-09-28T06:59:59.999Z"),
        at("2026-09-28T07:00:00.000Z"),
    ] {
        scorecard_once(pool, "failures", &restarted, now)
            .await
            .unwrap();
    }
    assert_eq!(saved(pool, "failures", "2026-09-28").await.0, 3);
    assert_eq!(runs(pool, "failures").await, 0);
    scorecard_once(pool, "failures", &restarted, monday + 7 * 86_400_000)
        .await
        .unwrap();
    assert_eq!(saved(pool, "failures", "2026-10-05").await.0, 1);
    assert_eq!(runs(pool, "failures").await, 1);
    fixture.close().await.unwrap();
}

#[tokio::test]
async fn scorecard_concurrent_claims_share_one_durable_budget() {
    let Some(fixture) = database().await else {
        return;
    };
    let pool = fixture.pool();
    let monday = at("2026-09-28T06:15:00.000Z");
    let claims = tokio::join!(
        reserve_attempt(pool, "claims", monday),
        reserve_attempt(pool, "claims", monday),
        reserve_attempt(pool, "claims", monday),
        reserve_attempt(pool, "claims", monday),
    );
    assert_eq!(
        [claims.0, claims.1, claims.2, claims.3]
            .into_iter()
            .filter(|claim| claim.as_ref().unwrap().is_some())
            .count(),
        1
    );
    // A cancelled/crashed worker never settles its reservation. Reloading the
    // row still preserves its delay and counts it toward the three-slot cap.
    for attempt in 2..=3 {
        assert!(
            reserve_attempt(pool, "claims", monday + (attempt - 1) * RETRY_DELAY_MS)
                .await
                .unwrap()
                .is_some()
        );
    }
    assert!(reserve_attempt(pool, "claims", monday + 3 * RETRY_DELAY_MS)
        .await
        .unwrap()
        .is_none());
    assert_eq!(saved(pool, "claims", "2026-09-28").await.0, 3);
    assert!(
        reserve_attempt(pool, "outside", at("2026-09-28T07:00:00.000Z"))
            .await
            .unwrap()
            .is_none()
    );
    let outside: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM community_scorecard_attempts WHERE guild_id='outside'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(outside, 0, "out-of-window ticks write no attempt marker");
    fixture.close().await.unwrap();
}

#[tokio::test]
async fn scorecard_monday_boot_completes_with_honest_incomplete_coverage() {
    let Some(fixture) = database().await else {
        return;
    };
    let pool = fixture.pool();
    let monday = at("2026-09-28T06:15:00.000Z");
    let mut state = fresh_state(enabled_gates(), 14);
    state.capture_started_at = format_iso_millis(monday);
    scorecard_once(pool, "monday-boot", &state, monday)
        .await
        .unwrap();
    let status: String = sqlx::query_scalar(
        "SELECT coverage_state FROM community_scorecard_runs WHERE guild_id='monday-boot'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(status, "incomplete");
    let beats: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM community_stream_heartbeats WHERE guild_id='monday-boot'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(beats, 0, "no fabricated or inverted closed-week coverage");
    scorecard_once(
        pool,
        "monday-boot",
        &fresh_state(enabled_gates(), 14),
        monday + RETRY_DELAY_MS,
    )
    .await
    .unwrap();
    assert_eq!(runs(pool, "monday-boot").await, 1);
    assert_eq!(
        saved(pool, "monday-boot", "2026-09-28").await,
        (1, monday + RETRY_DELAY_MS, true)
    );
    fixture.close().await.unwrap();
}
