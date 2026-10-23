#![cfg(test)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use two_bot_core::onboarding_store::has_onboarding_prompt;

use super::support::*;
use crate::discord_test_common::{ResponseGate, ScriptedResponse};
use crate::gateway::{build_pipeline, run_shard, GatewayState};
use crate::gateway_tests::{checkpoint, MockGateway};
use crate::onboarding::RuntimeError;
use tokio::sync::{watch, RwLock};
use twilight_gateway::{ConfigBuilder, Intents, Shard, ShardId};
use two_bot_core::gateway_funnel::FunnelBatch;
use two_bot_cutover::gateway_session::{GatewayJob, GatewaySessionStore};

fn unavailable(body: &Value, permission: bool) {
    let content = text(body).to_lowercase();
    assert!(
        content.contains("unavailable"),
        "failed reads cannot produce fabricated counts: {content:?}"
    );
    assert!(board(body).is_empty(), "no numeric board is fabricated");
    assert!(
        !content.contains("0 invite") && !content.contains("0 attributed"),
        "unavailable is never a numeric zero"
    );
    if permission {
        assert!(
            content.contains("permission") && content.contains("manage server"),
            "403 names the missing permission: {content:?}"
        );
    } else {
        assert!(
            !content.contains("permission") && !content.contains("manage server"),
            "non-permission failures must not diagnose missing Manage Server access: {content:?}"
        );
    }
}

async fn failed_invites(response: ScriptedResponse, permission: bool) {
    with_fixture(
        Discord {
            invites: response,
            ..Default::default()
        },
        move |f| {
            Box::pin(async move {
                // Historical evidence exists: an unavailable read is not proof of zero.
                f.credit(MEMBER, ALICE).await;
                let body = f.command(ALICE).await;
                unavailable(&body, permission);
            })
        },
    )
    .await;
}

#[tokio::test]
async fn successful_nonarray_invite_json_edits_unavailable_without_a_permission_diagnosis() {
    failed_invites(ScriptedResponse::json(200, json!({"invites": []})), false).await;
}

#[tokio::test]
async fn malformed_invite_json_edits_unavailable_without_a_permission_diagnosis() {
    let mut response = ScriptedResponse::status(200);
    response.body = b"{not-json".to_vec();
    failed_invites(response, false).await;
}

#[tokio::test]
async fn not_found_invite_read_edits_unavailable_without_a_permission_diagnosis() {
    failed_invites(
        ScriptedResponse::json(404, json!({"message": "Unknown Guild", "code": 10004})),
        false,
    )
    .await;
}

#[tokio::test]
async fn upstream_invite_failure_edits_unavailable_without_a_permission_diagnosis() {
    failed_invites(
        ScriptedResponse::json(503, json!({"message": "Temporarily unavailable"})),
        false,
    )
    .await;
}

#[tokio::test]
async fn truncated_invite_transport_edits_unavailable_without_a_permission_diagnosis() {
    failed_invites(
        ScriptedResponse::json(200, json!([])).truncated_body(),
        false,
    )
    .await;
}

#[tokio::test]
async fn forbidden_invite_read_edits_the_specific_manage_server_permission_notice() {
    failed_invites(
        ScriptedResponse::json(
            403,
            json!({"message": "Missing Permissions", "code": 50013}),
        ),
        true,
    )
    .await;
}

#[tokio::test]
async fn failed_count_store_edits_unavailable_instead_of_reporting_zero() {
    with_fixture(Discord::default(), |f| {
        Box::pin(async move {
            f.credit(MEMBER, ALICE).await;
            let failed_pool =
                f.db.independent_pool()
                    .await
                    .expect("independent fixture pool");
            failed_pool.close().await;
            unavailable(&command(&failed_pool, &f.mock, ALICE).await, false);
        })
    })
    .await;
}

#[tokio::test]
async fn locked_count_store_edits_unavailable_instead_of_reporting_zero() {
    with_fixture(Discord::default(), |f| {
        Box::pin(async move {
            f.credit(MEMBER, ALICE).await;
            let mut lock = f.db.pool().begin().await.expect("fixture lock transaction");
            sqlx::query("LOCK TABLE events IN ACCESS EXCLUSIVE MODE")
                .execute(&mut *lock)
                .await
                .expect("lock only fixture events");
            let body = f.command(ALICE).await;
            lock.rollback().await.expect("release fixture lock");
            unavailable(&body, false);
        })
    })
    .await;
}

