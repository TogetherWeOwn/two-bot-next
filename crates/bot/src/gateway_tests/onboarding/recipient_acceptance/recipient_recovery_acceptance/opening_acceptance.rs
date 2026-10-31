#![cfg(test)]
//! Authentication, cooperative opening shutdown, and omitted recipient flags.
//! Every socket and REST endpoint is a local double; only Case's guarded,
//! disposable database is touched. No task abort may satisfy graceful shutdown.

use super::*;

mod readiness_acceptance;

const QUIET_WINDOW: Duration = Duration::from_millis(300);
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(5);

/// Unlike gateway(), neither Hello nor READY is automatic. The test controls
/// both sides of authentication and can keep a connected socket pre-Hello.
async fn controlled_gateway() -> (GatewayFixture, tokio::sync::oneshot::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (auth_tx, auth) = mpsc::channel(4);
    let (dispatch, mut packets) =
        mpsc::channel::<(Value, tokio::sync::oneshot::Sender<std::time::Instant>)>(8);
    let (send_heartbeat, heartbeats) = mpsc::channel(16);
    let (connected, connection) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (_, mut ws) = ServerBuilder::new().accept(stream).await.unwrap();
        let _ = connected.send(());
        loop {
            tokio::select! {
                packet = packets.recv() => {
                    let Some((packet, receipt)) = packet else { return };
                    let sent_at = std::time::Instant::now();
                    if ws.send(Message::text(packet.to_string())).await.is_err() {
                        return;
                    }
                    let _ = receipt.send(sent_at);
                }
                message = ws.next() => {
                    let Some(Ok(message)) = message else { return };
                    if !message.is_text() { continue; }
                    let packet: Value = serde_json::from_str(message.as_text().unwrap()).unwrap();
                    match packet["op"].as_u64() {
                        Some(2 | 6) => {
                            if auth_tx.send(packet).await.is_err() { return; }
                        }
                        Some(1) => {
                            if ws.send(Message::text("{\"op\":11,\"d\":null}".to_owned())).await.is_err() {
                                return;
                            }
                            let _ = send_heartbeat.try_send(());
                        }
                        _ => {}
                    }
                }
            }
        }
    });
    (
        GatewayFixture {
            mock: MockGateway { url, auth, task },
            dispatch,
            heartbeats,
        },
        connection,
    )
}

async fn seed_opening(case: &Case, url: &str, resume: bool) {
    seed(case, url, 2).await;
    if !resume {
        // Losing only the session must not remove accepted durable work.
        case.store().clear().await.unwrap();
        assert!(case.store().load().await.unwrap().is_none());
    }
    assert_eq!(
        row(case, 2).await,
        (
            "pending".into(),
            0,
            Some(queued_welcome().durable_payload().unwrap()),
            RECEIPT_AT,
        )
    );
}

