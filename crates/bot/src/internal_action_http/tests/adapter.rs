#![cfg(test)]
//! Receiver boundary plus the real single-attempt adapter and durable send lane.
//! Only guarded TestDatabase pools and an ephemeral loopback Discord double.
use super::*;
use axum::{extract::State, serve::ListenerExt};
use tokio::time::{timeout_at, Instant};
use two_bot_core::{
    send_admission::{PgSendAdmission, SendAdmission},
    voice_move_decision::VoiceMoveDecision,
};
use two_bot_discord::{internal_actions::CooldownGovernor, ActionExecutor, DiscordError};

struct MockDiscord {
    origin: String,
    requests: Arc<Mutex<Vec<Value>>>,
    sockets: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
struct MockState {
    requests: Arc<Mutex<Vec<Value>>>,
    replies: Arc<Vec<(StatusCode, Value)>>,
    stall_first: bool,
}

impl MockDiscord {
    async fn start(replies: Vec<(StatusCode, Value)>) -> Self {
        Self::start_with_stall(replies, false).await
    }

    async fn start_with_stall(replies: Vec<(StatusCode, Value)>, stall_first: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .fallback(
                |State(state): State<MockState>, request: Request| async move {
                    let (parts, body) = request.into_parts();
                    let raw = to_bytes(body, 8192).await.unwrap();
                    let body: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
                    let index = {
                        let mut seen = state.requests.lock().unwrap();
                        let index = seen.len();
                        seen.push(json!({
                            "method": parts.method.as_str(),
                            "path": parts.uri.path(),
                            "body": body,
                        }));
                        index
                    };
                    if state.stall_first && index == 0 {
                        std::future::pending::<()>().await;
                    }
                    let (status, body) = state.replies[index.min(state.replies.len() - 1)].clone();
                    (status, Json(body))
                },
            )
            .with_state(MockState {
                requests: requests.clone(),
                replies: Arc::new(replies),
                stall_first,
            });
        let sockets = Arc::new(AtomicUsize::new(0));
        let accepted = sockets.clone();
        let listener = listener.tap_io(move |_| {
            accepted.fetch_add(1, Ordering::SeqCst);
        });
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            origin,
            requests,
            sockets,
            task,
        }
    }

    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl Drop for MockDiscord {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn token() -> String {
    format!("local-receiver-fixture-{}", std::process::id())
}

fn adapter(pool: sqlx::PgPool) -> AnnouncementExecutor {
    let _ = rustls::crypto::ring::default_provider().install_default();
    AnnouncementExecutor::with_admission(
        Arc::new(twilight_http::Client::new(token())),
        config().channel_keys().clone(),
        CooldownGovernor::new(),
        Arc::new(PgSendAdmission::new(pool, &token()).unwrap()),
    )
    .unwrap()
}

fn app(pool: sqlx::PgPool, mock: &MockDiscord) -> Router {
    let effect = adapter(pool.clone())
        .with_loopback_test_origin(&mock.origin)
        .unwrap();
    // Announcement-only harness: membership, event.read, event-mutation, the
    // member-moderation verbs and the channel-moderation verbs never arrive
    // here, so the doubles only assert they stay uncalled.
    let member = Arc::new(MockEffect::new(MockOutcome::Success));
    router(Arc::new(ReceiverState::new(
        config(),
        pool,
        Arc::new(effect),
        member,
        Arc::new(MockEventRead::default()),
        Arc::new(MockEventMutate),
        Arc::new(MockModeration::default()),
        Arc::new(MockChannel::default()),
    )))
}

#[tokio::test]
async fn loopback_seam_refuses_public_origins_and_missing_admission() {
    for origin in [
        "https://discord.com",
        "http://192.0.2.1:8091",
        "http://127.0.0.1:8091/api/v10",
        "http://fixture@127.0.0.1:8091",
        "http://127.0.0.1:8091?redirect=elsewhere",
    ] {
        assert!(adapter(lazy_pool())
            .with_loopback_test_origin(origin)
            .is_err());
    }
    let unadmitted = AnnouncementExecutor::new(
        Arc::new(twilight_http::Client::new(token())),
        config().channel_keys().clone(),
        CooldownGovernor::new(),
    );
    assert!(unadmitted
        .with_loopback_test_origin("http://127.0.0.1:8091")
        .is_err());
}

#[tokio::test]
async fn real_adapter_success_and_429_replay_preserve_cross_pool_shared_hold() {
    let Some(db) = database().await else { return };
    let mock = MockDiscord::start(vec![
        (
            StatusCode::OK,
            json!({"id":"444444444444444444", "channel_id":"333333333333333333"}),
        ),
        (
            StatusCode::TOO_MANY_REQUESTS,
            json!({"retry_after":120, "global":false}),
        ),
    ])
    .await;
    let first_app = app(db.pool().clone(), &mock);
    let (status, _, first) = answer(
        first_app.clone(),
        signed(payload(), "old", "intent-real-success"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["result"]["message_id"], "444444444444444444");
    {
        let requests = mock.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["method"], "POST");
        assert_eq!(
            requests[0]["path"],
            "/api/v10/channels/333333333333333333/messages"
        );
        // Kit embed treatment: the validated text renders as one branded
        // embed — ember default, Announcements surface footer — with no
        // plain content (colour + footer are asserted discord-side).
        assert!(requests[0]["body"].get("content").is_none());
        assert_eq!(
            requests[0]["body"]["embeds"][0]["title"],
            "fixture announcement"
        );
        assert_eq!(requests[0]["body"]["allowed_mentions"]["parse"], json!([]));
    }
    let (status, _, refused) = answer(
        first_app,
        signed(payload(), "old", "intent-real-rate-limit"),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(refused["error"]["code"], "discord_rejected");
    assert_eq!(refused["error"]["retryable"], false);
    assert_eq!(mock.count(), 2, "429 must not be retried internally");

    // New pools, stores, adapter and local governor: only the durable lane and
    // receipts survive. Key rotation must not grant a second execution.
    let second = db.independent_pool().await.unwrap();
    let restarted = app(second.clone(), &mock);
    for (intent, code) in [
        ("intent-real-success", None),
        ("intent-real-rate-limit", Some("discord_rejected")),
    ] {
        let (_, headers, replay) =
            answer(restarted.clone(), signed(payload(), "new", intent)).await;
        assert_eq!(headers["idempotent-replay"], "true");
        if let Some(code) = code {
            assert_eq!(replay["error"]["code"], code);
        } else {
            assert_eq!(replay["result"], first["result"]);
        }
    }
    let (_, _, independent) = answer(
        restarted,
        signed(payload(), "new", "intent-real-independent"),
    )
    .await;
    assert_eq!(independent["error"]["code"], "no_effect");
    assert_eq!(independent["error"]["retryable"], false);
    let held: bool = sqlx::query_scalar(
        "SELECT NOT in_flight AND hold_until_ms > \
         floor(extract(epoch FROM clock_timestamp()) * 1000)::bigint \
         FROM discord_send_admission",
    )
    .fetch_one(&second)
    .await
    .unwrap();
    assert!(held);
    let other = ActionExecutor::with_admission(
        token(),
        Some(mock.origin.clone()),
        Arc::new(PgSendAdmission::new(second.clone(), &token()).unwrap()),
    )
    .unwrap();
    assert!(matches!(
        other
            .ban("111111111111111111", "222222222222222222", "fixture")
            .await,
        Err(DiscordError::Unavailable(_))
    ));
    assert_eq!(
        mock.count(),
        2,
        "independent and cross-transport sends stay held"
    );
    let completed: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM internal_idempotency WHERE state = 'completed'")
            .fetch_one(&second)
            .await
            .unwrap();
    assert_eq!(completed, 3);
    second.close().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn real_adapter_invalid_receipt_keeps_intent_and_restart_send_lane_occupied() {
    let Some(db) = database().await else { return };
    let mock = MockDiscord::start(vec![(
        StatusCode::OK,
        json!({"id":"444444444444444444", "channel_id":"555555555555555555"}),
    )])
    .await;
    let (_, _, first) = answer(
        app(db.pool().clone(), &mock),
        signed(payload(), "old", "intent-real-uncertain"),
    )
    .await;
    assert_eq!(first["error"]["code"], "needs_reconciliation");
    assert_eq!(first["error"]["retryable"], false);
    let second = db.independent_pool().await.unwrap();
    let restarted = app(second.clone(), &mock);
    let (_, headers, replay) = answer(
        restarted.clone(),
        signed(payload(), "new", "intent-real-uncertain"),
    )
    .await;
    assert_eq!(replay["error"]["code"], "needs_reconciliation");
    assert!(!headers.contains_key("idempotent-replay"));
    let (_, _, independent) = answer(
        restarted,
        signed(payload(), "new", "intent-real-after-uncertain"),
    )
    .await;
    assert_eq!(independent["error"]["code"], "no_effect");
    let occupied: bool = sqlx::query_scalar("SELECT in_flight FROM discord_send_admission")
        .fetch_one(&second)
        .await
        .unwrap();
    assert!(occupied);
    assert_eq!(
        mock.count(),
        1,
        "uncertain effects never grant another send"
    );
    second.close().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn voice_evidence_checks_member_identity_and_preserves_read_errors() {
    let guild = staging_guild();
    let _ = rustls::crypto::ring::default_provider().install_default();
    for (status, returned_member) in [
        (StatusCode::OK, "3"),
        (StatusCode::OK, "9"),
        (StatusCode::FORBIDDEN, "3"),
    ] {
        let mock = MockDiscord::start(vec![
            (StatusCode::OK, json!({"id":"4","bot":true})),
            (
                StatusCode::OK,
                json!({
                    "id": guild, "owner_id": "8", "afk_channel_id": null,
                    "roles": [{"id": guild, "position": 0, "permissions": "8"}]
                }),
            ),
            (StatusCode::OK, voice_channel("2", 2, "target", 0, 3)),
            (status, voice_member(returned_member, false)),
            (StatusCode::OK, voice_channel("1", 2, "source", 0, 0)),
            (StatusCode::OK, voice_member("4", true)),
        ])
        .await;
        let api = ActionExecutor::with_proxy(token(), Some(mock.origin.clone())).unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        let target = api
            .voice_move_target_until(guild.parse().unwrap(), 2, deadline)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(target.user_limit(), 3);
        let result = api.voice_move_evidence_until(target, 3, 1, deadline).await;
        match (status, returned_member, result) {
            (StatusCode::OK, "3", Ok(evidence)) => assert_eq!(
                evidence.decide(1),
                VoiceMoveDecision::Proceed { from_channel_id: 1 }
            ),
            (StatusCode::OK, "9", Err(DiscordError::Unavailable(detail))) => {
                assert_eq!(detail, "voice_move_evidence_incomplete");
                assert_eq!(mock.count(), 4);
            }
            (StatusCode::FORBIDDEN, _, Err(DiscordError::Rejected(_))) => {
                assert_eq!(mock.count(), 4);
            }
            _ => panic!("unexpected voice evidence result"),
        }
        assert!(mock
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request["method"] == "GET"));
    }
}

#[tokio::test]
async fn governed_voice_read_cutoff_leaves_the_real_lane_reusable() {
    let Some(db) = database().await else { return };
    let mock = MockDiscord::start_with_stall(vec![(StatusCode::OK, Value::Null)], true).await;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let lane = Arc::new(PgSendAdmission::new(db.pool().clone(), &token()).unwrap());
    let api =
        ActionExecutor::with_admission(token(), Some(mock.origin.clone()), lane.clone()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let read = api.voice_move_target_until(staging_guild().parse().unwrap(), 2, deadline);
    assert!(matches!(
        timeout_at(deadline + Duration::from_secs(1), read).await,
        Ok(Err(DiscordError::Timeout))
    ));
    assert_eq!((mock.count(), mock.sockets.load(Ordering::SeqCst)), (1, 1));
    assert_eq!(mock.requests.lock().unwrap()[0]["method"], "GET");
    lane.admit_strict()
        .await
        .unwrap()
        .complete(None)
        .await
        .unwrap();
    db.close().await.unwrap();
}
