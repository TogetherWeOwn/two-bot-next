#![cfg(test)]

//! Real websocket dispatch acceptance; no direct invocation of retention hooks
//! substitutes for queue/cancel assertions. Nested under voice to reuse its
//! no-network RoomWrites fixture without modifying any prior assertions.
use super::*;
use futures_util::FutureExt as _;
use std::panic::AssertUnwindSafe;
use tokio::sync::watch;
use two_bot_cutover::retention::{handle_guild_delete, run_retention, RetentionMode};
use two_bot_testsupport::TestDatabase;

const RETENTION_GUILD: &str = "111111111111111111";
const RETENTION_GUILD_ID: u64 = 111111111111111111;
const RETENTION_MEMBER: &str = "333333333333333333";

async fn database() -> Option<TestDatabase> {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure retention gateway fixtures"
            );
            eprintln!("SKIP retention gateway database fixture: TWO_TEST_DATABASE_URL not set");
            return None;
        }
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("guarded retention gateway fixture"),
    )
}

struct Peer {
    url: String,
    packets: mpsc::Sender<Value>,
    auth: mpsc::Receiver<Value>,
    heartbeat_sequence: watch::Receiver<Option<u64>>,
    task: JoinHandle<()>,
}
impl Peer {
    async fn new() -> Self {
        Self::with_heartbeat_interval(45_000).await
    }
    async fn with_heartbeat_interval(heartbeat_interval_ms: u64) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (packets, mut input) = mpsc::channel::<Value>(16);
        let (auth_tx, auth) = mpsc::channel(2);
        let (heartbeat_tx, heartbeat_sequence) = watch::channel(None);
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (_, mut ws) = ServerBuilder::new().accept(stream).await.unwrap();
            ws.send(Message::text(
                json!({"op":10,"d":{"heartbeat_interval":heartbeat_interval_ms}}).to_string(),
            ))
            .await
            .unwrap();
            loop {
                tokio::select! {
                    packet = input.recv() => match packet {
                        Some(packet) => { if ws.send(Message::text(packet.to_string())).await.is_err() { break; } }
                        None => break,
                    },
                    message = ws.next() => {
                        let Some(Ok(message)) = message else { break };
                        if message.is_close() { break; }
                        if !message.is_text() { continue; }
                        let packet: Value = serde_json::from_str(message.as_text().unwrap()).unwrap();
                        let op = packet["op"].as_u64();
                        if op == Some(1) {
                            let _ = heartbeat_tx.send_replace(packet["d"].as_u64());
                        }
                        if op == Some(1)
                            && ws
                                .send(Message::text("{\"op\":11,\"d\":null}".to_owned()))
                                .await
                                .is_err()
                        {
                            break;
                        }
                        if matches!(op, Some(2 | 6)) && auth_tx.send(packet).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            url,
            packets,
            auth,
            heartbeat_sequence,
            task,
        }
    }
    async fn authentication(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(10), self.auth.recv())
            .await
            .expect("gateway handshake deadline")
            .expect("gateway authenticates")
    }
    async fn send(&self, packet: Value) {
        self.packets.send(packet).await.unwrap();
    }
    async fn close(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

fn delete(seq: u64, unavailable: Option<bool>) -> Value {
    let mut data = json!({"id":RETENTION_GUILD});
    if let Some(value) = unavailable {
        data["unavailable"] = json!(value);
    }
    json!({"op":0,"s":seq,"t":"GUILD_DELETE","d":data})
}

fn create(seq: u64, unavailable: bool) -> Value {
    let data = if unavailable {
        json!({"id":RETENTION_GUILD,"unavailable":true})
    } else {
        // Rewrite guild/everyone-role references only; all fixture member and
        // channel identities remain synthetic. Retention requires a snowflake.
        let fixture = include_str!("../../tests/fixtures/voice_guild.json")
            .replace("\"100\"", &format!("\"{RETENTION_GUILD}\""));
        let mut guild: Value = serde_json::from_str(&fixture).unwrap();
        guild["channels"].as_array_mut().unwrap().push(json!({"id":"700","guild_id":RETENTION_GUILD,"type":2,"name":"fixture room","parent_id":"400","position":1,"permission_overwrites":[],"user_limit":0,"bitrate":64000}));
        guild
    };
    json!({"op":0,"s":seq,"t":"GUILD_CREATE","d":data})
}

struct RunningGateway {
    store: GatewaySessionStore,
    stop: watch::Sender<bool>,
    task: JoinHandle<Result<(), sqlx::Error>>,
    state: Arc<RwLock<GatewayState>>,
    fresh: bool,
}

async fn start_runner(
    pool: &PgPool,
    peer: &Peer,
    voice: Option<Arc<dyn VoiceEventSink>>,
) -> RunningGateway {
    ensure_crypto_provider();
    let store = GatewaySessionStore::new(pool.clone(), RETENTION_GUILD.to_owned(), 0);
    // Rebinding the synthetic websocket endpoint is fixture setup only.
    sqlx::query("UPDATE gateway_sessions SET resume_url = $1")
        .bind(&peer.url)
        .execute(pool)
        .await
        .unwrap();
    let saved = load_boot_session(&store).await.unwrap();
    let shard = crate::gateway::build_shard(
        TOKEN.into(),
        Intents::empty(),
        saved.as_ref(),
        Some(&peer.url),
    );
    let pipeline = Arc::new(build_pipeline(store.milestones().await.unwrap(), None));
    let state = Arc::new(RwLock::new(GatewayState::Armed));
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(run_shard(
        shard,
        pipeline,
        state.clone(),
        store.clone(),
        None,
        None,
        None,
        None,
        voice,
        crate::server::shutdown_requested(rx),
    ));
    RunningGateway {
        store,
        stop,
        task,
        state,
        fresh: saved.is_none(),
    }
}

async fn runner(
    pool: &PgPool,
    peer: &mut Peer,
    voice: Option<Arc<dyn VoiceEventSink>>,
) -> (
    GatewaySessionStore,
    watch::Sender<bool>,
    JoinHandle<Result<(), sqlx::Error>>,
    Value,
) {
    let RunningGateway {
        store,
        stop,
        task,
        fresh,
        ..
    } = start_runner(pool, peer, voice).await;
    let auth = peer.authentication().await;
    if fresh {
        assert_eq!(auth["op"], 2);
        peer.send(ready(&peer.url, "retention-session")).await;
        wait_sequence(&store, 1).await;
    }
    (store, stop, task, auth)
}

async fn stop_runner(stop: watch::Sender<bool>, task: JoinHandle<Result<(), sqlx::Error>>) {
    stop.send_replace(true);
    tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("gateway shutdown joins writer")
        .unwrap()
        .unwrap();
}

async fn queued(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM retention_guild_purge_queue WHERE guild_id = $1")
        .bind(RETENTION_GUILD)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn seed_long_term(pool: &PgPool) {
    sqlx::query("INSERT INTO member_levels (guild_id, member_id, xp, message_xp, updated_at) VALUES ($1, $2, 150, 150, now() - interval '90 days')")
        .bind(RETENTION_GUILD).bind(RETENTION_MEMBER).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO member_ranks (guild_id, member_id, updated_at) VALUES ($1, $2, now() - interval '90 days')")
        .bind(RETENTION_GUILD).bind(RETENTION_MEMBER).execute(pool).await.unwrap();
}

async fn long_term(pool: &PgPool) -> (Value, Value) {
    let levels = sqlx::query_scalar("SELECT to_jsonb(r) FROM member_levels r WHERE guild_id = $1")
        .bind(RETENTION_GUILD)
        .fetch_one(pool)
        .await
        .unwrap();
    let ranks = sqlx::query_scalar("SELECT to_jsonb(r) FROM member_ranks r WHERE guild_id = $1")
        .bind(RETENTION_GUILD)
        .fetch_one(pool)
        .await
        .unwrap();
    (levels, ranks)
}

#[tokio::test]
async fn real_delete_flag_matrix_preserves_voice_cleanup_and_available_rejoin() {
    for unavailable in [None, Some(false), Some(true)] {
        let Some(db) = database().await else { return };
        let pool = db.pool();
        seed_long_term(pool).await;
        let before = long_term(pool).await;
        let rooms = PgRoomStore::new(pool.clone());
        rooms
            .add_creator(&CreatorChannel::new(RETENTION_GUILD_ID, 200))
            .await
            .unwrap();
        rooms
            .add_room(&VoiceRoom::from_spec(
                NewRoomSpec {
                    guild_id: RETENTION_GUILD_ID,
                    creator_channel_id: 200,
                    owner_id: 300,
                    seed: 7,
                    created_at: "2026-10-01T00:00:00Z".into(),
                },
                700,
            ))
            .await
            .unwrap();
        let writes = DeleteOnly::default();
        let (factory_rooms, factory_writes) = (rooms.clone(), writes.clone());
        let voice = Arc::new(VoiceRuntime::new(
            move || (factory_rooms.clone(), factory_writes.clone()),
            Duration::from_secs(86400),
            true,
        ));
        let mut peer = Peer::new().await;
        let (store, stop, task, _) = runner(pool, &mut peer, Some(voice.clone())).await;
        peer.send(create(2, false)).await;
        wait_sequence(&store, 2).await;
        assert!(
            voice.worker_status(RETENTION_GUILD_ID).await.is_some(),
            "real create must establish the actor"
        );
        peer.send(delete(3, unavailable)).await;
        wait_sequence(&store, 3).await;
        assert!(
            voice.worker_status(RETENTION_GUILD_ID).await.is_none(),
            "every deletion, including outage=true, invalidates the actor"
        );
        assert_eq!(
            queued(pool).await,
            i64::from(unavailable != Some(true)),
            "preserve Option<bool>: {unavailable:?}"
        );
        assert!(
            rooms
                .room_for(RETENTION_GUILD_ID, 700)
                .await
                .unwrap()
                .is_some(),
            "gateway cleanup is not durable room deletion"
        );
        assert_eq!(writes.0.load(Ordering::Relaxed), 0);
        peer.send(create(4, true)).await;
        wait_sequence(&store, 4).await;
        assert!(voice.worker_status(RETENTION_GUILD_ID).await.is_none());
        assert_eq!(
            queued(pool).await,
            i64::from(unavailable != Some(true)),
            "unavailable create cannot cancel a queued purge"
        );
        peer.send(create(5, false)).await;
        wait_sequence(&store, 5).await;
        assert!(voice.worker_status(RETENTION_GUILD_ID).await.is_some());
        assert_eq!(
            queued(pool).await,
            0,
            "available rejoin cancels before acknowledgement"
        );
        run_retention(pool, RetentionMode::Execute).await.unwrap();
        assert_eq!(
            long_term(pool).await,
            before,
            "rejoined long-term rows survive an actual retention run"
        );
        assert!(rooms
            .room_for(RETENTION_GUILD_ID, 700)
            .await
            .unwrap()
            .is_some());
        stop_runner(stop, task).await;
        peer.close().await;
        db.close().await.unwrap();
    }
}

#[tokio::test]
async fn queued_delete_then_create_is_ordered_before_durable_checkpoint() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    seed_long_term(pool).await;
    let before = long_term(pool).await;
    let mut peer = Peer::new().await;
    let (store, stop, task, _) = runner(pool, &mut peer, None).await;
    let independent = db.independent_pool().await.unwrap();
    let mut held = independent.begin().await.unwrap();
    sqlx::query("LOCK TABLE retention_guild_purge_queue IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *held)
        .await
        .unwrap();
    peer.send(delete(2, None)).await;
    peer.send(create(3, false)).await;
    // Observe a database lock waiter, not elapsed wall time. The writer must
    // reach the queue and stop before either dispatch is acknowledged.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_locks WHERE relation = 'retention_guild_purge_queue'::regclass AND NOT granted)").fetch_one(pool).await.unwrap();
            if waiting { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("gateway must attempt the queue write before acknowledging delete/create");
    assert_eq!(store.load().await.unwrap().unwrap().sequence, 1);
    held.rollback().await.unwrap();
    wait_sequence(&store, 3).await;
    assert_eq!(queued(pool).await, 0);
    run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(long_term(pool).await, before);
    stop_runner(stop, task).await;
    peer.close().await;
    independent.close().await;
    db.close().await.unwrap();
}

/// A pool whose connections run as a fresh role missing exactly one
/// privilege (`revoked`, a test-local literal) on the purge queue, plus the
/// role name so the caller can drop it with [`drop_limited_role`].
async fn limited_pool(pool: &PgPool, revoked: &str) -> (PgPool, String) {
    assert!(matches!(revoked, "DELETE" | "INSERT"));
    static NEXT_ROLE: AtomicU64 = AtomicU64::new(0);
    let role = format!(
        "retention_rejoin_{}_{}",
        std::process::id(),
        NEXT_ROLE.fetch_add(1, Ordering::Relaxed)
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE ROLE {role} NOLOGIN;
         GRANT USAGE ON SCHEMA public TO {role};
         GRANT ALL PRIVILEGES ON ALL TABLES IN SCHEMA public TO {role};
         GRANT ALL PRIVILEGES ON ALL SEQUENCES IN SCHEMA public TO {role};
         GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA public TO {role};
         REVOKE {revoked} ON TABLE retention_guild_purge_queue FROM {role}"
    )))
    .execute(pool)
    .await
    .unwrap();
    let runtime_role = role.clone();
    let limited = PgPoolOptions::new()
        .max_connections(3)
        .after_connect(move |connection, _| {
            let role = runtime_role.clone();
            Box::pin(async move {
                sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET ROLE {role}")))
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    (limited, role)
}

async fn drop_limited_role(pool: &PgPool, role: &str) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "REVOKE ALL PRIVILEGES ON ALL TABLES IN SCHEMA public FROM {role};
         REVOKE ALL PRIVILEGES ON ALL SEQUENCES IN SCHEMA public FROM {role};
         REVOKE ALL PRIVILEGES ON ALL FUNCTIONS IN SCHEMA public FROM {role};
         REVOKE USAGE ON SCHEMA public FROM {role};
         DROP ROLE {role}"
    )))
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn missing_rejoin_cancel_grant_holds_cursor_but_delete_insert_commits() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let (limited, role) = limited_pool(pool, "DELETE").await;
    let mut peer = Peer::new().await;
    let (store, stop, mut task, _) = runner(&limited, &mut peer, None).await;

    peer.send(delete(2, None)).await;
    wait_sequence(&store, 2).await;
    assert_eq!(queued(&limited).await, 1, "real deletion should queue");

    peer.send(create(3, false)).await;
    let result = tokio::time::timeout(Duration::from_secs(15), &mut task)
        .await
        .expect("missing rejoin-cancel grant must stop the gateway")
        .unwrap();
    assert!(result.is_err(), "42501 on rejoin must fail the dispatch");
    assert_eq!(
        store.load().await.unwrap().unwrap().sequence,
        2,
        "failed rejoin-cancel must leave the checkpoint before GUILD_CREATE"
    );

    peer.close().await;
    limited.close().await;
    drop_limited_role(pool, &role).await;
    db.close().await.unwrap();
    let _ = stop;
}

#[tokio::test]
async fn missing_queue_insert_grant_skips_real_delete_and_gateway_continues() {
    // A 42501 on a real removal is a schema-gap skip: it advances the
    // global replay-needed signal, so hold the serial lock to keep a
    // sibling delta-measurement exact.
    let _replay_serial = replay_needed_serial().lock().await;
    // The mirror of the rejoin hold above: a 42501 on a real GUILD_DELETE
    // (INSERT denied) is still a schema-gap skip — the dispatch checkpoints,
    // the gateway keeps running, and the Operator replay in docs/privacy.md
    // owns the dropped queue write. Widening the 42501 hold beyond
    // GUILD_CREATE would hold the cursor here and fail this test.
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let (limited, role) = limited_pool(pool, "INSERT").await;
    let mut peer = Peer::new().await;
    let (store, stop, task, _) = runner(&limited, &mut peer, None).await;

    peer.send(delete(2, None)).await;
    wait_sequence(&store, 2).await;
    assert!(
        !task.is_finished(),
        "42501 on a real GUILD_DELETE must skip, not stop the gateway"
    );
    assert_eq!(queued(&limited).await, 0, "denied insert queues nothing");

    stop_runner(stop, task).await;
    peer.close().await;
    limited.close().await;
    drop_limited_role(pool, &role).await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn missing_retention_tables_do_not_stop_the_gateway() {
    // The dropped queue turns the real removal below into a purge-skip,
    // which advances the global replay-needed signal: hold the serial
    // lock so a sibling delta-measurement stays exact.
    let _replay_serial = replay_needed_serial().lock().await;
    // TOG-25067 staging reproduction: the 0438 retention queues are absent
    // (an unapplied migration), so every queue/cancel write fails with 42P01.
    // The gateway must skip the auxiliary write and commit the dispatch
    // instead of holding the cursor and crash-looping on replay.
    let Some(db) = database().await else { return };
    let pool = db.pool();
    sqlx::query("DROP TABLE retention_guild_purge_queue")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE retention_deletion_requests")
        .execute(pool)
        .await
        .unwrap();
    let mut peer = Peer::new().await;
    let (store, stop, task, _) = runner(pool, &mut peer, None).await;
    peer.send(delete(2, None)).await;
    peer.send(create(3, false)).await;
    wait_sequence(&store, 3).await;
    assert!(
        !task.is_finished(),
        "schema-gapped retention writes must not stop the gateway"
    );
    stop_runner(stop, task).await;
    peer.close().await;
    db.close().await.unwrap();
}

/// Process-wide tracing capture for the schema-gap skip lines, shared with
/// every log-hygiene test in this binary (`crate::tracing_capture::global`).
/// A thread-local `set_default` capture drops events from the spawned gateway
/// worker on another thread once a sibling test caches callsite interest, so
/// this test reads only the text appended after its own start offset.
fn schema_gap_capture() -> &'static crate::tracing_capture::Capture {
    crate::tracing_capture::global()
}

#[tokio::test]
async fn schema_gapped_real_delete_survives_and_operator_replay_restores_purge() {
    // The dropped queue turns the real removal below into a purge-skip,
    // which advances the global replay-needed signal: hold the serial
    // lock so a sibling delta-measurement stays exact.
    let _replay_serial = replay_needed_serial().lock().await;
    // Paperclip Review on #978: acking a schema-gapped real GUILD_DELETE must
    // not silently drop the purge obligation. The gateway survives (the
    // sequence advances), the skip is pinned at error level with the guild and
    // SQLSTATE — forcing `is_real_guild_removal` false, downgrading `error!`
    // to `warn!`, or dropping the `guild`/`error_code` fields fails this test —
    // and the privacy.md Operator replay — the same insert the live path runs —
    // restores the purge once the queue table exists.
    let Some(db) = database().await else { return };
    let pool = db.pool();
    sqlx::query("DROP TABLE retention_guild_purge_queue")
        .execute(pool)
        .await
        .unwrap();
    let capture = schema_gap_capture();
    let start = capture.text().len();
    let mut peer = Peer::new().await;
    let (store, stop, task, _) = runner(pool, &mut peer, None).await;
    peer.send(delete(2, None)).await;
    wait_sequence(&store, 2).await;
    peer.send(create(3, false)).await;
    wait_sequence(&store, 3).await;
    assert!(
        !task.is_finished(),
        "schema-gapped real delete must not stop the gateway"
    );
    let logs = capture.text().split_off(start);
    assert!(
        logs.lines().any(|line| line.contains("ERROR")
            && line.contains("retention purge skipped; schema gap, operator replay required")
            && line.contains(RETENTION_GUILD)
            && line.contains("42P01")),
        "real removal skip must log at error with guild + SQLSTATE, got:\n{logs}"
    );
    assert!(
        logs.lines().any(|line| line.contains("WARN")
            && line.contains("retention lifecycle skipped; schema gap, nothing to cancel")),
        "rejoin-cancel skip must warn instead of erroring, got:\n{logs}"
    );
    // The migration lands: recreate exactly the 0438 queue table, still empty
    // because the skip dropped the obligation.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS retention_guild_purge_queue (guild_id TEXT PRIMARY KEY, enqueued_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp())",
    )
    .execute(pool)
    .await
    .unwrap();
    assert_eq!(
        queued(pool).await,
        0,
        "the skipped delete queues nothing by itself"
    );
    // The documented Operator replay: the live queue insert for the removed
    // guild from the error line.
    handle_guild_delete(pool, RETENTION_GUILD, None)
        .await
        .unwrap();
    assert_eq!(
        queued(pool).await,
        1,
        "operator replay must restore the skipped purge"
    );
    // The queue is live again, so a later available rejoin cancels the
    // replayed purge before any retention run can erase the guild's rows.
    peer.send(create(4, false)).await;
    wait_sequence(&store, 4).await;
    assert_eq!(
        queued(pool).await,
        0,
        "a rejoin after the replay must cancel the queued purge"
    );
    stop_runner(stop, task).await;
    peer.close().await;
    db.close().await.unwrap();
}

async fn failed_write_replays(cancel: bool, fail_checkpoint: bool) {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    seed_long_term(pool).await;
    if cancel {
        handle_guild_delete(pool, RETENTION_GUILD, None)
            .await
            .unwrap();
    }
    let mut peer = Peer::new().await;
    let (store, stop, mut task, _) = runner(pool, &mut peer, None).await;
    let trigger = if fail_checkpoint {
        "CREATE TRIGGER retention_dispatch_refuse BEFORE INSERT OR UPDATE ON gateway_sessions FOR EACH ROW EXECUTE FUNCTION retention_dispatch_refuse()"
    } else if cancel {
        "CREATE TRIGGER retention_dispatch_refuse BEFORE DELETE ON retention_guild_purge_queue FOR EACH ROW EXECUTE FUNCTION retention_dispatch_refuse()"
    } else {
        "CREATE TRIGGER retention_dispatch_refuse BEFORE INSERT ON retention_guild_purge_queue FOR EACH ROW EXECUTE FUNCTION retention_dispatch_refuse()"
    };
    sqlx::query("CREATE FUNCTION retention_dispatch_refuse() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic retention dispatch refusal'; END $$").execute(pool).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(trigger))
        .execute(pool)
        .await
        .unwrap();
    let packet = if cancel {
        create(2, false)
    } else {
        delete(2, None)
    };
    peer.send(packet.clone()).await;
    peer.send(json!({"op":0,"s":3,"t":"RESUMED","d":{}})).await;
    let stopped = tokio::time::timeout(Duration::from_secs(15), &mut task).await;
    if stopped.is_err() {
        stop.send_replace(true);
        let _ = tokio::time::timeout(Duration::from_secs(15), &mut task).await;
    }
    let result = stopped
        .expect("failed queue/cancel/checkpoint write must stop gateway; never acknowledge it")
        .unwrap();
    assert!(result.is_err());
    assert_eq!(
        store.load().await.unwrap().unwrap().sequence,
        1,
        "failed write and following packet cannot advance durable sequence"
    );
    // A queue mutation may have committed before checkpoint failure; replay
    // must repair it idempotently, so do not require a new multi-table API.
    if !fail_checkpoint {
        assert_eq!(queued(pool).await, i64::from(cancel));
    }
    peer.close().await;
    let table = if fail_checkpoint {
        "gateway_sessions"
    } else {
        "retention_guild_purge_queue"
    };
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP TRIGGER retention_dispatch_refuse ON {table}"
    )))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("DROP FUNCTION retention_dispatch_refuse()")
        .execute(pool)
        .await
        .unwrap();
    let independent = db.independent_pool().await.unwrap();
    let mut replay = Peer::new().await;
    let (store, stop, task, auth) = runner(&independent, &mut replay, None).await;
    assert_eq!(auth["op"], 6);
    assert_eq!(
        auth["d"]["seq"], 1,
        "restart resumes only the committed sequence"
    );
    replay.send(packet.clone()).await;
    replay
        .send(json!({"op":0,"s":3,"t":"RESUMED","d":{}}))
        .await;
    wait_sequence(&store, 3).await;
    assert_eq!(queued(pool).await, i64::from(!cancel));
    // A rejoin after the replayed delete must not be undone by an old replay.
    replay.send(create(4, false)).await;
    wait_sequence(&store, 4).await;
    assert_eq!(queued(pool).await, 0);
    replay.send(packet).await;
    replay
        .send(json!({"op":0,"s":5,"t":"RESUMED","d":{}}))
        .await;
    wait_sequence(&store, 5).await;
    assert_eq!(
        queued(pool).await,
        0,
        "already committed deletion cannot requeue a rejoined guild"
    );
    let before = long_term(pool).await;
    run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(long_term(pool).await, before);
    stop_runner(stop, task).await;
    replay.close().await;
    independent.close().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn failed_queue_write_is_not_acknowledged_and_replays_after_restart() {
    failed_write_replays(false, false).await;
}

