use super::*;
use tokio::sync::Notify;
use twilight_model::gateway::payload::incoming::{GuildCreate, Ready, VoiceStateUpdate};
use two_bot_discord::MemPipeline;

impl RoomPersistence for Arc<Store> {
    async fn creators(&self, guild: u64) -> Result<Vec<CreatorChannel>, StoreError> {
        self.as_ref().creators(guild).await
    }
    async fn rooms(&self, guild: u64) -> Result<Vec<VoiceRoom>, StoreError> {
        self.as_ref().rooms(guild).await
    }
    async fn access_controls(&self, guild: u64) -> Result<AccessControls, StoreError> {
        self.as_ref().access_controls(guild).await
    }
    async fn save_access_controls(
        &self,
        guild: u64,
        controls: &AccessControls,
    ) -> Result<(), StoreError> {
        self.as_ref().save_access_controls(guild, controls).await
    }
    async fn add_creator(&self, creator: &CreatorChannel) -> Result<(), StoreError> {
        self.as_ref().add_creator(creator).await
    }
    async fn persist(&self, room: &VoiceRoom) -> Result<(), StoreError> {
        self.as_ref().persist(room).await
    }
    async fn forget(&self, guild: u64, channel: u64) -> Result<(), StoreError> {
        self.as_ref().forget(guild, channel).await
    }
}

#[derive(Clone, Default)]
struct Gate {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl Gate {
    async fn wait(&self) {
        self.entered.notify_one();
        self.release.notified().await;
    }
    async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.entered.notified())
            .await
            .expect("write entered");
    }
}

#[derive(Clone)]
struct GatedHttp {
    http: Arc<Http>,
    create: Option<Gate>,
    delete: Option<Gate>,
}

impl RoomWrites for GatedHttp {
    async fn create(
        &self,
        guild: u64,
        name: &str,
        attributes: &RoomChannelAttributes,
        guard: WriteGuard,
    ) -> Result<Channel, RoomHttpError> {
        let result = self.http.create(guild, name, attributes, guard).await;
        if let Some(gate) = &self.create {
            gate.wait().await;
        }
        result
    }
    async fn move_member(
        &self,
        guild: u64,
        member: u64,
        channel: u64,
        guard: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        self.http.move_member(guild, member, channel, guard).await
    }
    async fn delete(&self, channel: u64, guard: WriteGuard) -> Result<(), RoomHttpError> {
        if let Some(gate) = &self.delete {
            gate.wait().await;
        }
        self.http.delete(channel, guard).await
    }
    async fn rename(&self, channel: u64, name: &str) -> Result<(), RoomHttpError> {
        self.http.rename(channel, name).await
    }
}

type Runtime = VoiceRuntime<Arc<Store>, GatedHttp>;

fn runtime(store: Arc<Store>, http: GatedHttp) -> Runtime {
    VoiceRuntime::new(
        move || (store.clone(), http.clone()),
        Duration::from_millis(10),
        true,
    )
}

fn ready_event() -> Event {
    Event::Ready(serde_json::from_value::<Ready>(json!({
        "v": 10, "user": {"id": "999", "username": "mock-bot", "discriminator": "0", "bot": true, "mfa_enabled": false},
        "session_id": "session", "resume_gateway_url": "wss://gateway.discord.gg",
        "guilds": [{"id": "100", "unavailable": true}], "application": {"id": "1111", "flags": 0}
    })).unwrap())
}

fn guild_event(extra: &[u64], allowed: Permissions) -> Event {
    let mut guild: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/voice_guild.json")).unwrap();
    guild["roles"][0]["permissions"] = json!(allowed.bits().to_string());
    for id in extra {
        guild["channels"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::to_value(channel(*id, 2, Some(CATEGORY))).unwrap());
    }
    Event::GuildCreate(Box::new(GuildCreate::Available(
        serde_json::from_value(guild).unwrap(),
    )))
}

