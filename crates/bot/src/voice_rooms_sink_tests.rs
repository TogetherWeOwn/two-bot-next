use super::*;
use tokio::sync::Notify;
use twilight_model::gateway::payload::incoming::{
    ChannelCreate, ChannelDelete, ChannelUpdate, GuildCreate, Ready, VoiceStateUpdate,
};
use two_bot_discord::MemPipeline;

#[path = "voice_access_role_slot_scope_tests.rs"]
mod access_role_slot_scope;
#[path = "voice_actor_init_jitter_adoption_tests.rs"]
mod init_jitter_adoption;
#[path = "voice_actor_init_recovery_tests.rs"]
mod init_recovery;

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
    async fn logging_settings(&self, guild: u64) -> Result<LoggingSettings, StoreError> {
        self.as_ref().logging_settings(guild).await
    }
    async fn save_logging_settings(
        &self,
        guild: u64,
        settings: &LoggingSettings,
    ) -> Result<(), StoreError> {
        self.as_ref().save_logging_settings(guild, settings).await
    }
    async fn add_creator(&self, creator: &CreatorChannel) -> Result<(), StoreError> {
        self.as_ref().add_creator(creator).await
    }
    async fn creator_for(
        &self,
        guild: u64,
        channel: u64,
    ) -> Result<Option<CreatorChannel>, StoreError> {
        self.as_ref().creator_for(guild, channel).await
    }
    async fn persist(&self, room: &VoiceRoom) -> Result<(), StoreError> {
        self.as_ref().persist(room).await
    }
    async fn custom_names(&self, guild: u64) -> Result<Vec<(u64, String)>, StoreError> {
        self.as_ref().custom_names(guild).await
    }
    async fn save_custom_name(
        &self,
        guild: u64,
        channel: u64,
        custom_name: Option<&str>,
    ) -> Result<bool, StoreError> {
        self.as_ref()
            .save_custom_name(guild, channel, custom_name)
            .await
    }
    async fn pending_owner_grants(&self, guild: u64) -> Result<Vec<u64>, StoreError> {
        self.as_ref().pending_owner_grants(guild).await
    }
    async fn prepare_owner_grants(
        &self,
        room: &VoiceRoom,
        previous_owner_id: u64,
    ) -> Result<OwnerGrantIntent, StoreError> {
        self.as_ref()
            .prepare_owner_grants(room, previous_owner_id)
            .await
    }
    async fn update_ownership(&self, room: &VoiceRoom, revision: &str) -> Result<bool, StoreError> {
        self.as_ref().update_ownership(room, revision).await
    }
    async fn forget(&self, guild: u64, channel: u64) -> Result<(), StoreError> {
        self.as_ref().forget(guild, channel).await
    }
    async fn claim_create(
        &self,
        guild: u64,
        user: u64,
        leaving: &[u64],
        config: &CreateAdmissionConfig,
        now_secs: i64,
    ) -> Result<CreateClaim, StoreError> {
        self.as_ref()
            .claim_create(guild, user, leaving, config, now_secs)
            .await
    }
    async fn bind_create_channel(
        &self,
        guild: u64,
        reservation_id: &str,
        channel: u64,
    ) -> Result<(), StoreError> {
        self.as_ref()
            .bind_create_channel(guild, reservation_id, channel)
            .await
    }
    async fn orphaned_create_channels(&self, guild: u64) -> Result<Vec<(String, u64)>, StoreError> {
        self.as_ref().orphaned_create_channels(guild).await
    }
    async fn persist_create(
        &self,
        reservation_id: &str,
        room: &VoiceRoom,
    ) -> Result<(), StoreError> {
        self.as_ref().persist_create(reservation_id, room).await
    }
    async fn settle_create(&self, reservation_id: &str) -> Result<bool, StoreError> {
        self.as_ref().settle_create(reservation_id).await
    }
    async fn config_snapshot(&self, guild: u64) -> Result<VoiceConfiguration, StoreError> {
        self.as_ref().config_snapshot(guild).await
    }
    async fn config_apply(
        &self,
        guild: u64,
        config: &VoiceConfiguration,
        expected: &VoiceConfiguration,
    ) -> Result<(), StoreError> {
        self.as_ref().config_apply(guild, config, expected).await
    }

    async fn companions(&self, guild: u64) -> Result<Vec<TextCompanion>, StoreError> {
        self.as_ref().companions(guild).await
    }
    async fn add_companion(&self, companion: &TextCompanion) -> Result<bool, StoreError> {
        self.as_ref().add_companion(companion).await
    }
    async fn remove_companion(
        &self,
        guild: u64,
        room: u64,
    ) -> Result<Option<TextCompanion>, StoreError> {
        self.as_ref().remove_companion(guild, room).await
    }
    async fn record_kick_audit(&self, rows: &[KickAuditRow]) -> Result<(), StoreError> {
        self.as_ref().record_kick_audit(rows).await
    }
    async fn privacy(&self, guild: u64) -> Result<BTreeMap<u64, PrivacyRecord>, StoreError> {
        self.as_ref().privacy(guild).await
    }
    async fn save_privacy(
        &self,
        guild: u64,
        room: u64,
        record: &PrivacyRecord,
    ) -> Result<bool, StoreError> {
        self.as_ref().save_privacy(guild, room, record).await
    }
    async fn join_grants(&self, guild: u64) -> Result<BTreeMap<u64, BTreeSet<u64>>, StoreError> {
        self.as_ref().join_grants(guild).await
    }
    async fn save_join_grant(
        &self,
        guild: u64,
        room: u64,
        member: u64,
    ) -> Result<JoinGrantIntent, StoreError> {
        self.as_ref().save_join_grant(guild, room, member).await
    }
    async fn remove_join_grant(
        &self,
        guild: u64,
        room: u64,
        member: u64,
    ) -> Result<(), StoreError> {
        self.as_ref().remove_join_grant(guild, room, member).await
    }
    async fn record_event_voice_join(&self, channel: u64, member: u64) -> Result<(), StoreError> {
        self.as_ref().record_event_voice_join(channel, member).await
    }
    async fn record_event_voice_leave(&self, channel: u64, member: u64) -> Result<(), StoreError> {
        self.as_ref()
            .record_event_voice_leave(channel, member)
            .await
    }
    async fn event_voice_guild_bindings(
        &self,
        guild: u64,
    ) -> Result<Vec<EventVoiceBinding>, StoreError> {
        self.as_ref().event_voice_guild_bindings(guild).await
    }
    async fn flush_event_voice_presence(
        &self,
        event_id: &str,
        instance: OffsetDateTime,
        occupants: &[u64],
    ) -> Result<u64, StoreError> {
        self.as_ref()
            .flush_event_voice_presence(event_id, instance, occupants)
            .await
    }
    async fn close_event_voice_spans_absent(
        &self,
        event_id: &str,
        instance: OffsetDateTime,
        present: &[u64],
    ) -> Result<u64, StoreError> {
        self.as_ref()
            .close_event_voice_spans_absent(event_id, instance, present)
            .await
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
    async fn disconnect(
        &self,
        guild: u64,
        member: u64,
        guard: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        self.http.disconnect(guild, member, guard).await
    }
    async fn deny_connect(
        &self,
        channel: u64,
        member: u64,
        guard: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        self.http.deny_connect(channel, member, guard).await
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
    async fn set_user_limit(
        &self,
        channel: u64,
        user_limit: u32,
        guard: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        self.http.set_user_limit(channel, user_limit, guard).await
    }
    async fn download_attachment(
        &self,
        url: &str,
        max_bytes: usize,
    ) -> Result<Vec<u8>, RoomHttpError> {
        self.http.download_attachment(url, max_bytes).await
    }

    async fn create_companion(
        &self,
        plan: &TextChannelPlan,
        bot_id: Snowflake,
        guard: WriteGuard,
    ) -> Result<Channel, RoomHttpError> {
        self.http.create_companion(plan, bot_id, guard).await
    }
    async fn grant_companion_view(
        &self,
        text_channel_id: Snowflake,
        member_id: Snowflake,
        guard: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        self.http
            .grant_companion_view(text_channel_id, member_id, guard)
            .await
    }
    async fn revoke_companion_view(
        &self,
        text_channel_id: Snowflake,
        member_id: Snowflake,
        guard: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        self.http
            .revoke_companion_view(text_channel_id, member_id, guard)
            .await
    }
}

type Runtime = VoiceRuntime<Arc<Store>, GatedHttp>;

fn runtime(store: Arc<Store>, http: GatedHttp) -> Runtime {
    // These races predate the empty-room grace and run in real time, so they
    // shorten it; the grace itself is pinned by the paused-time guard tests.
    VoiceRuntime::new(
        move || (store.clone(), http.clone()),
        Duration::from_millis(10),
        true,
    )
    .with_empty_grace(Duration::ZERO)
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

fn game_activity_delta(
    member_id: u64,
    game: Option<&str>,
    observed_at_ms: i64,
) -> two_bot_core::game_activity::PresenceDelta {
    let activities = game
        .into_iter()
        .map(|name| two_bot_core::game_activity::ActivitySnapshot {
            activity_type: two_bot_core::game_activity::ActivityType::Playing,
            name: name.to_owned(),
            application_id: None,
            details: None,
            state: None,
            party_size: None,
            assets: false,
            button_labels: Vec::new(),
            button_urls: Vec::new(),
            stream_url: None,
        })
        .collect();
    two_bot_core::game_activity::PresenceDelta {
        user_key: member_id.to_string(),
        observed_at_ms,
        activities,
    }
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
    // This guild grants Manage Channels through @everyone; the invoker has
    // no additional role IDs, especially none missing from the guild cache.
    let interaction = with_roles(
        voice_interaction(
            Some(command_data(
                "create",
                vec![command_option("name", "new creator")],
            )),
            Some(Permissions::MANAGE_CHANNELS),
            true,
        ),
        &[],
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
    // The presence hook records the join ahead of room handling.
    assert_eq!(
        *trace.lock().unwrap(),
        [
            "create",
            "add_creator:500",
            "evjoin:500:300",
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

#[tokio::test]
async fn poisoned_live_lock_does_not_restart_the_gateway() {
    let (store, http, _) = fixture();
    let runtime = runtime(store, http);
    let pipeline = MemPipeline::for_replay();
    bootstrap(&runtime, &pipeline, &[]);
    let actor = runtime.live_actor(GUILD).unwrap();
    // Poison the room lock from another thread: panic while holding the
    // write guard, as a crashed holder would.
    let live = actor.live.clone();
    std::thread::spawn(move || {
        let _guard = live.write_state();
        panic!("poison the live voice lock for test");
    })
    .join()
    .expect_err("poisoning thread must panic");
    // The dispatch worker (`gateway.rs` `voice.handle(...)`) still returns.
    feed(&runtime, &pipeline, voice_event(MEMBER, Some(CREATOR)));
    feed(&runtime, &pipeline, voice_event(MEMBER, None));
    // The recovered lock still serves reads and writes.
    assert!(actor.live.read_state().ready);
    actor.live.disconnect();
    assert!(!actor.live.read_state().ready);
    status(&runtime).await;
}

#[tokio::test]
async fn actor_load_failure_bumps_counter_and_arms_warn_throttle() {
    let series = "two_bot_db_errors_total{op=\"other\"} ";
    let before = global_series(series);
    observe_voice_actor_load_failure(GUILD, &StoreError::Unavailable);
    observe_voice_actor_load_failure(GUILD, &StoreError::Unavailable);
    // Global counters are monotonic and shared with parallel tests, so assert
    // a lower bound, not an exact value (see `global_series` convention).
    assert!(
        global_series(series) >= before + 2,
        "two load failures bump the `other` counter"
    );
    // The first failure arms the throttle timestamp; the immediate second
    // call stays silent. (Warn text itself is covered by the observability
    // event catalog and its conformance allowlist entry.)
    assert_ne!(
        super::super::VOICE_ACTOR_LOAD_WARN_LAST_SECS.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "first failure arms the warn throttle"
    );
}

#[tokio::test]
async fn guild_create_closes_open_voice_span_for_absent_member() {
    const MIN: i64 = 60_000;
    let (store, http, _) = fixture();
    let runtime = runtime(store, http);
    let pipeline = MemPipeline::for_replay();
    let collector = runtime.game_activity_collector();
    let now = i64::try_from(wall_ms()).unwrap_or(i64::MAX);
    let start = now - 10 * MIN;
    collector.observe_presence(GUILD, game_activity_delta(MEMBER, Some("Apex"), start));
    collector.observe_join(GUILD, MEMBER, start);

    feed(&runtime, &pipeline, guild_event(&[], permissions()));

    let stop = now + 10 * MIN;
    collector.observe_presence(GUILD, game_activity_delta(MEMBER, None, stop));
    collector.observe_leave(GUILD, MEMBER, stop);
    let rows = collector.flush(GUILD, 15 * MIN, stop);
    let apex = rows
        .iter()
        .find(|row| row.game.as_deref() == Some("Apex"))
        .expect("Apex row");
    assert!(
        (9 * MIN..15 * MIN).contains(&apex.in_voice_ms),
        "the guild snapshot closes at reconnect, not the later presence stop: {:?}",
        apex
    );
}

#[test]
fn disconnect_closes_open_voice_spans() {
    const MIN: i64 = 60_000;
    let (store, http, _) = fixture();
    let runtime = runtime(store, http);
    let collector = runtime.game_activity_collector();
    let now = i64::try_from(wall_ms()).unwrap_or(i64::MAX);
    let start = now - 10 * MIN;
    collector.observe_presence(GUILD, game_activity_delta(MEMBER, Some("Apex"), start));
    collector.observe_join(GUILD, MEMBER, start);

    runtime.disconnect();

    let stop = now + 10 * MIN;
    collector.observe_presence(GUILD, game_activity_delta(MEMBER, None, stop));
    collector.observe_leave(GUILD, MEMBER, stop);
    let rows = collector.flush(GUILD, 15 * MIN, stop);
    let apex = rows
        .iter()
        .find(|row| row.game.as_deref() == Some("Apex"))
        .expect("Apex row");
    assert!(
        (9 * MIN..15 * MIN).contains(&apex.in_voice_ms),
        "voice time ends at disconnect rather than covering the reconnect gap: {:?}",
        apex
    );
}

#[tokio::test]
async fn warm_resume_reopens_voice_span_for_cached_member() {
    const MIN: i64 = 60_000;
    let (store, http, _) = fixture();
    let runtime = runtime(store, http);
    let pipeline = MemPipeline::for_replay();
    bootstrap(&runtime, &pipeline, &[]);
    let collector = runtime.game_activity_collector();
    let now = i64::try_from(wall_ms()).unwrap_or(i64::MAX);
    let start = now - 10 * MIN;
    collector.observe_presence(GUILD, game_activity_delta(MEMBER, Some("Apex"), start));
    // The span opens at `start` for determinism; the voice frame populates
    // the gateway cache (its idempotent join keeps the earlier join).
    collector.observe_join(GUILD, MEMBER, start);
    feed(&runtime, &pipeline, voice_event(MEMBER, Some(CREATOR)));

    runtime.disconnect();
    feed(&runtime, &pipeline, Event::Resumed);

    let stop = now + 10 * MIN;
    collector.observe_presence(GUILD, game_activity_delta(MEMBER, None, stop));
    collector.observe_leave(GUILD, MEMBER, stop);
    let rows = collector.flush(GUILD, 15 * MIN, stop);
    let apex = rows
        .iter()
        .find(|row| row.game.as_deref() == Some("Apex"))
        .expect("Apex row");
    assert_eq!(
        apex.total_play_ms,
        20 * MIN,
        "play spans the resume, got {apex:?}"
    );
    assert!(
        (19 * MIN..21 * MIN).contains(&apex.in_voice_ms),
        "voice resumes after RESUMED instead of staying shut: {apex:?}",
    );
}

fn live_ready(runtime: &Runtime) -> bool {
    runtime
        .live_actor(GUILD)
        .expect("guild actor")
        .live
        .read_state()
        .ready
}

async fn wait_moves(trace: &Trace, member: u64, count: usize) {
    let prefix = format!("move:{member}:");
    tokio::time::timeout(Duration::from_secs(5), async {
        while trace
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| entry.starts_with(&prefix))
            .count()
            < count
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("member {member} moved {count} time(s); trace: {trace:?}"));
}

/// Production regression (build e18e9938, 2026-10-17 22:15Z to 10-18 14:38Z):
/// once the guild's last voice member left, Twilight dropped the guild's
/// voice-state set from the cache. The next transport loss cleared `ready`,
/// and the warm RESUME that followed found no complete cached snapshot, so
/// nothing was published: the live view stayed `ready == false` and every
/// later creator join was silently ignored (no accept, no refusal, flat
/// `two_bot_voice_joins_received_total`) until a restart re-ran
/// GUILD_CREATE. Every RESUMED must restore the live view and joins must
/// keep producing tickets, however many RESUMEs the process has seen.
#[tokio::test]
async fn warm_resume_after_guild_voice_empties_keeps_moving_creator_joins() {
    const LAST_LEAVER: u64 = 301;
    const PLAIN_VOICE: u64 = 700;
    let (store, http, trace) = fixture();
    let runtime = runtime(store, http);
    let pipeline = MemPipeline::for_replay();
    bootstrap(&runtime, &pipeline, &[PLAIN_VOICE]);
    assert!(live_ready(&runtime));
    // The guild's last voice member leaves; Twilight removes the now-empty
    // guild entry from its voice-state index (re-created on the next join).
    feed(
        &runtime,
        &pipeline,
        voice_event(LAST_LEAVER, Some(PLAIN_VOICE)),
    );
    feed(&runtime, &pipeline, voice_event(LAST_LEAVER, None));
    assert!(
        pipeline
            .cache()
            .guild_voice_states(Id::new(GUILD))
            .is_none(),
        "fixture precondition: Twilight drops the empty guild voice-state set"
    );
    // Production saw ~10 transport losses, each followed by a warm RESUME.
    for resume in 1..=3 {
        runtime.disconnect();
        assert!(!live_ready(&runtime));
        feed(&runtime, &pipeline, Event::Resumed);
        assert!(
            live_ready(&runtime),
            "live view must be ready again after warm RESUMED #{resume}"
        );
    }
    // The owner joins "Create a Lobby": the join must be ticketed and moved.
    feed(&runtime, &pipeline, voice_event(MEMBER, Some(CREATOR)));
    wait_moves(&trace, MEMBER, 1).await;
}

fn self_heals() -> u64 {
    let text = two_bot_core::metrics::global().render(None);
    let line = text
        .lines()
        .find(|line| line.starts_with("two_bot_voice_live_self_heal_total "))
        .expect("self-heal counter rendered");
    line.rsplit_once(' ').unwrap().1.parse().unwrap()
}

/// Self-correction: whatever leaves a live view not ready on a connected
/// gateway (here a RESUMED whose cache cannot build a snapshot), the view
/// reports its not-ready age for the watchdog, and the next dispatch
/// republishes from the cache so the join that carries it is still moved.
#[tokio::test]
async fn connected_gateway_republishes_a_not_ready_view_and_moves_the_join() {
    let (store, http, trace) = fixture();
    let runtime = runtime(store, http);
    let pipeline = MemPipeline::for_replay();
    bootstrap(&runtime, &pipeline, &[]);
    let live = runtime.live_actor(GUILD).expect("guild actor").live;
    let later = |secs| tokio::time::Instant::now() + Duration::from_secs(secs);
    assert_eq!(live.not_ready_secs(later(90)), 0);
    runtime.disconnect();
    // RESUMED handled against a cache that cannot build this guild.
    let cold = MemPipeline::for_replay();
    runtime.handle(&Event::Resumed, cold.cache());
    assert!(!live_ready(&runtime));
    let stamped = later(90);
    assert!(
        live.not_ready_secs(stamped) >= 89,
        "not ready on a connected gateway must be visible to the watchdog"
    );
    // A dispatch whose cache still cannot build the guild keeps the view
    // not ready, and the retry does not restart the watchdog clock.
    runtime.handle(&voice_event(301, None), cold.cache());
    assert!(!live_ready(&runtime));
    assert!(live.not_ready_secs(stamped) >= 89);
    let healed = self_heals();
    // The join's own dispatch heals the view and is still ticketed.
    feed(&runtime, &pipeline, voice_event(MEMBER, Some(CREATOR)));
    assert!(live_ready(&runtime));
    assert_eq!(live.not_ready_secs(later(90)), 0);
    assert!(self_heals() > healed);
    wait_moves(&trace, MEMBER, 1).await;
}

/// The heal is only for a connected gateway: during an outage a not-ready
/// view is correct (cached occupancy is not authoritative until RESUMED),
/// so no dispatch republishes it and the watchdog clock stays at zero.
#[tokio::test]
async fn transport_down_never_heals_or_counts_the_live_view() {
    let (store, http, trace) = fixture();
    let runtime = runtime(store, http);
    let pipeline = MemPipeline::for_replay();
    bootstrap(&runtime, &pipeline, &[]);
    let live = runtime.live_actor(GUILD).expect("guild actor").live;
    runtime.disconnect();
    feed(&runtime, &pipeline, voice_event(MEMBER, Some(CREATOR)));
    assert!(!live_ready(&runtime));
    assert_eq!(
        live.not_ready_secs(tokio::time::Instant::now() + Duration::from_secs(90)),
        0
    );
    status(&runtime).await;
    assert!(!trace
        .lock()
        .unwrap()
        .iter()
        .any(|entry| entry == "create" || entry.starts_with("move:")));
}

#[test]
fn guild_delete_drops_unflushed_game_activity_state() {
    let (store, http, _) = fixture();
    let runtime = runtime(store, http);
    let pipeline = MemPipeline::for_replay();
    let collector = runtime.game_activity_collector();
    let now = i64::try_from(wall_ms()).unwrap_or(i64::MAX);
    collector.observe_presence(GUILD, game_activity_delta(MEMBER, Some("Apex"), now));
    collector.observe_join(GUILD, MEMBER, now);

    let deleted = Event::GuildDelete(
        serde_json::from_value::<twilight_model::gateway::payload::incoming::GuildDelete>(json!({
            "id": GUILD.to_string(), "unavailable": false
        }))
        .unwrap(),
    );
    feed(&runtime, &pipeline, deleted);
    assert!(
        collector.guilds().is_empty(),
        "guild removal must discard all buffered and open collector state"
    );
}

#[tokio::test]
async fn guild_create_seeds_display_names_for_restarted_rooms() {
    let (store, http, _) = fixture();
    let runtime = runtime(store, http);
    let pipeline = MemPipeline::for_replay();
    feed(&runtime, &pipeline, ready_event());
    let mut guild: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/voice_guild.json")).unwrap();
    guild["roles"][0]["permissions"] = json!(permissions().bits().to_string());
    guild["members"].as_array_mut().unwrap().push(json!({
        "user": {"id": MEMBER.to_string(), "username": "alex", "global_name": "Alex", "discriminator": "0"},
        "roles": [], "deaf": false, "mute": false, "flags": 0, "joined_at": NOW
    }));
    // Without the presence intent Discord's GuildCreate carries the members
    // in voice together with their voice states.
    guild["voice_states"] = json!([{
        "channel_id": CREATOR.to_string(), "user_id": MEMBER.to_string(), "session_id": "voice",
        "deaf": false, "mute": false, "self_deaf": false, "self_mute": false,
        "self_video": false, "suppress": false
    }]);
    feed(
        &runtime,
        &pipeline,
        Event::GuildCreate(Box::new(GuildCreate::Available(
            serde_json::from_value(guild).unwrap(),
        ))),
    );
    let (_, directory) = tokio::time::timeout(Duration::from_secs(5), runtime.naming_probe(GUILD))
        .await
        .expect("probe deadline")
        .expect("actor exists");
    assert!(directory.knows(MEMBER));
}

/// Snapshot occupants never sent a transition (`publish_snapshot` only
/// sends `Reconcile`): the actor bootstrap flushes them join-only, so a
/// member already sitting in a channel at GuildCreate owns an open span.
/// Without the flush no join is recorded for the snapshot occupant.
#[tokio::test]
async fn guild_create_snapshot_occupant_gets_a_join_span_at_bootstrap() {
    let (store, http, trace) = fixture();
    let runtime = runtime(store, http);
    let pipeline = MemPipeline::for_replay();
    feed(&runtime, &pipeline, ready_event());
    let mut guild: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/voice_guild.json")).unwrap();
    guild["roles"][0]["permissions"] = json!(permissions().bits().to_string());
    guild["members"].as_array_mut().unwrap().push(json!({
        "user": {"id": MEMBER.to_string(), "username": "alex", "global_name": "Alex", "discriminator": "0"},
        "roles": [], "deaf": false, "mute": false, "flags": 0, "joined_at": NOW
    }));
    guild["voice_states"] = json!([{
        "channel_id": CREATOR.to_string(), "user_id": MEMBER.to_string(), "session_id": "voice",
        "deaf": false, "mute": false, "self_deaf": false, "self_mute": false,
        "self_video": false, "suppress": false
    }]);
    feed(
        &runtime,
        &pipeline,
        Event::GuildCreate(Box::new(GuildCreate::Available(
            serde_json::from_value(guild).unwrap(),
        ))),
    );
    wait_trace(&trace, &format!("evjoin:{CREATOR}:{MEMBER}")).await;
}

/// The actor maintenance tick drives the event-voice guild repair through
/// the real actor loop: with a live binding seeded, the first maintenance
/// tick (immediate, because the repair starts due) flushes the occupant
/// snapshot and closes absent spans. Without the wiring — the snapshot
/// collection, the due gate or the call itself — the repair markers never
/// appear.
#[tokio::test]
async fn maintenance_tick_repairs_event_voice_spans_through_the_actor() {
    let (store, http, trace) = fixture();
    store.ev_bindings.lock().unwrap().push(EventVoiceBinding {
        channel: CREATOR,
        event_id: "ev-repair".to_owned(),
        instance: OffsetDateTime::now_utc(),
    });
    let runtime = runtime(store, http);
    let pipeline = MemPipeline::for_replay();
    feed(&runtime, &pipeline, ready_event());
    let mut guild: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/voice_guild.json")).unwrap();
    guild["roles"][0]["permissions"] = json!(permissions().bits().to_string());
    guild["members"].as_array_mut().unwrap().push(json!({
        "user": {"id": MEMBER.to_string(), "username": "alex", "global_name": "Alex", "discriminator": "0"},
        "roles": [], "deaf": false, "mute": false, "flags": 0, "joined_at": NOW
    }));
    guild["voice_states"] = json!([{
        "channel_id": CREATOR.to_string(), "user_id": MEMBER.to_string(), "session_id": "voice",
        "deaf": false, "mute": false, "self_deaf": false, "self_mute": false,
        "self_video": false, "suppress": false
    }]);
    feed(
        &runtime,
        &pipeline,
        Event::GuildCreate(Box::new(GuildCreate::Available(
            serde_json::from_value(guild).unwrap(),
        ))),
    );
    // MEMBER sits in the bound channel, so the repair sees one present
    // occupant: both the flush and the close-absent run through the loop.
    wait_trace(&trace, "evrepair_flush:ev-repair:1").await;
    wait_trace(&trace, "evrepair_close:ev-repair:1").await;
}

async fn creator_ids(runtime: &Runtime) -> Vec<u64> {
    tokio::time::timeout(Duration::from_secs(5), runtime.creators_probe(GUILD))
        .await
        .expect("probe deadline")
        .expect("actor exists")
}

async fn settle_first_merge(runtime: &Runtime) {
    // Let the spawned actor load and run its first (interval) merge, so a
    // row pushed afterwards can only converge via a flagged refresh.
    for _ in 0..50 {
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        if creator_ids(runtime).await.contains(&CREATOR) {
            return;
        }
    }
    panic!("the actor must load its creator view before the test row lands");
}

/// Drive one channel event through the gateway sink and prove the worker
/// merges a store row its view was missing: without the event hook (or its
/// caller) the row stays absent past the debounce. The probe reads the
/// worker view directly, never a join, so the join read-through cannot
/// mask a dead event hook.
async fn channel_event_merges_a_missing_creator_row(make_event: impl FnOnce(Channel) -> Event) {
    const ADDED: u64 = 201;
    let (store, http, _) = fixture();
    let runtime = runtime(store.clone(), http);
    let pipeline = MemPipeline::for_replay();
    bootstrap(&runtime, &pipeline, &[]);
    settle_first_merge(&runtime).await;
    store
        .creators
        .lock()
        .unwrap()
        .push(CreatorChannel::new(GUILD, ADDED));
    assert!(
        !creator_ids(&runtime).await.contains(&ADDED),
        "the pushed row must start outside the worker view"
    );
    feed(
        &runtime,
        &pipeline,
        make_event(channel(ADDED, 2, Some(CATEGORY))),
    );
    for _ in 0..120 {
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
    }
    assert!(
        creator_ids(&runtime).await.contains(&ADDED),
        "a channel event must merge a missing creator row past the debounce"
    );
}

#[tokio::test(start_paused = true)]
async fn sink_channel_create_merges_a_missing_creator_row() {
    channel_event_merges_a_missing_creator_row(|channel| {
        Event::ChannelCreate(Box::new(ChannelCreate(channel)))
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn sink_channel_update_merges_a_missing_creator_row() {
    channel_event_merges_a_missing_creator_row(|channel| {
        Event::ChannelUpdate(Box::new(ChannelUpdate(channel)))
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn sink_channel_delete_merges_a_missing_creator_row() {
    channel_event_merges_a_missing_creator_row(|channel| {
        Event::ChannelDelete(Box::new(ChannelDelete(channel)))
    })
    .await;
}

/// A nickname change renders into `@@owner@@` rooms: the gateway sink must
/// forward it as a display update so the worker's name directory (which the
/// background custom-name re-render reads) learns the fresh nickname.
#[tokio::test(start_paused = true)]
async fn member_nick_update_reaches_the_worker_name_directory() {
    let (store, http, _) = fixture();
    let runtime = runtime(store, http);
    let pipeline = MemPipeline::for_replay();
    bootstrap(&runtime, &pipeline, &[]);
    let _ = status(&runtime).await;
    feed(
        &runtime,
        &pipeline,
        Event::MemberUpdate(Box::new(
            serde_json::from_value(json!({
                "guild_id": GUILD.to_string(), "roles": [],
                "nick": "Bob",
                "user": {"id": MEMBER.to_string(), "username": "alex", "discriminator": "0"}
            }))
            .unwrap(),
        )),
    );
    let mut directory = None;
    for _ in 0..50 {
        let probed = tokio::time::timeout(Duration::from_secs(5), runtime.naming_probe(GUILD))
            .await
            .expect("probe deadline")
            .expect("actor exists")
            .1;
        if format!("{probed:?}").contains("Bob") {
            directory = Some(probed);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let directory = directory.expect("worker directory must learn Bob from the nick update");
    assert!(directory.knows(MEMBER));
}
