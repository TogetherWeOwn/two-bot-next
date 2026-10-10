use super::*;
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use two_bot_core::apply_web_contract;
use two_bot_testsupport::{guard_database_url, TestDatabase};

use crate::discord_test_common::{MockRest, ScriptedResponse};

fn executor(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy(
        "synthetic-snapshot-test-token".to_owned(),
        Some(mock.origin()),
    )
    .unwrap()
}

fn event(id: &str) -> Value {
    json!({"id":id, "name":"Test night", "scheduled_start_time":"2026-10-01T19:00:00Z", "status":1})
}

fn malformed_snapshots() -> Vec<Value> {
    let mut malformed = vec![
        json!(null),
        json!(true),
        json!(false),
        json!(0),
        json!(42),
        json!(""),
        json!("event"),
        json!([]),
        json!({}),
    ];
    for field in ["channel_id", "description"] {
        for value in [json!(42), json!(true), json!(false), json!([]), json!({})] {
            let mut row = event("3001");
            row[field] = value;
            malformed.push(row);
        }
    }
    for value in [
        json!(null),
        json!("1"),
        json!(true),
        json!(1.5),
        json!(0),
        json!([]),
        json!({}),
    ] {
        let mut row = event("3001");
        row["status"] = value;
        malformed.push(row);
    }
    malformed
        .into_iter()
        .flat_map(|bad| {
            [
                json!([bad.clone()]),
                json!([event("3000"), bad.clone()]),
                json!([bad, event("3000")]),
            ]
        })
        .collect()
}

/// A lazy pool never connects unless malformed/stopped work reaches storage.
/// Its short acquire timeout bounds a regression, and only the test service is named.
fn poison_pool() -> PgPool {
    guard_database_url("postgres://agent_test:@agent-testdb:5432/two_bot_test_poison")
        .expect("poison pool must reject ambient PostgreSQL connection settings");
    PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(20))
        .connect_lazy_with(
            PgConnectOptions::new_without_pgpass()
                .host("agent-testdb")
                .port(5432)
                .username("agent_test")
                .password("")
                .database("two_bot_test_poison"),
        )
}

#[tokio::test]
async fn malformed_event_elements_and_optionals_never_reach_storage() {
    let snapshots = malformed_snapshots();
    let mock = MockRest::start(
        snapshots
            .iter()
            .cloned()
            .map(|v| ScriptedResponse::json(200, v))
            .collect(),
        ScriptedResponse::status(403),
    )
    .await;
    let pool = poison_pool();
    let (_stop, shutdown) = watch::channel(false);
    for snapshot in &snapshots {
        assert_eq!(
            run_once(
                Kind::Events,
                &pool,
                &executor(&mock),
                "2222",
                &Mutex::new(()),
                &shutdown,
                false
            )
            .await,
            Err(ErrorClass::Rest),
            "{snapshot}"
        );
    }
    assert_eq!(mock.requests().len(), snapshots.len());
    mock.shutdown().await;
    pool.close().await;
}

#[test]
fn valid_optional_combinations_keep_their_representation() {
    let values = [
        None,
        Some(Value::Null),
        Some(json!("")),
        Some(json!("value")),
    ];
    for channel in &values {
        for description in &values {
            let mut row = event("3000");
            if let Some(value) = channel {
                row["channel_id"] = value.clone();
            }
            if let Some(value) = description {
                row["description"] = value.clone();
            }
            let normalized = normalize_events(&[raw_event(&row).unwrap()]).unwrap();
            assert_eq!(
                normalized[0].channel_id.as_deref(),
                channel.as_ref().and_then(Value::as_str)
            );
            assert_eq!(
                normalized[0].description.as_deref(),
                description.as_ref().and_then(Value::as_str)
            );
            assert_eq!(normalized[0].starts_at, "2026-10-01T19:00:00.000Z");
        }
    }
}

#[tokio::test]
async fn stopped_or_closed_attempts_and_queued_observations_do_no_io() {
    let pool = poison_pool();
    let mock = MockRest::start(vec![], ScriptedResponse::status(403)).await;
    let rest = executor(&mock);
    for kind in [Kind::Counter, Kind::Rank, Kind::Events] {
        for closed in [false, true] {
            let (stop, shutdown) = watch::channel(!closed);
            if closed {
                drop(stop);
            }
            assert_eq!(
                run_once(
                    kind,
                    &pool,
                    &rest,
                    "2222",
                    &Mutex::new(()),
                    &shutdown,
                    false
                )
                .await,
                Ok(())
            );
        }
    }
    for kind in [Kind::Counter, Kind::Rank] {
        let observation = Mutex::new(());
        let held = observation.lock().await;
        let (stop, shutdown) = watch::channel(false);
        let attempt = run_once(kind, &pool, &rest, "2222", &observation, &shutdown, false);
        tokio::pin!(attempt);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut attempt)
                .await
                .is_err()
        );
        stop.send_replace(true);
        stop.send_replace(true);
        tokio::time::timeout(Duration::from_secs(1), &mut attempt)
            .await
            .unwrap()
            .unwrap();
        drop(held);
    }
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
    pool.close().await;
}

