//! Builder regressions for the inclusive fact ceiling and typed refusal.
#![cfg(feature = "db")]

use two_bot_core::community_store::{
    load_scorecard_inputs, run_closed_week, CommunityStoreError, ScorecardLoad,
};
use two_bot_testsupport::TestDatabase;

#[tokio::test]
async fn exactly_ten_thousand_facts_load_but_one_more_refuses_without_publishing() {
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("TWO_TEST_DATABASE_URL must name the disposable test bootstrap");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create guarded scorecard boundary fixture");
    let pool = db.pool();
    let params = ScorecardLoad {
        guild_id: "boundary-guild".into(),
        classifier_version: "boundary-v1".into(),
        week_start: "2026-08-31T00:00:00.000Z".into(),
        week_end: "2026-09-07T00:00:00.000Z".into(),
        watermark: 10_000,
        generated_at: "2026-09-07T06:15:00.000Z".into(),
        recommendations_enabled: true,
        correction_cycles: 0,
    };
    sqlx::query(
        "INSERT INTO community_facts
           (id,guild_id,event_type,source_event_id,occurred_at,source,
            classifier_version,classification,matched_rule,idempotency_key)
         SELECT n,$1,'message_created','boundary-' || n,$2,'fixture',
                $3,'test','fixture','boundary-key-' || n
           FROM generate_series(1, 10000) AS n",
    )
    .bind(&params.guild_id)
    .bind("2026-09-02T12:00:00.000Z")
    .bind(&params.classifier_version)
    .execute(pool)
    .await
    .expect("seed exactly 10000 facts");
    let at_ceiling = load_scorecard_inputs(pool, &params).await;
    sqlx::query(
        "INSERT INTO community_facts
           (id,guild_id,event_type,source_event_id,occurred_at,source,
            classifier_version,classification,matched_rule,idempotency_key)
         VALUES (10001,$1,'message_created','boundary-over',$2,'fixture',
                 $3,'test','fixture','boundary-key-over')",
    )
    .bind(&params.guild_id)
    .bind("2026-09-02T12:00:00.000Z")
    .bind(&params.classifier_version)
    .execute(pool)
    .await
    .expect("seed the 10001st fact");
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM community_facts")
        .fetch_one(pool)
        .await
        .expect("count boundary facts");
    let over_params = ScorecardLoad {
        watermark: 10_001,
        ..params
    };
    let over_ceiling = load_scorecard_inputs(pool, &over_params).await;
    let run = run_closed_week(pool, over_params).await;
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM community_scorecard_runs),
                (SELECT count(*) FROM community_scorecard_alerts)",
    )
    .fetch_one(pool)
    .await
    .expect("inspect publication counts");
    db.close().await.expect("close owned boundary fixture");

    let loaded = at_ceiling.expect("exactly 10000 scoped facts must load");
    assert_eq!(loaded.facts.len(), 10_000);
    assert!(loaded.facts.iter().map(|fact| fact.id).eq(1..=10_000));
    assert_eq!(count, 10_001);
    assert!(matches!(
        over_ceiling,
        Err(CommunityStoreError::TooManyFacts { ceiling: 10_000 })
    ));
    assert!(matches!(
        run,
        Err(CommunityStoreError::TooManyFacts { ceiling: 10_000 })
    ));
    assert_eq!(counts, (0, 0), "refusal must not publish runs or alerts");
}
