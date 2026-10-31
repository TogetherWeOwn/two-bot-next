#![cfg(test)]
//! Protected acceptance: a cold shard must revalidate persisted recipients.
//! Seed through the real transactional queue, leave a running claim as a dead
//! worker would, then run the real shard against local WebSocket/REST fixtures.
//! Neither a warm cache nor a pure deserialize/handle test can satisfy this.

use super::*;

mod opening_acceptance;

async fn seed(case: &Case, url: &str, seq: u64) {
    let (action, id) = case
        .store()
        .commit_dispatch_with_job(
            &checkpoint(SESSION, seq, url),
            FunnelBatch::default(),
            Some(GatewayJob {
                payload: queued_welcome().durable_payload().unwrap(),
                occurred_at_ms: RECEIPT_AT,
            }),
        )
        .await
        .expect("checkpoint and durable welcome commit together");
    assert_eq!(action, DispatchAction::Apply);
    assert!(id.is_some());
}

async fn row(case: &Case, seq: i64) -> (String, i32, Option<String>, i64) {
    sqlx::query_as("SELECT state, attempts, payload, occurred_at_ms FROM gateway_onboarding_jobs WHERE session_id = $1 AND seq = $2")
        .bind(SESSION).bind(seq).fetch_one(case.pool()).await.expect("durable recipient receipt")
}

async fn wait_row(case: &Case, seq: i64, state: &str) -> (String, i32, Option<String>, i64) {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let saved = row(case, seq).await;
            if saved.0 == state {
                return saved;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("bounded recipient receipt wait")
}

async fn interrupted(case: &Case) {
    let job = case
        .store()
        .claim_onboarding_job()
        .await
        .unwrap()
        .expect("simulate interrupted first worker");
    assert_eq!(job.occurred_at_ms, RECEIPT_AT);
    match OnboardingJob::recover(&job.payload).unwrap().unwrap() {
        OnboardingJob::Welcome {
            guild_id,
            member_id,
            bot,
            pending,
            trigger,
            roles,
        } => {
            assert_eq!(
                (guild_id, member_id, bot, pending),
                (2222, 77, false, false)
            );
            assert_eq!(trigger, MembershipTrigger::Joined { pending: false });
            assert_eq!(roles, vec![GAME_PICKS[0].role_id.to_owned()]);
        }
        _ => panic!("persisted welcome expected"),
    }
    let saved = row(case, 2).await;
    assert_eq!((saved.0.as_str(), saved.1), ("running", 1));
    assert!(saved.2.is_some());
}

async fn spawn(case: &Case, mock: &MockRest, url: &str) -> Runner {
    spawn_with_shutdown(case, mock, url, None).await
}

async fn spawn_with_shutdown(
    case: &Case,
    mock: &MockRest,
    url: &str,
    shutdown: Option<tokio::sync::watch::Receiver<bool>>,
) -> Runner {
    let store = case.store();
    let saved = load_boot_session(&store).await.unwrap();
    let config = crate::gateway::build_shard_config(TOKEN.into(), Intents::empty(), saved.as_ref());
    let shard = Shard::with_config(
        ShardId::ONE,
        ConfigBuilder::from(config)
            .proxy_url(url.to_owned())
            .build(),
    );
    let pipeline = Arc::new(build_pipeline(store.milestones().await.unwrap(), None));
    let state = Arc::new(RwLock::new(GatewayState::Armed));
    let task = tokio::spawn(run_shard(
        shard,
        pipeline.clone(),
        state.clone(),
        store,
        None,
        Some(Arc::new(runtime_for(case.pool(), mock, "legacy", false))),
        None,
        None,
        None,
        async move {
            match shutdown {
                Some(receiver) => crate::server::shutdown_requested(receiver).await,
                None => std::future::pending().await,
            }
        },
    ));
    Runner {
        task,
        pipeline,
        state,
    }
}

async fn authenticate(case: &Case, ws: &mut GatewayFixture) {
    let auth = ws.mock.authentication().await;
    assert_eq!(auth["op"], 6);
    assert_eq!(auth["d"]["session_id"], SESSION);
    assert_eq!(auth["d"]["seq"], 2);
    ws.send(resumed(3)).await;
    wait_sequence(&case.store(), 3).await;
}

fn assert_cold(runner: &Runner) {
    assert!(
        runner
            .pipeline
            .cache()
            .member(Id::new(2222), Id::new(77))
            .is_none(),
        "fresh shard has no cached recipient or pending transition"
    );
}

async fn completed(case: &Case, seq: i64, attempts: i32) -> (String, i32, Option<String>, i64) {
    let saved = wait_row(case, seq, "completed").await;
    assert_eq!(saved.1, attempts, "bounded claim count");
    assert!(saved.2.is_none(), "terminal receipt clears member payload");
    assert_eq!(
        saved.3, RECEIPT_AT,
        "recovery preserves original dispatch time"
    );
    saved
}

async fn recovered_terminal(evidence: Evidence) {
    let case = Case::new().await;
    let (mock, state) = recipient_discord(evidence).await;
    let result = bounded(async {
        let mut ws = gateway(true).await;
        seed(&case, &ws.mock.url, 2).await;
        interrupted(&case).await;
        case.assert_unsent(&mock).await;
        let runner = spawn(&case, &mock, &ws.mock.url).await;
        assert_cold(&runner);
        authenticate(&case, &mut ws).await;
        let saved = completed(&case, 2, 2).await;
        case.assert_unsent(&mock).await;
        assert!(
            !runner.task.is_finished(),
            "proven recipient ineligibility does not fail the gateway"
        );

        state.lock().unwrap().evidence = Evidence::Valid;
        // A fenced old dispatch cannot resurrect its completed job. A genuinely
        // new pending join followed by a gate clear is a new admitted trigger.
        ws.send(member_add(2, false, &joined_at_wire(RECEIPT_AT)))
            .await;
        ws.send(member_add(4, true, &joined_at_wire(RECEIPT_AT)))
            .await;
        wait_sequence(&case.store(), 4).await;
        wait_row(&case, 4, "completed").await;
        assert_eq!(row(&case, 2).await, saved);
        case.assert_unsent(&mock).await;
        ws.send(gate_clear(5, &joined_at_wire(RECEIPT_AT))).await;
        wait_sequence(&case.store(), 5).await;
        wait_row(&case, 5, "completed").await;
        assert_welcome(&mock, "legacy");
        assert_eq!(
            case.prompts().await,
            1,
            "no false recovered marker suppresses the later gate clear"
        );
        assert!(!runner.task.is_finished());
        runner.stop().await;
    })
    .await;
    finish(case, mock, result).await;
}

async fn wait_failure(case: &Case, runner: &Runner) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if runner.task.is_finished() { return; }
            let saved = row(case, 2).await;
            assert_ne!(saved.0, "completed", "uncertain current recipient was falsely completed instead of retaining retryable work");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("recipient failure must stop the worker within a bounded evidence budget");
}

