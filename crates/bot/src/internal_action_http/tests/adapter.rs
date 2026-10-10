#![cfg(test)]
//! Receiver boundary plus the real single-attempt adapter and durable send lane.
//! Only guarded TestDatabase pools and an ephemeral loopback Discord double.
use super::*;
use axum::extract::State;
use two_bot_core::send_admission::PgSendAdmission;
use two_bot_discord::{internal_actions::CooldownGovernor, ActionExecutor, DiscordError};

struct MockDiscord {
    origin: String,
    requests: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
struct MockState {
    requests: Arc<Mutex<Vec<Value>>>,
    replies: Arc<Vec<(StatusCode, Value)>>,
}

impl MockDiscord {
    async fn start(replies: Vec<(StatusCode, Value)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .fallback(
                |State(state): State<MockState>, request: Request| async move {
                    let (parts, body) = request.into_parts();
                    let raw = to_bytes(body, 8192).await.unwrap();
                    let body: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
                    let reply = {
                        let mut seen = state.requests.lock().unwrap();
                        let reply = state.replies[seen.len().min(state.replies.len() - 1)].clone();
                        seen.push(json!({
                            "method": parts.method.as_str(),
                            "path": parts.uri.path(),
                            "body": body,
                        }));
                        reply
                    };
                    (reply.0, Json(reply.1))
                },
            )
            .with_state(MockState {
                requests: requests.clone(),
                replies: Arc::new(replies),
            });
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            origin,
            requests,
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
    // Announcement-only harness: event and moderation verbs never arrive
    // here, so the read, mutation and moderation doubles only assert they
    // stay uncalled.
    router(Arc::new(ReceiverState::new(
        config(),
        pool.clone(),
        Arc::new(effect),
        Arc::new(MockEventRead::default()),
        Arc::new(MockEventMutate),
        Arc::new(MockModeration::default()),
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
        assert_eq!(requests[0]["body"]["content"], "fixture announcement");
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
