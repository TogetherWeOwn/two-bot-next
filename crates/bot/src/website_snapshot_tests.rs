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
    json!({"id":id, "guild_id":"2222", "name":"Test night", "scheduled_start_time":"2026-10-01T19:00:00Z", "status":1})
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
    // Channel strings must be canonical nonzero snowflakes; descriptions
    // keep the empty/value string cases.
    let channel_values = [
        None,
        Some(Value::Null),
        Some(json!("4000")),
        Some(json!("4001")),
    ];
    let description_values = [
        None,
        Some(Value::Null),
        Some(json!("")),
        Some(json!("value")),
    ];
    for channel in &channel_values {
        for description in &description_values {
            let mut row = event("3000");
            if let Some(value) = channel {
                row["channel_id"] = value.clone();
            }
            if let Some(value) = description {
                row["description"] = value.clone();
            }
            let normalized = normalize_events(&[raw_event(&row, "2222").unwrap()]).unwrap();
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

#[test]
fn canonical_snowflake_extremes_are_accepted() {
    // Minimum nonzero (1) and maximum u64 snowflakes stay valid identities.
    let max = u64::MAX.to_string();
    for (id, channel) in [("1", max.as_str()), (max.as_str(), "1")] {
        let mut row = event(id);
        row["channel_id"] = json!(channel);
        let normalized = normalize_events(&[raw_event(&row, "2222").unwrap()]).unwrap();
        assert_eq!(normalized.len(), 1);
        assert_eq!(normalized[0].channel_id.as_deref(), Some(channel));
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
    let seed = normalize_events(&[raw_event(&event("3000"), "2222").unwrap()]).unwrap();
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
        Some(json!("4000")),
        Some(json!("4001")),
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

fn named_event(id: &str, name: &str, start: &str, status: u64) -> Value {
    json!({"id":id, "guild_id":"2222", "name":name, "scheduled_start_time":start, "status":status})
}

fn duplicate_id_snapshots() -> Vec<Value> {
    let first = named_event("3000", "First name", "2026-10-01T19:00:00Z", 1);
    let second = named_event("3000", "Second name", "2026-10-02T19:00:00Z", 2);
    vec![
        // Identical duplicates refuse under the same policy as conflicts.
        json!([event("3000"), event("3000")]),
        // Conflicting duplicates refuse in either order; no last-row winner.
        json!([first.clone(), second.clone()]),
        json!([second, first]),
        // A duplicate anywhere refuses the whole snapshot.
        json!([event("3001"), event("3000"), event("3000")]),
        json!([event("3000"), event("3001"), event("3002"), event("3000")]),
        json!([event("3000"), event("3000"), event("3000")]),
    ]
}

#[tokio::test]
async fn duplicate_event_ids_never_reach_storage() {
    let snapshots = duplicate_id_snapshots();
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

#[tokio::test]
async fn duplicate_event_ids_preserve_full_mirror_and_valid_controls_still_replace() {
    let Some(db) = fixture().await else {
        return;
    };
    let pool = db.pool();
    let (_stop, shutdown) = watch::channel(false);
    let observation = Mutex::new(());
    let seed = normalize_events(&[raw_event(&event("3000"), "2222").unwrap()]).unwrap();
    replace_events(pool, "2222", "2026-09-30T12:00:00.123Z", &seed)
        .await
        .unwrap();
    replace_events(pool, "3333", "2026-09-30T12:00:00.123Z", &seed)
        .await
        .unwrap();
    let before = mirror(pool).await;
    let snapshots = duplicate_id_snapshots();
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
            "duplicate-id snapshot changed rows/timestamps/contract pin: {snapshot}"
        );
    }
    mock.shutdown().await;

    // Controls: a unique snapshot still replaces, and a valid empty snapshot
    // still performs the intentional empty-set replacement for its guild.
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!([event("3001"), event("3002")])),
            ScriptedResponse::json(200, json!([])),
        ],
        ScriptedResponse::status(403),
    )
    .await;
    let rest = executor(&mock);
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
    let replaced: Vec<String> = sqlx::query_scalar(
        "SELECT event_id FROM scheduled_events WHERE guild_id='2222' ORDER BY event_id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(replaced, ["3001", "3002"]);
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

// ── Scheduled-event snapshot bounds (bounded-claim decision, caps item) ──
// Tick-level pins for the caps the pure `normalize_events` suite cannot see:
// the raw-body 1 MiB cap (body bytes never reach `normalize_*`), plus
// tick wiring for the row and name bounds (refuse, never truncate-then-
// accept). Every refusal below must surface as `Err(ErrorClass::Rest)`
// with the poison pool untouched, which proves zero writes: no partial
// application, and the last good snapshot kept by construction. Controls
// assert `Err(ErrorClass::Database)`: with a poison pool that error is the
// observable proof an in-bounds snapshot still attempted publication.
// The channel-id length bound is pinned only in the pure core suite: the
// tick's pre-existing canonical-snowflake check rejects non-snowflake
// channels before normalization, so over-length channels are unreachable
// here by a stricter, older property.

/// Raw response-body cap: 1 MiB.
const SNAPSHOT_BODY_CAP: usize = 1_048_576;

/// Cap the decision pins on rows per snapshot.
const SNAPSHOT_ROW_CAP: usize = 100;

/// Two valid events with an unknown-field pad sized so the served body is
/// exactly `target_len` bytes. Known fields stay in bounds and rows stay far
/// under the row cap, so only the body bound can refuse. Padding hides in an
/// unknown field on purpose: the bound measures RAW body bytes, so a
/// stripped-known-fields measure (which would see ~200 bytes here) must not
/// satisfy it.
fn padded_snapshot_body(target_len: usize) -> Value {
    let mut first = event("3000");
    first["pad"] = json!("");
    let mut body = json!([first, event("3001")]);
    let base_len = body.to_string().len();
    assert!(
        target_len > base_len,
        "target {target_len} must exceed the unpadded body {base_len}"
    );
    body[0]["pad"] = json!("x".repeat(target_len - base_len));
    let wire = body.to_string();
    assert_eq!(
        wire.len(),
        target_len,
        "served body must be exactly the target length"
    );
    body
}

/// 101 valid small events: rows over cap, every field in bounds.
fn over_cap_row_snapshot() -> Value {
    Value::Array(
        (0..SNAPSHOT_ROW_CAP + 1)
            .map(|i| event(&format!("3{:03}", 100 + i)))
            .collect(),
    )
}

/// 100 events with at-cap names and descriptions: fully in bounds.
fn at_cap_snapshot() -> Value {
    Value::Array(
        (0..SNAPSHOT_ROW_CAP)
            .map(|i| {
                let mut row = event(&format!("3{:03}", 100 + i));
                row["name"] = json!("n".repeat(100));
                row["description"] = json!("d".repeat(1000));
                row
            })
            .collect(),
    )
}

/// Run one Events tick against a single scripted body and a poison pool.
/// Returns the tick outcome plus the mock-side request count.
async fn run_events_tick(body: Value) -> (Result<(), ErrorClass>, usize) {
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, body)],
        ScriptedResponse::status(403),
    )
    .await;
    let pool = poison_pool();
    let (_stop, shutdown) = watch::channel(false);
    let outcome = run_once(
        Kind::Events,
        &pool,
        &executor(&mock),
        "2222",
        &Mutex::new(()),
        &shutdown,
        false,
    )
    .await;
    let requests = mock.requests().len();
    mock.shutdown().await;
    pool.close().await;
    (outcome, requests)
}