async fn fixture() -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP snapshot DB integration: TWO_TEST_DATABASE_URL is not set");
        return None;
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .unwrap();
    apply_web_contract(db.pool()).await.unwrap();
    Some(db)
}

async fn mirror(pool: &PgPool) -> Vec<String> {
    let mut rows = Vec::new();
    for table in [
        "scheduled_events",
        "web_contract_meta",
        "guild_counters",
        "counter_snapshots",
        "rank_snapshots",
        "rank_ladder",
        "member_ranks",
        "member_exclusions",
    ] {
        // Fixed fixture-only identifiers; include every field and timestamp.
        let sql = sqlx::AssertSqlSafe(format!(
            "SELECT to_jsonb(t)::text FROM {table} t ORDER BY to_jsonb(t)::text"
        ));
        rows.extend(
            sqlx::query_scalar::<_, String>(sql)
                .fetch_all(pool)
                .await
                .unwrap(),
        );
    }
    rows
}

#[tokio::test]
async fn malformed_responses_preserve_full_mirror_and_valid_replacement_is_atomic() {
    let Some(db) = fixture().await else {
        return;
    };
    let pool = db.pool();
    let (_stop, shutdown) = watch::channel(false);
    let observation = Mutex::new(());
    let seed = normalize_events(&[raw_event(&event("3000")).unwrap()]).unwrap();
    replace_events(pool, "2222", "2026-09-30T12:00:00.123Z", &seed)
        .await
        .unwrap();
    replace_events(pool, "3333", "2026-09-30T12:00:00.123Z", &seed)
        .await
        .unwrap();
    let before = mirror(pool).await;
    let snapshots = malformed_snapshots();
    let mock = MockRest::start(
        snapshots
            .iter()
            .cloned()
            .map(|v| ScriptedResponse::json(200, v))
            .collect(),
        ScriptedResponse::status(403),
    )
    .await;
    let rest = executor(&mock);
    for snapshot in &snapshots {
        assert_eq!(
            run_once(
                Kind::Events,
                pool,
                &rest,
                "2222",
                &observation,
                &shutdown,
                false
            )
            .await,
            Err(ErrorClass::Rest),
            "{snapshot}"
        );
        assert_eq!(
            mirror(pool).await,
            before,
            "invalid response changed rows/timestamps/contract pin: {snapshot}"
        );
    }
    mock.shutdown().await;

    // An insert failure after DELETE and the first INSERT must roll back both,
    // including the contract pin. The constraint exists only in this disposable DB.
    sqlx::query(
        "ALTER TABLE scheduled_events ADD CONSTRAINT fixture_reject CHECK (event_id <> '3999')",
    )
    .execute(pool)
    .await
    .unwrap();
    let mut optionals = Vec::new();
    for (i, channel) in [
        None,
        Some(Value::Null),
        Some(json!("")),
        Some(json!("value")),
    ]
    .into_iter()
    .enumerate()
    {
        for (j, description) in [
            None,
            Some(Value::Null),
            Some(json!("")),
            Some(json!("value")),
        ]
        .into_iter()
        .enumerate()
        {
            let mut row = event(&(3100 + i * 4 + j).to_string());
            if let Some(value) = &channel {
                row["channel_id"] = value.clone();
            }
            if let Some(value) = description {
                row["description"] = value;
            }
            optionals.push(row);
        }
    }
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!([event("3001"), event("3999")])),
            ScriptedResponse::json(200, json!(optionals)),
            ScriptedResponse::json(200, json!([])),
        ],
        ScriptedResponse::status(403),
    )
    .await;
    let rest = executor(&mock);
    assert_eq!(
        run_once(
            Kind::Events,
            pool,
            &rest,
            "2222",
            &observation,
            &shutdown,
            false
        )
        .await,
        Err(ErrorClass::Database)
    );
    assert_eq!(
        mirror(pool).await,
        before,
        "failed replacement was not atomic"
    );
    run_once(
        Kind::Events,
        pool,
        &rest,
        "2222",
        &observation,
        &shutdown,
        false,
    )
    .await
    .unwrap();
    type EventRow = (String, Option<String>, Option<String>, String, String);
    let rows: Vec<EventRow> = sqlx::query_as(
        "SELECT event_id, channel_id, description, starts_at, updated_at FROM scheduled_events WHERE guild_id='2222' ORDER BY event_id"
    ).fetch_all(pool).await.unwrap();
    assert_eq!(rows.len(), 16);
    for (actual, expected) in rows.iter().zip(&optionals) {
        assert_eq!(actual.0, expected["id"].as_str().unwrap());
        assert_eq!(actual.1.as_deref(), expected["channel_id"].as_str());
        assert_eq!(actual.2.as_deref(), expected["description"].as_str());
        assert_eq!(actual.3, "2026-10-01T19:00:00.000Z");
        assert_eq!(actual.4, rows[0].4, "one replacement has one timestamp");
    }
    let pin: String =
        sqlx::query_scalar("SELECT guild_id FROM web_contract_meta WHERE singleton=TRUE")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(pin, "2222");
    run_once(
        Kind::Events,
        pool,
        &rest,
        "2222",
        &observation,
        &shutdown,
        false,
    )
    .await
    .unwrap();
    let remaining: Vec<String> =
        sqlx::query_scalar("SELECT guild_id FROM scheduled_events ORDER BY guild_id")
            .fetch_all(pool)
            .await
            .unwrap();
    assert_eq!(
        remaining,
        ["3333"],
        "valid empty response clears only its own guild"
    );
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn stop_during_fetch_discards_snapshot_results_without_post_shutdown_publication() {
    let Some(db) = fixture().await else {
        return;
    };
    let pool = db.pool();
    for (index, anomaly) in two_bot_core::RAID_ANOMALIES.iter().enumerate() {
        let (start, _) = two_bot_core::window_bounds(anomaly.start, anomaly.end).unwrap();
        sqlx::query("INSERT INTO members (guild_id, member_id, joined_at, is_bot) VALUES ('2222',$1,$2::timestamptz,FALSE)")
            .bind((9000 + index).to_string()).bind(start).execute(pool).await.unwrap();
    }
    let members = json!([{"user":{"id":"1000","bot":false},"roles":["11"]}]);
    let roles = json!({"roles":[
        {"id":"11","name":"Prospect"}, {"id":"12","name":"Member"},
        {"id":"13","name":"Soldier"}, {"id":"14","name":"Veteran"}, {"id":"15","name":"Legend"}
    ]});
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, members.clone()),
            ScriptedResponse::json(200, members.clone()),
            ScriptedResponse::json(200, roles.clone()),
            ScriptedResponse::json(200, json!([event("3000")])),
        ],
        ScriptedResponse::status(403),
    )
    .await;
    let (_stop, shutdown) = watch::channel(false);
    let observation = Arc::new(Mutex::new(()));
    for kind in [Kind::Counter, Kind::Rank, Kind::Events] {
        run_once(
            kind,
            pool,
            &executor(&mock),
            "2222",
            &observation,
            &shutdown,
            false,
        )
        .await
        .unwrap();
    }
    mock.shutdown().await;
    let excluded: Vec<String> = sqlx::query_scalar(
        "SELECT member_id FROM member_exclusions WHERE guild_id='2222' ORDER BY member_id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(
        excluded,
        (0..two_bot_core::RAID_ANOMALIES.len())
            .map(|index| (9000 + index).to_string())
            .collect::<Vec<_>>(),
        "baseline must include every seeded raid exclusion"
    );
    assert!(
        !excluded.is_empty(),
        "exclusion preservation must not be vacuous"
    );
    let before = mirror(pool).await;

    for (kind, script, expected_requests) in [
        (
            Kind::Counter,
            vec![ScriptedResponse::json(200, json!([]))],
            1,
        ),
        (
            Kind::Rank,
            vec![
                ScriptedResponse::json(200, members),
                ScriptedResponse::json(200, roles),
            ],
            2,
        ),
        (
            Kind::Events,
            vec![ScriptedResponse::json(200, json!([]))],
            1,
        ),
        (Kind::Events, vec![ScriptedResponse::status(403)], 1),
    ] {
        let (mock, gate) = MockRest::start_gated(script, ScriptedResponse::status(403)).await;
        let (stop, shutdown) = watch::channel(false);
        let mut attempt = {
            let pool = pool.clone();
            let rest = executor(&mock);
            let observation = observation.clone();
            tokio::spawn(async move {
                run_once(kind, &pool, &rest, "2222", &observation, &shutdown, false).await
            })
        };
        tokio::time::timeout(Duration::from_secs(5), gate.wait_for_request())
            .await
            .expect("the final fetch must reach the response gate");
        assert_eq!(mock.requests().len(), expected_requests);
        assert!(!attempt.is_finished(), "the response is still withheld");
        stop.send_replace(true);
        stop.send_replace(true);
        // Shutdown must complete while REST is gated, independently of scheduler speed.
        let cancelled = tokio::time::timeout(Duration::from_secs(5), &mut attempt).await;
        gate.release();
        tokio::time::timeout(Duration::from_secs(5), gate.wait_for_completion())
            .await
            .expect("the released mock handler must finish");
        cancelled
            .expect("shutdown must cancel the fetch before its response is released")
            .unwrap()
            .unwrap();
        assert_eq!(mock.requests().len(), expected_requests);
        assert_eq!(
            mirror(pool).await,
            before,
            "stopped fetch changed the atomic mirrors"
        );
        let next = run_once(
            kind,
            pool,
            &executor(&mock),
            "2222",
            &observation,
            &stop.subscribe(),
            false,
        )
        .await;
        assert_eq!(next, Ok(()));
        assert_eq!(
            mock.requests().len(),
            expected_requests,
            "post-stop callbacks do no I/O"
        );
        mock.shutdown().await;
    }
    db.close().await.unwrap();
}
