//! Opt-in database + gateway tests. Never inherit the runtime DATABASE_URL.
//! Only agent-testdb or CI's loopback service, as agent_test, is accepted.

#[path = "../tests/common/database_guard.rs"]
mod database_guard;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, LazyLock,
};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};
use tokio::task::JoinHandle;
use tokio_websockets::{Message, ServerBuilder};
use twilight_gateway::{ConfigBuilder, Intents, Shard, ShardId};
use two_bot_core::gateway_funnel::{FunnelBatch, GatewayFunnelBuffer};
use two_bot_core::gateway_session::{DispatchAction, GatewaySession, SESSION_MAX_AGE_MS};
use two_bot_core::{EventType, FunnelEvent, FunnelStore};
use two_bot_cutover::gateway_session::GatewaySessionStore;

use crate::gateway::{
    build_pipeline, ensure_crypto_provider, load_boot_session, run_shard, GatewayState,
};

mod commands;
mod custom_composition;
mod deadline;
mod force_identify;
mod member_journey;
mod onboarding;
mod persistent;
mod recovery;
mod rsvp;
mod voice;

const GUILD: &str = "2222";
const TOKEN: &str = "mock-token";

/// `GatewaySessionStore` serializes every checkpoint on
/// `pg_advisory_xact_lock(hashtextextended('gateway:{guild}:{shard}', 0))`.
/// An advisory lock belongs to the database, not to a schema, so all the
/// schema-isolated `TestDb`s of one process (same guild, same shard) contend
/// on a single key. A test that holds that key on purpose to block a
/// checkpoint therefore also stalls every sibling test's checkpoint past
/// `CHECKPOINT_IO_MAX`; the sibling's worker then panics "gateway checkpoint
/// failed" and the test hangs until its own deadline.
///
/// Ordinary tests hold this fence shared for the life of their `TestDb`; a test
/// that holds the checkpoint key (`TestDb::exclusive*`) holds it exclusively,
/// so it runs alone while the rest queue in `TestDb::new`. The serial CI step
/// (`--test-threads=1`) is unaffected.
static CHECKPOINT_KEY_FENCE: LazyLock<Arc<RwLock<()>>> = LazyLock::new(Arc::default);

/// Held only for its `Drop`.
#[allow(dead_code)]
enum CheckpointKeyFence {
    Shared(OwnedRwLockReadGuard<()>),
    Exclusive(OwnedRwLockWriteGuard<()>),
}

struct TestDb {
    pool: PgPool,
    admin: PgPool,
    schema: String,
    store: GatewaySessionStore,
    // Declared last: released only after `close` has dropped the schema.
    _fence: CheckpointKeyFence,
}

impl TestDb {
    async fn new() -> Self {
        Self::with_pool_max(3).await
    }

    async fn with_pool_max(pool_max: u32) -> Self {
        let fence = CheckpointKeyFence::Shared(CHECKPOINT_KEY_FENCE.clone().read_owned().await);
        Self::create(pool_max, fence).await
    }

    /// For a test that takes `gateway:{GUILD}:0` itself; see [`CHECKPOINT_KEY_FENCE`].
    async fn exclusive() -> Self {
        Self::exclusive_with_pool_max(3).await
    }

    async fn exclusive_with_pool_max(pool_max: u32) -> Self {
        let fence = CheckpointKeyFence::Exclusive(CHECKPOINT_KEY_FENCE.clone().write_owned().await);
        Self::create(pool_max, fence).await
    }