#[tokio::test]
async fn over_cap_body_is_read_failure_and_never_reaches_storage() {
    let body = padded_snapshot_body(SNAPSHOT_BODY_CAP + 1);
    let (outcome, requests) = run_events_tick(body).await;
    assert_eq!(requests, 1, "the tick must fetch before it can refuse");
    assert_eq!(
        outcome,
        Err(ErrorClass::Rest),
        "a 1 MiB + 1 byte body must refuse before storage; reaching the \
         poison pool would surface a database error instead"
    );
}

#[tokio::test]
async fn exact_cap_body_still_reaches_storage() {
    let (outcome, requests) = run_events_tick(padded_snapshot_body(SNAPSHOT_BODY_CAP)).await;
    assert_eq!(requests, 1);
    assert_eq!(
        outcome,
        Err(ErrorClass::Database),
        "an exactly-1 MiB body is in bounds and must still attempt publication"
    );
}

#[tokio::test]
async fn just_under_cap_body_still_reaches_storage() {
    let (outcome, requests) = run_events_tick(padded_snapshot_body(SNAPSHOT_BODY_CAP - 1)).await;
    assert_eq!(requests, 1);
    assert_eq!(
        outcome,
        Err(ErrorClass::Database),
        "a 1 MiB - 1 byte body is in bounds and must still attempt publication"
    );
}