async fn assert_opening_quiet(case: &Case, mock: &MockRest, runner: &Runner) {
    let original = (
        "pending".into(),
        0,
        Some(queued_welcome().durable_payload().unwrap()),
        RECEIPT_AT,
    );
    let until = tokio::time::Instant::now() + QUIET_WINDOW;
    loop {
        assert_eq!(
            row(case, 2).await,
            original,
            "no claim or payload change before authenticated dispatch"
        );
        assert!(
            mock.requests().is_empty(),
            "no member or permission read before authenticated dispatch"
        );
        assert!(
            !runner.task.is_finished(),
            "queued REST failure must not kill an unauthenticated socket"
        );
        if tokio::time::Instant::now() >= until {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    case.assert_unsent(mock).await;
}

#[derive(Clone, Copy)]
enum Authentication {
    IdentifyReady,
    ResumeResumed,
    ResumeReplay,
}

async fn opening_requires_dispatch(authentication: Authentication) {
    let case = Case::new().await;
    let (mock, _) = recipient_discord(Evidence::Forbidden).await;
    let result = bounded(async {
        let (mut ws, connected) = controlled_gateway().await;
        let resume = !matches!(authentication, Authentication::IdentifyReady);
        seed_opening(&case, &ws.mock.url, resume).await;
        let runner = spawn(&case, &mock, &ws.mock.url).await;
        connected.await.expect("real mock WebSocket connected");
        assert_cold(&runner);
        assert_opening_quiet(&case, &mock, &runner).await;
        ws.send(json!({"op":10,"d":{"heartbeat_interval":45000}}))
            .await;
        let auth = ws.mock.authentication().await;
        assert_eq!(auth["op"], if resume { 6 } else { 2 });
        if resume {
            assert_eq!(auth["d"]["session_id"], SESSION);
            assert_eq!(auth["d"]["seq"], 2);
        }
        // Receiving the auth frame, Hello, and even a heartbeat ACK does not
        // demonstrate Discord accepted authentication. Only an op-0 dispatch can.
        ws.send(json!({"op":11,"d":null})).await;
        assert_opening_quiet(&case, &mock, &runner).await;
        ws.send(match authentication {
            Authentication::IdentifyReady => ready(&ws.mock.url, SESSION),
            Authentication::ResumeResumed => resumed(3),
            // A valid-session replay is auth evidence even when the durable
            // duplicate fence suppresses its side effects and cache update.
            Authentication::ResumeReplay => member_add(2, false, &joined_at_wire(RECEIPT_AT)),
        })
        .await;
        wait_request(&mock, "GET", RECIPIENT_PATH).await;
        wait_failure(&case, &runner).await;
        runner.failed().await;
        let saved = row(&case, 2).await;
        assert_eq!(
            (saved.0.as_str(), saved.1, saved.3),
            ("running", 1, RECEIPT_AT)
        );
        assert_eq!(
            saved.2.as_deref(),
            Some(queued_welcome().durable_payload().unwrap().as_str())
        );
        let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM gateway_onboarding_jobs")
            .fetch_one(case.pool())
            .await
            .unwrap();
        assert_eq!(
            jobs, 1,
            "authentication/replay does not create another welcome"
        );
        case.assert_unsent(&mock).await;
    })
    .await;
    finish(case, mock, result).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_gateway_cold_identify_waits_for_ready_before_claiming_retryable_welcome() {
    opening_requires_dispatch(Authentication::IdentifyReady).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_gateway_resume_waits_for_resumed_before_claiming_retryable_welcome() {
    opening_requires_dispatch(Authentication::ResumeResumed).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_gateway_valid_session_replay_releases_opening_before_duplicate_fence() {
    opening_requires_dispatch(Authentication::ResumeReplay).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_gateway_cooperative_shutdown_before_hello_preserves_unclaimed_work() {
    for resume in [false, true] {
        let case = Case::new().await;
        let (mock, _) = recipient_discord(Evidence::Forbidden).await;
        let result = bounded(async {
            let (ws, connected) = controlled_gateway().await;
            seed_opening(&case, &ws.mock.url, resume).await;
            let original = row(&case, 2).await;
            let (shutdown, receiver) = tokio::sync::watch::channel(false);
            let mut runner = spawn_with_shutdown(&case, &mock, &ws.mock.url, Some(receiver)).await;
            connected.await.expect("connected without Hello");
            assert_opening_quiet(&case, &mock, &runner).await;
            let started = tokio::time::Instant::now();
            shutdown.send(true).expect("cooperative shutdown request");
            tokio::time::timeout(SHUTDOWN_BUDGET, &mut runner.task)
                .await
                .expect("closed opening sender must not swallow writer-drained notification")
                .expect("cooperative shutdown must not panic")
                .expect("pre-Hello shutdown succeeds without waiting for the 30s drain budget");
            assert!(started.elapsed() < SHUTDOWN_BUDGET);
            assert_eq!(*runner.state.read().await, GatewayState::Draining);
            assert_eq!(
                row(&case, 2).await,
                original,
                "state/attempts/payload/original time unchanged"
            );
            assert!(
                mock.requests().is_empty(),
                "shutdown never opens the recipient gate"
            );
            let saved = case.store().load().await.unwrap();
            if resume {
                let saved = saved.expect("original checkpoint retained");
                assert_eq!((saved.session_id.as_str(), saved.sequence), (SESSION, 2));
            } else {
                assert!(saved.is_none(), "shutdown invents no session");
            }
            case.assert_unsent(&mock).await;
        })
        .await;
        finish(case, mock, result).await;
    }
}

async fn omitted_flags_recovery(evidence: Evidence) {
    let case = Case::new().await;
    let (mock, _) = recipient_discord(evidence).await;
    let result = bounded(async {
        let mut ws = gateway(true).await;
        seed(&case, &ws.mock.url, 2).await;
        interrupted(&case).await;
        let runner = spawn(&case, &mock, &ws.mock.url).await;
        assert_cold(&runner);
        authenticate(&case, &mut ws).await;
        let saved = completed(&case, 2, 2).await;
        assert_current_defaults(&assert_welcome(&mock, "legacy"));
        assert_eq!(case.prompts().await, 1, "exactly one durable prompt marker");
        assert!(has_onboarding_prompt(case.pool(), GUILD, "77")
            .await
            .unwrap());
        assert!(mock
            .requests()
            .iter()
            .any(|r| r.method == "GET" && r.path == RECIPIENT_PATH));
        // A later, genuinely queued welcome must be suppressed by that marker,
        // not only by the gateway's same-sequence duplicate fence.
        ws.send(member_add(4, false, &joined_at_wire(RECEIPT_AT)))
            .await;
        wait_sequence(&case.store(), 4).await;
        wait_row(&case, 4, "completed").await;
        assert_eq!(row(&case, 2).await, saved);
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
async fn onboarding_recipient_omitted_bot_recovers_once_with_current_role_defaults() {
    omitted_flags_recovery(Evidence::OmittedBot).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_omitted_pending_recovers_once_with_current_role_defaults() {
    omitted_flags_recovery(Evidence::OmittedPending).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_omitted_both_recovers_once_with_current_role_defaults() {
    omitted_flags_recovery(Evidence::OmittedBoth).await;
}