#[tokio::test]
async fn failed_cancel_write_is_not_acknowledged_and_replays_after_restart() {
    failed_write_replays(true, false).await;
}

#[tokio::test]
async fn failed_checkpoint_after_delete_or_create_replays_without_losing_retention_work() {
    for cancel in [false, true] {
        failed_write_replays(cancel, true).await;
    }
}

async fn blocked_lifecycle_obeys_checkpoint_deadline(cancel: bool) {
    // A 2s heartbeat gives checkpoint I/O a 1s client budget. The entire
    // observation must finish before the fixture's unchanged 5s SQL timeout.
    const RETURN_BOUND: Duration = Duration::from_secs(2);
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let mut peer = None;
    let mut independent = None;
    let mut held = None;
    let mut running = None;
    let mut joined = false;
    let checked = AssertUnwindSafe(async {
        let server_timeout_ms: i32 =
            sqlx::query_scalar("SELECT setting::integer FROM pg_settings WHERE name = 'statement_timeout'")
                .fetch_one(pool).await.unwrap();
        assert_eq!(server_timeout_ms, 5_000, "keep the fixture's server timeout");
        if cancel {
            handle_guild_delete(pool, RETENTION_GUILD, None).await.unwrap();
        }
        peer = Some(Peer::with_heartbeat_interval(2_000).await);
        let peer = peer.as_mut().unwrap();
        running = Some(start_runner(pool, peer, None).await);
        let run = running.as_mut().unwrap();
        assert!(run.fresh);
        assert_eq!(peer.authentication().await["op"], 2);
        peer.send(ready(&peer.url, "retention-deadline-session")).await;
        wait_sequence(&run.store, 1).await;
        peer.send(json!({"op":1,"d":null})).await;
        wait_connected(&run.state).await;
        assert_eq!(queued(pool).await, i64::from(cancel));

        independent = Some(db.independent_pool().await.unwrap());
        held = Some(independent.as_ref().unwrap().begin().await.unwrap());
        let transaction = held.as_mut().unwrap();
        let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut **transaction).await.unwrap();
        sqlx::query("LOCK TABLE retention_guild_purge_queue IN SHARE ROW EXCLUSIVE MODE")
            .execute(&mut **transaction).await.unwrap();
        let (operation, query_pattern, packet) = if cancel {
            ("cancel", "%DELETE%retention_guild_purge_queue%", create(2, false))
        } else {
            ("queue", "%INSERT%retention_guild_purge_queue%", delete(2, None))
        };
        let started = tokio::time::Instant::now();
        let return_by = started + RETURN_BOUND;
        peer.send(packet).await;
        peer.send(json!({"op":0,"s":3,"t":"RESUMED","d":{}})).await;
        peer.send(json!({"op":1,"d":null})).await;
        tokio::time::timeout_at(return_by, async {
            loop {
                let waiting: bool = sqlx::query_scalar(
                    "SELECT EXISTS (
                        SELECT 1 FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid
                        WHERE a.datname = current_database() AND a.wait_event_type = 'Lock'
                          AND l.relation = 'retention_guild_purge_queue'::regclass
                          AND l.mode = 'RowExclusiveLock' AND NOT l.granted
                          AND $1 = ANY(pg_blocking_pids(a.pid)) AND a.query ILIKE $2
                    )",
                ).bind(holder_pid).bind(query_pattern).fetch_one(pool).await.unwrap();
                if waiting { break; }
                tokio::task::yield_now().await;
            }
        }).await.expect("the lifecycle write must wait on this fixture's held purge-queue lock");
        let armed_while_blocked = *run.state.read().await == GatewayState::Armed;
        // A heartbeat carrying 3 proves the real shard received the following
        // dispatch while the serial worker was still stuck on sequence 2.
        let received_later = tokio::time::timeout_at(
            return_by,
            peer.heartbeat_sequence.wait_for(|sequence| *sequence == Some(3)),
        ).await.is_ok_and(|result| result.is_ok());
        assert!(received_later, "following dispatch must reach the shard before the deadline");

        let stopped = tokio::time::timeout_at(return_by, &mut run.task).await;
        joined = stopped.is_ok();
        let elapsed = started.elapsed();
        let client_deadline = matches!(stopped, Ok(Ok(Err(ref error)))
            if error.to_string().contains("gateway checkpoint deadline exceeded"));
        let lock_still_held: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_locks
                WHERE pid = $1 AND relation = 'retention_guild_purge_queue'::regclass
                  AND mode = 'ShareRowExclusiveLock' AND granted)",
        ).bind(holder_pid).fetch_one(pool).await.unwrap();
        let sequence = run.store.load().await.unwrap().unwrap().sequence;
        let queue_count = queued(pool).await;
        eprintln!(
            "retention {operation}: lock_wait=true, later_received={received_later}, armed={armed_while_blocked}, returned={joined}, client_deadline={client_deadline}, elapsed_ms={}, lock_held={lock_still_held}, sequence={sequence}, queued={queue_count}",
            elapsed.as_millis(),
        );
        assert!(lock_still_held, "lock release must not complete the lifecycle write");
        assert_eq!(sequence, 1, "neither blocked write nor following dispatch may checkpoint");
        assert_eq!(queue_count, i64::from(cancel), "timed-out write is still blocked");
        assert!(armed_while_blocked, "{operation} lock wait must arm readiness, not stay Connected");
        assert!(client_deadline, "{operation} must return the client checkpoint deadline, not SQL timeout or outer timeout");
        assert!(elapsed < RETURN_BOUND, "{operation} must fail within the client return bound");
    }).catch_unwind().await;

    // Keep cleanup outside the assertion future: even RED must release the
    // lock, join the writer and close the owned fixture before unwinding.
    let rollback = match held {
        Some(transaction) => transaction.rollback().await,
        None => Ok(()),
    };
    let mut writer_joined = true;
    let mut sequence_after_unlock = None;
    if let Some(mut run) = running {
        run.stop.send_replace(true);
        if !joined {
            match tokio::time::timeout(Duration::from_secs(15), &mut run.task).await {
                Ok(result) => writer_joined = result.is_ok(),
                Err(_) => {
                    run.task.abort();
                    let _ = run.task.await;
                    writer_joined = false;
                }
            }
        }
        sequence_after_unlock = Some(
            run.store
                .load()
                .await
                .map(|saved| saved.map(|s| s.sequence)),
        );
    }
    if let Some(peer) = peer {
        peer.close().await;
    }
    if let Some(independent) = independent {
        independent.close().await;
    }
    let closed = db.close().await;
    assert!(rollback.is_ok(), "release the fixture's purge-queue lock");
    assert!(writer_joined, "join the gateway writer during cleanup");
    assert!(closed.is_ok(), "close the owned retention database fixture");
    if let Err(panic) = checked {
        std::panic::resume_unwind(panic);
    }
    assert!(
        matches!(sequence_after_unlock, Some(Ok(Some(1)))),
        "releasing the lock must not let cancelled or later work checkpoint",
    );
}

