//! Shared Postgres lane plus both real transports pointed at mock Discord.
use super::*;
use crate::{ActionExecutor, DiscordError};
use two_bot_core::send_admission::PgSendAdmission;
use two_bot_testsupport::TestDatabase;

mod uncertainty;

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit test database URL required");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .unwrap()
}

fn token() -> String {
    format!("local-fixture-{}", std::process::id())
}

fn announcement_executor(mock: &MockDiscord, gate: Arc<dyn SendAdmission>) -> AnnouncementExecutor {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = Arc::new(TwilightClient::builder().token(token()).build());
    let mut executor =
        AnnouncementExecutor::with_admission(client, keys(), CooldownGovernor::new(), gate)
            .unwrap();
    executor.api_origin = mock.origin.clone();
    executor.timeout = Duration::from_millis(500);
    executor
}

async fn reached_wire(mock: &MockDiscord) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while mock.count() == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn admission_success_releases_lane_for_another_transport() {
    let db = database().await;
    let mock = MockDiscord::start(Reply::success()).await;
    let gate = Arc::new(PgSendAdmission::new(db.pool().clone(), &token()).unwrap());
    let announced = announcement_executor(&mock, gate.clone());
    assert!(matches!(
        run_once(&announced, &announcement("first intent")).await,
        ExecutionOutcome::Posted(_)
    ));
    let action = ActionExecutor::with_admission(token(), Some(mock.origin.clone()), gate).unwrap();
    action
        .ban("111111111111111111", "222222222222222222", "fixture")
        .await
        .unwrap();
    let occupied: bool = sqlx::query_scalar("SELECT in_flight FROM public.discord_send_admission")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(!occupied);
    assert_eq!(mock.count(), 2);
    db.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn admission_each_action_retry_rechecks_shared_hold() {
    let db = database().await;
    let mut reply = Reply::new(429, r#"{"retry_after":0.2,"global":false}"#);
    reply.delay = Duration::from_millis(100);
    let mock = MockDiscord::start(reply).await;
    let gate = Arc::new(PgSendAdmission::new(db.pool().clone(), &token()).unwrap());
    let action =
        ActionExecutor::with_admission(token(), Some(mock.origin.clone()), gate.clone()).unwrap();
    let sending = tokio::spawn(async move {
        action
            .publish_guild_commands(111111111111111111, 222222222222222222, &[])
            .await
    });
    reached_wire(&mock).await;
    gate.extend(two_bot_core::send_admission::SendCooldown::Indefinite)
        .await
        .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(2), sending)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(outcome, Err(DiscordError::Unavailable(_))));
    assert_eq!(
        mock.count(),
        1,
        "retry must not bypass an extended lane hold"
    );
    db.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn admission_announcement_429_blocks_action_backup_and_restart_cross_pool() {
    let db = database().await;
    let second = db.independent_pool().await.unwrap();
    let mock = MockDiscord::start(Reply::new(429, r#"{"retry_after":120,"global":false}"#)).await;
    let gate = Arc::new(PgSendAdmission::new(db.pool().clone(), &token()).unwrap());
    let announced = announcement_executor(&mock, gate);
    let outcome = run_once(&announced, &announcement("first independent intent")).await;
    assert!(matches!(
        outcome,
        ExecutionOutcome::RateLimited(RateLimitCooldown {
            retry_after_ms: Some(120_000),
            ..
        })
    ));
    let gate = Arc::new(PgSendAdmission::new(second.clone(), &format!("Bot {}", token())).unwrap());
    let action =
        ActionExecutor::with_admission(token(), Some(mock.origin.clone()), gate.clone()).unwrap();
    assert!(matches!(
        action
            .ban("111111111111111111", "222222222222222222", "fixture")
            .await,
        Err(DiscordError::Unavailable(_))
    ));
    let api_base = format!("{}/api/v10", mock.origin);
    let backup = two_bot_core::backup::guild_config_api::GuildConfigDiscordApi::with_admission(
        Some(&api_base),
        None,
        token(),
        "fixture-app".to_owned(),
        "fixture-guild".to_owned(),
        gate.clone(),
    )
    .unwrap();
    assert!(backup
        .request_json("GET", "/users/@me", None)
        .await
        .is_err());
    let restarted = announcement_executor(&mock, gate);
    assert_eq!(
        run_once(&restarted, &announcement("second independent intent")).await,
        ExecutionOutcome::NoEffect(Refusal::SendAdmissionBlocked)
    );
    assert_eq!(
        mock.count(),
        1,
        "no hidden retry, backup or cross-transport send"
    );
    second.close().await;
    db.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn admission_action_429_without_timing_installs_indefinite_global_hold() {
    let db = database().await;
    let mock = MockDiscord::start(Reply::new(429, "broken provider body")).await;
    let gate = Arc::new(PgSendAdmission::new(db.pool().clone(), &token()).unwrap());
    let action =
        ActionExecutor::with_admission(token(), Some(mock.origin.clone()), gate.clone()).unwrap();
    assert_eq!(
        action
            .ban("111111111111111111", "222222222222222222", "fixture")
            .await,
        Err(DiscordError::RateLimited)
    );
    let indefinite: bool =
        sqlx::query_scalar("SELECT indefinite FROM public.discord_send_admission")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert!(indefinite);
    let announced = announcement_executor(&mock, gate);
    assert_eq!(
        run_once(&announced, &announcement("new intent")).await,
        ExecutionOutcome::NoEffect(Refusal::SendAdmissionBlocked)
    );
    assert_eq!(mock.count(), 1);
    db.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn admission_cancellation_after_wire_keeps_restart_and_other_transport_blocked() {
    let db = database().await;
    let mut reply = Reply::success();
    reply.delay = Duration::from_secs(1);
    let mock = MockDiscord::start(reply).await;
    let gate = Arc::new(PgSendAdmission::new(db.pool().clone(), &token()).unwrap());
    let executor = Arc::new(announcement_executor(&mock, gate.clone()));
    let task_executor = executor.clone();
    let sending = tokio::spawn(async move {
        task_executor
            .execute("announcement.post", &announcement("cancelled intent"))
            .await
    });
    reached_wire(&mock).await;
    sending.abort();
    assert!(sending.await.unwrap_err().is_cancelled());
    let action =
        ActionExecutor::with_admission(token(), Some(mock.origin.clone()), gate.clone()).unwrap();
    assert!(action
        .ban("111111111111111111", "222222222222222222", "fixture")
        .await
        .is_err());
    let restarted = announcement_executor(&mock, gate);
    assert_eq!(
        run_once(&restarted, &announcement("new independent intent")).await,
        ExecutionOutcome::NoEffect(Refusal::SendAdmissionBlocked)
    );
    assert_eq!(mock.count(), 1);
    db.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn admission_completion_storage_failure_preserves_definitive_429_no_resend() {
    let db = database().await;
    let second = db.independent_pool().await.unwrap();
    let mut reply = Reply::new(429, r#"{"retry_after":10,"global":true}"#);
    reply.delay = Duration::from_millis(100);
    let mock = MockDiscord::start(reply).await;
    let gate = Arc::new(PgSendAdmission::new(second.clone(), &token()).unwrap());
    let executor = Arc::new(announcement_executor(&mock, gate));
    let task_executor = executor.clone();
    let sending = tokio::spawn(async move {
        task_executor
            .execute("announcement.post", &announcement("one claimed intent"))
            .await
    });
    reached_wire(&mock).await;
    second.close().await;
    assert!(matches!(
        sending.await.unwrap(),
        ExecutionOutcome::RateLimited(_)
    ));
    let restarted = announcement_executor(
        &mock,
        Arc::new(PgSendAdmission::new(db.pool().clone(), &token()).unwrap()),
    );
    assert_eq!(
        run_once(&restarted, &announcement("other intent")).await,
        ExecutionOutcome::NoEffect(Refusal::SendAdmissionBlocked)
    );
    assert_eq!(mock.count(), 1);
    db.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn admission_storage_failure_before_send_and_token_mismatch_refuse_locally() {
    let db = database().await;
    let second = db.independent_pool().await.unwrap();
    let mock = MockDiscord::start(Reply::success()).await;
    let gate = Arc::new(PgSendAdmission::new(second.clone(), &token()).unwrap());
    second.close().await;
    let executor = announcement_executor(&mock, gate.clone());
    assert_eq!(
        run_once(&executor, &announcement("new intent")).await,
        ExecutionOutcome::NoEffect(Refusal::SendAdmissionBlocked)
    );
    assert!(ActionExecutor::with_admission(
        "different-fixture".to_owned(),
        Some(mock.origin.clone()),
        gate
    )
    .is_err());
    assert_eq!(mock.count(), 0);
    db.close().await.unwrap();
}
