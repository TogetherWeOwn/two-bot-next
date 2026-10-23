use super::*;

#[path = "live_settings_context_fixture.rs"]
mod live_settings_context_fixture;

use serde_json::json;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use two_bot_core::{activation::LiveCapability, apply_web_contract, RankKey};
use two_bot_testsupport::{guard_database_url, TestDatabase};

use crate::{
    activation::fixtures,
    discord_test_common::{MockRest, RestRequest, ScriptedResponse},
};

use crate::tracing_capture;

#[path = "website_event_age_acceptance_tests.rs"]
mod website_event_age_acceptance_tests;

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
fn decode_reason(header: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let input = header.as_bytes();
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' {
            let hex = header.get(i + 1..i + 3)?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            bytes.push(input[i]);
            i += 1;
        }
    }
    String::from_utf8(bytes).ok()
}

fn executor(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy("synthetic-job-test-token".to_owned(), Some(mock.origin())).unwrap()
}

fn member(id: u64, bot: bool, roles: &[&str]) -> Value {
    json!({"user": {"id": id.to_string(), "bot": bot}, "roles": roles})
}

fn event() -> Value {
    json!({"id":"3000", "guild_id":"2222", "name":"Test night", "scheduled_start_time":"2026-10-01T19:00:00Z", "status":1, "channel_id":null, "description":"fixture"})
}

#[tokio::test]
async fn recovery_registration_does_not_unpark_unavailable_jobs() {
    let empty = registered_statuses(&[], &[]).await.unwrap();
    assert!(empty.read().await.values().all(|entry| entry.parked));
    let recovery = Job {
        name: RECOVERY_JOB_NAME,
        cadence: Duration::from_secs(30),
        cadence_source: None,
        startup_jitter: Duration::ZERO,
        timeout: Duration::from_secs(25),
        action: Arc::new(|| Box::pin(async { Ok(()) })),
    };
    let status = registered_statuses(&[recovery], &[]).await.unwrap();
    let entries = status.read().await;
    assert!(!entries[RECOVERY_JOB_NAME].parked);
    for name in NAMES.into_iter().chain(community_jobs::NAMES) {
        assert!(entries[name].parked);
        assert!(!entries[name].running);
    }
}

#[tokio::test]
async fn registered_statuses_fails_fast_on_an_unknown_job_name() {
    let ghost = Job {
        name: "ghost",
        cadence: Duration::from_secs(30),
        cadence_source: None,
        startup_jitter: Duration::ZERO,
        timeout: Duration::from_secs(25),
        action: Arc::new(|| Box::pin(async { Ok(()) })),
    };
    let error = registered_statuses(&[ghost], &[])
        .await
        .expect_err("an unknown job name fails at registration, never at unpark");
    assert_eq!(error.to_string(), "unregistered job: ghost");
}

#[tokio::test]
async fn prepare_job_statuses_cancels_boot_siblings_before_returning_an_error() {
    let ghost = Job {
        name: "ghost",
        cadence: Duration::from_secs(30),
        cadence_source: None,
        startup_jitter: Duration::ZERO,
        timeout: Duration::from_secs(25),
        action: Arc::new(|| Box::pin(async { Ok(()) })),
    };
    let (shutdown, stopping) = watch::channel(false);
    let error = prepare_job_statuses(&[ghost], &[], &shutdown)
        .await
        .expect_err("boot must reject an unknown job before serving HTTP");
    assert_eq!(error.kind(), std::io::ErrorKind::Other);
    assert_eq!(error.to_string(), "unregistered job: ghost");
    assert!(
        error
            .get_ref()
            .unwrap()
            .downcast_ref::<jobs::UnregisteredJob>()
            .is_some(),
        "keep the typed registration error inside the I/O error"
    );
    assert!(*stopping.borrow(), "cancel boot siblings before returning");
    assert!(
        *shutdown.subscribe().borrow(),
        "late boot subscribers must also observe cancellation"
    );
}

