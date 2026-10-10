#![cfg(test)]
//! Regression coverage for queued defers, accepted-work drain and RESUMED sync.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use two_bot_core::{
    send_admission::{PgSendAdmission, SendAdmission},
    ClassifierConfig, InteractionRouter, RouterGates,
};
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
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

fn runtime(db: &TestDb, rest: &MockRest) -> Arc<InteractionRuntime> {
    Arc::new(InteractionRuntime::with_router(
        InteractionRouter::new(gates()),
        db.pool.clone(),
        ActionExecutor::with_proxy(TOKEN.into(), Some(rest.origin())).unwrap(),
        0,
        ClassifierConfig::default(),
    ))
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
        None,
        Some(crate::command_runtime::CommandRuntime::new(
            db.pool.clone(),
            ActionExecutor::with_proxy(TOKEN.into(), Some(rest.origin())).unwrap(),
            crate::command_runtime::router_with_commands(gates()),
            GUILD.parse().unwrap(),
            true,
        )),
        None,
        None,
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

/// Production-shaped ingress: the ordered runtime and the command runtime send
/// through the same durable single-flight lane, so a held lane exercises the
/// receipt-callback retry instead of the ungoverned loopback path.
async fn spawn_governed(
    db: &TestDb,
    url: &str,
    rest: &MockRest,
    token: &str,
    shutdown: Option<tokio::sync::watch::Receiver<bool>>,
) -> JoinHandle<Result<(), sqlx::Error>> {
    ensure_crypto_provider();
    // One lane row per test: the admission table is database-wide and holds no
    // expiry, so a leaked permit under one token would fence every later
    // governed test that shares it.
    let admission: Arc<dyn SendAdmission> =
        Arc::new(PgSendAdmission::new(db.pool.clone(), token).unwrap());
    let saved = load_boot_session(&db.store).await.unwrap();
    let shard =
        crate::gateway::build_shard(TOKEN.into(), Intents::empty(), saved.as_ref(), Some(url));
    let ordered = Arc::new(InteractionRuntime::with_router(
        InteractionRouter::new(gates()),
        db.pool.clone(),
        ActionExecutor::with_admission(token.into(), Some(rest.origin()), Arc::clone(&admission))
            .unwrap(),
        0,
        ClassifierConfig::default(),
    ));
    tokio::spawn(run_shard(
        shard,
        Arc::new(build_pipeline(db.store.milestones().await.unwrap(), None)),
        Arc::new(RwLock::new(GatewayState::Armed)),
        db.store.clone(),
        Some(ordered),
        None,
        Some(crate::command_runtime::CommandRuntime::new(
            db.pool.clone(),
            ActionExecutor::with_admission(token.into(), Some(rest.origin()), admission).unwrap(),
            crate::command_runtime::router_with_commands(gates()),
            GUILD.parse().unwrap(),
            true,
        )),
        None,
        None,
        async move {
            match shutdown {
                Some(receiver) => crate::server::shutdown_requested(receiver).await,
                None => std::future::pending().await,
            }
        },
    ))
}

async fn connect_governed(
    db: &TestDb,
    rest: &MockRest,
    token: &str,
    shutdown: Option<tokio::sync::watch::Receiver<bool>>,
) -> (
    JoinHandle<Result<(), sqlx::Error>>,
    tokio_websockets::WebSocketStream<tokio::net::TcpStream>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let runner = spawn_governed(db, &url, rest, token, shutdown).await;
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

/// Route-aware mock that holds the send lane on the first live-event read: the
/// delayed response keeps the request in flight, so the shared single-flight
/// lane stays occupied until the hold elapses. Sets `seen` the moment the held
/// read arrives, so the test can send the next command into certain occupancy.
/// Later reads pass through immediately.
async fn lane_holding_rest(seen: Arc<AtomicBool>, hold: Duration) -> MockRest {
    let held = Arc::new(AtomicBool::new(false));
    MockRest::with_responder(move |request| {
        if request.method == "GET" && request.path.contains("scheduled-events") {
            seen.store(true, Ordering::Release);
            if !held.swap(true, Ordering::AcqRel) {
                return ScriptedResponse::json(
                    200,
                    json!({"id":EVENT,"guild_id":GUILD,"status":1}),
                )
                .delayed(hold);
            }
            return ScriptedResponse::json(200, json!({"id":EVENT,"guild_id":GUILD,"status":1}));
        }
        if request.method == "GET" {
            // RA-01 live-membership gate echoes the looked-up user id back
            // inside `user.id`: answer member reads with that shape so fenced
            // commands pass. Every other GET (boot application id, guild
            // reads) keeps the legacy bare-id body its consumer parses.
            if request.path.contains("/members/") {
                let user = request.path.rsplit('/').next().unwrap_or_default();
                return ScriptedResponse::json(200, json!({"user": {"id": user}, "roles": []}));
            }
            return ScriptedResponse::json(200, json!({"id":"1111"}));
        }
        if request.method == "PUT" {
            return ScriptedResponse::status(200);
        }
        if request.path.ends_with("/callback") {
            return ScriptedResponse::status(204);
        }
        // Deferred original edits require ID-bearing 200 receipts.
        ScriptedResponse::json(200, json!({"id":"99"}))
    })
    .await
}

async fn wait_flag(flag: &Arc<AtomicBool>) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !flag.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("lane hold never started");
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
    // Route-aware responder: the first live-event read is held (or instant
    // for the slow-database case) while later reads pass through, and
    // membership reads echo the RA-01 `user.id` evidence. A strict FIFO
    // script cannot survive the concurrent defer/execute interleave, so the
    // exact per-RSVP call sequence is pinned in the discord runtime tests
    // instead; here only arrival order and timing are asserted.
    let rest = lane_holding_rest(
        Arc::new(AtomicBool::new(false)),
        if slow_database {
            Duration::ZERO
        } else {
            Duration::from_millis(3200)
        },
    )
    .await;
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let (runner, mut ws) = connect_with_shutdown(&db, &rest, Some(receiver)).await;
    ws.send(Message::text(interaction(2, "going").to_string()))
        .await
        .unwrap();
    wait_requests(&rest, 4).await; // First defer and live-event read are underway.
    let delivered = tokio::time::Instant::now();
    ws.send(Message::text(interaction(3, "interested").to_string()))
        .await
        .unwrap();
    // Overflow is fatal under the shared dispatcher. Error/shutdown cases stay
    // below capacity so their original cause, not overflow, controls the drain.
    // The graceful case sends only the two commands: both dispatches are already
    // funneled before shutdown, so the drain must commit both deterministically.
    let last = if overflow {
        68
    } else if graceful_shutdown {
        3
    } else {
        6
    };
    for sequence in 4..=last {
        ws.send(Message::text(leave(sequence).to_string()))
            .await
            .unwrap();
    }
    wait_requests(&rest, 5).await;
    let requests = rest.requests();
    // Index 5: boot application id, registry PUT, first defer, pre-write
    // membership read, held event read, then the second command's defer.
    assert!(requests[5]
        .path
        .ends_with("/interactions/3/mock-rsvp-3/callback"));
    assert!(requests[5].received_at.duration_since(delivered) < Duration::from_secs(3));
    assert_eq!(
        serde_json::from_slice::<Value>(&requests[5].body).unwrap()["type"],
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
        // The in-flight command commits and already-funneled accepted commands
        // drain through cooperative shutdown; only a fatal worker error stops
        // the writer from admitting buffered funnel work.
        assert_eq!(db.store.load().await.unwrap().unwrap().sequence, 3);
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
        if slow_database {
            assert_eq!(db.count().await, 0);
        } else {
            // Fatal backlog drain commits queued funnel work before returning:
            // leaves received before overflow are processed, so the events
            // table holds their collapsed same-member rows instead of zero.
            // The I/O-deadline sibling above still expects zero (no drain).
            assert!(db.count().await > 0);
        }
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
    // Route-aware responder: the first live-event read is held 3.2 s while
    // later reads pass through, membership reads echo the RA-01 `user.id`
    // evidence, and callbacks/edits get wire-correct receipts regardless of
    // the sticky/feed defer interleave.
    let rest = lane_holding_rest(
        Arc::new(AtomicBool::new(false)),
        Duration::from_millis(3200),
    )
    .await;
    let (runner, mut ws) = connect(&db, &rest).await;
    ws.send(Message::text(interaction(2, "going").to_string()))
        .await
        .unwrap();
    wait_requests(&rest, 4).await;
    let delivered = tokio::time::Instant::now();
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
    // Twelve: boot application id + registry PUT, defer, pre-write
    // membership/event reads, both validation defers with edits, then the
    // fence re-reads and completion edit. No second dispatch at consumption.
    assert_eq!(rest.requests().len(), 12); // No second dispatch at queue consumption.
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
    assert_eq!(rest.requests().len(), 12);
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

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn receipt_callback_waits_out_brief_lane_occupancy() {
    // Bound every unbounded await below: a wedge must fail loudly with a
    // message instead of burning the CI budget with zero output.
    let db = tokio::time::timeout(Duration::from_secs(120), TestDb::new())
        .await
        .expect("test database setup deadline");
    let seen = Arc::new(AtomicBool::new(false));
    // A's live-event read holds the single-flight lane for 1.5 s: inside B's
    // 2.5 s retry budget and Discord's three-second acknowledgement window.
    let rest = lane_holding_rest(Arc::clone(&seen), Duration::from_millis(1500)).await;
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let (runner, mut ws) = tokio::time::timeout(
        Duration::from_secs(120),
        connect_governed(&db, &rest, "mock-token-waits", Some(receiver)),
    )
    .await
    .expect("governed gateway connect deadline");
    tokio::time::timeout(
        Duration::from_secs(30),
        ws.send(Message::text(interaction(2, "going").to_string())),
    )
    .await
    .expect("gateway dispatch send deadline")
    .unwrap();
    // B is sent into certain occupancy: A's read has reached the mock, so the
    // lane stays held until the delayed response lands.
    wait_flag(&seen).await;
    let delivered = tokio::time::Instant::now();
    tokio::time::timeout(
        Duration::from_secs(30),
        ws.send(Message::text(interaction(3, "interested").to_string())),
    )
    .await
    .expect("gateway dispatch send deadline")
    .unwrap();
    wait_sequence(&db.store, 3).await;
    let requests = rest.requests();
    let callbacks: Vec<_> = requests
        .iter()
        .filter(|request| {
            request
                .path
                .ends_with("/interactions/3/mock-rsvp-3/callback")
        })
        .collect();
    // Exactly one callback reached the wire: every earlier attempt met the held
    // lane and backed off instead of failing B outright.
    assert_eq!(callbacks.len(), 1);
    let waited = callbacks[0].received_at.duration_since(delivered);
    assert!(waited > Duration::from_millis(1000), "{waited:?}");
    assert!(waited < Duration::from_secs(3), "{waited:?}");
    // Ordered effects still execute once, in gateway dispatch order.
    let edits: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "PATCH" && request.path.contains("@original"))
        .collect();
    assert_eq!(edits.len(), 2);
    assert!(edits[0].path.contains("mock-rsvp-2"));
    assert!(edits[1].path.contains("mock-rsvp-3"));
    assert_eq!(
        serde_json::from_slice::<Value>(&edits[0].body).unwrap()["content"],
        "RSVP saved: going."
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&edits[1].body).unwrap()["content"],
        "RSVP saved: interested."
    );
    let status: String = tokio::time::timeout(
        Duration::from_secs(60),
        sqlx::query_scalar("SELECT status FROM event_rsvps").fetch_one(&db.pool),
    )
    .await
    .expect("rsvp status read deadline")
    .unwrap();
    assert_eq!(status, "interested");
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(60),
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM announcements_audit_log")
                .fetch_one(&db.pool),
        )
        .await
        .expect("audit count read deadline")
        .unwrap(),
        2
    );
    shutdown.send_replace(true);
    // Bound the shutdown drain: a wedged runner must fail loudly with a
    // message instead of burning the CI budget with zero output.
    tokio::time::timeout(Duration::from_secs(30), runner)
        .await
        .expect("governed runner shutdown deadline")
        .unwrap()
        .unwrap();
    drop(ws);
    rest.shutdown().await;
    // Bound teardown likewise: dropping the schema must not wait forever.
    tokio::time::timeout(Duration::from_secs(30), db.close())
        .await
        .expect("test schema teardown deadline");
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn exhausted_lane_hold_fences_checkpoint_past_unacked_command() {
    let db = TestDb::new().await;
    let seen = Arc::new(AtomicBool::new(false));
    // Past B's 2.5 s retry budget but inside the 5 s wire timeout, so A's read
    // still completes while only B's acknowledgement is lost. The loss is
    // warn-and-advance, never fatal: holding the cursor could not recover the
    // command, and failing the worker would turn one lost callback into a
    // process-wide outage.
    let rest = lane_holding_rest(Arc::clone(&seen), Duration::from_millis(3500)).await;
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let (runner, mut ws) =
        connect_governed(&db, &rest, "mock-token-exhausted", Some(receiver)).await;
    ws.send(Message::text(interaction(2, "going").to_string()))
        .await
        .unwrap();
    wait_flag(&seen).await;
    let delivered = tokio::time::Instant::now();
    ws.send(Message::text(interaction(3, "interested").to_string()))
        .await
        .unwrap();
    // The runner keeps going and the checkpoint advances past B: B's loss
    // waited out the retry budget instead of failing fast.
    wait_sequence(&db.store, 3).await;
    assert!(delivered.elapsed() >= Duration::from_secs(2));
    // A committed and B advanced past with no callback and no store effect.
    assert_eq!(db.store.load().await.unwrap().unwrap().sequence, 3);
    let status: String = sqlx::query_scalar("SELECT status FROM event_rsvps")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(status, "going");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM announcements_audit_log")
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        1
    );
    // B never reached the wire: no callback and no original-message edit.
    let requests = rest.requests();
    assert!(
        requests
            .iter()
            .all(|request| !request.path.contains("mock-rsvp-3")),
        "{}",
        requests.len()
    );
    shutdown.send_replace(true);
    // Bound the shutdown drain: a wedged runner must fail loudly with a
    // message instead of burning the CI budget with zero output.
    tokio::time::timeout(Duration::from_secs(30), runner)
        .await
        .expect("governed runner shutdown deadline")
        .unwrap()
        .unwrap();
    drop(ws);
    rest.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn governed_sticky_defers_through_rsvp_lane_hold() {
    let db = TestDb::new().await;
    let seen = Arc::new(AtomicBool::new(false));
    // A's live-event read holds the single-flight lane for 1.5 s while a
    // sticky command arrives through the shared governed gate: the sticky
    // defer must wait out the occupancy instead of failing pre-wire.
    let rest = lane_holding_rest(Arc::clone(&seen), Duration::from_millis(1500)).await;
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let (mut runner, mut ws) =
        connect_governed(&db, &rest, "mock-token-sticky", Some(receiver)).await;
    ws.send(Message::text(interaction(2, "going").to_string()))
        .await
        .unwrap();
    wait_flag(&seen).await;
    let delivered = tokio::time::Instant::now();
    let mut sticky = interaction(3, "going");
    sticky["d"]["member"]["permissions"] = json!("32");
    sticky["d"]["data"]["name"] = json!("sticky");
    sticky["d"]["data"]["options"] = json!([]);
    ws.send(Message::text(sticky.to_string())).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let requests = rest.requests();
            let has_callback = requests.iter().any(|request| {
                request
                    .path
                    .ends_with("/interactions/3/mock-rsvp-3/callback")
            });
            let has_edit = requests.iter().any(|request| {
                request.method == "PATCH"
                    && request.path.contains("mock-rsvp-3")
                    && request.path.contains("@original")
            });
            if has_callback && has_edit {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("sticky defer/edit deadline");
    let requests = rest.requests();
    let callbacks: Vec<_> = requests
        .iter()
        .filter(|request| {
            request
                .path
                .ends_with("/interactions/3/mock-rsvp-3/callback")
        })
        .collect();
    // Exactly one defer reached the wire inside Discord's acknowledgement
    // window, even though the lane was held for most of that window.
    assert_eq!(callbacks.len(), 1);
    assert!(
        callbacks[0].received_at.duration_since(delivered) < Duration::from_secs(3),
        "{:?}",
        callbacks[0].received_at.duration_since(delivered)
    );
    let body: Value = serde_json::from_slice(&callbacks[0].body).unwrap();
    assert_eq!(body["type"], 5);
    assert_eq!(body["data"]["flags"], 64);
    let edits: Vec<_> = requests
        .iter()
        .filter(|request| {
            request.method == "PATCH"
                && request.path.contains("mock-rsvp-3")
                && request.path.contains("@original")
        })
        .collect();
    assert_eq!(edits.len(), 1);
    // The RSVP that held the lane still completes exactly once. Poll for
    // its completion edit: it lands after the same lane hold releases, so a
    // snapshot taken at the sticky edit may not contain it yet.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let done = rest.requests().iter().any(|request| {
                request.method == "PATCH"
                    && request.path.contains("mock-rsvp-2")
                    && request.path.contains("@original")
            });
            if done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("rsvp completion deadline");
    // Surface a quiet runner death with its error instead of timing out: the
    // runner owns the only copy of a checkpoint-hold or I/O failure. The
    // fence is at-least: the worker can commit the sticky dispatch
    // microseconds after the RSVP one, skipping an exact-sequence poll.
    tokio::select! {
        _ = wait_sequence_at_least(&db.store, 2) => {},
        result = &mut runner => {
            panic!("governed runner exited before sequence 2: {result:?}");
        }
    }
    let requests = rest.requests();
    let rsvp_edits: Vec<_> = requests
        .iter()
        .filter(|request| {
            request.method == "PATCH"
                && request.path.contains("mock-rsvp-2")
                && request.path.contains("@original")
        })
        .collect();
    assert_eq!(rsvp_edits.len(), 1);
    assert_eq!(
        serde_json::from_slice::<Value>(&rsvp_edits[0].body).unwrap()["content"],
        "RSVP saved: going."
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM announcements_audit_log")
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        1
    );
    // Both the RSVP and the sticky defer complete through the ordered drain
    // (the drain holds the cursor only on admission-Blocked exhaustion, and
    // every lane wait here stays inside the receipt budget), so the cursor
    // advances past both and the runner shuts down cleanly.
    shutdown.send_replace(true);
    tokio::time::timeout(Duration::from_secs(30), runner)
        .await
        .expect("governed runner shutdown deadline")
        .unwrap()
        .unwrap();
    drop(ws);
    rest.shutdown().await;
    // Bound teardown likewise: dropping the schema must not wait forever.
    tokio::time::timeout(Duration::from_secs(30), db.close())
        .await
        .expect("test schema teardown deadline");
}