#[tokio::test]
async fn twenty_second_reply_budget_edits_unavailable_instead_of_posting_partial_counts() {
    with_fixture(
        Discord {
            member_delay: Duration::from_millis(2100),
            ..Default::default()
        },
        |f| {
            Box::pin(async move {
                for index in 0..10 {
                    f.credit(20000 + index, 10000 + index).await;
                }
                // Eleven successful names at 2.1 s each exceed 20 s in aggregate,
                // but no individual request reaches its own transport timeout.
                let start = Instant::now();
                let body = f.command(9555).await;
                let elapsed = start.elapsed();
                unavailable(&body, false);
                assert!(
                    elapsed >= Duration::from_secs(19),
                    "the aggregate budget, not an earlier transport failure, was exercised"
                );
                assert!(
                    elapsed < Duration::from_secs(24),
                    "the 20-second budget bounds the reply"
                );
                let name_reads = f
                    .mock
                    .requests()
                    .iter()
                    .filter(|request| request.method == "GET" && request.path.contains("/members/"))
                    .count();
                assert!(
                    (8..=10).contains(&name_reads),
                    "budget aborts before all eleven names are fetched: {name_reads}"
                );
            })
        },
    )
    .await;
}

/// Owns the real gateway worker and its local WebSocket even on assertion failure.
struct Worker {
    task: tokio::task::JoinHandle<Result<(), sqlx::Error>>,
    shutdown: watch::Sender<bool>,
    gateway: MockGateway,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.task.abort();
        self.gateway.task.abort();
    }
}

async fn worker(f: &Fixture, pool: &sqlx::PgPool, seed: bool) -> Worker {
    let mut gateway = MockGateway::new(false, true).await;
    let store = GatewaySessionStore::new(f.db.pool().clone(), GUILD.to_string(), 0);
    if seed {
        store
            .commit_dispatch_with_job(
                &checkpoint("invite-review-recovery", 2, &gateway.url),
                FunnelBatch::default(),
                Some(GatewayJob {
                    payload: welcome_job(MEMBER)
                        .durable_payload()
                        .expect("durable welcome"),
                    occurred_at_ms: NOW,
                }),
            )
            .await
            .expect("commit durable welcome");
    } else {
        sqlx::query(
            "UPDATE gateway_sessions SET resume_url = $1 WHERE guild_id = '2222' AND shard_id = 0",
        )
        .bind(&gateway.url)
        .execute(f.db.pool())
        .await
        .expect("repoint only the local WebSocket fixture");
    }
    let saved = store.load().await.expect("stored checkpoint");
    let config =
        crate::gateway::build_shard_config("mock-token".into(), Intents::empty(), saved.as_ref());
    let shard = Shard::with_config(
        ShardId::ONE,
        ConfigBuilder::from(config)
            .proxy_url(gateway.url.clone())
            .build(),
    );
    let pipeline = Arc::new(build_pipeline(
        store.milestones().await.expect("fixture milestones"),
        None,
    ));
    let state = Arc::new(RwLock::new(GatewayState::Armed));
    let runtime = Arc::new(onboarding(pool, &f.mock));
    let (shutdown, receiver) = watch::channel(false);
    let task = tokio::spawn(run_shard(
        shard,
        pipeline,
        state,
        store,
        None,
        Some(runtime),
        None,
        None,
        None,
        async move { crate::server::shutdown_requested(receiver).await },
    ));
    assert_eq!(
        gateway.authentication().await["op"],
        6,
        "cold worker resumes its persisted checkpoint"
    );
    Worker {
        task,
        shutdown,
        gateway,
    }
}

async fn job_row(f: &Fixture) -> (String, i32, Option<String>) {
    sqlx::query_as("SELECT state, attempts, payload FROM gateway_onboarding_jobs WHERE session_id = 'invite-review-recovery' AND seq = 2")
        .fetch_one(f.db.pool()).await.expect("durable welcome receipt")
}