#[tokio::test]
async fn prepare_job_statuses_preserves_valid_registration_without_shutdown() {
    let recovery = Job {
        name: RECOVERY_JOB_NAME,
        cadence: Duration::from_secs(30),
        cadence_source: None,
        startup_jitter: Duration::ZERO,
        timeout: Duration::from_secs(25),
        action: Arc::new(|| Box::pin(async { Ok(()) })),
    };
    let (shutdown, stopping) = watch::channel(false);
    let status = prepare_job_statuses(&[recovery], &[crate::retention_jobs::NAME], &shutdown)
        .await
        .unwrap();
    let entries = status.read().await;
    assert!(!entries[RECOVERY_JOB_NAME].parked);
    assert!(entries[crate::retention_jobs::NAME].parked);
    assert!(entries.values().all(|entry| !entry.running));
    assert!(!*stopping.borrow(), "valid registration must not stop boot");
    assert!(!stopping.has_changed().unwrap());
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
    let status = registered_statuses(&registered, &[]).await.unwrap();
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
    let status = registered_statuses(&registered, &[]).await.unwrap();
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
    let RosterScan::Complete(members) = roster(&executor(&mock), "2222", BOT_FLOOR_MAX_PAGES)
        .await
        .unwrap()
    else {
        panic!("a 1001-member roster fits within the page budget");
    };
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
            roster(&executor(&mock), "2222", BOT_FLOOR_MAX_PAGES)
                .await
                .err(),
            Some(ErrorClass::Rest)
        );
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn roster_errors_do_not_retry_member_page_requests() {
    for (case, response) in [
        ("403", ScriptedResponse::status(403)),
        ("429", ScriptedResponse::status(429)),
        ("500", ScriptedResponse::status(500)),
        (
            "malformed 200",
            ScriptedResponse::json(200, json!({"not":"array"})),
        ),
    ] {
        let mock = MockRest::start(vec![response], ScriptedResponse::status(500)).await;
        assert_eq!(
            roster(&executor(&mock), "2222", BOT_FLOOR_MAX_PAGES)
                .await
                .err(),
            Some(ErrorClass::Rest),
            "{case} member page",
        );
        assert_eq!(
            mock.requests().len(),
            1,
            "{case} member page must not be retried",
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
                cadence_source: None,
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
        task.await.unwrap().unwrap();
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

/// Discord path without the API prefix or query string.
fn discord_route(path: &str) -> &str {
    let path = path.strip_prefix("/api/v10").unwrap_or(path);
    match path.split_once('?') {
        Some((route, _)) => route,
        None => path,
    }
}

/// `PUT /guilds/2222/members/{id}/roles/{role}` as `(member, role)`.
fn member_role_target(route: &str) -> Option<(&str, &str)> {
    let rest = route.strip_prefix("/guilds/2222/members/")?;
    let (id, role) = rest.split_once("/roles/")?;
    if id.is_empty() || role.is_empty() || id.contains('/') || role.contains('/') {
        return None;
    }
    Some((id, role))
}

fn point_member(id: &str, roles: &[String]) -> Value {
    json!({"user": {"id": id, "bot": false}, "roles": roles})
}

/// Route-aware heal fixture. Bot identity is `999`. Member `1001` currently
/// holds Soldier (`13`), not Member, so a Member grant stays eligible after a
/// point read. A 204 PUT sticks that role; any other PUT status leaves member
/// truth unchanged. Unknown routes panic.
async fn start_heal_mock(put_status: u16, bot_roles: Vec<String>) -> MockRest {
    let members = Arc::new(std::sync::Mutex::new(HashMap::from([(
        "1001".to_owned(),
        vec!["13".to_owned()],
    )])));
    MockRest::with_responder(move |request: &RestRequest| {
        let route = discord_route(&request.path);
        if request.method == "GET" && route == "/users/@me" {
            return ScriptedResponse::json(200, json!({"id": "999", "bot": true}));
        }
        if request.method == "GET" && route == "/guilds/2222/members/999" {
            return ScriptedResponse::json(200, json!({"roles": bot_roles.clone()}));
        }
        if request.method == "GET" && route == "/guilds/2222/members/1001" {
            let roles = members
                .lock()
                .expect("member truth")
                .get("1001")
                .cloned()
                .expect("member 1001");
            return ScriptedResponse::json(200, point_member("1001", &roles));
        }
        if request.method == "PUT" {
            if put_status == 204 {
                if let Some((id, role)) = member_role_target(route) {
                    let mut members = members.lock().expect("member truth");
                    let held = members.entry(id.to_owned()).or_default();
                    if !held.iter().any(|have| have == role) {
                        held.push(role.to_owned());
                    }
                    held.sort();
                }
            }
            return ScriptedResponse::status(put_status);
        }
        panic!("unexpected mock Discord route: {} {route}", request.method);
    })
    .await
}

/// Point-read ledger for one rank publication tick. `point_reads` records
/// `(grants already stuck, roles returned)` for every non-bot member GET.
struct RankMemberTruth {
    roles: HashMap<String, Vec<String>>,
    point_reads: Vec<(usize, Vec<String>)>,
    grants_stuck: usize,
}

/// Route-aware publication fixture. The list roster stays the frozen capture.
/// Point reads serve current member truth, and a 204 role PUT sticks that role
/// before any later read. Unknown routes and unknown members panic.
async fn start_rank_publication_mock(
    roster: Value,
    roles_doc: Value,
) -> (MockRest, Arc<std::sync::Mutex<RankMemberTruth>>) {
    let mut roles = HashMap::new();
    for member in roster.as_array().expect("roster array") {
        let id = member["user"]["id"]
            .as_str()
            .expect("roster member id")
            .to_owned();
        let held = member["roles"]
            .as_array()
            .expect("roster roles")
            .iter()
            .map(|role| role.as_str().expect("role id").to_owned())
            .collect();
        roles.insert(id, held);
    }
    let truth = Arc::new(std::sync::Mutex::new(RankMemberTruth {
        roles,
        point_reads: Vec::new(),
        grants_stuck: 0,
    }));
    let shared = Arc::clone(&truth);
    let mock = MockRest::with_responder(move |request: &RestRequest| {
        let route = discord_route(&request.path);
        let mut state = shared.lock().expect("member truth");
        if request.method == "GET" && route == "/guilds/2222" {
            return ScriptedResponse::json(200, roles_doc.clone());
        }
        if request.method == "GET" && route == "/guilds/2222/members" {
            return ScriptedResponse::json(200, roster.clone());
        }
        if request.method == "GET" && route == "/users/@me" {
            return ScriptedResponse::json(200, json!({"id": "999", "bot": true}));
        }
        if request.method == "GET" && route == "/guilds/2222/members/999" {
            return ScriptedResponse::json(200, json!({"roles": ["10"]}));
        }
        if request.method == "GET" {
            if let Some(id) = route.strip_prefix("/guilds/2222/members/") {
                if id.is_empty() || id.contains('/') {
                    panic!("unexpected mock Discord route: GET {route}");
                }
                let held = state
                    .roles
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| panic!("unexpected mock Discord member: {id}"));
                let stuck = state.grants_stuck;
                state.point_reads.push((stuck, held.clone()));
                return ScriptedResponse::json(200, point_member(id, &held));
            }
        }
        if request.method == "PUT" {
            let (id, role) = member_role_target(route)
                .unwrap_or_else(|| panic!("unexpected mock Discord route: PUT {route}"));
            {
                let held = state
                    .roles
                    .get_mut(id)
                    .unwrap_or_else(|| panic!("unexpected mock Discord member: {id}"));
                if !held.iter().any(|have| have == role) {
                    held.push(role.to_owned());
                }
                held.sort();
            }
            state.grants_stuck += 1;
            return ScriptedResponse::status(204);
        }
        panic!("unexpected mock Discord route: {} {route}", request.method);
    })
    .await;
    (mock, truth)
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
    assert_eq!(
        requests.len(),
        4,
        "member revalidation must precede the grant: {requests:?}"
    );
    let put_at = requests
        .iter()
        .position(|request| request.method == "PUT")
        .expect("role grant");
    assert!(
        put_at > 0
            && requests[put_at - 1].method == "GET"
            && requests[put_at - 1].path == "/api/v10/guilds/2222/members/1001",
        "member revalidation must precede the grant: {requests:?}"
    );
    let put = &requests[put_at];
    assert_eq!(put.path, "/api/v10/guilds/2222/members/1001/roles/12");
    let reason = put
        .header("x-audit-log-reason")
        .expect("audit reason header");
    assert_eq!(
        decode_reason(reason).as_deref(),
        Some(RANK_SELF_HEAL_AUDIT_REASON),
        "{reason}"
    );
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
/// and publishes instead of failing as `Configuration`. That grant is preceded
/// by a point read of member 1001, and a later point read shows both rungs.
/// A genuinely invalid ladder (Legend missing) still fails closed with no mutation.
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
            let observation = Arc::new(Mutex::new(()));
            // A genuinely invalid ladder still fails closed: Legend missing,
            // nothing granted, nothing published. This control stays on the
            // scripted queue and runs before the freshness assertions.
            let bad_roles = json!({"roles": [
                {"id":"11","name":"Prospect","position":1},
                {"id":"12","name":"Member","position":2},
                {"id":"13","name":"Soldier","position":3},
                {"id":"14","name":"Veteran","position":4},
            ]});
            let mock = MockRest::start(
                vec![
                    ScriptedResponse::json(200, roster.clone()),
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

            let (mock, truth) = start_rank_publication_mock(roster, roles).await;
            tick(Kind::Rank, &pool, &executor(&mock), guild, &observation)
                .await
                .expect("non-cumulative ladder heals and publishes");
            let requests = mock.requests();
            let puts: Vec<_> = requests
                .iter()
                .enumerate()
                .filter(|(_, request)| request.method == "PUT")
                .collect();
            assert_eq!(puts.len(), 1, "exactly one original grant: {requests:?}");
            let (put_at, put) = puts[0];
            // Member 1001 holds Member ("12") without Prospect ("11"), so the
            // heal grants exactly the missing lower rung.
            assert_eq!(put.path, "/api/v10/guilds/2222/members/1001/roles/11");
            assert!(
                put.header("x-audit-log-reason")
                    .and_then(decode_reason)
                    .is_some_and(|reason| reason == RANK_SELF_HEAL_AUDIT_REASON),
                "heal grant carries the exact audit reason"
            );
            let prior = if put_at == 0 {
                None
            } else {
                requests.get(put_at - 1)
            };
            let prior_desc = prior
                .map(|request| format!("{} {}", request.method, request.path))
                .unwrap_or_else(|| "none".to_owned());
            assert!(
                prior.is_some_and(|request| {
                    request.method == "GET"
                        && request.path == "/api/v10/guilds/2222/members/1001"
                }),
                "fresh point read must sit immediately before the original grant: prior {prior_desc}"
            );
            assert!(
                requests.iter().skip(put_at + 1).any(|request| {
                    request.method == "GET"
                        && request.path == "/api/v10/guilds/2222/members/1001"
                }),
                "post-repair member read must observe the granted rung: {requests:?}"
            );
            assert!(
                requests.iter().any(|request| {
                    request.method == "GET"
                        && discord_route(&request.path) == "/guilds/2222/members"
                }),
                "frozen roster list still required: {requests:?}"
            );
            {
                let truth = truth.lock().expect("member truth");
                assert!(
                    truth
                        .point_reads
                        .iter()
                        .any(|(stuck, roles)| *stuck == 0 && roles.as_slice() == ["12".to_owned()]),
                    "initial point read must show Member only: {:?}",
                    truth.point_reads
                );
                assert!(
                    truth.point_reads.iter().any(|(stuck, roles)| {
                        *stuck >= 1
                            && roles.iter().any(|role| role == "11")
                            && roles.iter().any(|role| role == "12")
                    }),
                    "post-repair point read must show Prospect and Member: {:?}",
                    truth.point_reads
                );
            }
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

/// A ladder missing the top rung refuses with `Configuration`, writes
/// nothing, sends no Discord write, and names the failing rung by its fixed
/// label. Uses the process-wide capture: a scoped capture can miss the tick's
/// warn when sibling tests run on other threads.
#[tokio::test]
async fn rank_tick_missing_rung_names_the_rung_and_writes_nothing() {
    let capture = tracing_capture::global();
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP rank ladder diagnostic: TWO_TEST_DATABASE_URL is not set");
        return;
    };
    let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated agent-testdb fixture");
    let pool = fixture.pool().clone();
    apply_web_contract(&pool).await.unwrap();
    let guild = "2222";
    for (index, anomaly) in two_bot_core::RAID_ANOMALIES.iter().enumerate() {
        let (start, _) = two_bot_core::window_bounds(anomaly.start, anomaly.end).unwrap();
        sqlx::query("INSERT INTO members (guild_id, member_id, joined_at, is_bot) VALUES ($1,$2,$3::timestamptz,FALSE)")
            .bind(guild).bind((9100 + index).to_string()).bind(start).execute(&pool).await.unwrap();
    }
    // Distinctive fixture ids plus injection text: none may reach the log.
    // Note: the probe id is a short numeric id so `snowflake()` parses it
    // while the snowflake gate (only 17-20 digit literals) stays clean.
    let probe_id = "79999";
    let probe_name = "ladder-probe-evil-<script>alert(1)</script>";
    let members = json!([member(1000, false, &["11"])]);
    let roles = json!({"roles": [
        {"id": "11", "name": "Prospect"},
        {"id": "12", "name": "Member"},
        {"id": "13", "name": "Soldier"},
        {"id": "14", "name": "Veteran"},
        {"id": probe_id, "name": probe_name},
    ]});
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, members),
            ScriptedResponse::json(200, roles),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let observation = Mutex::new(());
    // Mark the capture point just before the tick: earlier text includes sqlx
    // migration statements that mention rung-like words, so starting here
    // proves the warning names the failing rung.
    let start = capture.text().len();
    let outcome = tick(Kind::Rank, &pool, &executor(&mock), guild, &observation).await;
    assert_eq!(outcome, Err(ErrorClass::Configuration));
    for query in [
        "SELECT count(*) FROM rank_snapshots WHERE guild_id=$1",
        "SELECT count(*) FROM member_ranks WHERE guild_id=$1",
        "SELECT count(*) FROM member_exclusions WHERE guild_id=$1",
    ] {
        let rows: i64 = sqlx::query_scalar(query)
            .bind(guild)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0, "{query} wrote on a refused ladder");
    }
    let requests = mock.requests();
    assert_eq!(requests.len(), 2, "roster plus guild reads only");
    for request in &requests {
        assert_eq!(request.method, "GET", "refused tick sends no Discord write");
    }
    mock.shutdown().await;
    fixture
        .close()
        .await
        .expect("drop disposable test database");
    let logs = capture.text().split_off(start);
    assert!(
        logs.contains("Legend missing matches=0"),
        "missing exact rung diagnostic in logs"
    );
    for leaked in [probe_id, probe_name] {
        assert!(!logs.contains(leaked), "logs leaked {leaked:?}");
    }
}

/// Builder pin for the identity-binding guard behind C1/D8: a point-member
/// read bound to a different user id is unproven evidence, even when its
/// roles look nested. Deleting the user-id check in `rank_repair_member`
/// turns this red. Offline: no database.
#[tokio::test]
async fn rank_repair_member_rejects_misbound_user_id() {
    // Wrong identity carrying nested Prospect-only roles.
    let mock = MockRest::start(
        vec![ScriptedResponse::json(
            200,
            json!({"user": {"id": "1002", "bot": false}, "roles": ["11"]}),
        )],
        ScriptedResponse::status(500),
    )
    .await;
    assert_eq!(
        rank_repair_member(&executor(&mock), "2222", "1001").await,
        Err(ErrorClass::Rest)
    );
    mock.shutdown().await;

    // Sanity: correctly bound nested evidence verifies.
    let mock = MockRest::start(
        vec![ScriptedResponse::json(
            200,
            json!({"user": {"id": "1001", "bot": false}, "roles": ["11"]}),
        )],
        ScriptedResponse::status(500),
    )
    .await;
    let observed = rank_repair_member(&executor(&mock), "2222", "1001")
        .await
        .expect("correctly bound member verifies");
    assert_eq!(
        observed.map(|member| member.user_id),
        Some("1001".to_owned())
    );
    mock.shutdown().await;
}

/// Builder pin for the wrong-identity publication hazard: the point read for
/// 1001 returns a record bound to 1002 carrying nested Prospect-only roles.
/// Without the user-id check the tick would publish Ok with the wrong member;
/// with it the tick fails closed with no PUT and no publication. Lives here
/// (not in the frozen acceptance file) per its builder contract.
#[tokio::test]
async fn rank_tick_wrong_identity_with_nested_roles_fails_closed() {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP wrong-identity nested integration: TWO_TEST_DATABASE_URL is not set");
        return;
    };
    let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated agent-testdb fixture");
    let pool = fixture.pool().clone();
    apply_web_contract(&pool).await.unwrap();
    let guild = "2222";
    for (index, anomaly) in two_bot_core::RAID_ANOMALIES.iter().enumerate() {
        let (start, _) = two_bot_core::window_bounds(anomaly.start, anomaly.end).unwrap();
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
    let roles_doc = json!({"roles": [
        {"id":"10","name":"Bot","position":99},
        {"id":"11","name":"Prospect","position":1},
        {"id":"12","name":"Member","position":2},
        {"id":"13","name":"Soldier","position":3},
        {"id":"14","name":"Veteran","position":4},
        {"id":"15","name":"Legend","position":5},
    ]});
    let mock = MockRest::with_responder(move |request: &RestRequest| {
        let route = discord_route(&request.path);
        if request.method == "GET" && route == "/guilds/2222" {
            return ScriptedResponse::json(200, roles_doc.clone());
        }
        if request.method == "GET" && route == "/guilds/2222/members" {
            return ScriptedResponse::json(200, roster.clone());
        }
        if request.method == "GET" && route == "/users/@me" {
            return ScriptedResponse::json(200, json!({"id": "999", "bot": true}));
        }
        if request.method == "GET" && route == "/guilds/2222/members/999" {
            return ScriptedResponse::json(200, json!({"roles": ["10"]}));
        }
        if request.method == "GET" && route == "/guilds/2222/members/1001" {
            // Misbound evidence with nested roles: without the user-id check
            // the post-repair refresh would publish this as 1001.
            return ScriptedResponse::json(
                200,
                json!({"user": {"id": "1002", "bot": false}, "roles": ["11"]}),
            );
        }
        if request.method == "PUT" {
            return ScriptedResponse::status(204);
        }
        panic!("unexpected mock Discord route: {} {route}", request.method);
    })
    .await;
    let observation = Arc::new(Mutex::new(()));
    let outcome = tick(Kind::Rank, &pool, &executor(&mock), guild, &observation).await;
    assert!(outcome.is_err(), "misbound nested evidence fails closed");
    let requests = mock.requests();
    assert!(
        requests.iter().all(|request| request.method != "PUT"),
        "misbound evidence must not grant: {requests:?}"
    );
    let published: i64 =
        sqlx::query_scalar("SELECT count(*) FROM rank_snapshots WHERE guild_id=$1")
            .bind(guild)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(published, 0, "misbound evidence publishes nothing");
    mock.shutdown().await;
    fixture
        .close()
        .await
        .expect("drop disposable test database");
}

/// A lazy pool that never connects: reaching storage surfaces `Database`, so
/// a `Rest` outcome proves the refusal happened before any write. Mirrors the
/// snapshot suite's poison pool (sibling test modules cannot share it).
fn unconnected_pool() -> PgPool {
    guard_database_url("postgres://agent_test:@agent-testdb:5432/two_bot_test_poison")
        .expect("seam pool must reject ambient PostgreSQL connection settings");
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

/// The injected-stamp seam behaves exactly like the production `run_once`
/// path (which stamps `now_iso()` before delegating): an in-bounds snapshot
/// still attempts publication. Keep the observation safely inside the five-minute
/// age bound; the protected age suite pins the stale boundary independently.
#[tokio::test]
async fn events_seam_with_injected_stamp_reaches_storage() {
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, json!([event()]))],
        ScriptedResponse::status(403),
    )
    .await;
    let pool = unconnected_pool();
    let (_stop, shutdown) = watch::channel(false);
    let observed_at = (OffsetDateTime::now_utc() - time::Duration::seconds(30))
        .format(&Rfc3339)
        .unwrap();
    let outcome =
        snapshot_events_once(&pool, &executor(&mock), "2222", &shutdown, &observed_at).await;
    assert_eq!(
        mock.requests().len(),
        1,
        "the seam must fetch before it can publish"
    );
    assert_eq!(
        outcome,
        Err(ErrorClass::Database),
        "an in-bounds snapshot through the seam must still attempt publication"
    );
    mock.shutdown().await;
    pool.close().await;
}