/// Occupancy-aware responder: the first and third live-event reads hold the
/// single-flight lane with a delayed response; every other read passes
/// through immediately. `seen` fires on each held read so the test can send
/// the next operation into certain occupancy.
async fn handoff_rest(seen: Arc<AtomicBool>) -> MockRest {
    use std::sync::atomic::AtomicUsize;
    let calls = Arc::new(AtomicUsize::new(0));
    MockRest::with_responder(move |request| {
        if request.method == "GET" && request.path.contains("scheduled-events") {
            seen.store(true, Ordering::Release);
            let call = calls.fetch_add(1, Ordering::AcqRel);
            if call == 0 || call == 2 {
                return ScriptedResponse::json(
                    200,
                    json!({"id":EVENT,"guild_id":GUILD,"status":1}),
                )
                .delayed(Duration::from_millis(1500));
            }
            return ScriptedResponse::json(200, json!({"id":EVENT,"guild_id":GUILD,"status":1}));
        }
        if request.method == "PATCH" {
            return ScriptedResponse::json(200, json!({"id":"99"}));
        }
        ScriptedResponse::status(204)
    })
    .await
}

fn governed_executor(
    rest: &MockRest,
    admission: Arc<dyn SendAdmission>,
    token: &str,
) -> two_bot_discord::ActionExecutor {
    // The executor token must match the admission row's token key: main #117
    // binds the durable lane to one token, so a shared-token executor against
    // a per-test admission row fails closed with a token mismatch.
    ActionExecutor::with_admission(token.into(), Some(rest.origin()), admission).unwrap()
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn rsvp_lookup_and_completion_edit_wait_out_lane_hold() {
    let db = TestDb::new().await;
    let seen = Arc::new(AtomicBool::new(false));
    let rest = handoff_rest(Arc::clone(&seen)).await;
    let admission: Arc<dyn SendAdmission> =
        Arc::new(PgSendAdmission::new(db.pool.clone(), "mock-token-lookup").unwrap());
    let holder = governed_executor(&rest, Arc::clone(&admission), "mock-token-lookup");
    let worker = governed_executor(&rest, Arc::clone(&admission), "mock-token-lookup");
    // Occupy the lane with a first lookup whose delayed response holds it.
    let occupied = tokio::spawn(async move { holder.get_scheduled_event(GUILD, EVENT).await });
    wait_flag(&seen).await;
    // The second lookup must wait out the hold and return the event instead
    // of converting a pre-wire Blocked lane into a terminal validation error.
    let start = tokio::time::Instant::now();
    let event = worker.get_scheduled_event(GUILD, EVENT).await;
    let waited = start.elapsed();
    assert_eq!(
        event.unwrap(),
        Some(json!({"id":EVENT,"guild_id":GUILD,"status":1}))
    );
    assert!(waited >= Duration::from_millis(1000), "{waited:?}");
    assert!(waited < Duration::from_secs(3), "{waited:?}");
    assert_eq!(
        occupied.await.unwrap().unwrap(),
        Some(json!({"id":EVENT,"guild_id":GUILD,"status":1}))
    );
    // Same handoff shape for the completion PATCH: occupy the lane again and
    // require the final edit to wait instead of losing the RSVP reply.
    seen.store(false, Ordering::Release);
    let holder = governed_executor(&rest, Arc::clone(&admission), "mock-token-lookup");
    let occupied = tokio::spawn(async move { holder.get_scheduled_event(GUILD, EVENT).await });
    wait_flag(&seen).await;
    let start = tokio::time::Instant::now();
    worker
        .edit_interaction_response_with_blocked_retry(1111, "mock-rsvp-9", "RSVP saved: going.")
        .await
        .unwrap();
    let waited = start.elapsed();
    assert!(waited >= Duration::from_millis(1000), "{waited:?}");
    assert!(waited < Duration::from_secs(3), "{waited:?}");
    assert!(occupied.await.unwrap().unwrap().is_some());
    let snapshot = rest.requests();
    let patches: Vec<_> = snapshot
        .iter()
        .filter(|request| request.method == "PATCH")
        .collect();
    assert_eq!(patches.len(), 1);
    rest.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn receipt_callback_total_stays_inside_absolute_budget() {
    use twilight_model::{
        channel::message::MessageFlags,
        http::interaction::{
            InteractionResponse, InteractionResponseData, InteractionResponseType,
        },
    };
    let db = TestDb::new().await;
    let seen = Arc::new(AtomicBool::new(false));
    let seen_probe = Arc::clone(&seen);
    let rest = MockRest::with_responder(move |request| {
        if request.method == "GET" && request.path.contains("scheduled-events") {
            seen_probe.store(true, Ordering::Release);
            // Occupancy ends 100 ms before the receipt budget does, but the
            // admitted callback still needs 600 ms of transport.
            return ScriptedResponse::json(200, json!({"id":EVENT,"guild_id":GUILD,"status":1}))
                .delayed(Duration::from_millis(2400));
        }
        if request.path.ends_with("/callback") {
            return ScriptedResponse::status(204).delayed(Duration::from_millis(600));
        }
        ScriptedResponse::json(200, json!({"id":"99"}))
    })
    .await;
    let admission: Arc<dyn SendAdmission> =
        Arc::new(PgSendAdmission::new(db.pool.clone(), "mock-token-total").unwrap());
    let holder = governed_executor(&rest, Arc::clone(&admission), "mock-token-total");
    let worker = governed_executor(&rest, Arc::clone(&admission), "mock-token-total");
    let occupied = tokio::spawn(async move { holder.get_scheduled_event(GUILD, EVENT).await });
    wait_flag(&seen).await;
    let deferred = InteractionResponse {
        kind: InteractionResponseType::DeferredChannelMessageWithSource,
        data: Some(InteractionResponseData {
            flags: Some(MessageFlags::EPHEMERAL),
            ..Default::default()
        }),
    };
    let start = tokio::time::Instant::now();
    let result = worker
        .answer_interaction_with_blocked_retry(9, "mock-rsvp-9", &deferred)
        .await;
    let elapsed = start.elapsed();
    // The absolute budget refuses to overrun the receipt window: the attempt
    // admitted near the deadline is cut off instead of landing past it.
    assert!(
        result.is_err(),
        "callback unexpectedly succeeded past the window"
    );
    assert!(elapsed < Duration::from_millis(2900), "{elapsed:?}");
    assert!(occupied.await.unwrap().unwrap().is_some());
    rest.shutdown().await;
    db.close().await;
}
