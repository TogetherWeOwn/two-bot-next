use super::*;
use serde_json::json;
use two_bot_testsupport::TestDatabase;

#[allow(dead_code)]
#[path = "../../discord/tests/common/mod.rs"]
mod common;

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

#[tokio::test]
async fn bot_floor_scan_caps_requests_and_never_returns_a_partial_count() {
    use two_bot_core::BotFloorScan;

    for total in [0_u64, 23, 999, 1000, 10_000, 10_999, 11_000, 11_001, 12_000] {
        let mut responses = Vec::new();
        for start in (0..=total).step_by(1000) {
            let page: Vec<_> = (start + 1..=(start + 1000).min(total))
                .map(|id| member(id, id % 10 == 0, &[]))
                .collect();
            responses.push(ScriptedResponse::json(200, json!(page)));
        }
        let mock = MockRest::start(responses, ScriptedResponse::status(500)).await;
        let outcome = bot_floor_scan(&executor(&mock), "2222").await.unwrap();
        let expected = if total >= 11_000 {
            BotFloorScan::Truncated
        } else {
            BotFloorScan::Complete((total / 10) as i64)
        };
        assert_eq!(outcome, expected, "guild size {total}");
        let requests = mock.requests();
        assert_eq!(requests.len(), ((total / 1000 + 1) as usize).min(11));
        for (index, request) in requests.iter().enumerate() {
            let (_, query) = request.path.split_once('?').unwrap();
            let params: std::collections::HashMap<_, _> = query
                .split('&')
                .map(|pair| pair.split_once('=').unwrap())
                .collect();
            assert_eq!(params["limit"], "1000");
            assert_eq!(params["after"], (index * 1000).to_string());
        }
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn bot_floor_scan_errors_do_not_retry_or_report_a_count() {
    for response in [
        ScriptedResponse::status(403),
        ScriptedResponse::status(429),
        ScriptedResponse::status(500),
        ScriptedResponse::json(200, json!({"not":"array"})),
        ScriptedResponse::json(200, json!([member(2, false, &[]), member(2, true, &[])])),
    ] {
        let mock = MockRest::start(vec![response], ScriptedResponse::status(500)).await;
        assert_eq!(
            bot_floor_scan(&executor(&mock), "2222").await,
            Err(ErrorClass::Rest)
        );
        assert_eq!(mock.requests().len(), 1, "wire request is not retried");
        mock.shutdown().await;
    }
}

/// Real migrations + actual REST executor/mock + all three scheduled actions.
/// Uses the shared strict fixture and a unique disposable database:
/// no bootstrap reset and no interference with the core acceptance tests.
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
    let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated agent-testdb fixture");
    let pool = fixture.pool().clone();
    apply_web_contract(&pool).await.unwrap();
    let guild = "2222";
    let observation = Arc::new(Mutex::new(()));

    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let rest = executor(&mock);
    run_once(Kind::Rank, &pool, &rest, guild, &observation)
        .await
        .unwrap();
    run_once(Kind::Counter, &pool, &rest, guild, &observation)
        .await
        .unwrap();
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
            ScriptedResponse::json(200, roles.clone()),
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
        let observation = observation.clone();
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
                    let observation = observation.clone();
                    Box::pin(
                        async move { run_once(kind, &pool, &rest, "2222", &observation).await },
                    )
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
            run_once(Kind::Events, &pool, &rest, guild, &observation).await,
            Err(ErrorClass::Rest)
        );
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM scheduled_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1, "failed/malformed reads preserve mirror");
    }
    run_once(Kind::Events, &pool, &rest, guild, &observation)
        .await
        .unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM scheduled_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "valid empty response clears mirror");
    assert_eq!(mock.requests().len(), 8);
    mock.shutdown().await;
    for query in [
        "SELECT human_member_count_at FROM guild_counters WHERE guild_id=$1",
        "SELECT human_member_count_at FROM counter_snapshots WHERE guild_id=$1",
        "SELECT snapshot_at FROM rank_snapshots WHERE guild_id=$1",
        "SELECT updated_at FROM member_ranks WHERE guild_id=$1",
    ] {
        let timestamps: Vec<String> = sqlx::query_scalar(query)
            .bind(guild)
            .fetch_all(&pool)
            .await
            .unwrap();
        assert!(!timestamps.is_empty());
        for timestamp in timestamps {
            assert_iso_millis(&timestamp);
        }
    }
    concurrent_publications_keep_newest_counter(&pool, roles).await;
    fixture
        .close()
        .await
        .expect("drop disposable test database");
}

fn assert_iso_millis(timestamp: &str) {
    assert_eq!(timestamp.len(), 24, "{timestamp}");
    assert_eq!(timestamp.as_bytes()[19], b'.');
    assert!(timestamp.as_bytes()[20..23].iter().all(u8::is_ascii_digit));
    assert!(timestamp.ends_with('Z'));
    assert!(two_bot_core::parse_iso_millis(timestamp).is_some());
}

async fn concurrent_publications_keep_newest_counter(pool: &PgPool, roles: Value) {
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!([member(1000, false, &["11"])])),
            ScriptedResponse::json(200, roles).delayed(Duration::from_secs(2)),
            ScriptedResponse::json(200, json!([event()])),
            ScriptedResponse::json(
                200,
                json!([
                    member(1000, false, &["11"]),
                    member(1001, false, &[]),
                    member(1003, false, &[]),
                ]),
            ),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let observation = Arc::new(Mutex::new(()));
    let rank = {
        let pool = pool.clone();
        let rest = executor(&mock);
        let observation = observation.clone();
        tokio::spawn(async move { run_once(Kind::Rank, &pool, &rest, "2222", &observation).await })
    };
    // Rank has observed its old roster and is stalled on the role response.
    tokio::time::timeout(Duration::from_secs(5), async {
        while mock.requests().len() < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let mut counter = {
        let pool = pool.clone();
        let rest = executor(&mock);
        let observation = observation.clone();
        tokio::spawn(
            async move { run_once(Kind::Counter, &pool, &rest, "2222", &observation).await },
        )
    };
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut counter)
            .await
            .is_err()
    );
    assert_eq!(
        mock.requests().len(),
        2,
        "counter must wait before observing"
    );
    // Independent events still publish while the shared denominator lane is busy.
    run_once(Kind::Events, pool, &executor(&mock), "2222", &observation)
        .await
        .unwrap();
    assert!(!rank.is_finished());
    let updated_at: String =
        sqlx::query_scalar("SELECT updated_at FROM scheduled_events WHERE guild_id='2222'")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_iso_millis(&updated_at);
    rank.await.unwrap().unwrap();
    counter.await.unwrap().unwrap();
    for query in [
        "SELECT human_member_count FROM guild_counters WHERE guild_id='2222'",
        "SELECT human_member_count FROM counter_snapshots WHERE guild_id='2222'",
    ] {
        let count: i32 = sqlx::query_scalar(query).fetch_one(pool).await.unwrap();
        assert_eq!(
            count, 3,
            "the newest roster must win in both counter tables"
        );
    }
    assert_eq!(mock.requests().len(), 4);
    mock.shutdown().await;
}