#[tokio::test]
async fn events_seam_refuses_malformed_before_storage() {
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, json!([event(), json!(null)]))],
        ScriptedResponse::status(403),
    )
    .await;
    let pool = unconnected_pool();
    let (_stop, shutdown) = watch::channel(false);
    let outcome = snapshot_events_once(
        &pool,
        &executor(&mock),
        "2222",
        &shutdown,
        "2026-10-01T18:00:00.000Z",
    )
    .await;
    assert_eq!(mock.requests().len(), 1);
    assert_eq!(
        outcome,
        Err(ErrorClass::Rest),
        "one malformed row through the seam must refuse before storage"
    );
    mock.shutdown().await;
    pool.close().await;
}

/// One Events tick against raw wire bytes (not a re-serialized `Value`) and a
/// poison pool. Returns the tick outcome plus the mock-side request count.
async fn run_events_tick_raw(wire: Vec<u8>) -> (Result<(), ErrorClass>, usize) {
    let mock = MockRest::start(
        vec![ScriptedResponse {
            body: wire,
            ..ScriptedResponse::status(200)
        }],
        ScriptedResponse::status(403),
    )
    .await;
    let pool = unconnected_pool();
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

/// One valid event wrapped in JSON-insignificant whitespace so the wire body is
/// exactly `target_len` bytes while its re-serialization stays tiny. A cap
/// measured over the re-serialized value would admit every leg below.
fn whitespace_padded_wire(target_len: usize) -> Vec<u8> {
    let compact = event().to_string();
    let mut wire = Vec::with_capacity(target_len);
    wire.push(b'[');
    wire.extend(std::iter::repeat_n(
        b' ',
        target_len - compact.len() - 2, // '[' + event + ']'
    ));
    wire.extend_from_slice(compact.as_bytes());
    wire.push(b']');
    assert_eq!(wire.len(), target_len);
    let reserialized: Value = serde_json::from_slice(&wire).unwrap();
    assert!(
        reserialized.to_string().len() < 1024,
        "the re-serialization must stay far under the cap, or this leg \
         cannot distinguish a wire measure from a re-serialized one"
    );
    wire
}

#[tokio::test]
async fn events_tick_refuses_whitespace_padded_wire_body_over_cap() {
    let (outcome, requests) =
        run_events_tick_raw(whitespace_padded_wire(MAX_SCHEDULED_EVENTS_BODY_BYTES + 1)).await;
    assert_eq!(requests, 1, "the tick must fetch before it can refuse");
    assert_eq!(
        outcome,
        Err(ErrorClass::Rest),
        "a wire body 1 byte over cap must refuse even when its re-serialization \
         is tiny; reaching the poison pool would surface a database error instead"
    );
}

#[tokio::test]
async fn events_tick_accepts_whitespace_padded_wire_body_at_cap() {
    let (outcome, requests) =
        run_events_tick_raw(whitespace_padded_wire(MAX_SCHEDULED_EVENTS_BODY_BYTES)).await;
    assert_eq!(requests, 1);
    assert_eq!(
        outcome,
        Err(ErrorClass::Database),
        "an exactly-at-cap wire body is in bounds and must still attempt publication"
    );
}

/// The seam's injected stamp — not store time — is what the mirror stores and
/// orders by. A fresh row must carry the injected stamp exactly, and a seeded
/// newer row must survive a snapshot taken with a still-fresh older injected
/// stamp. The epoch-claim gate, not the age guard, rolls the older snapshot
/// back, so the newer row survives with its stamp untouched.
/// Stamping at store time instead of using the injected value
/// fails both assertions: the fresh row would carry now(), and the older
/// snapshot would win the epoch claim and clobber the seeded row.
#[tokio::test]
async fn events_seam_stores_the_injected_stamp_and_keeps_newer_rows() {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP seam stamp integration: TWO_TEST_DATABASE_URL is not set");
        return;
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated agent-testdb fixture");
    let pool = db.pool();
    apply_web_contract(pool).await.unwrap();
    let now = OffsetDateTime::now_utc();
    let observed_at = (now - time::Duration::seconds(30))
        .format(&Rfc3339)
        .unwrap();
    let newer_at = (now - time::Duration::seconds(10))
        .format(&Rfc3339)
        .unwrap();
    // Guild 3333: seed event 3000 with a stamp newer than the snapshot's
    // injected stamp but older than any store-time stamp, so a store-time
    // regression wins the epoch claim and clobbers it while the injected
    // stamp loses the claim and preserves it. Guild 2222 stays empty so the
    // fresh row below witnesses a genesis claim through the seam.
    let mut seeded = event();
    seeded["guild_id"] = json!("3333");
    let seed = normalize_events(&[raw_event(&seeded, "3333").unwrap()]).unwrap();
    replace_events(pool, "3333", &newer_at, &seed)
        .await
        .unwrap();
    let mut fresh = event();
    fresh["id"] = json!("3001");
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!([fresh])),
            ScriptedResponse::json(200, json!([seeded])),
        ],
        ScriptedResponse::status(403),
    )
    .await;
    let (_stop, shutdown) = watch::channel(false);
    snapshot_events_once(pool, &executor(&mock), "2222", &shutdown, &observed_at)
        .await
        .unwrap();
    // Still fresh, but older than guild 3333's committed epoch: the whole swap
    // rolls back and the seam reports Ok (epoch skip), writing nothing.
    snapshot_events_once(pool, &executor(&mock), "3333", &shutdown, &observed_at)
        .await
        .unwrap();
    assert_eq!(mock.requests().len(), 2);
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT guild_id, event_id, updated_at FROM scheduled_events \
         WHERE guild_id IN ('2222', '3333') ORDER BY guild_id, event_id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            ("2222".to_owned(), "3001".to_owned(), observed_at),
            ("3333".to_owned(), "3000".to_owned(), newer_at),
        ],
        "the fresh row must carry the injected stamp exactly (not store time), \
         and the seeded newer row must survive the older snapshot"
    );
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn events_seam_refuses_malformed_observed_at_before_storage() {
    for observed_at in ["", "not-a-timestamp", "2026-99-99T25:61:61Z"] {
        let mock = MockRest::start(
            vec![ScriptedResponse::json(200, json!([event()]))],
            ScriptedResponse::status(500),
        )
        .await;
        let pool = unconnected_pool();
        pool.close().await;
        let (_stop, shutdown) = watch::channel(false);
        let outcome =
            snapshot_events_once(&pool, &executor(&mock), "2222", &shutdown, observed_at).await;
        let requests = mock.requests();
        mock.shutdown().await;
        assert_eq!(
            outcome,
            Err(ErrorClass::Configuration),
            "invalid observation time must refuse before publication; Database would mean storage was reached"
        );
        assert!(requests.len() <= 1, "invalid timestamps must not retry");
        assert!(requests.iter().all(|request| request.method == "GET"));
        assert_eq!(pool.size(), 0, "the closed pool must stay offline");
    }
}

