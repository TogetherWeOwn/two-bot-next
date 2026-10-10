use super::*;
use serde_json::json;
use two_bot_core::{activation::LiveCapability, apply_web_contract, RankKey};
use two_bot_testsupport::TestDatabase;

use crate::{
    activation::fixtures,
    discord_test_common::{MockRest, RestRequest, ScriptedResponse},
};

use crate::tracing_capture;

#[test]
fn admission_lazy_pool_rejects_query_secrets_before_sqlx_logging() {
    let capture = tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let error = admission_pool("postgres://fixture:fixture-password@127.0.0.1:1/fixture?api_key=fixture-admission-query-secret").unwrap_err();
            assert_eq!(error, "invalid admission authority");
            // Remote plaintext is refused under either policy (weak mode under
            // `Required`, remote host under `LocalOnly`), so the env wrapper
            // reads as the same redacted code regardless of process policy.
            let error = admission_pool("postgres://fixture-user:fixture-db-password@ep-fixture-host.us-east-2.aws.neon.tech/fixture-db?sslmode=disable").unwrap_err();
            assert_eq!(error, "invalid admission authority");
            assert!(!error.contains("fixture"));
            tracing::warn!("website admission capture remains active");
        });
    });
    let text = capture.text();
    assert!(text.contains("website admission capture remains active"));
    assert!(!text.contains("fixture-admission-query-secret"));
    assert!(!text.contains("fixture-db-password"));
    assert!(!text.contains("ep-fixture-host"));
    assert!(!text.contains("ignoring unrecognized connect parameter"));
}

/// Threat-model F6: the shared admission fence refuses plaintext, unverified
/// and wrong-host URLs with fixed strings before SQLx parses or connects, and
/// the `LocalOnly` happy path builds a lazy pool with no socket. Mirrors
/// `crates/store/tests/tls_refusal.rs`.
#[test]
fn admission_pool_tls_fence_refuses_plaintext_and_wrong_hosts() {
    use two_bot_core::database_tls::TlsPolicy;
    let cases = [
        (
            "postgres://fixture-user:fixture-db-password@ep-fixture-host.us-east-2.aws.neon.tech/fixture-db?sslmode=disable",
            TlsPolicy::Required,
            "database sslmode does not require TLS",
        ),
        (
            "postgres://fixture-user:fixture-db-password@ep-fixture-host.us-east-2.aws.neon.tech/fixture-db?sslmode=prefer",
            TlsPolicy::Required,
            "database sslmode does not require TLS",
        ),
        (
            "postgres://fixture-user:fixture-db-password@ep-fixture-host.us-east-2.aws.neon.tech/fixture-db",
            TlsPolicy::Required,
            "database URL must set sslmode under the required TLS policy",
        ),
        (
            "postgres://fixture-user:fixture-db-password@fixture-host/fixture-db?sslmode=verify-full",
            TlsPolicy::Required,
            "local database host is refused under the required TLS policy",
        ),
        (
            "postgres://fixture-user:fixture-db-password@ep-fixture-host.us-east-2.aws.neon.tech/fixture-db?sslmode=verify-full",
            TlsPolicy::LocalOnly,
            "remote database host is refused under the local-only TLS policy",
        ),
        (
            "postgres://fixture-user:fixture-db-password@fixture-host/fixture-db?sslmode=fixture-mode",
            TlsPolicy::LocalOnly,
            "unsupported database sslmode",
        ),
    ];
    let capture = tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            for (url, policy, expected) in cases {
                let error = admission_pool_with_tls(url, policy).unwrap_err();
                assert_eq!(error, expected, "{url}");
                assert!(!error.contains("fixture"), "TLS refusal echoed the URL");
            }
            // Happy paths: no socket opens (lazy), so no database is needed.
            for (url, policy) in [
                (
                    "postgres://fixture:fixture-password@127.0.0.1:1/fixture?sslmode=disable",
                    TlsPolicy::LocalOnly,
                ),
                (
                    "postgres://fixture:fixture-password@127.0.0.1:1/fixture",
                    TlsPolicy::LocalOnly,
                ),
                (
                    "postgres://fixture-user:fixture-db-password@ep-fixture-host.us-east-2.aws.neon.tech/fixture-db?sslmode=verify-full",
                    TlsPolicy::Required,
                ),
            ] {
                let pool = admission_pool_with_tls(url, policy).unwrap();
                assert_eq!(pool.size(), 0);
                pool.close().await;
            }
            tracing::warn!("website admission capture remains active");
        });
    });
    let text = capture.text();
    assert!(text.contains("website admission capture remains active"));
    assert!(!text.contains("fixture"), "TLS refusal reached logs");
    assert!(!text.contains("ignoring unrecognized connect parameter"));
}