    async fn create(pool_max: u32, fence: CheckpointKeyFence) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let options = database_guard::test_options();
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await
            .expect("agent test database");
        let schema = format!(
            "gateway_test_{}_{}_{}",
            std::process::id(),
            two_bot_core::funnel::now_millis_for_test(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        // Identifiers contain only this fixed prefix and generated numeric IDs.
        // Source: https://docs.rs/sqlx/0.9.0/sqlx/struct.AssertSqlSafe.html
        assert!(schema
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_'));
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .expect("create isolated schema");
        let pool = PgPoolOptions::new()
            .max_connections(pool_max)
            .connect_with(options.options([("search_path", schema.clone())]))
            .await
            .expect("scoped test pool");
        sqlx::migrate!("../cutover/migrations")
            .run(&pool)
            .await
            .expect("migrations");
        let store = GatewaySessionStore::new(pool.clone(), GUILD.to_owned(), 0);
        Self {
            pool,
            admin,
            schema,
            store,
            _fence: fence,
        }
    }

    async fn independent_pool(&self, pool_max: u32) -> PgPool {
        PgPoolOptions::new()
            .max_connections(pool_max)
            .connect_with(
                database_guard::test_options().options([("search_path", self.schema.clone())]),
            )
            .await
            .expect("independent scoped test pool")
    }

    async fn close(self) {
        self.pool.close().await;
        assert!(self
            .schema
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_'));
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA IF EXISTS {}_web_v1 CASCADE; DROP SCHEMA {} CASCADE",
            self.schema, self.schema
        )))
        .execute(&self.admin)
        .await
        .expect("drop own test schema");
        self.admin.close().await;
    }

    async fn count(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM events")
            .fetch_one(&self.pool)
            .await
            .expect("row count")
    }
}

fn checkpoint(id: &str, seq: u64, url: &str) -> GatewaySession {
    GatewaySession {
        session_id: id.into(),
        sequence: seq,
        resume_url: url.into(),
        updated_at_ms: two_bot_core::funnel::now_millis_for_test(),
    }
}

