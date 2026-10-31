//! Protected acceptance for the real Monday store path, using only the
//! guarded disposable database. No ignored tests or CI opt-in are needed.
#![cfg(feature = "db")]

use sqlx::{Pool, Postgres};
use two_bot_core::community::{
    build_scorecard, FactRow, ScorecardInputs, StreamCoverage, COMMUNITY_FACT_TYPES,
};
use two_bot_core::community_store::{
    load_scorecard_inputs, mark_stream_coverage, run_closed_week, CommunityStoreError,
    ScorecardLoad,
};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "load-acceptance-guild";
const VERSION: &str = "load-acceptance-v1";
const START: &str = "2026-08-31T00:00:00.000Z";
const END: &str = "2026-09-07T00:00:00.000Z";
// Acceptance resource ceiling: the store must refuse a 2x-10,000-fact week,
// not return the first 10,000 as though it represented the entire week.
// The Builder may select a stricter ceiling; the ordinary fixture is tiny.
const WEEK_FACT_CEILING: i64 = 10_000;

fn load(watermark: i64) -> ScorecardLoad {
    ScorecardLoad {
        guild_id: GUILD.into(),
        classifier_version: VERSION.into(),
        week_start: START.into(),
        week_end: END.into(),
        watermark,
        generated_at: "2026-09-07T06:15:00.000Z".into(),
        recommendations_enabled: true,
        correction_cycles: 0,
    }
}

async fn fixture() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("TWO_TEST_DATABASE_URL must name the authorized disposable test bootstrap");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .unwrap_or_else(|_| panic!("create guarded disposable scorecard database"))
}

async fn cover_week(pool: &Pool<Postgres>) {
    for stream in COMMUNITY_FACT_TYPES {
        mark_stream_coverage(pool, GUILD, stream, START, END, END)
            .await
            .unwrap_or_else(|_| panic!("seed complete stream coverage"));
    }
}

async fn seed_row(pool: &Pool<Postgres>, row: &FactRow) {
    sqlx::query(
        "INSERT INTO community_facts
          (id, guild_id, event_type, source_event_id, actor_id, occurred_at, source,
           classifier_version, classification, matched_rule, metadata, idempotency_key)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)",
    )
    .bind(row.id)
    .bind(&row.guild_id)
    .bind(&row.event_type)
    .bind(&row.source_event_id)
    .bind(&row.actor_id)
    .bind(&row.occurred_at)
    .bind(&row.source)
    .bind(&row.classifier_version)
    .bind(&row.classification)
    .bind(&row.matched_rule)
    .bind(&row.metadata)
    .bind(&row.idempotency_key)
    .execute(pool)
    .await
    .unwrap_or_else(|_| panic!("seed scorecard row"));
}

fn normal_row(id: i64, kind: &str, at: &str) -> FactRow {
    FactRow {
        id,
        guild_id: GUILD.into(),
        event_type: kind.into(),
        source_event_id: format!("event-{id}"),
        actor_id: Some(format!("actor-{id}")),
        occurred_at: at.into(),
        source: "channel:café".into(),
        classifier_version: VERSION.into(),
        classification: "eligible_human".into(),
        matched_rule: "fixture".into(),
        metadata: Some("{ \"channelClass\": \"human\", \"contentLength\": 42 }".into()),
        idempotency_key: format!("fact-{id}"),
    }
}