#[tokio::test]
async fn blocked_queue_write_obeys_checkpoint_deadline() {
    blocked_lifecycle_obeys_checkpoint_deadline(false).await;
}

#[tokio::test]
async fn blocked_cancel_write_obeys_checkpoint_deadline() {
    blocked_lifecycle_obeys_checkpoint_deadline(true).await;
}

// --- Retention replay-needed pollable signal (acceptance battery) ---
//
// Contract for the implementing change, stated here so no assertion below is
// a hidden trap:
// - The pollable surface is the existing `/metrics` render
//   (`two_bot_core::metrics::Metrics::render`, served by
//   `metrics_http::scrape`). A fresh `Metrics` renders the signal at zero;
//   no new endpoint, file, or environment variable is the signal.
// - The rendered metric name contains `retention_replay_needed` (the repo
//   convention prefixes `two_bot_`; counter or gauge type is the
//   implementer's choice). Labelled samples are summed by the parser below.
// - Exactly the forced purge-skip path advances it: a real guild removal
//   whose queue write hits the schema gap (the `42P01` error line pinned by
//   `schema_gapped_real_delete_survives_and_operator_replay_restores_purge`).
//   The rejoin-cancel skip ("nothing to cancel") and successful queue
//   writes must not advance it.
// - The increment call site in `gateway.rs` textually references
//   `retention_replay_needed` (method or constant named after the metric,
//   as `set_audit_delivery_halt` mirrors `two_bot_audit_delivery_halt`) and
//   lives inside the function that emits the skip line, so confinement can
//   be checked offline.
// - `docs/privacy.md` points operators at the signal by name while keeping
//   the manual replay insert.
//
// The database tests below share the process-global counter, and `cargo
// test` / nextest run them on parallel threads. An exact delta is only
// meaningful when no sibling test advances the signal mid-measurement, so
// every test that can advance it (each real-removal purge-skip) and every
// test that reads its delta holds `replay_needed_serial` for its whole
// body. A `tokio::sync::Mutex` (not `std`) because the guard is held
// across `.await`s.