/// Seed independently of the mirror writer, with an epoch older than the
/// 301-second observation. Without the age guard, the incoming snapshot would
/// win the epoch claim and overwrite or delete the last-good rows.
async fn assert_stale_events_preserve_last_good(body: Value) {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP stale event retention: TWO_TEST_DATABASE_URL is not set");
        return;
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated agent-testdb fixture");
    let pool = db.pool();
    apply_web_contract(pool).await.unwrap();
    let seed_at = (OffsetDateTime::now_utc() - time::Duration::seconds(600))
        .format(&Rfc3339)
        .unwrap();
    sqlx::query(
        "INSERT INTO scheduled_events \
         (guild_id, event_id, name, starts_at, channel_id, description, status, updated_at) \
         VALUES ('2222', '3000', 'Café 🎉', '2026-10-25T19:00:00.000Z', '4444', \
                 E'Line one\\nLine two — unchanged', 'scheduled', $1), \
                ('2222', '3001', 'Second event', '2026-10-26T20:30:00.000Z', NULL, \
                 NULL, 'active', $1)",
    )
    .bind(&seed_at)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO scheduled_events_claim (guild_id, epoch) VALUES ('2222', $1)")
        .bind(&seed_at)
        .execute(pool)
        .await
        .unwrap();
    let read_rows = "SELECT to_jsonb(e) FROM scheduled_events e ORDER BY guild_id, event_id";
    let before: Vec<Value> = sqlx::query_scalar(read_rows).fetch_all(pool).await.unwrap();
    let expected = vec![
        json!({
            "guild_id": "2222", "event_id": "3000", "name": "Café 🎉",
            "starts_at": "2026-10-25T19:00:00.000Z", "channel_id": "4444",
            "description": "Line one\nLine two — unchanged", "status": "scheduled",
            "updated_at": seed_at
        }),
        json!({
            "guild_id": "2222", "event_id": "3001", "name": "Second event",
            "starts_at": "2026-10-26T20:30:00.000Z", "channel_id": null,
            "description": null, "status": "active", "updated_at": seed_at
        }),
    ];
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, body)],
        ScriptedResponse::status(500),
    )
    .await;
    let (_stop, shutdown) = watch::channel(false);
    let observed_at = (OffsetDateTime::now_utc() - time::Duration::seconds(301))
        .format(&Rfc3339)
        .unwrap();
    let outcome =
        snapshot_events_once(pool, &executor(&mock), "2222", &shutdown, &observed_at).await;
    let after: Vec<Value> = sqlx::query_scalar(read_rows).fetch_all(pool).await.unwrap();
    let epochs: Vec<(String, String)> =
        sqlx::query_as("SELECT guild_id, epoch FROM scheduled_events_claim ORDER BY guild_id")
            .fetch_all(pool)
            .await
            .unwrap();
    let requests = mock.requests();
    mock.shutdown().await;
    db.close().await.expect("drop disposable test database");
    assert_eq!(
        before, expected,
        "independent seed pins every last-good field"
    );
    assert_eq!(
        outcome,
        Ok(()),
        "301-second snapshots must skip successfully"
    );
    assert_eq!(
        after, before,
        "the stale path must preserve every stored field, UTF-8 byte, null, timestamp and row"
    );
    assert_eq!(
        epochs,
        [("2222".to_owned(), seed_at)],
        "an age-refused snapshot must not advance the committed epoch"
    );
    assert!(requests.len() <= 1, "stale snapshots must not retry");
    assert!(requests.iter().all(|request| request.method == "GET"));
}

