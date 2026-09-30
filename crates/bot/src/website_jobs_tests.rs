use super::*;
use serde_json::json;
use sqlx::postgres::PgPoolOptions;

#[allow(dead_code)]
#[path = "../../discord/tests/common/mod.rs"]
mod common;
#[path = "../../core/tests/common/website_db.rs"]
mod website_db;

use common::{MockRest, ScriptedResponse};

fn executor(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy("synthetic-job-test-token".to_owned(), Some(mock.origin())).unwrap()
}

fn member(id: u64, bot: bool, roles: &[&str]) -> Value {
    json!({"user": {"id": id.to_string(), "bot": bot}, "roles": roles})
}

fn event() -> Value {
    json!({"id":"3000", "name":"Test night", "scheduled_start_time":"2026-10-01T19:00:00Z", "status":1, "channel_id":null, "description":"fixture"})
}

#[tokio::test]
async fn roster_paginates_and_rejects_failed_or_repeated_pages() {
    let page: Vec<_> = (1..=1000).map(|id| member(id, false, &[])).collect();
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!(page)),
            ScriptedResponse::json(200, json!([member(1001, false, &[])])),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let members = roster(&executor(&mock), "2222").await.unwrap();
    assert_eq!(members.len(), 1001);
    let requests = mock.requests();
    assert!(requests[0]
        .path
        .starts_with("/api/v10/guilds/2222/members?"));
    assert!(requests[0].path.contains("limit=1000"));
    assert!(requests[1].path.contains("limit=1000"));
    assert!(
        requests[1].path.contains("after=1000"),
        "{}",
        requests[1].path
    );
    mock.shutdown().await;

    for response in [
        ScriptedResponse::status(403),
        ScriptedResponse::json(200, json!({"not":"array"})),
        ScriptedResponse::json(200, json!([member(2, false, &[]), member(2, false, &[])])),
        ScriptedResponse::json(200, json!([{"user":{"id":"2"},"roles":null}])),
    ] {
        let mock = MockRest::start(vec![response], ScriptedResponse::status(500)).await;
        assert_eq!(
            roster(&executor(&mock), "2222").await.err(),
            Some(ErrorClass::Rest)
        );
        mock.shutdown().await;
    }
}

/// Real migrations + actual REST executor/mock + all three scheduled actions.
/// Shares the existing strict test-container guard, but uses a unique schema:
/// no public reset and no interference with the core acceptance tests.
#[tokio::test]
async fn three_website_ticks_publish_rows_and_fail_closed() {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP website job integration: TWO_TEST_DATABASE_URL is not set");
        return;
    };
    let options = website_db::test_db_options(&url).expect("refusing non-test database");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await
        .expect("agent-testdb only");
    let schema = format!("jobs_10855_{:016x}", rand::random::<u64>());
    // Audited identifiers contain only this fixed prefix and random hex digits.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let search_path = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .after_connect(move |connection, _| {
            let search_path = search_path.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('search_path', $1, false)")
                    .bind(&search_path)
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await
        .unwrap();
    sqlx::migrate!("../cutover/migrations")
        .run(&pool)
        .await
        .unwrap();
    apply_web_contract(&pool).await.unwrap();
    let guild = "2222";

    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let rest = executor(&mock);
    run_once(Kind::Rank, &pool, &rest, guild).await.unwrap();
    run_once(Kind::Counter, &pool, &rest, guild).await.unwrap();
    assert!(
        mock.requests().is_empty(),
        "ungrounded raid history skips before REST"
    );
    mock.shutdown().await;

    for (index, anomaly) in two_bot_core::RAID_ANOMALIES.iter().enumerate() {
        let (start, _) = two_bot_core::window_bounds(anomaly.start, anomaly.end).unwrap();
        sqlx::query("INSERT INTO members (guild_id, member_id, joined_at, is_bot) VALUES ($1,$2,$3::timestamptz,FALSE)")
            .bind(guild).bind((9000 + index).to_string()).bind(start).execute(&pool).await.unwrap();
    }
    let members = json!([
        member(1000, false, &["11", "12"]),
        member(1001, false, &["11"]),
        member(1002, true, &[]),
        member(9000, false, &[])
    ]);
    let roles = json!({"roles": [
        {"id":"11","name":"Prospect"}, {"id":"12","name":"Member"},
        {"id":"13","name":"Soldier"}, {"id":"14","name":"Veteran"}, {"id":"15","name":"Legend"},
    ]});
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, members.clone()),
            ScriptedResponse::json(200, members),
            ScriptedResponse::json(200, roles),
            ScriptedResponse::json(200, json!([event()])),
            ScriptedResponse::json(200, json!({"not":"an array"})),
            ScriptedResponse::json(200, json!([event(), {"id":"bad"}])),
            ScriptedResponse::status(403),
            ScriptedResponse::json(200, json!([])),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);
    // Exercise the supervisor as well as the adapters, sequentially for the
    // ordered-response mock. Each job has an immediate first deadline.
    for (name, kind) in NAMES
        .into_iter()
        .zip([Kind::Counter, Kind::Rank, Kind::Events])
    {
        let pool = pool.clone();
        let rest = rest.clone();
        let status = jobs::statuses(&[name], false);
        let (stop, rx) = watch::channel(false);
        let task = tokio::spawn(jobs::supervise(
            vec![Job {
                name,
                cadence: Duration::from_secs(600),
                startup_jitter: Duration::ZERO,
                timeout: Duration::from_secs(10),
                action: Arc::new(move || {
                    let pool = pool.clone();
                    let rest = rest.clone();
                    Box::pin(async move { run_once(kind, &pool, &rest, "2222").await })
                }),
            }],
            status.clone(),
            rx,
        ));
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let current = status.read().await[name].clone();
                assert_eq!(current.last_error_class, None, "scheduled action failed");
                if current.last_success.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        stop.send(true).unwrap();
        task.await.unwrap();
    }
    let count: i32 =
        sqlx::query_scalar("SELECT human_member_count FROM guild_counters WHERE guild_id=$1")
            .bind(guild)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 2, "bot and grounded raid join excluded");
    for (query, expected) in [
        (
            "SELECT count(*) FROM counter_snapshots WHERE guild_id=$1",
            1_i64,
        ),
        ("SELECT count(*) FROM rank_snapshots WHERE guild_id=$1", 5),
        ("SELECT count(*) FROM member_ranks WHERE guild_id=$1", 2),
        ("SELECT count(*) FROM scheduled_events WHERE guild_id=$1", 1),
    ] {
        let rows: i64 = sqlx::query_scalar(query)
            .bind(guild)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, expected, "{query}");
    }
    for _ in 0..3 {
        assert_eq!(
            run_once(Kind::Events, &pool, &rest, guild).await,
            Err(ErrorClass::Rest)
        );
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM scheduled_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1, "failed/malformed reads preserve mirror");
    }
    run_once(Kind::Events, &pool, &rest, guild).await.unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM scheduled_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "valid empty response clears mirror");
    assert_eq!(mock.requests().len(), 8);
    mock.shutdown().await;
    pool.close().await;
    // Same generated hex-only identifiers as the CREATE above.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP SCHEMA {schema}_web_v1 CASCADE; DROP SCHEMA {schema} CASCADE;"
    )))
    .execute(&admin)
    .await
    .unwrap();
    admin.close().await;
}