fn voice_event(member: u64, channel: Option<u64>) -> Event {
    Event::VoiceStateUpdate(Box::new(serde_json::from_value::<VoiceStateUpdate>(json!({
        "guild_id": GUILD.to_string(), "user_id": member.to_string(),
        "channel_id": channel.map(|id| id.to_string()), "session_id": "voice",
        "deaf": false, "mute": false, "self_deaf": false, "self_mute": false,
        "self_video": false, "suppress": false,
        "member": {"user": {"id": member.to_string(), "username": "human", "discriminator": "0"},
            "roles": [], "deaf": false, "mute": false, "flags": 0, "joined_at": NOW}
    })).unwrap()))
}

fn feed(runtime: &Runtime, pipeline: &MemPipeline, event: Event) {
    pipeline.handle(&event);
    runtime.handle(&event, pipeline.cache());
}

fn bootstrap(runtime: &Runtime, pipeline: &MemPipeline, extra: &[u64]) {
    feed(runtime, pipeline, ready_event());
    feed(runtime, pipeline, guild_event(extra, permissions()));
}

async fn status(runtime: &Runtime) -> WorkerStatus {
    tokio::time::timeout(Duration::from_secs(5), runtime.worker_status(GUILD))
        .await
        .expect("actor status deadline")
        .expect("actor exists")
}

async fn wait_trace(trace: &Trace, entry: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if trace.lock().unwrap().iter().any(|value| value == entry) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("lifecycle trace deadline");
}

fn fixture() -> (Arc<Store>, GatedHttp, Trace) {
    let trace = Trace::default();
    let store = Arc::new(Store::new(trace.clone()));
    let http = GatedHttp {
        http: Arc::new(Http::new(trace.clone())),
        create: None,
        delete: None,
    };
    (store, http, trace)
}

#[tokio::test]
async fn production_cache_ready_and_guild_create_start_actor() {
    let (store, http, _) = fixture();
    let runtime = runtime(store, http);
    let pipeline = MemPipeline::for_replay();
    assert!(runtime.needs_bootstrap(pipeline.cache()));
    bootstrap(&runtime, &pipeline, &[]);
    assert!(!runtime.needs_bootstrap(pipeline.cache()));
    assert_eq!(status(&runtime).await.tracked_rooms, 0);
    let actor = runtime.live_actor(GUILD).unwrap();
    assert!(actor.live.inner.read().unwrap().ready);
    assert!(can_manage_room(
        actor.live.inner.read().unwrap().permissions(GUILD, CREATOR)
    ));
}

#[tokio::test]
async fn sink_leave_during_awaited_create_cancels_move() {
    let (store, mut http, trace) = fixture();
    let gate = Gate::default();
    http.create = Some(gate.clone());
    let runtime = runtime(store, http);
    let pipeline = MemPipeline::for_replay();
    bootstrap(&runtime, &pipeline, &[]);
    feed(&runtime, &pipeline, voice_event(MEMBER, Some(CREATOR)));
    gate.entered().await;
    feed(&runtime, &pipeline, voice_event(MEMBER, None));
    gate.release.notify_one();
    wait_trace(&trace, "delete:500").await;
    assert!(!trace
        .lock()
        .unwrap()
        .iter()
        .any(|entry| entry.starts_with("move:")));
}

#[tokio::test]
async fn sink_join_during_awaited_delete_preserves_occupied_room() {
    let (store, mut http, trace) = fixture();
    store.rooms.lock().unwrap().insert(700, room(700));
    let gate = Gate::default();
    http.delete = Some(gate.clone());
    let runtime = runtime(store, http);
    let pipeline = MemPipeline::for_replay();
    bootstrap(&runtime, &pipeline, &[700]);
    gate.entered().await;
    feed(&runtime, &pipeline, voice_event(MEMBER, Some(700)));
    gate.release.notify_one();
    assert_eq!(status(&runtime).await.tracked_rooms, 1);
    assert!(!trace
        .lock()
        .unwrap()
        .iter()
        .any(|entry| entry.starts_with("delete:")));
}

#[tokio::test]
async fn outage_invalidates_an_in_flight_delete_before_send() {
    let (store, mut http, trace) = fixture();
    store.rooms.lock().unwrap().insert(700, room(700));
    let gate = Gate::default();
    http.delete = Some(gate.clone());
    let runtime = runtime(store, http);
    let pipeline = MemPipeline::for_replay();
    bootstrap(&runtime, &pipeline, &[700]);
    gate.entered().await;
    runtime.disconnect();
    gate.release.notify_one();
    assert_eq!(status(&runtime).await.tracked_rooms, 1);
    assert!(trace.lock().unwrap().is_empty());
    assert!(
        !runtime
            .live_actor(GUILD)
            .unwrap()
            .live
            .inner
            .read()
            .unwrap()
            .ready
    );
}