#[tokio::test]
async fn events_seam_age_301_changed_rows_preserve_all_last_good_fields() {
    assert_stale_events_preserve_last_good(json!([
        {
            "id": "3000", "guild_id": "2222", "name": "Stale replacement",
            "scheduled_start_time": "2026-11-01T21:00:00Z", "status": 2,
            "channel_id": "5555", "description": "Must not replace the stored bytes"
        },
        {
            "id": "3002", "guild_id": "2222", "name": "Must not be inserted",
            "scheduled_start_time": "2026-11-02T22:00:00Z", "status": 1,
            "channel_id": null, "description": null
        }
    ]))
    .await;
}

#[tokio::test]
async fn events_seam_age_301_empty_response_preserves_all_last_good_fields() {
    assert_stale_events_preserve_last_good(json!([])).await;
}

/// The age bound is strictly greater than five minutes: exactly 300 s stays
/// fresh, 300 s plus one nanosecond is stale. Deterministic pure-predicate
/// pin; the seam suites cover 299/301 through `snapshot_events_once` but
/// cannot hit exactly 300 s because the gate samples `now_utc()` internally.
#[test]
fn snapshot_age_bound_is_strictly_greater_than_five_minutes() {
    let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
    for (age, exceeded) in [
        (time::Duration::seconds(299), false),
        (time::Duration::seconds(300), false),
        (
            time::Duration::seconds(300) + time::Duration::nanoseconds(1),
            true,
        ),
        (time::Duration::seconds(301), true),
    ] {
        assert_eq!(
            snapshot_age_exceeded(now - age, now),
            exceeded,
            "age {age:?} must {} the five-minute bound",
            if exceeded { "exceed" } else { "not exceed" },
        );
    }
    assert!(
        !snapshot_age_exceeded(now + time::Duration::seconds(1), now),
        "a future observation is never stale"
    );
}