fn event(kind: EventType, at: &str) -> FunnelEvent {
    FunnelEvent {
        guild_id: 2222,
        member_id: Some(77),
        event_type: kind,
        occurred_at: at.into(),
        source: "gateway".into(),
        metadata: None,
        dedupe_token: None,
    }
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn gateway_transaction_rolls_back_effects_and_sequence_and_rejects_replay() {
    let db = TestDb::new().await;
    let first = event(EventType::FirstMessage, "2026-09-29T12:00:00.000Z");
    let saved = checkpoint("mock-session", 42, "ws://mock");
    db.store
        .commit_dispatch(
            &saved,
            FunnelBatch {
                events: vec![first.clone()],
                ..Default::default()
            },
        )
        .await
        .expect("commit");
    assert_eq!(db.count().await, 1);
    let mut changed_timestamp = event(EventType::MemberLeave, "2026-09-29T12:00:01.000Z");
    assert_eq!(
        db.store
            .commit_dispatch(
                &saved,
                FunnelBatch {
                    events: vec![changed_timestamp.clone()],
                    ..Default::default()
                }
            )
            .await
            .expect("duplicate"),
        DispatchAction::Duplicate
    );
    assert_eq!(db.count().await, 1);
    let hydrated =
        GatewayFunnelBuffer::from_milestones(db.store.milestones().await.expect("milestones"));
    assert_eq!(
        hydrated.next_message_rung(2222, 77, "2026-09-29T12:00:01.000Z"),
        Some(EventType::SecondMessage)
    );
    changed_timestamp.occurred_at = "not-a-timestamp".into();
    let bad = checkpoint("mock-session", 43, "ws://mock");
    let batch = FunnelBatch {
        events: vec![
            event(EventType::SecondMessage, "2026-09-29T12:00:02.000Z"),
            changed_timestamp,
        ],
        ..Default::default()
    };
    assert!(db.store.commit_dispatch(&bad, batch).await.is_err());
    assert_eq!(
        db.store
            .load()
            .await
            .expect("load")
            .expect("session")
            .sequence,
        42
    );
    assert_eq!(db.count().await, 1);
    // Same-session sequence regression is ignored; a fresh session may reset.
    assert_eq!(
        db.store
            .commit_dispatch(
                &checkpoint("mock-session", 41, "ws://mock"),
                FunnelBatch::default()
            )
            .await
            .expect("regression"),
        DispatchAction::Duplicate
    );
    db.store
        .commit_dispatch(
            &checkpoint("new-session", 1, "ws://mock"),
            FunnelBatch::default(),
        )
        .await
        .expect("reset");
    assert_eq!(
        db.store
            .load()
            .await
            .expect("load")
            .expect("session")
            .sequence,
        1
    );
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn gateway_stale_session_is_cleared_before_boot() {
    let db = TestDb::new().await;
    let mut saved = checkpoint("expired", 42, "ws://mock");
    saved.updated_at_ms -= SESSION_MAX_AGE_MS + 1;
    db.store
        .commit_dispatch(&saved, FunnelBatch::default())
        .await
        .expect("save stale");
    assert!(load_boot_session(&db.store).await.expect("boot").is_none());
    assert!(db.store.load().await.expect("load").is_none());
    db.close().await;
}

fn ready(url: &str, session: &str) -> Value {
    json!({"op":0,"s":1,"t":"READY","d":{
        "v":10,"user":{"id":"999","username":"mock-bot","discriminator":"0","mfa_enabled":false},
        "session_id":session,"resume_gateway_url":url,"guilds":[],"application":{"id":"1111","flags":0}
    }})
}

fn leave(seq: u64) -> Value {
    json!({"op":0,"s":seq,"t":"GUILD_MEMBER_REMOVE","d":{
        "guild_id":GUILD,"user":{"id":"77","username":"mock-member","discriminator":"0"}
    }})
}

struct MockGateway {
    url: String,
    auth: mpsc::Receiver<Value>,
    task: JoinHandle<()>,
}

impl MockGateway {
    /// Two connections in invalid-session mode: reject RESUME with opcode 9,
    /// then accept IDENTIFY. Otherwise run the supplied dispatch script.
    async fn new(invalid: bool, resume: bool) -> Self {
        Self::with_close(invalid, resume, None).await
    }

    async fn with_close(invalid: bool, resume: bool, close: Option<u16>) -> Self {
        let invalid = invalid || close.is_some();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("mock listen");
        let url = format!("ws://{}", listener.local_addr().expect("address"));
        let (sender, auth) = mpsc::channel(4);
        let gateway_url = url.clone();
        let task = tokio::spawn(async move {
            for connection in 0..if invalid { 2 } else { 1 } {
                let (stream, _) = listener.accept().await.expect("mock accept");
                let (_, mut ws) = ServerBuilder::new()
                    .accept(stream)
                    .await
                    .expect("websocket");
                ws.send(Message::text(
                    json!({"op":10,"d":{"heartbeat_interval":45000}}).to_string(),
                ))
                .await
                .expect("hello");
                while let Some(Ok(message)) = ws.next().await {
                    if !message.is_text() {
                        continue;
                    }
                    let packet: Value =
                        serde_json::from_str(message.as_text().expect("text")).expect("packet");
                    match packet["op"].as_u64() {
                        Some(1) => ws
                            .send(Message::text("{\"op\":11,\"d\":null}".to_owned()))
                            .await
                            .expect("ack"),
                        Some(2 | 6) => {
                            sender.send(packet).await.expect("auth capture");
                            if invalid && connection == 0 {
                                let rejection = match close {
                                    Some(code) => Message::close(
                                        Some(tokio_websockets::CloseCode::try_from(code).unwrap()),
                                        "invalid session",
                                    ),
                                    None => Message::text("{\"op\":9,\"d\":false}".to_owned()),
                                };
                                ws.send(rejection).await.expect("invalid session");
                                // Wait for Twilight's normal-close response before
                                // accepting its IDENTIFY reconnect.
                                while let Some(Ok(message)) = ws.next().await {
                                    if message.is_close() {
                                        break;
                                    }
                                }
                                break;
                            }
                            if resume {
                                // Same dispatch replayed after restart: its original
                                // generated timestamp is unavailable to the new process.
                                ws.send(Message::text(leave(2).to_string()))
                                    .await
                                    .expect("replayed leave");
                                ws.send(Message::text(
                                    json!({"op":0,"s":3,"t":"RESUMED","d":{}}).to_string(),
                                ))
                                .await
                                .expect("resumed");
                            } else {
                                ws.send(Message::text(
                                    ready(&gateway_url, "fresh-session").to_string(),
                                ))
                                .await
                                .expect("ready");
                                ws.send(Message::text(leave(2).to_string()))
                                    .await
                                    .expect("leave");
                            }
                        }
                        _ => {}
                    }
                }
            }
        });
        Self { url, auth, task }
    }

    async fn authentication(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(20), self.auth.recv())
            .await
            .expect("auth deadline")
            .expect("auth")
    }
}

async fn wait_sequence(store: &GatewaySessionStore, sequence: u64) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if store
                .load()
                .await
                .expect("load")
                .is_some_and(|saved| saved.sequence == sequence)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("checkpoint deadline");
}

