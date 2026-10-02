#![cfg(test)]
//! Regression coverage for queued defers, accepted-work drain and RESUMED sync.
use super::*;
use two_bot_core::{ClassifierConfig, InteractionRouter, RouterGates};
use two_bot_discord::{interactions::InteractionRuntime, ActionExecutor};

use crate::discord_test_common::{MockRest, ScriptedResponse};

const EVENT: &str = "1546451670500642999";

fn interaction(sequence: u64, status: &str) -> Value {
    json!({"op":0,"s":sequence,"t":"INTERACTION_CREATE","d":{
        "application_id":"1111", "authorizing_integration_owners":{"0":GUILD},
        "id":sequence.to_string(), "token":format!("mock-rsvp-{sequence}"),
        "type":2, "version":1, "guild_id":GUILD,
        "member":{"permissions":"0", "roles":[], "deaf":false, "mute":false,
            "flags":0, "user":{"id":"77", "username":"human", "discriminator":"0"}},
        "data":{"id":"4444", "name":"rsvp", "type":1, "options":[
            {"name":"event-id", "type":3, "value":EVENT},
            {"name":"status", "type":3, "value":status}
        ]}
    }})
}

fn gates() -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD.parse().unwrap()),
        announcements: true,
        scorecard: true,
        automations: true,
        moderation: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

fn runtime(db: &TestDb, rest: &MockRest) -> Arc<InteractionRuntime> {
    Arc::new(InteractionRuntime {
        router: InteractionRouter::new(gates()),
        pool: db.pool.clone(),
        executor: ActionExecutor::with_proxy(TOKEN.into(), Some(rest.origin())).unwrap(),
        classifier: ClassifierConfig::default(),
    })
}

async fn spawn(db: &TestDb, url: &str, rest: &MockRest) -> JoinHandle<Result<(), sqlx::Error>> {
    spawn_with_shutdown(db, url, rest, None).await
}

async fn spawn_with_shutdown(
    db: &TestDb,
    url: &str,
    rest: &MockRest,
    shutdown: Option<tokio::sync::watch::Receiver<bool>>,
) -> JoinHandle<Result<(), sqlx::Error>> {
    ensure_crypto_provider();
    let saved = load_boot_session(&db.store).await.unwrap();
    let shard =
        crate::gateway::build_shard(TOKEN.into(), Intents::empty(), saved.as_ref(), Some(url));
    tokio::spawn(run_shard(
        shard,
        Arc::new(build_pipeline(db.store.milestones().await.unwrap(), None)),
        Arc::new(RwLock::new(GatewayState::Armed)),
        db.store.clone(),
        Some(runtime(db, rest)),
        Some(crate::command_runtime::CommandRuntime::new(
            db.pool.clone(),
            ActionExecutor::with_proxy(TOKEN.into(), Some(rest.origin())).unwrap(),
            crate::command_runtime::router_with_commands(gates()),
            GUILD.parse().unwrap(),
            true,
        )),
        async move {
            match shutdown {
                Some(receiver) => crate::server::shutdown_requested(receiver).await,
                None => std::future::pending().await,
            }
        },
    ))
}

async fn connect(
    db: &TestDb,
    rest: &MockRest,
) -> (
    JoinHandle<Result<(), sqlx::Error>>,
    tokio_websockets::WebSocketStream<tokio::net::TcpStream>,
) {
    connect_with_shutdown(db, rest, None).await
}

async fn connect_with_shutdown(
    db: &TestDb,
    rest: &MockRest,
    shutdown: Option<tokio::sync::watch::Receiver<bool>>,
) -> (
    JoinHandle<Result<(), sqlx::Error>>,
    tokio_websockets::WebSocketStream<tokio::net::TcpStream>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let runner = spawn_with_shutdown(db, &url, rest, shutdown).await;
    let (socket, _) = listener.accept().await.unwrap();
    let (_, mut ws) = ServerBuilder::new().accept(socket).await.unwrap();
    ws.send(Message::text(
        json!({"op":10,"d":{"heartbeat_interval":45000}}).to_string(),
    ))
    .await
    .unwrap();
    // Accept IDENTIFY, answering any jittered initial heartbeat.
    loop {
        let message = ws.next().await.unwrap().unwrap();
        if !message.is_text() {
            continue;
        }
        let packet: Value = serde_json::from_str(message.as_text().unwrap()).unwrap();
        if packet["op"] == 2 {
            break;
        }
        if packet["op"] == 1 {
            ws.send(Message::text("{\"op\":11,\"d\":null}".to_owned()))
                .await
                .unwrap();
        }
    }
    ws.send(Message::text(ready(&url, "rsvp-session").to_string()))
        .await
        .unwrap();
    wait_sequence(&db.store, 1).await;
    (runner, ws)
}

