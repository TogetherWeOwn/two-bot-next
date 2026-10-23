#![cfg(test)]
//! Protected gateway acceptance: best-effort celebrations cannot poison the
//! checkpoint deadline. Builders must preserve this file and its registration.
//! Real apply_dispatch, XP stores and cursor; disposable TestDatabase and
//! loopback Discord HTTP only. Direct handle_at/drain await semantics are not
//! constrained by these tests.
use super::*;

use crate::discord_test_common::{MockRest, ScriptedResponse};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{json, Value};
use std::time::Duration;
use tower::ServiceExt as _;
use two_bot_core::{
    format_iso_millis, gateway_funnel::FunnelBatch, leveling_store, OnboardingGates, OnboardingMode,
};
use two_bot_discord::ActionExecutor;
use two_bot_testsupport::TestDatabase;

const GUILD: u64 = 1545644954272137297;
const MEMBER: u64 = 100000000000000001;
const CHANNEL: u64 = 300000000000000001;
const ROLE: &str = "200000000000000001";

fn at(seconds: i64) -> String {
    format_iso_millis(1_786_771_200_000 + seconds * 1000)
}

fn message(seconds: i64) -> Event {
    Event::MessageCreate(Box::new(
        serde_json::from_value(json!({
            "id": (4_000_000_000_000_000_000_u64 + seconds as u64).to_string(),
            "guild_id": GUILD.to_string(), "channel_id": CHANNEL.to_string(),
            "author": {"id": MEMBER.to_string(), "username": "human", "bot": false,
                "discriminator": "0", "avatar": null},
            "content": "eligible message", "timestamp": at(seconds).replace('Z', "+00:00"),
            "edited_timestamp": null, "tts": false, "mention_everyone": false,
            "mentions": [], "mention_roles": [], "attachments": [], "embeds": [],
            "pinned": false, "type": 0, "components": []
        }))
        .unwrap(),
    ))
}

fn checkpoint(sequence: u64) -> GatewaySession {
    GatewaySession {
        session_id: "level-up-session".into(),
        sequence,
        resume_url: "wss://gateway.discord.gg".into(),
        updated_at_ms: two_bot_core::funnel::now_millis_for_test(),
    }
}

struct Fixture {
    db: TestDatabase,
    pipeline: GatewayPipeline,
    store: GatewaySessionStore,
    state: Arc<RwLock<GatewayState>>,
    generation: AtomicU64,
    failures: AtomicU64,
}

impl Fixture {
    async fn new(rest: &MockRest, reward: bool) -> Self {
        let url = std::env::var("TWO_TEST_DATABASE_URL")
            .expect("explicit disposable test database bootstrap required");
        let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create isolated migrated test database; no fallback");
        sqlx::query("INSERT INTO member_levels (guild_id,member_id,xp,imported_xp,updated_at) VALUES ($1,$2,90,90,NOW())")
            .bind(GUILD.to_string())
            .bind(MEMBER.to_string())
            .execute(db.pool())
            .await
            .unwrap();
        if reward {
            leveling_store::replace_role_rewards(
                db.pool(),
                &GUILD.to_string(),
                &[two_bot_core::leveling::LevelRoleReward {
                    level: 1,
                    role_id: ROLE.into(),
                }],
            )
            .await
            .unwrap();
        }
        let executor = ActionExecutor::with_proxy("mock-only-token".into(), Some(rest.origin()))
            .expect("loopback executor");
        let pipeline = build_pipeline(
            vec![],
            Some(LevelingRuntime::new(
                db.pool().clone(),
                Arc::new(executor),
                GUILD,
                OnboardingGates {
                    mode: OnboardingMode::Legacy,
                    dry_run: false,
                },
            )),
        );
        let store = GatewaySessionStore::new(db.pool().clone(), GUILD.to_string(), 0);
        store
            .commit_dispatch(&checkpoint(1), FunnelBatch::default())
            .await
            .expect("seed prior cursor");
        Self {
            db,
            pipeline,
            store,
            state: Arc::new(RwLock::new(GatewayState::Connected)),
            generation: AtomicU64::new(0),
            failures: AtomicU64::new(0),
        }
    }

    async fn readyz(&self) -> StatusCode {
        crate::server::router(crate::server::SharedState::new(
            self.state.clone(),
            Some(self.db.pool().clone()),
        ))
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
    }

    fn dispatch(&self, sequence: u64, seconds: i64, deadline: Duration) -> Result<(), sqlx::Error> {
        let mut dispatch = ReceivedDispatch::new(message(seconds));
        dispatch.observed_at = at(seconds);
        // The production dispatch worker runs this synchronous step on a
        // blocking thread, not directly on a Tokio async worker.
        tokio::task::block_in_place(|| {
            apply_dispatch(
                &tokio::runtime::Handle::current(),
                &self.state,
                &self.generation,
                &self.failures,
                &self.pipeline,
                &self.store,
                None,
                None,
                None,
                None,
                LiveInteractions::default(),
                Arc::new(tokio::sync::Notify::new()),
                Some(Box::new(dispatch)),
                None,
                checkpoint(sequence),
                deadline,
                self.generation.load(Ordering::Acquire),
                None,
                None,
            )
        })
    }
}