const REPLAY_NEEDED_TOKEN: &str = "retention_replay_needed";

/// Process-wide serialiser for the global `retention_replay_needed`
/// counter: holders run their purge-skip / delta-measurement alone, so a
/// parallel sibling cannot pollute an exact-delta assertion.
fn replay_needed_serial() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Sum of every rendered sample whose metric name contains the token.
/// `None` when no such sample exists at all (signal not implemented).
fn replay_needed_value(rendered: &str) -> Option<u64> {
    let mut total: Option<f64> = None;
    for line in rendered.lines() {
        if line.starts_with('#') {
            continue;
        }
        let Some((name, value)) = line.rsplit_once(' ') else {
            continue;
        };
        let metric = name.split('{').next().unwrap_or(name);
        if !metric.contains(REPLAY_NEEDED_TOKEN) {
            continue;
        }
        let parsed: f64 = value
            .parse()
            .unwrap_or_else(|_| panic!("replay-needed sample must be numeric: {line}"));
        total = Some(total.unwrap_or(0.0) + parsed);
    }
    total.map(|value| {
        assert!(
            value.fract() == 0.0 && value >= 0.0,
            "replay-needed signal must be a whole count, got {value}"
        );
        value as u64
    })
}

fn global_replay_needed() -> u64 {
    let rendered = two_bot_core::metrics::global().render(None);
    replay_needed_value(&rendered).expect(
        "global /metrics render must expose a retention_replay_needed sample; \
         the purge-skip path currently emits only a log line",
    )
}