async fn tick(
    kind: Kind,
    pool: &PgPool,
    rest: &ActionExecutor,
    guild: &str,
    observation: &Mutex<()>,
) -> Result<(), ErrorClass> {
    // Tests exercise the permitted path by default; the refused-identity path
    // goes through `tick_gated` with the real fence decision.
    tick_gated(kind, pool, rest, guild, observation, true).await
}

async fn tick_gated(
    kind: Kind,
    pool: &PgPool,
    rest: &ActionExecutor,
    guild: &str,
    observation: &Mutex<()>,
    rank_heal: bool,
) -> Result<(), ErrorClass> {
    let (_stop, shutdown) = watch::channel(false);
    run_once(kind, pool, rest, guild, observation, &shutdown, rank_heal).await
}

/// Twilight percent-encodes `X-Audit-Log-Reason` on the wire (`-` arrives as
/// `%2D`, spaces as `%20`), so assertions decode the captured header first.
fn decode_reason(header: &str) -> String {
    let mut bytes = Vec::new();
    let input = header.as_bytes();
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' {
            bytes.push(u8::from_str_radix(&header[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            bytes.push(input[i]);
            i += 1;
        }
    }
    String::from_utf8(bytes).unwrap()
}

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
async fn recovery_registration_does_not_unpark_unavailable_jobs() {
    let empty = registered_statuses(&[], &[]).await;
    assert!(empty.read().await.values().all(|entry| entry.parked));
    let recovery = Job {
        name: RECOVERY_JOB_NAME,
        cadence: Duration::from_secs(30),
        startup_jitter: Duration::ZERO,
        timeout: Duration::from_secs(25),
        action: Arc::new(|| Box::pin(async { Ok(()) })),
    };
    let status = registered_statuses(&[recovery], &[]).await;
    let entries = status.read().await;
    assert!(!entries[RECOVERY_JOB_NAME].parked);
    for name in NAMES.into_iter().chain(community_jobs::NAMES) {
        assert!(entries[name].parked);
        assert!(!entries[name].running);
    }
}

/// TOG-15758: a job the identity fence refuses is absent from the supervised
/// set, so `/readyz` lists it parked and never running, while the staging pair
/// with the same environment unparks both posting jobs.
#[tokio::test]
async fn refused_identity_parks_the_posting_jobs_in_the_readyz_status_map() {
    let gates = two_bot_core::FeatureGates::from_map(&std::collections::HashMap::from([
        ("TWO_AUTOMATIONS".to_owned(), "1".to_owned()),
        ("TWO_ANNOUNCEMENTS".to_owned(), "1".to_owned()),
    ]))
    .unwrap();
    let action: jobs::JobAction = Arc::new(|| Box::pin(async { Ok(()) }));
    let register = |activation: &BootActivation| {
        let mut registered = Vec::new();
        registered.extend(scheduled_jobs::register_fenced(
            gates,
            activation,
            action.clone(),
        ));
        registered.extend(feed_jobs::register_fenced(
            gates,
            activation,
            action.clone(),
            String::new(),
        ));
        registered
    };
    let posting = [scheduled_jobs::NAMES[0], feed_jobs::NAME];

    let registered = register(&fixtures::staging());
    let status = registered_statuses(&registered, &[]).await;
    let entries = status.read().await;
    for name in posting {
        assert!(
            !entries[name].parked,
            "{name} registers on the staging pair"
        );
    }
    drop(entries);

    let registered = register(&fixtures::live());
    assert!(registered.is_empty(), "live identity builds no posting job");
    let status = registered_statuses(&registered, &[]).await;
    let entries = status.read().await;
    for name in posting {
        assert!(entries[name].parked, "{name} is parked on the live pair");
        assert!(!entries[name].running);
    }
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
    let rest = governed_executor(
        "synthetic-job-test-token",
        Some(mock.origin()),
        pool.clone(),
    )
    .unwrap();
    tick(Kind::Rank, &pool, &rest, guild, &observation)
        .await
        .unwrap();
    tick(Kind::Counter, &pool, &rest, guild, &observation)
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
    let rest = governed_executor(
        "synthetic-job-test-token",
        Some(mock.origin()),
        pool.clone(),
    )
    .unwrap();
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
                    Box::pin(async move { tick(kind, &pool, &rest, "2222", &observation).await })
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
            tick(Kind::Events, &pool, &rest, guild, &observation).await,
            Err(ErrorClass::Rest)
        );
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM scheduled_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1, "failed/malformed reads preserve mirror");
    }
    tick(Kind::Events, &pool, &rest, guild, &observation)
        .await
        .unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM scheduled_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "valid empty response clears mirror");
    assert_eq!(mock.requests().len(), 8);
    // The production constructor joins the same lane even for a mock proxy.
    let guarded = governed_executor(
        "synthetic-job-test-token",
        Some(mock.origin()),
        pool.clone(),
    )
    .unwrap();
    let admission = two_bot_core::send_admission::PgSendAdmission::new(
        pool.clone(),
        "synthetic-job-test-token",
    )
    .unwrap();
    admission
        .extend(two_bot_core::send_admission::SendCooldown::Indefinite)
        .await
        .unwrap();
    let (_stop, shutdown) = watch::channel(false);
    assert_eq!(
        run_once(
            Kind::Events,
            &pool,
            &guarded,
            guild,
            &observation,
            &shutdown,
            false
        )
        .await,
        Err(ErrorClass::Rest)
    );
    assert_eq!(mock.requests().len(), 8, "held job must not reach HTTP");
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

