//! Builder regression for the scheduler's capacity-refusal classification.
#![cfg(test)]

use super::*;
use two_bot_testsupport::TestDatabase;

#[tokio::test]
async fn scorecard_over_ceiling_is_configuration_and_consumes_one_attempt() {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP scorecard capacity integration: TWO_TEST_DATABASE_URL is not set");
        return;
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create guarded scheduler boundary fixture");
    let pool = db.pool();
    let guild = "scorecard-capacity";
    let monday = parse_iso_millis("2026-09-28T06:15:00.000Z").expect("Monday clock");
    let state = State {
        capture_started_at: "2026-09-28T06:15:00.000Z".into(),
        classifier_version: "boundary-v1".into(),
        gates: ScorecardGates {
            enabled: true,
            recommendations_enabled: false,
            correction_cycles: 0,
        },
        inactivity_days: 14,
        scorecard_lane: Mutex::new(()),
        probe_lease: Mutex::new(None),
    };
    let seeded = sqlx::query(
        "INSERT INTO community_facts
           (guild_id,event_type,source_event_id,occurred_at,source,
            classifier_version,classification,matched_rule,idempotency_key)
         SELECT $1,'message_created','scheduler-' || n,
                '2026-09-23T12:00:00.000Z','fixture',
                'boundary-v1','test','fixture','scheduler-key-' || n
           FROM generate_series(1, 10001) AS n",
    )
    .bind(guild)
    .execute(pool)
    .await
    .expect("seed over-ceiling closed week")
    .rows_affected();
    let outcome = scorecard_once(pool, guild, &state, monday).await;
    let attempt: (i32, i64, bool) = sqlx::query_as(
        "SELECT attempts, next_attempt_at, completed FROM community_scorecard_attempts
         WHERE guild_id=$1 AND week_key='2026-09-28'",
    )
    .bind(guild)
    .fetch_one(pool)
    .await
    .expect("read durable reservation");
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM community_scorecard_runs WHERE guild_id=$1),
                (SELECT count(*) FROM community_scorecard_alerts WHERE guild_id=$1)",
    )
    .bind(guild)
    .fetch_one(pool)
    .await
    .expect("read publication counts");
    db.close().await.expect("close owned scheduler fixture");

    assert_eq!(seeded, 10_001);
    assert_eq!(outcome, Err(ErrorClass::Configuration));
    assert_eq!(attempt, (1, monday + 300_000, false));
    assert_eq!(counts, (0, 0), "capacity refusal publishes no run or alert");
}