#[test]
fn retention_replay_needed_signal_is_pollable_and_renders_zero_when_fresh() {
    let rendered = two_bot_core::metrics::Metrics::default().render(None);
    assert!(
        rendered.lines().any(|line| line.starts_with("# TYPE ")
            && line
                .split_whitespace()
                .nth(2)
                .is_some_and(|name| name.contains(REPLAY_NEEDED_TOKEN))
            && (line.ends_with(" counter") || line.ends_with(" gauge"))),
        "fresh metrics must declare a retention_replay_needed counter/gauge \
         in the pollable render, got:\n{rendered}"
    );
    assert_eq!(
        replay_needed_value(&rendered),
        Some(0),
        "a fresh process with no skipped purge must render the signal at zero"
    );
}

#[tokio::test]
async fn retention_replay_needed_skip_path_increments_exactly_once() {
    // The delta below is read from the process-global counter, so hold
    // the serial lock for the whole measurement: a parallel sibling
    // purge-skip would otherwise pollute the exact increment.
    let _replay_serial = replay_needed_serial().lock().await;
    // Forced purge-skip: the queue table is missing (42P01), so the real
    // delete drops its purge obligation. That one skip must advance the
    // pollable signal by exactly one; the following rejoin-cancel skip
    // ("nothing to cancel") must not advance it again.
    let Some(db) = database().await else { return };
    let pool = db.pool();
    sqlx::query("DROP TABLE retention_guild_purge_queue")
        .execute(pool)
        .await
        .unwrap();
    let before = global_replay_needed();
    let mut peer = Peer::new().await;
    let (store, stop, task, _) = runner(pool, &mut peer, None).await;
    peer.send(delete(2, None)).await;
    wait_sequence(&store, 2).await;
    peer.send(create(3, false)).await;
    wait_sequence(&store, 3).await;
    assert!(
        !task.is_finished(),
        "schema-gapped real delete must not stop the gateway"
    );
    assert_eq!(
        global_replay_needed() - before,
        1,
        "one forced purge-skip must increment retention_replay_needed exactly \
         once, and the rejoin-cancel skip must not increment it"
    );
    stop_runner(stop, task).await;
    peer.close().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn retention_replay_needed_successful_purge_queue_leaves_signal_at_zero() {
    // The delta below is read from the process-global counter, so hold
    // the serial lock for the whole measurement: a parallel sibling
    // purge-skip would otherwise look like movement on this path.
    let _replay_serial = replay_needed_serial().lock().await;
    // Successful path: the queue table exists, the real delete queues its
    // purge, and the signal must not move.
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let before = global_replay_needed();
    let mut peer = Peer::new().await;
    let (store, stop, task, _) = runner(pool, &mut peer, None).await;
    peer.send(delete(2, None)).await;
    wait_sequence(&store, 2).await;
    assert_eq!(
        queued(pool).await,
        1,
        "the successful delete must queue its purge (precondition)"
    );
    assert_eq!(
        global_replay_needed() - before,
        0,
        "a successful purge queue write must leave retention_replay_needed \n         unchanged"
    );
    stop_runner(stop, task).await;
    peer.close().await;
    db.close().await.unwrap();
}

fn repo_text(relative: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// Line range (inclusive) of the top-level function enclosing `anchor`.
fn enclosing_fn_lines(lines: &[&str], anchor: &str) -> (usize, usize) {
    let is_fn_start = |line: &str| {
        line.starts_with("fn ")
            || line.starts_with("pub fn ")
            || line.starts_with("pub(crate) fn ")
            || line.starts_with("async fn ")
            || line.starts_with("pub async fn ")
            || line.starts_with("pub(crate) async fn ")
    };
    let anchor_idx = lines
        .iter()
        .position(|line| line.contains(anchor))
        .unwrap_or_else(|| panic!("anchor not found: {anchor}"));
    let start = (0..=anchor_idx)
        .rev()
        .find(|&idx| is_fn_start(lines[idx]))
        .expect("anchor must sit inside a top-level function");
    let end = ((anchor_idx + 1)..lines.len())
        .find(|&idx| is_fn_start(lines[idx]))
        .map_or(lines.len() - 1, |idx| idx - 1);
    (start, end)
}

#[test]
fn retention_replay_needed_increment_is_confined_to_the_purge_skip_path() {
    let gateway = repo_text("src/gateway.rs");
    let lines: Vec<&str> = gateway.lines().collect();
    let (start, end) = enclosing_fn_lines(
        &lines,
        "retention purge skipped; schema gap, operator replay required",
    );
    let token_lines: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains(REPLAY_NEEDED_TOKEN))
        .map(|(idx, _)| idx)
        .collect();
    assert!(
        !token_lines.is_empty(),
        "gateway.rs must reference retention_replay_needed at the purge-skip \
         increment site; the skip is currently a bare log line"
    );
    assert!(
        token_lines
            .iter()
            .all(|idx| (start..=end).contains(idx)),
        "every retention_replay_needed reference in gateway.rs must sit \n         inside the function emitting the purge-skip line (lines {}..={}), \n         found at lines {:?}",
        start + 1,
        end + 1,
        token_lines.iter().map(|idx| idx + 1).collect::<Vec<_>>()
    );
}

#[test]
fn retention_replay_needed_metric_name_is_declared_in_core_metrics() {
    let metrics = repo_text("../core/src/metrics.rs");
    assert!(
        metrics.contains(REPLAY_NEEDED_TOKEN),
        "crates/core/src/metrics.rs must declare/render the \n         retention_replay_needed metric so /metrics polls it"
    );
}

#[test]
fn retention_replay_needed_runbook_points_at_the_signal() {
    let privacy = repo_text("../../docs/privacy.md");
    assert!(
        privacy.contains(REPLAY_NEEDED_TOKEN),
        "docs/privacy.md must point operators at retention_replay_needed \n         instead of only the log line"
    );
    assert!(
        privacy.contains("INSERT INTO retention_guild_purge_queue"),
        "docs/privacy.md must keep the manual Operator replay insert"
    );
}