#[tokio::test]
async fn ordinary_load_preserves_every_scoped_byte_and_id_order() {
    let db = fixture().await;
    let pool = db.pool();
    cover_week(pool).await;
    let first = normal_row(4, "message_created", START);
    let mut second = normal_row(9, "member_joined", "2026-09-02T12:34:56.789Z");
    second.actor_id = None;
    second.metadata = None;
    let third = normal_row(21, "message_created", "2026-09-06T23:59:59.999Z");
    // Insert out of order; raw metadata whitespace, UTF-8, NULLs and exact
    // timestamp precision must survive without normalization or sampling.
    for row in [&third, &first, &second] {
        seed_row(pool, row).await;
    }
    let mut foreign = normal_row(5, "message_created", "2026-09-03T00:00:00.000Z");
    foreign.guild_id = "foreign-guild".into();
    for row in [
        foreign,
        normal_row(6, "message_created", "2026-08-30T23:59:59.999Z"),
        normal_row(7, "message_created", END),
        normal_row(22, "message_created", "2026-09-03T00:00:00.000Z"),
    ] {
        seed_row(pool, &row).await;
    }
    let params = load(21);
    let actual = load_scorecard_inputs(pool, &params)
        .await
        .unwrap_or_else(|_| panic!("ordinary scoped week must load"));
    let expected = ScorecardInputs {
        guild_id: GUILD.into(),
        classifier_version: VERSION.into(),
        week_start: START.into(),
        week_end: END.into(),
        watermark: 21,
        generated_at: params.generated_at,
        recommendations_enabled: true,
        correction_cycles: 0,
        facts: vec![first, second, third],
        coverage: COMMUNITY_FACT_TYPES
            .iter()
            .map(|stream| StreamCoverage {
                stream: (*stream).into(),
                covered_from: START.into(),
                covered_through: END.into(),
            })
            .collect(),
        duplicate_source_ids: false,
        existing_revision_count: 0,
        existing_scorecard_json: None,
    };
    db.close().await.expect("close owned scorecard fixture");
    // Heartbeat query order is not a contract; fact ID encounter order is.
    let mut actual = actual;
    actual.coverage.sort_by(|a, b| a.stream.cmp(&b.stream));
    let mut expected = expected;
    expected.coverage.sort_by(|a, b| a.stream.cmp(&b.stream));
    assert_eq!(
        actual, expected,
        "normal load must preserve all scoped input bytes"
    );
    assert_eq!(
        build_scorecard(actual)
            .expect("loaded build")
            .scorecard_json,
        build_scorecard(expected)
            .expect("literal input build")
            .scorecard_json,
        "the capped store must not perturb ordinary scorecard bytes"
    );
}

fn unrepresentative_refusal<T>(result: &Result<T, CommunityStoreError>) -> bool {
    result.as_ref().err().is_some_and(|error| {
        // A failed SQL statement or unknown fixture type is infra/setup RED,
        // never proof of the cap's domain refusal.
        if matches!(
            error,
            CommunityStoreError::Db(_) | CommunityStoreError::UnknownFactType(_)
        ) {
            return false;
        }
        let reason = error.to_string().to_ascii_lowercase();
        reason.contains("unrepresentative") || reason.contains("insufficient")
    })
}

#[tokio::test]
async fn twice_the_week_ceiling_refuses_loading_and_publishing_a_sampled_run() {
    let db = fixture().await;
    let pool = db.pool();
    cover_week(pool).await;
    let seeded = 2 * WEEK_FACT_CEILING;
    sqlx::query(
        "INSERT INTO community_facts
           (guild_id,event_type,source_event_id,actor_id,occurred_at,source,
            classifier_version,classification,matched_rule,metadata,idempotency_key)
         SELECT $1,'message_created','bulk-' || n,NULL,$2,'fixture',
                $3,'test','fixture',NULL,'bulk-key-' || n
           FROM generate_series(1, $4::bigint) AS n",
    )
    .bind(GUILD)
    .bind("2026-09-02T12:00:00.000Z")
    .bind(VERSION)
    .bind(seeded)
    .execute(pool)
    .await
    .unwrap_or_else(|_| panic!("seed exactly 2x the acceptance ceiling"));
    let (count, watermark): (i64, i64) =
        sqlx::query_as("SELECT count(*), max(id) FROM community_facts WHERE guild_id = $1")
            .bind(GUILD)
            .fetch_one(pool)
            .await
            .unwrap_or_else(|_| panic!("verify seeded size and watermark"));
    let loaded = load_scorecard_inputs(pool, &load(watermark)).await;
    let run = run_closed_week(pool, load(watermark)).await;
    let (runs, alerts): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM community_scorecard_runs),
                (SELECT count(*) FROM community_scorecard_alerts)",
    )
    .fetch_one(pool)
    .await
    .unwrap_or_else(|_| panic!("inspect durable run and alert counts"));
    db.close().await.expect("close owned scorecard fixture");
    assert_eq!(count, 20_000, "fixture must really exceed the cap");
    assert_eq!(
        (
            unrepresentative_refusal(&loaded),
            unrepresentative_refusal(&run),
            runs,
            alerts
        ),
        (true, true, 0, 0),
        "a 2x-cap week must fail as unrepresentative, never silently truncate, persist, or notify"
    );
}
