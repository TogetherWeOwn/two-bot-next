//! Regression coverage for queued defers, accepted-work drain and RESUMED sync.
use super::*;
use two_bot_core::{ClassifierConfig, InteractionRouter, RouterGates};
use two_bot_discord::{interactions::InteractionRuntime, ActionExecutor};

#[allow(dead_code)]
#[path = "../../../discord/tests/common/mod.rs"]
mod common;
use common::{MockRest, ScriptedResponse};

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

fn runtime(db: &TestDb, rest: &MockRest) -> Arc<InteractionRuntime> {
    Arc::new(InteractionRuntime {
        router: InteractionRouter::new(RouterGates {
            configured_guild: Some(GUILD.parse().unwrap()),
            announcements: true,
            scorecard: true,
            automations: false,
            moderation: false,
            tickets: false,
            self_roles: false,
            onboarding_picker: false,
            session_picker: false,
        }),
        pool: db.pool.clone(),
        executor: ActionExecutor::with_proxy(TOKEN.into(), Some(rest.origin())).unwrap(),
        classifier: ClassifierConfig::default(),
    })
}

async fn spawn(db: &TestDb, url: &str, rest: &MockRest) -> JoinHandle<Result<(), sqlx::Error>> {
    ensure_crypto_provider();
    let saved = load_boot_session(&db.store).await.unwrap();
    let config = crate::gateway::build_shard_config(TOKEN.into(), Intents::empty(), saved.as_ref());
    let shard = Shard::with_config(
        ShardId::ONE,
        ConfigBuilder::from(config)
            .proxy_url(url.to_owned())
            .build(),
    );
    tokio::spawn(run_shard(
        shard,
        Arc::new(build_pipeline(db.store.milestones().await.unwrap())),
        Arc::new(RwLock::new(GatewayState::Armed)),
        db.store.clone(),
        Some(runtime(db, rest)),
    ))
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

async fn queued_commands(slow_database: bool) {
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
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let runner = spawn(&db, &url, &rest).await;
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
    ws.send(Message::text(interaction(2, "going").to_string()))
        .await
        .unwrap();
    wait_requests(&rest, 4).await; // First defer and live-event read are underway.
    let delivered = std::time::Instant::now();
    ws.send(Message::text(interaction(3, "interested").to_string()))
        .await
        .unwrap();
    // Exceed the read-ahead cap while the first acknowledged command is pending.
    for sequence in 4..=68 {
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
    if slow_database {
        // Cross the old cancellation deadline with an acknowledged, uncommitted
        // SQL write. Keep the websocket alive until the deliberate lock releases.
        let release = tokio::time::sleep(Duration::from_millis(30100));
        tokio::pin!(release);
        loop {
            tokio::select! {
                _ = &mut release => break,
                message = ws.next() => {
                    let message = message.unwrap().unwrap();
                    if message.is_text() {
                        let packet: Value = serde_json::from_str(message.as_text().unwrap()).unwrap();
                        if packet["op"] == 1 {
                            ws.send(Message::text("{\"op\":11,\"d\":null}".to_owned())).await.unwrap();
                        }
                    }
                }
            }
        }
        lock.take().unwrap().rollback().await.unwrap();
    }
    wait_sequence(&db.store, 68).await;
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
    // Replayed committed dispatches must not send duplicate callbacks or effects.
    ws.send(Message::text(interaction(3, "going").to_string()))
        .await
        .unwrap();
    ws.send(Message::text(leave(69).to_string())).await.unwrap();
    wait_sequence(&db.store, 69).await;
    assert_eq!(rest.requests().len(), 8);
    runner.abort();
    let _ = runner.await;
    drop(ws);
    rest.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn queued_rsvp_is_deferred_within_three_seconds_and_drains_on_overflow() {
    queued_commands(false).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn acknowledged_rsvp_survives_feature_deadline_and_backlog() {
    queued_commands(true).await;
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