async fn repoint(case: &Case, url: &str) {
    sqlx::query("UPDATE gateway_sessions SET resume_url = $1 WHERE guild_id = $2 AND shard_id = 0")
        .bind(url)
        .bind(GUILD)
        .execute(case.pool())
        .await
        .expect("replace only the local mock port");
}

async fn recovered_retryable(evidence: Evidence) {
    let case = Case::new().await;
    let (mock, state) = recipient_discord(evidence).await;
    let result = bounded(async {
        let mut first = gateway(true).await;
        seed(&case, &first.mock.url, 2).await;
        interrupted(&case).await;
        let runner = spawn(&case, &mock, &first.mock.url).await;
        assert_cold(&runner);
        assert_eq!(first.mock.authentication().await["op"], 6);
        // A validated replay proves authentication without advancing seq 2.
        // Receiving Resume alone must not start retryable recipient work.
        first.send(resumed(2)).await;
        wait_failure(&case, &runner).await;
        runner.failed().await;
        let saved = row(&case, 2).await;
        assert_eq!((saved.0.as_str(), saved.1), ("running", 2));
        assert_eq!(
            saved.2.as_deref(),
            Some(queued_welcome().durable_payload().unwrap().as_str())
        );
        case.assert_unsent(&mock).await;
        drop(first);

        state.lock().unwrap().evidence = Evidence::ChangedRoles;
        let mut second = gateway(true).await;
        repoint(&case, &second.mock.url).await;
        let runner = spawn(&case, &mock, &second.mock.url).await;
        assert_cold(&runner);
        authenticate(&case, &mut second).await;
        completed(&case, 2, 3).await;
        assert_current_defaults(&assert_welcome(&mock, "legacy"));
        assert_eq!(case.prompts().await, 1);
        assert!(!runner.task.is_finished());
        runner.stop().await;
    })
    .await;
    finish(case, mock, result).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_recovery_absent_has_no_marker_and_later_gate_clear_can_send() {
    recovered_terminal(Evidence::Absent).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_recovery_pending_has_no_marker_and_later_gate_clear_can_send() {
    recovered_terminal(Evidence::Pending).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_recovery_now_bot_has_no_marker_and_later_gate_clear_can_send() {
    recovered_terminal(Evidence::Bot).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_recovery_wrong_identity_retains_job_for_a_valid_retry() {
    recovered_retryable(Evidence::WrongIdentity).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_recovery_malformed_roles_retains_job_for_a_valid_retry() {
    recovered_retryable(Evidence::NonArrayRoles).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_recovery_malformed_membership_retains_job_for_a_valid_retry() {
    recovered_retryable(Evidence::MalformedPending).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_recovery_forbidden_retains_job_for_a_valid_retry() {
    recovered_retryable(Evidence::Forbidden).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_recovery_timeout_retains_job_for_a_valid_retry() {
    recovered_retryable(Evidence::Timeout).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_recovery_renders_current_not_captured_role_defaults() {
    let case = Case::new().await;
    let (mock, _) = recipient_discord(Evidence::ChangedRoles).await;
    let result = bounded(async {
        let mut ws = gateway(true).await;
        seed(&case, &ws.mock.url, 2).await;
        interrupted(&case).await;
        let runner = spawn(&case, &mock, &ws.mock.url).await;
        assert_cold(&runner);
        authenticate(&case, &mut ws).await;
        completed(&case, 2, 2).await;
        assert_current_defaults(&assert_welcome(&mock, "legacy"));
        assert_eq!(case.prompts().await, 1);
        let at: i64 = sqlx::query_scalar("SELECT floor(extract(epoch FROM occurred_at) * 1000)::bigint FROM events WHERE event_type = 'onboarding_prompted'")
            .fetch_one(case.pool()).await.unwrap();
        assert_eq!(at, RECEIPT_AT);
        assert_cold(&runner);
        runner.stop().await;
    }).await;
    finish(case, mock, result).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_recovery_concurrent_valid_jobs_and_sequence_redelivery_send_once() {
    let case = Case::new().await;
    let (mock, _) = recipient_discord(Evidence::Valid).await;
    let result = bounded(async {
        let mut ws = gateway(true).await;
        seed(&case, &ws.mock.url, 2).await;
        interrupted(&case).await;
        seed(&case, &ws.mock.url, 3).await;
        let runner = spawn(&case, &mock, &ws.mock.url).await;
        assert_cold(&runner);
        let auth = ws.mock.authentication().await;
        assert_eq!(auth["op"], 6);
        assert_eq!(auth["d"]["seq"], 3);
        ws.send(resumed(3)).await;
        let first = completed(&case, 2, 2).await;
        completed(&case, 3, 1).await;
        assert_welcome(&mock, "legacy");
        assert_eq!(case.prompts().await, 1);
        ws.send(member_add(2, false, &joined_at_wire(RECEIPT_AT)))
            .await;
        ws.send(resumed(4)).await;
        wait_sequence(&case.store(), 4).await;
        assert_eq!(row(&case, 2).await, first);
        assert_eq!(posts(&mock).len(), 1);
        assert_cold(&runner);
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM gateway_onboarding_jobs")
            .fetch_one(case.pool())
            .await
            .unwrap();
        assert_eq!(
            count, 2,
            "same-sequence dispatch never creates a third delivery"
        );
        assert!(!runner.task.is_finished());
        runner.stop().await;
    })
    .await;
    finish(case, mock, result).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_recovery_persistent_failure_preserves_three_claim_dead_letter_limit()
{
    let case = Case::new().await;
    let (mock, _) = recipient_discord(Evidence::Forbidden).await;
    let result = bounded(async {
        let mut first = gateway(true).await;
        seed(&case, &first.mock.url, 2).await;
        interrupted(&case).await;
        let runner = spawn(&case, &mock, &first.mock.url).await;
        assert_eq!(first.mock.authentication().await["op"], 6);
        first.send(resumed(2)).await;
        wait_failure(&case, &runner).await;
        runner.failed().await;
        assert_eq!(row(&case, 2).await.1, 2);
        case.assert_unsent(&mock).await;
        drop(first);
        let mut second = gateway(true).await;
        repoint(&case, &second.mock.url).await;
        let runner = spawn(&case, &mock, &second.mock.url).await;
        assert_eq!(second.mock.authentication().await["op"], 6);
        second.send(resumed(2)).await;
        wait_failure(&case, &runner).await;
        runner.failed().await;
        assert_eq!(row(&case, 2).await.1, 3);
        assert!(
            case.store().recover_onboarding_jobs().await.is_err(),
            "exhaustion must remain visible at boot"
        );
        let failed = row(&case, 2).await;
        assert_eq!((failed.0.as_str(), failed.1), ("failed", 3));
        assert!(
            failed.2.is_some(),
            "dead-letter diagnostic payload is retained"
        );
        assert!(
            case.store().claim_onboarding_job().await.unwrap().is_none(),
            "no fourth attempt"
        );
        case.assert_unsent(&mock).await;
    })
    .await;
    finish(case, mock, result).await;
}