async fn stalled_celebration_preserves_checkpoint(delay: Duration, deadline: Duration) {
    assert!(
        delay > deadline,
        "fixture must outlive the checkpoint budget"
    );
    let rest = MockRest::start(
        vec![ScriptedResponse::json(200, json!({"id": "900000000000000001"})).delayed(delay)],
        ScriptedResponse::status(500),
    )
    .await;
    // No role ladder: the only external I/O is the celebration POST.
    let fixture = Fixture::new(&rest, false).await;
    let outcome = fixture.dispatch(2, 0, deadline);
    let first_cursor = fixture.store.load().await.unwrap().unwrap().sequence;
    let first_state = *fixture.state.read().await;
    let first_failures = fixture.failures.load(Ordering::Acquire);
    let first_readyz = fixture.readyz().await;
    let first_profile =
        leveling_store::profile(fixture.db.pool(), &GUILD.to_string(), &MEMBER.to_string())
            .await
            .unwrap();
    let first_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM xp_awards")
        .fetch_one(fixture.db.pool())
        .await
        .unwrap();

    // Keep using the same worker state/pipeline after the slow send, rather
    // than hiding a poisoned worker behind a new runtime or readiness reset.
    let next = fixture.dispatch(3, 120, CHECKPOINT_IO_MAX);
    let next_cursor = fixture.store.load().await.unwrap().unwrap().sequence;
    let next_state = *fixture.state.read().await;
    let next_readyz = fixture.readyz().await;
    let next_profile =
        leveling_store::profile(fixture.db.pool(), &GUILD.to_string(), &MEMBER.to_string())
            .await
            .unwrap();
    let next_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM xp_awards")
        .fetch_one(fixture.db.pool())
        .await
        .unwrap();
    let requests = rest.requests();
    rest.shutdown().await;
    fixture
        .db
        .close()
        .await
        .expect("drop isolated test database");

    assert_eq!(requests.len(), 1, "no roles, retries or second crossing");
    assert_eq!(requests[0].method, "POST");
    assert_eq!(
        requests[0].path,
        format!("/api/v10/channels/{CHANNEL}/messages")
    );
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    assert_eq!(
        (
            first_profile.xp,
            first_profile.message_xp,
            first_profile.level
        ),
        (105, 15, 1)
    );
    assert_eq!(first_rows, 1, "the crossing's earning commits once");
    assert!(
        outcome.is_ok(),
        "best-effort send must not fail the worker: {outcome:?}"
    );
    assert_eq!(
        first_cursor, 2,
        "a slow celebration cannot hold the gateway cursor"
    );
    assert_eq!(
        first_state,
        GatewayState::Connected,
        "slow delivery cannot poison readiness"
    );
    assert_eq!(
        first_failures, 0,
        "a sender timeout is not a checkpoint failure"
    );
    assert_eq!(first_readyz, StatusCode::OK);
    assert!(
        next.is_ok(),
        "the same worker accepts the next dispatch: {next:?}"
    );
    assert_eq!(next_cursor, 3);
    assert_eq!(next_state, GatewayState::Connected);
    assert_eq!(next_readyz, StatusCode::OK);
    assert_eq!(
        (next_profile.xp, next_profile.message_xp, next_profile.level),
        (120, 30, 1)
    );
    assert_eq!(
        next_rows, 2,
        "subsequent earning is neither lost nor replayed"
    );
}

// Catches putting delivery back under the supplied heartbeat-relative
// checkpoint budget, even if a successful HTTP reply arrives shortly later.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_success_past_supplied_deadline_does_not_fail_gateway_worker() {
    stalled_celebration_preserves_checkpoint(Duration::from_secs(3), Duration::from_secs(2)).await;
}

// Catches the bounded sender timeout leaking into the gateway's failure
// counter/readiness path. The double outlives even CHECKPOINT_IO_MAX (5 s).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn celebration_stall_past_five_seconds_does_not_fail_gateway_worker() {
    stalled_celebration_preserves_checkpoint(Duration::from_secs(6), CHECKPOINT_IO_MAX).await;
}

// Separating best-effort delivery must not accidentally erase an earlier
// role reconciliation failure and advance the cursor as though it succeeded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn celebration_delivery_does_not_erase_prior_role_failure() {
    let rest = MockRest::with_responder(|request| {
        if request.method == "GET" && request.path.contains("/members/") {
            ScriptedResponse::json(
                403,
                json!({"code": 50013, "message": "Missing Permissions"}),
            )
        } else if request.method == "POST" && request.path.ends_with("/messages") {
            ScriptedResponse::json(200, json!({"id": "900000000000000001"}))
        } else {
            ScriptedResponse::status(500)
        }
    })
    .await;
    let fixture = Fixture::new(&rest, true).await;
    let outcome = fixture.dispatch(2, 0, CHECKPOINT_IO_MAX);
    let cursor = fixture.store.load().await.unwrap().unwrap().sequence;
    let profile =
        leveling_store::profile(fixture.db.pool(), &GUILD.to_string(), &MEMBER.to_string())
            .await
            .unwrap();
    let requests = rest.requests();
    rest.shutdown().await;
    fixture
        .db
        .close()
        .await
        .expect("drop isolated test database");

    assert_eq!(
        (profile.xp, profile.message_xp),
        (105, 15),
        "role failure never reverts XP"
    );
    assert!(
        requests
            .iter()
            .any(|r| r.method == "GET" && r.path.contains("/members/")),
        "fixture reached role reconciliation"
    );
    assert!(
        outcome.is_err(),
        "best-effort delivery cannot swallow a prior role failure"
    );
    assert_eq!(
        cursor, 1,
        "the role failure still holds the original cursor"
    );
}