#[tokio::test]
async fn outage_holds_delete_backoff_until_warm_resume() {
    let (store, http, trace) = fixture();
    store.rooms.lock().unwrap().insert(700, room(700));
    http.http
        .delete_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::RateLimited {
            retry_after_ms: 40,
            global: false,
        });
    let runtime = runtime(store, http);
    let pipeline = MemPipeline::for_replay();
    bootstrap(&runtime, &pipeline, &[700]);
    wait_trace(&trace, "delete:700").await;
    runtime.disconnect();
    status(&runtime).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        trace
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| *entry == "delete:700")
            .count(),
        1
    );
    feed(&runtime, &pipeline, Event::Resumed);
    wait_trace(&trace, "forget:700").await;
    assert_eq!(
        trace
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| *entry == "delete:700")
            .count(),
        2
    );
}

#[tokio::test]
async fn create_notifies_existing_actor_before_next_join() {
    let (store, http, trace) = fixture();
    let runtime = runtime(store, http);
    let pipeline = MemPipeline::for_replay();
    bootstrap(&runtime, &pipeline, &[]);
    status(&runtime).await;
    let interaction = voice_interaction(
        Some(command_data(
            "create",
            vec![command_option("name", "new creator")],
        )),
        Some(Permissions::MANAGE_CHANNELS),
        true,
    );
    let seen = Arc::new(Mutex::new(None));
    let writer = seen.clone();
    assert!(
        handle_voice_interaction(&runtime, &interaction, |response| {
            *writer.lock().unwrap() = Some(response);
            async {}
        })
        .await
    );
    assert!(response_text(seen.lock().unwrap().as_ref().unwrap()).contains("Created <#500>"));
    feed(&runtime, &pipeline, voice_event(MEMBER, Some(500)));
    wait_trace(&trace, "move:300:501").await;
    assert_eq!(
        *trace.lock().unwrap(),
        [
            "create",
            "add_creator:500",
            "create",
            "persist:501",
            "move:300:501"
        ]
    );
}

fn bot_roles_event(roles: &[u64]) -> Event {
    Event::MemberUpdate(Box::new(serde_json::from_value(json!({
        "guild_id": GUILD.to_string(), "roles": roles.iter().map(u64::to_string).collect::<Vec<_>>(),
        "user": {"id": "999", "username": "mock-bot", "discriminator": "0", "bot": true}
    })).unwrap()))
}

#[tokio::test]
async fn bot_member_role_assignment_resumes_room_and_removal_revokes_access() {
    let (store, http, trace) = fixture();
    store.rooms.lock().unwrap().insert(700, room(700));
    let runtime = runtime(store, http);
    let pipeline = MemPipeline::for_replay();
    feed(&runtime, &pipeline, ready_event());
    let Event::GuildCreate(mut event) = guild_event(&[700], Permissions::empty()) else {
        unreachable!()
    };
    let GuildCreate::Available(guild) = event.as_mut() else {
        unreachable!()
    };
    let mut extra_role = role(permissions());
    extra_role.id = Id::new(555);
    guild.roles.push(extra_role);
    feed(&runtime, &pipeline, Event::GuildCreate(event));
    assert_eq!(status(&runtime).await.tracked_rooms, 1);
    assert!(trace.lock().unwrap().is_empty());
    feed(&runtime, &pipeline, bot_roles_event(&[555]));
    wait_trace(&trace, "forget:700").await;
    let actor = runtime.live_actor(GUILD).unwrap();
    assert!(can_manage_room(
        actor.live.inner.read().unwrap().permissions(GUILD, CREATOR)
    ));
    feed(&runtime, &pipeline, bot_roles_event(&[]));
    assert!(!can_manage_room(
        actor.live.inner.read().unwrap().permissions(GUILD, CREATOR)
    ));
}