#[tokio::test]
async fn over_100_rows_never_reaches_storage() {
    let body = over_cap_row_snapshot();
    assert!(
        body.to_string().len() < SNAPSHOT_BODY_CAP,
        "row fixture must stay under the body cap to isolate the row bound"
    );
    let (outcome, requests) = run_events_tick(body).await;
    assert_eq!(requests, 1);
    assert_eq!(
        outcome,
        Err(ErrorClass::Rest),
        "101 rows must refuse the whole snapshot; applying any prefix would \
         reach the poison pool and surface a database error instead"
    );
}

#[tokio::test]
async fn over_length_name_is_refused_not_truncated() {
    let mut row = event("3000");
    row["name"] = json!("n".repeat(101));
    let (outcome, requests) = run_events_tick(json!([row])).await;
    assert_eq!(requests, 1);
    assert_eq!(
        outcome,
        Err(ErrorClass::Rest),
        "a 101-char name must refuse; truncating to 100 and accepting would \
         reach the poison pool and surface a database error instead"
    );
}

#[tokio::test]
async fn at_cap_rows_and_fields_reach_storage() {
    let body = at_cap_snapshot();
    assert_eq!(body.as_array().expect("array snapshot").len(), 100);
    assert!(
        body.to_string().len() < SNAPSHOT_BODY_CAP,
        "at-cap fixture must stay under the body cap to isolate the row/field bounds"
    );
    let (outcome, requests) = run_events_tick(body).await;
    assert_eq!(requests, 1);
    assert_eq!(
        outcome,
        Err(ErrorClass::Database),
        "100 rows with at-cap fields are in bounds and must still attempt publication"
    );
}

#[tokio::test]
async fn stale_tick_returns_ok_and_writes_nothing() {
    let Some(db) = fixture().await else {
        return;
    };
    let pool = db.pool();
    // Seed a newer snapshot with a far-future stamp: the live tick stamps
    // `now`, strictly older, so the epoch gate must roll the swap back.
    let seed = normalize_events(&[raw_event(&event("3000"), "2222").unwrap()]).unwrap();
    replace_events(pool, "2222", "2036-01-01T00:00:00.000Z", &seed)
        .await
        .unwrap();
    let before = mirror(pool).await;
    // The stale fetch carries a never-before-seen id: only the epoch gate
    // stops the resurrection, and the tick still reports success.
    let mock = MockRest::start(
        vec![ScriptedResponse::json(
            200,
            json!([event("3000"), event("3009")]),
        )],
        ScriptedResponse::status(403),
    )
    .await;
    let (_stop, shutdown) = watch::channel(false);
    assert_eq!(
        run_once(
            Kind::Events,
            pool,
            &executor(&mock),
            "2222",
            &Mutex::new(()),
            &shutdown,
            false
        )
        .await,
        Ok(())
    );
    assert_eq!(mock.requests().len(), 1);
    assert_eq!(
        mirror(pool).await,
        before,
        "stale tick changed rows/timestamps/contract pin"
    );
    mock.shutdown().await;
    db.close().await.unwrap();
}