/// Fence wait for tests whose later dispatches commit back-to-back with the
/// awaited one: a 10 ms poller can miss an exact sequence when the worker
/// commits the next dispatch microseconds later, so waiting for at least the
/// sequence asserts the cursor fenced past it without the skip race.
async fn wait_sequence_at_least(store: &GatewaySessionStore, sequence: u64) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if store
                .load()
                .await
                .expect("load")
                .is_some_and(|saved| saved.sequence >= sequence)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("checkpoint deadline");
}

// A visible checkpoint precedes the runner's in-memory readiness update.
async fn wait_connected(state: &RwLock<GatewayState>) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while *state.read().await != GatewayState::Connected {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("gateway connected deadline");
}

#[tokio::test(start_paused = true)]
async fn readiness_wait_requires_connected_state() {
    let state = Arc::new(RwLock::new(GatewayState::Armed));
    let waiting_state = state.clone();
    let waiting = tokio::spawn(async move { wait_connected(&waiting_state).await });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(50)).await;
    assert!(!waiting.is_finished(), "Armed is not ready");
    *state.write().await = GatewayState::Connected;
    tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .expect("connected state observed")
        .expect("readiness waiter");
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "gateway connected deadline")]
async fn readiness_wait_has_a_bounded_deadline() {
    wait_connected(&RwLock::new(GatewayState::Armed)).await;
}

async fn spawn_runner(
    db: &TestDb,
    url: &str,
) -> (
    JoinHandle<Result<(), sqlx::Error>>,
    Arc<RwLock<GatewayState>>,
) {
    spawn_runner_until_shutdown(db, url, None, std::future::pending()).await
}

async fn spawn_runner_with_commands(
    db: &TestDb,
    url: &str,
    commands: Option<Arc<crate::command_runtime::CommandRuntime>>,
) -> (
    JoinHandle<Result<(), sqlx::Error>>,
    Arc<RwLock<GatewayState>>,
) {
    spawn_runner_until_shutdown(db, url, commands, std::future::pending()).await
}

