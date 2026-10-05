use super::*;
use sqlx::ConnectOptions;
use tokio::sync::Notify;
use twilight_model::{channel::Channel, guild::Permissions};
use two_bot::voice_rooms::{RoomWrites, VoiceEventSink, VoiceRuntime, WriteGuard};
use two_bot_core::{
    voice_rooms::{CreatorChannel, NewRoomSpec, VoiceRoom},
    voice_text_channel::TextChannelPlan,
    Snowflake,
};
use two_bot_cutover::voice_rooms::PgRoomStore;
use two_bot_discord::voice_rooms::{RoomChannelAttributes, RoomHttpError};

#[derive(Clone, Default)]
struct DeleteOnly(Arc<AtomicU64>);

impl RoomWrites for DeleteOnly {
    async fn create(
        &self,
        _: u64,
        _: &str,
        _: &RoomChannelAttributes,
        _: WriteGuard,
    ) -> Result<Channel, RoomHttpError> {
        panic!("reconciliation must not create a room");
    }
    async fn move_member(
        &self,
        _: u64,
        _: u64,
        _: u64,
        _: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        panic!("reconciliation must not move a member");
    }
    async fn disconnect(&self, _: u64, _: u64, _: WriteGuard) -> Result<(), RoomHttpError> {
        panic!("reconciliation must not disconnect a member");
    }
    async fn deny_connect(&self, _: u64, _: u64, _: WriteGuard) -> Result<(), RoomHttpError> {
        panic!("reconciliation must not deny connect");
    }
    async fn delete(&self, channel: u64, guard: WriteGuard) -> Result<(), RoomHttpError> {
        assert_eq!(channel, 700);
        if !guard() {
            return Err(RoomHttpError::Cancelled);
        }
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    async fn rename(&self, _: u64, _: &str) -> Result<(), RoomHttpError> {
        panic!("reconciliation must not rename a room");
    }
    async fn set_user_limit(&self, _: u64, _: u32, _: WriteGuard) -> Result<(), RoomHttpError> {
        panic!("reconciliation must not change a room limit");
    }
    async fn download_attachment(&self, _: &str, _: usize) -> Result<Vec<u8>, RoomHttpError> {
        panic!("reconciliation must not download an import file");
    }

    async fn create_companion(
        &self,
        _: &TextChannelPlan,
        _: Snowflake,
        _: WriteGuard,
    ) -> Result<Channel, RoomHttpError> {
        panic!("reconciliation must not create a companion");
    }
    async fn grant_companion_view(
        &self,
        _: Snowflake,
        _: Snowflake,
        _: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        panic!("reconciliation must not grant companion view");
    }
    async fn revoke_companion_view(
        &self,
        _: Snowflake,
        _: Snowflake,
        _: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        panic!("reconciliation must not revoke companion view");
    }
}

struct RecordingVoice {
    runtime: VoiceRuntime<PgRoomStore, DeleteOnly>,
    leaves: AtomicU64,
    disconnects: AtomicU64,
}

impl VoiceEventSink for RecordingVoice {
    fn handle(
        &self,
        event: &twilight_gateway::Event,
        cache: &twilight_cache_inmemory::DefaultInMemoryCache,
    ) {
        if matches!(event, twilight_gateway::Event::MemberRemove(_)) {
            self.leaves.fetch_add(1, Ordering::Relaxed);
        }
        self.runtime.handle(event, cache);
    }
    fn disconnect(&self) {
        self.disconnects.fetch_add(1, Ordering::Relaxed);
        self.runtime.disconnect();
    }
    fn needs_bootstrap(&self, cache: &twilight_cache_inmemory::DefaultInMemoryCache) -> bool {
        self.runtime.needs_bootstrap(cache)
    }
}

async fn cold_voice_gateway(release_ready: Arc<Notify>) -> MockGateway {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let gateway_url = url.clone();
    let (sender, auth) = mpsc::channel(4);
    let task = tokio::spawn(async move {
        for connection in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let (_, mut ws) = ServerBuilder::new().accept(stream).await.unwrap();
            ws.send(Message::text(
                json!({"op":10,"d":{"heartbeat_interval":45000}}).to_string(),
            ))
            .await
            .unwrap();
            while let Some(Ok(message)) = ws.next().await {
                if !message.is_text() {
                    continue;
                }
                let packet: Value = serde_json::from_str(message.as_text().unwrap()).unwrap();
                match packet["op"].as_u64() {
                    Some(1) => ws
                        .send(Message::text("{\"op\":11,\"d\":null}".to_owned()))
                        .await
                        .unwrap(),
                    Some(2 | 6) => {
                        sender.send(packet).await.unwrap();
                        if connection == 0 {
                            // Replay the saved dispatch plus one missed dispatch before
                            // RESUMED. Fresh IDENTIFY is allowed only after these commit.
                            for packet in [
                                leave(2),
                                leave(3),
                                json!({"op":0,"s":4,"t":"RESUMED","d":{}}),
                            ] {
                                ws.send(Message::text(packet.to_string())).await.unwrap();
                            }
                        } else {
                            release_ready.notified().await;
                            let mut packet = ready(&gateway_url, "voice-session");
                            packet["d"]["guilds"] = json!([{"id":"100","unavailable":true}]);
                            ws.send(Message::text(packet.to_string())).await.unwrap();
                            let mut guild: Value = serde_json::from_str(include_str!(
                                "../../tests/fixtures/voice_guild.json"
                            ))
                            .unwrap();
                            let allowed = Permissions::VIEW_CHANNEL
                                | Permissions::CONNECT
                                | Permissions::MANAGE_CHANNELS
                                | Permissions::MOVE_MEMBERS
                                | Permissions::MANAGE_ROLES;
                            guild["roles"][0]["permissions"] = json!(allowed.bits().to_string());
                            guild["channels"].as_array_mut().unwrap().push(json!({
                                "id":"700","guild_id":"100","type":2,"name":"tracked room","parent_id":"400","permission_overwrites":[]
                            }));
                            ws.send(Message::text(
                                json!({"op":0,"s":2,"t":"GUILD_CREATE","d":guild}).to_string(),
                            ))
                            .await
                            .unwrap();
                        }
                    }
                    _ => {}
                }
            }
        }
    });
    MockGateway { url, auth, task }
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn cold_voice_resume_commits_replay_before_identify_and_reconciles_stored_rooms() {
    let db = TestDb::new().await;
    let release = Arc::new(Notify::new());
    let mut mock = cold_voice_gateway(release.clone()).await;
    db.store
        .commit_dispatch(
            &checkpoint("saved-session", 2, &mock.url),
            FunnelBatch {
                events: vec![event(EventType::MemberLeave, "2026-09-30T00:00:00.000Z")],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let rooms = PgRoomStore::new(db.pool.clone());
    rooms
        .add_creator(&CreatorChannel::new(100, 200))
        .await
        .unwrap();
    rooms
        .add_room(&VoiceRoom::from_spec(
            NewRoomSpec {
                guild_id: 100,
                creator_channel_id: 200,
                owner_id: 300,
                seed: 7,
                created_at: "2026-09-30T00:00:00.000Z".to_owned(),
            },
            700,
        ))
        .await
        .unwrap();
    let writes = DeleteOnly::default();
    let deletions = writes.0.clone();
    let store = rooms.clone();
    let voice = Arc::new(RecordingVoice {
        // Real-time reconcile proof: shorten the grace the paused-time guard
        // tests pin at 60 s.
        runtime: VoiceRuntime::new(
            move || (store.clone(), writes.clone()),
            Duration::from_millis(10),
            true,
        )
        .with_empty_grace(Duration::ZERO),
        leaves: AtomicU64::new(0),
        disconnects: AtomicU64::new(0),
    });
    ensure_crypto_provider();
    let saved = load_boot_session(&db.store).await.unwrap();
    let shard = crate::gateway::build_shard(
        TOKEN.into(),
        two_bot_discord::gateway_intents(false),
        saved.as_ref(),
        Some(&mock.url),
    );
    let pipeline = Arc::new(build_pipeline(db.store.milestones().await.unwrap(), None));
    let state = Arc::new(RwLock::new(GatewayState::Armed));
    let runner = tokio::spawn(run_shard(
        shard,
        pipeline.clone(),
        state.clone(),
        db.store.clone(),
        None,
        None,
        None,
        Some(voice.clone()),
        std::future::pending(),
    ));
    let resume = mock.authentication().await;
    assert_eq!(resume["op"], 6);
    assert_eq!(resume["d"]["seq"], 2);
    assert_eq!(mock.authentication().await["op"], 2);
    let saved = db.store.load().await.unwrap().unwrap();
    assert_eq!(
        (saved.session_id.as_str(), saved.sequence),
        ("saved-session", 4)
    );
    assert_eq!(db.count().await, 2);
    assert_eq!(voice.leaves.load(Ordering::Relaxed), 1);
    assert_eq!(*state.read().await, GatewayState::Armed);
    assert!(pipeline.cache().current_user().is_none());
    assert!(voice.runtime.worker_status(100).await.is_none());
    assert!(voice.disconnects.load(Ordering::Relaxed) >= 1);
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(20), async {
        while !rooms.rooms_in_guild(100).await.unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("authoritative snapshot reconciles tracked room");
    assert_eq!(deletions.load(Ordering::Relaxed), 1);
    assert_eq!(pipeline.cache().current_user().unwrap().id.get(), 999);
    assert_eq!(
        voice
            .runtime
            .worker_status(100)
            .await
            .unwrap()
            .tracked_rooms,
        0
    );
    wait_sequence(&db.store, 2).await;
    assert_eq!(
        db.store.load().await.unwrap().unwrap().session_id,
        "voice-session"
    );
    assert_eq!(*state.read().await, GatewayState::Connected);
    let disconnects = voice.disconnects.load(Ordering::Relaxed);
    runner.abort();
    let _ = runner.await;
    assert!(voice.disconnects.load(Ordering::Relaxed) > disconnects);
    mock.task.abort();
    drop(voice);
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn voice_startup_uses_dml_role_without_migration_ledger_access() {
    let db = TestDb::new().await;
    let role = format!("{}_runtime", db.schema);
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE ROLE {role} NOLOGIN; GRANT USAGE ON SCHEMA {schema} TO {role}; GRANT SELECT, INSERT, UPDATE, DELETE ON {schema}.voice_creators, {schema}.voice_rooms TO {role}", schema = db.schema
    ))).execute(&db.admin).await.unwrap();
    // This is the authorized test-container identity throughout. Startup role
    // options exercise restricted DML privileges, not a second credential.
    let mut url = database_guard::test_options().to_url_lossy();
    url.query_pairs_mut().append_pair(
        "options",
        &format!("-c search_path={} -c role={role}", db.schema),
    );
    let restricted = PgPoolOptions::new()
        .max_connections(1)
        .connect(url.as_str())
        .await
        .unwrap();
    let current: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(&restricted)
        .await
        .unwrap();
    let ledger = sqlx::query("SELECT * FROM _sqlx_migrations")
        .execute(&restricted)
        .await;
    let config = two_bot_core::Config {
        discord_token: Some(two_bot_core::Secret::new(TOKEN.to_owned())),
        database_url: Some(two_bot_core::Secret::new(url.to_string())),
        listen_addr: "127.0.0.1:0".to_owned(),
        guild_id: Some(100),
    };
    let sink = crate::gateway::build_voice_runtime(&config, true).await;
    let enabled = sink.is_some();
    drop(sink);
    restricted.close().await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP OWNED BY {role}; DROP ROLE {role}"
    )))
    .execute(&db.admin)
    .await
    .unwrap();
    db.close().await;
    assert_eq!(current, role);
    assert!(
        matches!(ledger, Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("42501"))
    );
    assert!(
        enabled,
        "DML-only voice startup must not attempt migrations"
    );
}