#[tokio::test]
async fn actual_gateway_worker_retains_attribution_failure_and_recovers_the_same_welcome_job() {
    let gate = Arc::new(ResponseGate::default());
    with_fixture(Discord { gate: Some((MEMBER, gate.clone())), ..Default::default() }, move |f| Box::pin(async move {
        f.credit(MEMBER, ALICE).await;
        let failed_pool = f.db.independent_pool().await.expect("independent runtime pool");
        let mut first = worker(f, &failed_pool, true).await;
        tokio::time::timeout(Duration::from_secs(10), gate.wait_for_request()).await.expect("eligible recipient reached");
        failed_pool.close().await;
        gate.release();
        let result = tokio::time::timeout(Duration::from_secs(15), &mut first.task).await.expect("failed worker stops").expect("worker joins");
        assert!(result.is_err(), "the real worker cannot finish an attribution store error as success");
        let row = job_row(f).await;
        assert_eq!((row.0.as_str(), row.1), ("running", 1));
        assert_eq!(row.2.as_deref(), Some(welcome_job(MEMBER).durable_payload().expect("durable payload").as_str()));
        assert!(welcome_posts(&f.mock).is_empty());
        assert!(!has_onboarding_prompt(f.db.pool(), &GUILD.to_string(), &MEMBER.to_string()).await.expect("no prompt claim"));
        drop(first);
        let mut second = worker(f, f.db.pool(), false).await;
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if job_row(f).await.0 == "completed" { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("cold worker retries the retained job");
        assert_eq!(job_row(f).await, ("completed".into(), 2, None));
        let posts = welcome_posts(&f.mock);
        assert_eq!(posts.len(), 1, "only the recovered attempt sends a welcome");
        assert!(text(&posts[0]).ends_with("Invited by Alice."));
        assert_eq!(posts[0]["allowed_mentions"], json!({"parse": [], "users": [MEMBER.to_string()], "roles": [], "replied_user": false}));
        second.shutdown.send(true).expect("stop recovered worker");
        tokio::time::timeout(Duration::from_secs(15), &mut second.task).await.expect("worker drains").expect("worker joins").expect("clean shutdown");
    })).await;
}

#[derive(Clone, Copy)]
enum StoreFailure {
    Closed,
    Locked,
    AcquisitionTimeout,
}

async fn recover_preclaim(failure: StoreFailure) {
    let gate = Arc::new(ResponseGate::default());
    with_fixture(Discord { gate: Some((MEMBER, gate.clone())), ..Default::default() }, move |f| Box::pin(async move {
        f.credit(MEMBER, ALICE).await;
        // Reuse guarded connection options, never a runtime DATABASE_URL.
        // The long acquisition budget allows the caller's 10 s read deadline
        // to be challenged independently of SQL's 5 s statement timeout.
        let runtime_pool = PgPoolOptions::new().max_connections(1).acquire_timeout(Duration::from_secs(30))
            .connect_with(f.db.pool().connect_options().as_ref().clone()).await.expect("scoped runtime pool");
        let runtime = onboarding(&runtime_pool, &f.mock);
        let payload = welcome_job(MEMBER).durable_payload().expect("retryable job payload");
        let queued = crate::onboarding::OnboardingJob::recover(&payload).expect("recover payload").expect("welcome job");
        let task = tokio::spawn(async move { runtime.handle(queued, NOW).await });
        tokio::time::timeout(Duration::from_secs(10), gate.wait_for_request()).await.expect("fresh eligible recipient read reached");
        // The eligibility response is held on the socket. Earlier prompt and
        // permissions reads succeeded; faults below affect current_inviter.
        let mut lock = None;
        let mut occupied = None;
        match failure {
            StoreFailure::Closed => runtime_pool.close().await,
            StoreFailure::Locked => {
                let mut tx = f.db.pool().begin().await.expect("lock transaction");
                sqlx::query("LOCK TABLE events IN ACCESS EXCLUSIVE MODE").execute(&mut *tx).await.expect("hold events after eligibility reads");
                lock = Some(tx);
            }
            StoreFailure::AcquisitionTimeout => occupied = Some(runtime_pool.acquire().await.expect("occupy sole runtime connection")),
        }
        let start = Instant::now();
        gate.release();
        let result = tokio::time::timeout(Duration::from_secs(13), task).await.expect("current inviter error is bounded").expect("welcome task joins");
        let elapsed = start.elapsed();
        if let Some(tx) = lock { tx.rollback().await.expect("release test lock"); }
        drop(occupied);
        runtime_pool.close().await;
        assert!(matches!(result, Err(RuntimeError::Store)), "store failure propagates instead of claiming an inviter-less success: {result:?}");
        if matches!(failure, StoreFailure::AcquisitionTimeout) {
            assert!(elapsed >= Duration::from_secs(9) && elapsed < Duration::from_secs(13), "outer 10 s deadline, not the 30 s acquisition deadline");
        }
        assert!(!has_onboarding_prompt(f.db.pool(), &GUILD.to_string(), &MEMBER.to_string()).await.expect("prompt remains readable"), "attribution failure must precede prompt claim");
        assert!(welcome_posts(&f.mock).is_empty(), "no false success is sent on a store failure");
        // Replay the same durable payload after recovery. Successful delivery
        // claims the prompt; replaying again cannot duplicate the welcome.
        let recovered = crate::onboarding::OnboardingJob::recover(&payload).expect("recover retry").expect("retry payload retained");
        onboarding(f.db.pool(), &f.mock).handle(recovered, NOW).await.expect("retry succeeds without clearing any prompt");
        let posts = welcome_posts(&f.mock);
        assert_eq!(posts.len(), 1);
        assert!(text(&posts[0]).ends_with("Invited by Alice."));
        assert_eq!(posts[0]["allowed_mentions"], json!({"parse": [], "users": [MEMBER.to_string()], "roles": [], "replied_user": false}));
        onboarding(f.db.pool(), &f.mock).handle(welcome_job(MEMBER), NOW).await.expect("duplicate delivery is idempotent");
        assert_eq!(welcome_posts(&f.mock).len(), 1);
    })).await;
}

#[tokio::test]
async fn current_inviter_store_error_after_eligibility_leaves_prompt_unclaimed_and_retryable() {
    recover_preclaim(StoreFailure::Closed).await;
}

#[tokio::test]
async fn current_inviter_sql_timeout_after_eligibility_leaves_prompt_unclaimed_and_retryable() {
    recover_preclaim(StoreFailure::Locked).await;
}

#[tokio::test]
async fn current_inviter_outer_timeout_after_eligibility_leaves_prompt_unclaimed_and_retryable() {
    recover_preclaim(StoreFailure::AcquisitionTimeout).await;
}

#[tokio::test]
async fn stalled_inviter_name_sends_one_member_fallback_welcome_with_only_recipient_mentions() {
    let gate = Arc::new(ResponseGate::default());
    with_fixture(Discord { gate: Some((ALICE, gate.clone())), ..Default::default() }, move |f| Box::pin(async move {
        f.credit(MEMBER, ALICE).await;
        let runtime = onboarding(f.db.pool(), &f.mock);
        let task = tokio::spawn(async move { runtime.handle(welcome_job(MEMBER), NOW).await });
        tokio::time::timeout(Duration::from_secs(10), gate.wait_for_request()).await.expect("inviter-name endpoint reached");
        let result = tokio::time::timeout(Duration::from_secs(7), task).await.expect("stalled name is bounded").expect("welcome task joins");
        gate.release();
        result.expect("name failure uses a fallback rather than failing the welcome");
        let posts = welcome_posts(&f.mock);
        assert_eq!(posts.len(), 1, "exactly one fallback welcome");
        assert!(text(&posts[0]).ends_with("Invited by Member 9004."), "timeout fallback: {:?}", text(&posts[0]));
        assert_eq!(posts[0]["allowed_mentions"], json!({"parse": [], "users": [MEMBER.to_string()], "roles": [], "replied_user": false}));
        onboarding(f.db.pool(), &f.mock).handle(welcome_job(MEMBER), NOW).await.expect("duplicate retry");
        assert_eq!(welcome_posts(&f.mock).len(), 1, "retry cannot duplicate fallback welcome");
    })).await;
}