/// Hand-built single-grant plan: grant Member ("12") to fixture member 1001.
fn heal_plan_single() -> Vec<RankHeal> {
    vec![RankHeal {
        member_id: "1001".to_owned(),
        key: RankKey::Member,
        role_id: "12".to_owned(),
    }]
}

/// Guild role array with ladder roles 11-15 at positions 1-5; the bot holds
/// `bot_role`. The heal reads hierarchy from this already-fetched payload.
fn heal_roles(bot_role: &str, bot_position: i64) -> Value {
    let mut roles = vec![
        json!({"id": "11", "name": "Prospect", "position": 1}),
        json!({"id": "12", "name": "Member", "position": 2}),
        json!({"id": "13", "name": "Soldier", "position": 3}),
        json!({"id": "14", "name": "Veteran", "position": 4}),
        json!({"id": "15", "name": "Legend", "position": 5}),
    ];
    if !["11", "12", "13", "14", "15"].contains(&bot_role) {
        roles.push(json!({"id": bot_role, "name": "Bot", "position": bot_position}));
    }
    Value::Array(roles)
}

/// Route-aware heal fixture: bot identity `999`, bot member roles
/// `bot_roles`, every role PUT answered with `put_status`.
async fn start_heal_mock(put_status: u16, bot_roles: Vec<String>) -> MockRest {
    MockRest::with_responder(move |request: &RestRequest| {
        if request.path.ends_with("/users/@me") {
            ScriptedResponse::json(200, json!({"id": "999", "bot": true}))
        } else if request.method == "GET" && request.path.contains("/members/999") {
            ScriptedResponse::json(200, json!({"roles": bot_roles}))
        } else if request.method == "PUT" {
            ScriptedResponse::status(put_status)
        } else {
            ScriptedResponse::status(500)
        }
    })
    .await
}

fn open_shutdown() -> (watch::Sender<bool>, watch::Receiver<bool>) {
    watch::channel(false)
}

#[tokio::test]
async fn rank_heal_grants_missing_rungs_with_audit_reason() {
    let mock = start_heal_mock(204, vec!["10".to_owned()]).await;
    let (_stop, shutdown) = open_shutdown();
    let healed = heal_rank_ladder(
        &executor(&mock),
        "2222",
        &heal_roles("10", 99),
        &heal_plan_single(),
        &shutdown,
    )
    .await;
    assert_eq!(healed, Ok(true));
    let requests = mock.requests();
    assert_eq!(requests.len(), 3, "{requests:?}");
    let put = requests
        .iter()
        .find(|request| request.method == "PUT")
        .expect("role grant");
    assert_eq!(put.path, "/api/v10/guilds/2222/members/1001/roles/12");
    let reason = put
        .header("x-audit-log-reason")
        .expect("audit reason header");
    assert!(decode_reason(reason).contains("self-heal"), "{reason}");
    mock.shutdown().await;
}