async fn wait_requests(rest: &MockRest, count: usize) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while rest.requests().len() < count {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("REST request deadline");
}

#[derive(Clone, Copy)]
enum CheckpointFailure {
    Rejected,
    Timeout,
}

async fn queued_commands(
    slow_database: bool,
    checkpoint_failure: Option<CheckpointFailure>,
    graceful_shutdown: bool,
    overflow: bool,
) {
    let db = TestDb::new().await;
    let mut lock = if slow_database {
        let mut tx = db.pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE event_rsvps IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await
            .unwrap();
        Some(tx)
    } else {
        None
    };
    let event = ScriptedResponse::json(200, json!({"id":EVENT,"guild_id":GUILD,"status":1}));
    let rest = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!({"id":"1111"})),
            ScriptedResponse::status(200),
            ScriptedResponse::status(204),
            event.clone().delayed(if slow_database {
                Duration::ZERO
            } else {
                Duration::from_millis(3200)
            }),
            ScriptedResponse::status(204),
            ScriptedResponse::status(200),
            event,
            ScriptedResponse::status(200),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let (runner, mut ws) = connect_with_shutdown(&db, &rest, Some(receiver)).await;
    ws.send(Message::text(interaction(2, "going").to_string()))
        .await
        .unwrap();
    wait_requests(&rest, 4).await; // First defer and live-event read are underway.
    let delivered = std::time::Instant::now();
    ws.send(Message::text(interaction(3, "interested").to_string()))
        .await
        .unwrap();
    // Overflow is fatal under the shared dispatcher. Error/shutdown cases stay
    // below capacity so their original cause, not overflow, controls the drain.
    let last = if overflow { 68 } else { 6 };
    for sequence in 4..=last {
        ws.send(Message::text(leave(sequence).to_string()))
            .await
            .unwrap();
    }
    wait_requests(&rest, 5).await;
    let requests = rest.requests();
    assert!(requests[4]
        .path
        .ends_with("/interactions/3/mock-rsvp-3/callback"));
    assert!(requests[4].received_at.duration_since(delivered) < Duration::from_secs(3));
    assert_eq!(
        serde_json::from_slice::<Value>(&requests[4].body).unwrap()["type"],
        5
    );
    assert_eq!(db.store.load().await.unwrap().unwrap().sequence, 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM announcements_audit_log")
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        0
    );
    if graceful_shutdown {
        shutdown.send_replace(true);
    }
    match checkpoint_failure {
        Some(CheckpointFailure::Rejected) => {
            sqlx::raw_sql(
                "CREATE FUNCTION reject_checkpoint() RETURNS trigger LANGUAGE plpgsql AS $$
                 BEGIN RAISE EXCEPTION 'fixture checkpoint failure'; END; $$;
                 CREATE TRIGGER reject_checkpoint BEFORE INSERT OR UPDATE ON gateway_sessions
                 FOR EACH ROW EXECUTE FUNCTION reject_checkpoint();",
            )
            .execute(&db.pool)
            .await
            .unwrap();
        }
        Some(CheckpointFailure::Timeout) => {
            let mut tx = db.pool.begin().await.unwrap();
            sqlx::query("LOCK TABLE gateway_sessions IN ACCESS EXCLUSIVE MODE")
                .execute(&mut *tx)
                .await
                .unwrap();
            lock = Some(tx);
        }
        None => {}
    }
    if slow_database {
        // Exceed the shared dispatch I/O deadline while an accepted SQL write
        // is pending. The runner may fail, but must drain both accepted replies.
        tokio::time::sleep(Duration::from_millis(30100)).await;
        lock.take().unwrap().rollback().await.unwrap();
    }
    let runner = if let Some(failure) = checkpoint_failure {
        // The runner must finish both accepted commands, then return the original
        // checkpoint error. It may not checkpoint later funnel packets.
        let error = tokio::time::timeout(Duration::from_secs(15), runner)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        let message = match failure {
            CheckpointFailure::Rejected => "fixture checkpoint failure",
            CheckpointFailure::Timeout => "checkpoint deadline exceeded",
        };
        assert!(error.to_string().contains(message));
        if let Some(tx) = lock.take() {
            tx.rollback().await.unwrap();
        }
        assert_eq!(db.store.load().await.unwrap().unwrap().sequence, 1);
        assert_eq!(db.count().await, 0);
        None
    } else if graceful_shutdown {
        tokio::time::timeout(Duration::from_secs(15), runner)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        // The in-flight command commits; queued accepted commands drain but
        // buffered gateway dispatches are not admitted after shutdown.
        assert_eq!(db.store.load().await.unwrap().unwrap().sequence, 2);
        assert_eq!(db.count().await, 0);
        None
    } else if overflow {
        let error = tokio::time::timeout(Duration::from_secs(15), runner)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        let expected = if slow_database {
            "dispatch I/O deadline exceeded"
        } else {
            "dispatch backlog full"
        };
        assert!(error.to_string().contains(expected), "{error}");
        assert_eq!(db.count().await, 0);
        None
    } else {
        wait_sequence(&db.store, 6).await;
        Some(runner)
    };
    let status: String = sqlx::query_scalar("SELECT status FROM event_rsvps")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(status, "interested"); // Effects remained in gateway dispatch order.
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM announcements_audit_log")
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        2
    );
    let requests = rest.requests();
    assert_eq!(requests.len(), 8);
    for (index, content) in [(5, "RSVP saved: going."), (7, "RSVP saved: interested.")] {
        assert_eq!(requests[index].method, "PATCH");
        assert_eq!(
            serde_json::from_slice::<Value>(&requests[index].body).unwrap()["content"],
            content
        );
    }
    if let Some(runner) = runner {
        // Replayed committed dispatches must not send duplicate callbacks or effects.
        ws.send(Message::text(interaction(3, "going").to_string()))
            .await
            .unwrap();
        ws.send(Message::text(leave(7).to_string())).await.unwrap();
        wait_sequence(&db.store, 7).await;
        assert_eq!(rest.requests().len(), 8);
        shutdown.send_replace(true);
        runner.await.unwrap().unwrap();
    }
    drop(ws);
    rest.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn queued_rsvp_completes_in_order_and_committed_replay_is_silent() {
    queued_commands(false, None, false, false).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn queued_rsvp_is_deferred_within_three_seconds_and_drains_on_overflow() {
    queued_commands(false, None, false, true).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn acknowledged_rsvp_drains_before_fatal_dispatch_deadline_returns() {
    queued_commands(true, None, false, true).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn acknowledged_queue_drains_before_checkpoint_error_returns() {
    queued_commands(false, Some(CheckpointFailure::Rejected), false, false).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn acknowledged_queue_drains_before_checkpoint_timeout_returns() {
    queued_commands(false, Some(CheckpointFailure::Timeout), false, false).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn acknowledged_queue_drains_before_graceful_shutdown_returns() {
    queued_commands(false, None, true, false).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn sticky_and_feed_are_deferred_at_receipt_while_rsvp_is_pending() {
    let db = TestDb::new().await;
    let rest = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!({"id":"1111"})),
            ScriptedResponse::status(200),
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!({"id":EVENT,"guild_id":GUILD,"status":1}))
                .delayed(Duration::from_millis(3200)),
        ],
        ScriptedResponse::status(200),
    )
    .await;
    let (runner, mut ws) = connect(&db, &rest).await;
    ws.send(Message::text(interaction(2, "going").to_string()))
        .await
        .unwrap();
    wait_requests(&rest, 4).await;
    let delivered = std::time::Instant::now();
    let commands: Vec<_> = [(3, "sticky"), (4, "feed-remove")]
        .into_iter()
        .map(|(sequence, name)| {
            let mut packet = interaction(sequence, "going");
            packet["d"]["member"]["permissions"] = json!("32");
            packet["d"]["data"]["name"] = json!(name);
            packet["d"]["data"]["options"] = json!([]);
            packet
        })
        .collect();
    // These validation replies need no store mutation, but still use the same
    // defer/edit envelope as real sticky/feed effects. No channel is supplied.
    for command in &commands {
        ws.send(Message::text(command.to_string())).await.unwrap();
    }
    wait_requests(&rest, 8).await;
    let requests = rest.requests();
    for sequence in [3, 4] {
        let suffix = format!("/interactions/{sequence}/mock-rsvp-{sequence}/callback");
        let callbacks: Vec<_> = requests
            .iter()
            .filter(|request| request.path.ends_with(&suffix))
            .collect();
        assert_eq!(callbacks.len(), 1);
        assert!(callbacks[0].received_at.duration_since(delivered) < Duration::from_secs(3));
        let body: Value = serde_json::from_slice(&callbacks[0].body).unwrap();
        assert_eq!(body["type"], 5);
        assert_eq!(body["data"]["flags"], 64);
        let suffix = format!("/webhooks/1111/mock-rsvp-{sequence}/messages/@original");
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.method == "PATCH" && request.path.ends_with(&suffix))
                .count(),
            1
        );
    }
    assert_eq!(db.store.load().await.unwrap().unwrap().sequence, 1);
    ws.send(Message::text(leave(5).to_string())).await.unwrap();
    wait_sequence(&db.store, 5).await;
    assert_eq!(rest.requests().len(), 9); // No second dispatch at queue consumption.
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM announcements_audit_log")
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        1
    );
    for command in &commands {
        ws.send(Message::text(command.to_string())).await.unwrap();
    }
    ws.send(Message::text(leave(6).to_string())).await.unwrap();
    wait_sequence(&db.store, 6).await;
    assert_eq!(rest.requests().len(), 9);
    runner.abort();
    let _ = runner.await;
    drop(ws);
    rest.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn invalid_application_identity_prevents_registry_and_gateway_startup() {
    let db = TestDb::new().await;
    for response in [
        ScriptedResponse::status(403),
        ScriptedResponse::status(200),
        ScriptedResponse::json(200, json!({"id":"0"})),
        ScriptedResponse::json(200, json!({"id":"not-an-id"})),
    ] {
        let rest = MockRest::start(vec![response], ScriptedResponse::status(500)).await;
        let runner = spawn(&db, "ws://127.0.0.1:1", &rest).await;
        assert!(tokio::time::timeout(Duration::from_secs(10), runner)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        let requests = rest.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert!(db.store.load().await.unwrap().is_none());
        rest.shutdown().await;
    }
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn resumed_startup_publishes_full_registry_without_ready() {
    let db = TestDb::new().await;
    let mut gateway = MockGateway::new(false, true).await;
    db.store
        .commit_dispatch(
            &checkpoint("persisted-session", 2, &gateway.url),
            FunnelBatch::default(),
        )
        .await
        .unwrap();
    let rest = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!({"id":"1111"})),
            ScriptedResponse::status(200),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runner = spawn(&db, &gateway.url, &rest).await;
    assert_eq!(gateway.authentication().await["op"], 6);
    wait_sequence(&db.store, 3).await;
    let requests = rest.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/api/v10/applications/@me");
    assert_eq!(requests[1].method, "PUT");
    assert_eq!(
        requests[1].path,
        "/api/v10/applications/1111/guilds/2222/commands"
    );
    let published: Vec<Value> = serde_json::from_slice(&requests[1].body).unwrap();
    let names: Vec<_> = published
        .iter()
        .map(|command| command["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.iter().filter(|name| **name == "rsvp").count(), 1);
    assert_eq!(
        names
            .iter()
            .filter(|name| **name == "rsvp-attendance")
            .count(),
        1
    );
    assert_eq!(
        names.len(),
        runtime(&db, &rest).router.publish_set(&[]).unwrap().len()
    );
    assert_eq!(
        names.iter().filter(|name| **name == "attendance").count(),
        1
    );
    assert!(names.contains(&"rank"));
    runner.abort();
    let _ = runner.await;
    gateway.task.abort();
    rest.shutdown().await;
    db.close().await;
}