async fn spawn_runner_until_shutdown(
    db: &TestDb,
    url: &str,
    commands: Option<Arc<crate::command_runtime::CommandRuntime>>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> (
    JoinHandle<Result<(), sqlx::Error>>,
    Arc<RwLock<GatewayState>>,
) {
    ensure_crypto_provider();
    let saved = load_boot_session(&db.store)
        .await
        .expect("load boot session");
    // Shard::with_config consumes session/resume_url out of Config, so inject
    // the mock bootstrap URL before construction, using the production factory.
    let config = crate::gateway::build_shard_config(TOKEN.into(), Intents::empty(), saved.as_ref());
    let builder = ConfigBuilder::from(config).proxy_url(url.to_owned());
    let shard = Shard::with_config(ShardId::ONE, builder.build());
    let pipeline = Arc::new(build_pipeline(
        db.store.milestones().await.expect("milestones"),
        None,
    ));
    let state = Arc::new(RwLock::new(GatewayState::Armed));
    let task = tokio::spawn(run_shard(
        shard,
        pipeline,
        state.clone(),
        db.store.clone(),
        None,
        None,
        commands,
        None,
        None,
        shutdown,
    ));
    (task, state)
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn http_shutdown_stops_the_real_gateway_runner_and_preserves_checkpoint() {
    let db = TestDb::new().await;
    let mut mock = MockGateway::new(false, false).await;
    let (shutdown, mut stopping) = tokio::sync::watch::channel(false);
    let (runner, state) = spawn_runner_until_shutdown(&db, &mock.url, None, async move {
        stopping.wait_for(|stopping| *stopping).await.unwrap();
    })
    .await;
    assert_eq!(mock.authentication().await["op"], 2);
    wait_sequence(&db.store, 2).await;
    assert_eq!(*state.read().await, GatewayState::Connected);
    tokio::time::timeout(
        Duration::from_secs(2),
        crate::supervise_gateway(runner, async { Ok(()) }, state.clone(), shutdown),
    )
    .await
    .expect("production runner must receive shutdown and join its writer")
    .unwrap();
    assert_eq!(*state.read().await, GatewayState::Draining);
    assert_eq!(db.store.load().await.unwrap().unwrap().sequence, 2);
    assert_eq!(db.count().await, 1);
    mock.task.abort();
    let _ = mock.task.await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn pending_readyz_request_observes_drain_after_database_acquisition() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt as _;

    let db = TestDb::new().await;
    let gateway = Arc::new(RwLock::new(GatewayState::Connected));
    let app = crate::server::router(crate::server::SharedState::new(
        gateway.clone(),
        Some(db.pool.clone()),
    ));
    // Hold every connection so the real ping waits in pool acquisition.
    let mut held = Vec::new();
    for _ in 0..db.pool.options().get_max_connections() {
        held.push(db.pool.acquire().await.unwrap());
    }
    let response = app.oneshot(
        Request::builder()
            .uri("/readyz")
            .body(Body::empty())
            .unwrap(),
    );
    futures_util::pin_mut!(response);
    assert!(futures_util::poll!(&mut response).is_pending());
    *gateway.write().await = GatewayState::Draining;
    drop(held);
    let response = tokio::time::timeout(Duration::from_secs(2), response)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn gateway_resume_after_restart_has_no_duplicate_funnel_rows_and_is_ready() {
    let db = TestDb::new().await;
    let mut first = MockGateway::new(false, false).await;
    let (runner, _) = spawn_runner(&db, &first.url).await;
    let auth = first.authentication().await;
    assert_eq!(auth["op"], 2);
    wait_sequence(&db.store, 2).await;
    assert_eq!(db.count().await, 1);
    runner.abort();
    let _ = runner.await;
    first.task.abort();
    // New pipeline and shard represent a new Container process. Repoint the
    // mock-only URL because each local listener receives a new ephemeral port.
    let mut second = MockGateway::new(false, true).await;
    let mut saved = db.store.load().await.expect("load").expect("session");
    // Preserve seq but update only the mock endpoint, not through commit_dispatch.
    saved.resume_url = second.url.clone();
    sqlx::query("UPDATE gateway_sessions SET resume_url = $1")
        .bind(&saved.resume_url)
        .execute(&db.pool)
        .await
        .expect("mock URL");
    let (runner, state) = spawn_runner(&db, "ws://127.0.0.1:1").await;
    let auth = second.authentication().await;
    assert_eq!(auth["op"], 6);
    assert_eq!(auth["d"]["seq"], 2);
    assert_eq!(auth["d"]["session_id"], "fresh-session");
    wait_sequence(&db.store, 3).await;
    assert_eq!(db.count().await, 1);
    wait_connected(&state).await;
    runner.abort();
    let _ = runner.await;
    second.task.abort();
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn gateway_invalid_session_falls_back_to_identify_and_replaces_checkpoint() {
    let db = TestDb::new().await;
    let mut mock = MockGateway::new(true, false).await;
    db.store
        .commit_dispatch(
            &checkpoint("invalid-session", 42, &mock.url),
            FunnelBatch::default(),
        )
        .await
        .expect("seed");
    let (runner, state) = spawn_runner(&db, &mock.url).await;
    let resume = mock.authentication().await;
    assert_eq!(resume["op"], 6);
    assert_eq!(resume["d"]["seq"], 42);
    let identify = mock.authentication().await;
    assert_eq!(identify["op"], 2);
    wait_sequence(&db.store, 2).await;
    assert_eq!(
        db.store
            .load()
            .await
            .expect("load")
            .expect("checkpoint")
            .session_id,
        "fresh-session"
    );
    wait_connected(&state).await;
    runner.abort();
    let _ = runner.await;
    mock.task.abort();
    db.close().await;
}