#[tokio::test]
async fn rank_heal_refuses_targets_at_or_above_the_bot() {
    // The bot tops out at Soldier (position 3): a Veteran grant is refused,
    // and so is a role with no position in the guild payload, both before
    // any mutation.
    for (plan, roles) in [
        (
            vec![RankHeal {
                member_id: "1001".to_owned(),
                key: RankKey::Veteran,
                role_id: "14".to_owned(),
            }],
            heal_roles("13", 3),
        ),
        (
            heal_plan_single(),
            json!([
                {"id": "10", "name": "Bot", "position": 99},
                {"id": "11", "name": "Prospect", "position": 1},
                {"id": "12", "name": "Member"},
            ]),
        ),
    ] {
        let mock = start_heal_mock(204, vec!["13".to_owned()]).await;
        let (_stop, shutdown) = open_shutdown();
        assert_eq!(
            heal_rank_ladder(&executor(&mock), "2222", &roles, &plan, &shutdown).await,
            Err(ErrorClass::Configuration)
        );
        assert!(
            mock.requests()
                .iter()
                .all(|request| request.method != "PUT"),
            "refused heal must not mutate"
        );
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn rank_heal_fails_closed_on_empty_or_over_bound_plans() {
    let mock = start_heal_mock(204, vec!["10".to_owned()]).await;
    let (_stop, shutdown) = open_shutdown();
    assert_eq!(
        heal_rank_ladder(
            &executor(&mock),
            "2222",
            &heal_roles("10", 99),
            &[],
            &shutdown
        )
        .await,
        Err(ErrorClass::Configuration)
    );
    assert!(mock.requests().is_empty());
    mock.shutdown().await;

    let over_bound: Vec<RankHeal> = (0..MAX_RANK_SELF_HEAL_GRANTS_PER_TICK + 1)
        .map(|index| RankHeal {
            member_id: format!("2{index:03}"),
            key: RankKey::Member,
            role_id: "12".to_owned(),
        })
        .collect();
    assert!(over_bound.len() > MAX_RANK_SELF_HEAL_GRANTS_PER_TICK);
    let mock = start_heal_mock(204, vec!["10".to_owned()]).await;
    let (_stop, shutdown) = open_shutdown();
    assert_eq!(
        heal_rank_ladder(
            &executor(&mock),
            "2222",
            &heal_roles("10", 99),
            &over_bound,
            &shutdown
        )
        .await,
        Err(ErrorClass::Configuration)
    );
    assert!(
        mock.requests().is_empty(),
        "over-bound heal must not mutate"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn rank_heal_maps_grant_failures_to_precise_classes() {
    // A confirmed refusal (403) is a configuration failure; a wire failure
    // (500) stays retryable.
    for (put_status, expected) in [(403, ErrorClass::Configuration), (500, ErrorClass::Rest)] {
        let mock = start_heal_mock(put_status, vec!["10".to_owned()]).await;
        let (_stop, shutdown) = open_shutdown();
        assert_eq!(
            heal_rank_ladder(
                &executor(&mock),
                "2222",
                &heal_roles("10", 99),
                &heal_plan_single(),
                &shutdown
            )
            .await,
            Err(expected),
            "PUT {put_status}"
        );
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn rank_heal_stops_before_wire_on_shutdown() {
    let mock = start_heal_mock(204, vec!["10".to_owned()]).await;
    let (_stop, shutdown) = watch::channel(true);
    assert_eq!(
        heal_rank_ladder(
            &executor(&mock),
            "2222",
            &heal_roles("10", 99),
            &heal_plan_single(),
            &shutdown
        )
        .await,
        Ok(false)
    );
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}

/// Rank-tick self-heal end to end (needs the guarded test database, like
/// `three_website_ticks_publish_rows_and_fail_closed`): member 1001 holds
/// Member without Prospect, so the tick grants Prospect with an audit reason
/// and publishes instead of failing as `Configuration`. A genuinely invalid
/// ladder (Legend missing) still fails closed with no mutation.
#[test]
fn rank_tick_self_heals_non_cumulative_ladder_and_publishes() {
    let capture = tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
                assert!(
                    std::env::var("GITHUB_ACTIONS").is_err(),
                    "CI must supply the guarded test database"
                );
                eprintln!("SKIP rank self-heal integration: TWO_TEST_DATABASE_URL is not set");
                return;
            };
            let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
                .await
                .expect("create migrated agent-testdb fixture");
            let pool = fixture.pool().clone();
            apply_web_contract(&pool).await.unwrap();
            let guild = "2222";
            for (index, anomaly) in two_bot_core::RAID_ANOMALIES.iter().enumerate() {
                let (start, _) =
                    two_bot_core::window_bounds(anomaly.start, anomaly.end).unwrap();
                sqlx::query("INSERT INTO members (guild_id, member_id, joined_at, is_bot) VALUES ($1,$2,$3::timestamptz,FALSE)")
                    .bind(guild).bind((9000 + index).to_string()).bind(start).execute(&pool).await.unwrap();
            }
            let roster = json!([
                member(1000, false, &["11"]),
                member(1001, false, &["12"]),
                member(9000, false, &[]),
                member(9001, false, &[]),
                member(9002, false, &[]),
            ]);
            let roles = json!({"roles": [
                {"id":"10","name":"Bot","position":99},
                {"id":"11","name":"Prospect","position":1},
                {"id":"12","name":"Member","position":2},
                {"id":"13","name":"Soldier","position":3},
                {"id":"14","name":"Veteran","position":4},
                {"id":"15","name":"Legend","position":5},
            ]});
            let mock = MockRest::start(
                vec![
                    ScriptedResponse::json(200, roster.clone()),
                    ScriptedResponse::json(200, roles),
                    ScriptedResponse::json(200, json!({"id": "999", "bot": true})),
                    ScriptedResponse::json(200, json!({"roles": ["10"]})),
                    ScriptedResponse::status(204),
                ],
                ScriptedResponse::status(500),
            )
            .await;
            let observation = Arc::new(Mutex::new(()));
            tick(Kind::Rank, &pool, &executor(&mock), guild, &observation)
                .await
                .expect("non-cumulative ladder heals and publishes");
            let requests = mock.requests();
            assert_eq!(requests.len(), 5, "{requests:?}");
            let put = requests
                .iter()
                .find(|request| request.method == "PUT")
                .expect("role grant");
            // Member 1001 holds Member ("12") without Prospect ("11"), so the
            // heal grants exactly the missing lower rung.
            assert_eq!(put.path, "/api/v10/guilds/2222/members/1001/roles/11");
            assert!(
                put.header("x-audit-log-reason")
                    .is_some_and(|reason| decode_reason(reason).contains("self-heal")),
                "heal grant carries the audit reason"
            );
            let rank: Option<String> =
                sqlx::query_scalar("SELECT rank_key FROM member_ranks WHERE guild_id=$1 AND member_id='1001'")
                    .bind(guild)
                    .fetch_optional(&pool)
                    .await
                    .unwrap();
            assert_eq!(rank.as_deref(), Some("member"));
            let holders: i32 =
                sqlx::query_scalar("SELECT holders_count FROM rank_snapshots WHERE guild_id=$1 AND rank_key='prospect'")
                    .bind(guild)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(holders, 2, "healed Prospect rung counts both holders");
            mock.shutdown().await;

            // A genuinely invalid ladder still fails closed: Legend missing,
            // nothing granted, nothing published.
            let bad_roles = json!({"roles": [
                {"id":"11","name":"Prospect","position":1},
                {"id":"12","name":"Member","position":2},
                {"id":"13","name":"Soldier","position":3},
                {"id":"14","name":"Veteran","position":4},
            ]});
            let mock = MockRest::start(
                vec![
                    ScriptedResponse::json(200, roster),
                    ScriptedResponse::json(200, bad_roles),
                ],
                ScriptedResponse::status(500),
            )
            .await;
            assert_eq!(
                tick(Kind::Rank, &pool, &executor(&mock), guild, &observation).await,
                Err(ErrorClass::Configuration)
            );
            assert_eq!(mock.requests().len(), 2);
            assert!(
                mock.requests().iter().all(|request| request.method != "PUT"),
                "invalid ladder must not mutate"
            );
            mock.shutdown().await;
            fixture
                .close()
                .await
                .expect("drop disposable test database");
            let text = capture.text();
            assert!(
                text.contains("rank ladder self-healed"),
                "repair alert missing: {text}"
            );
            assert!(
                text.contains("1001"),
                "repair alert names the member: {text}"
            );
            assert!(
                text.contains("Prospect"),
                "repair alert names the role: {text}"
            );
        });
    });
}

/// A refused identity never grants: the live pair is not cleared for the
/// rank-heal capability, so a non-cumulative ladder keeps the old
/// `Configuration` refusal with no PUT and nothing published.
#[test]
fn rank_tick_refused_identity_sends_no_put() {
    assert!(
        fixtures::staging().permitted(LiveCapability::RankHeal),
        "staging pair heals rank ladders"
    );
    for (label, activation) in fixtures::refused() {
        assert!(
            !activation.permitted(LiveCapability::RankHeal),
            "{label} must not heal rank ladders"
        );
    }
    let capture = tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
                assert!(
                    std::env::var("GITHUB_ACTIONS").is_err(),
                    "CI must supply the guarded test database"
                );
                eprintln!("SKIP refused rank-heal integration: TWO_TEST_DATABASE_URL is not set");
                return;
            };
            let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
                .await
                .expect("create migrated agent-testdb fixture");
            let pool = fixture.pool().clone();
            apply_web_contract(&pool).await.unwrap();
            let guild = "2222";
            for (index, anomaly) in two_bot_core::RAID_ANOMALIES.iter().enumerate() {
                let (start, _) =
                    two_bot_core::window_bounds(anomaly.start, anomaly.end).unwrap();
                sqlx::query("INSERT INTO members (guild_id, member_id, joined_at, is_bot) VALUES ($1,$2,$3::timestamptz,FALSE)")
                    .bind(guild).bind((9000 + index).to_string()).bind(start).execute(&pool).await.unwrap();
            }
            // Member 1001 holds Member ("12") without Prospect ("11"): the
            // same non-cumulative ladder the permitted path heals.
            let roster = json!([
                member(1001, false, &["12"]),
                member(9000, false, &[]),
                member(9001, false, &[]),
                member(9002, false, &[]),
            ]);
            let roles = json!({"roles": [
                {"id":"10","name":"Bot","position":99},
                {"id":"11","name":"Prospect","position":1},
                {"id":"12","name":"Member","position":2},
                {"id":"13","name":"Soldier","position":3},
                {"id":"14","name":"Veteran","position":4},
                {"id":"15","name":"Legend","position":5},
            ]});
            let mock = MockRest::start(
                vec![
                    ScriptedResponse::json(200, roster),
                    ScriptedResponse::json(200, roles),
                ],
                ScriptedResponse::status(500),
            )
            .await;
            let observation = Arc::new(Mutex::new(()));
            assert_eq!(
                tick_gated(Kind::Rank, &pool, &executor(&mock), guild, &observation, false).await,
                Err(ErrorClass::Configuration)
            );
            let requests = mock.requests();
            assert_eq!(requests.len(), 2, "{requests:?}");
            assert!(
                requests.iter().all(|request| request.method != "PUT"),
                "refused heal must not mutate"
            );
            let published: i64 =
                sqlx::query_scalar("SELECT count(*) FROM rank_snapshots WHERE guild_id=$1")
                    .bind(guild)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(published, 0, "refused heal publishes nothing");
            mock.shutdown().await;
            fixture
                .close()
                .await
                .expect("drop disposable test database");
            assert!(
                capture.text().contains("self-heal refused for this identity"),
                "refusal names the fence"
            );
        });
    });
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
        tokio::spawn(async move { tick(Kind::Rank, &pool, &rest, "2222", &observation).await })
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
        tokio::spawn(async move { tick(Kind::Counter, &pool, &rest, "2222", &observation).await })
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
    tick(Kind::Events, pool, &executor(&mock), "2222", &observation)
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
