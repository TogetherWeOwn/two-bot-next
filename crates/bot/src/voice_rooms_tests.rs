use super::*;
use serde_json::json;
use std::sync::Mutex;

const GUILD: u64 = 100;
const CREATOR: u64 = 200;
const CATEGORY: u64 = 400;
const MEMBER: u64 = 300;
const NOW: &str = "2026-09-30T00:00:00.000Z";
type Trace = Arc<Mutex<Vec<String>>>;
type Hook = Arc<dyn Fn() + Send + Sync>;

fn permissions() -> Permissions {
    Permissions::VIEW_CHANNEL
        | Permissions::CONNECT
        | Permissions::MANAGE_CHANNELS
        | Permissions::MOVE_MEMBERS
        | Permissions::MANAGE_ROLES
}

fn role(permissions: Permissions) -> Role {
    serde_json::from_value(json!({
        "id": "100", "name": "everyone", "color": 0, "hoist": false,
        "managed": false, "mentionable": false, "position": 0,
        "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null },
        "permissions": permissions.bits().to_string(), "flags": 0
    }))
    .unwrap()
}

fn channel(id: u64, kind: u8, parent: Option<u64>) -> Channel {
    serde_json::from_value(json!({
        "id": id.to_string(), "guild_id": GUILD.to_string(), "type": kind,
        "name": "room", "parent_id": parent.map(|id| id.to_string()),
        "bitrate": 96000, "rtc_region": "rotterdam", "video_quality_mode": 2,
        "nsfw": true, "user_limit": 8, "permission_overwrites": []
    }))
    .unwrap()
}

fn snapshot(extra: &[u64], members: Vec<VoiceMember>) -> GuildSnapshot {
    let mut channels = vec![
        channel(CREATOR, 2, Some(CATEGORY)),
        channel(CATEGORY, 4, None),
    ];
    channels.extend(extra.iter().map(|id| channel(*id, 2, Some(CATEGORY))));
    GuildSnapshot {
        channels,
        members,
        bot: BotAccess {
            member_id: 999,
            guild_owner_id: 998,
            member_roles: vec![],
            roles: vec![role(permissions())],
        },
    }
}

fn room(channel_id: u64) -> VoiceRoom {
    VoiceRoom::from_spec(
        NewRoomSpec {
            guild_id: GUILD,
            creator_channel_id: CREATOR,
            owner_id: MEMBER,
            seed: 7,
            created_at: NOW.to_owned(),
        },
        channel_id,
    )
}

struct Store {
    trace: Trace,
    creators: Vec<CreatorChannel>,
    rooms: Mutex<HashMap<u64, VoiceRoom>>,
    persist_error: Option<StoreError>,
    forget_errors: Mutex<VecDeque<StoreError>>,
    after_persist: Option<Hook>,
}

impl Store {
    fn new(trace: Trace) -> Self {
        Self {
            trace,
            creators: vec![CreatorChannel::new(GUILD, CREATOR)],
            rooms: Mutex::new(HashMap::new()),
            persist_error: None,
            forget_errors: Mutex::new(VecDeque::new()),
            after_persist: None,
        }
    }
}

impl RoomPersistence for Store {
    async fn creators(&self, _: u64) -> Result<Vec<CreatorChannel>, StoreError> {
        Ok(self.creators.clone())
    }
    async fn rooms(&self, _: u64) -> Result<Vec<VoiceRoom>, StoreError> {
        Ok(self.rooms.lock().unwrap().values().cloned().collect())
    }
    async fn persist(&self, room: &VoiceRoom) -> Result<(), StoreError> {
        self.trace
            .lock()
            .unwrap()
            .push(format!("persist:{}", room.channel_id));
        if let Some(hook) = &self.after_persist {
            hook();
        }
        if let Some(error) = self.persist_error {
            return Err(error);
        }
        self.rooms
            .lock()
            .unwrap()
            .insert(room.channel_id, room.clone());
        Ok(())
    }
    async fn forget(&self, _: u64, channel: u64) -> Result<(), StoreError> {
        self.trace.lock().unwrap().push(format!("forget:{channel}"));
        if let Some(error) = self.forget_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        self.rooms.lock().unwrap().remove(&channel);
        Ok(())
    }
}

struct Http {
    trace: Trace,
    next_id: Mutex<u64>,
    create_errors: Mutex<VecDeque<RoomHttpError>>,
    move_errors: Mutex<VecDeque<RoomHttpError>>,
    delete_errors: Mutex<VecDeque<RoomHttpError>>,
    rename_errors: Mutex<VecDeque<RoomHttpError>>,
    created_attributes: Mutex<Vec<RoomChannelAttributes>>,
    after_create: Option<Hook>,
    before_move: Option<Hook>,
    before_delete: Option<Hook>,
}

impl Http {
    fn new(trace: Trace) -> Self {
        Self {
            trace,
            next_id: Mutex::new(500),
            create_errors: Mutex::new(VecDeque::new()),
            move_errors: Mutex::new(VecDeque::new()),
            delete_errors: Mutex::new(VecDeque::new()),
            rename_errors: Mutex::new(VecDeque::new()),
            created_attributes: Mutex::new(Vec::new()),
            after_create: None,
            before_move: None,
            before_delete: None,
        }
    }
}

impl RoomWrites for Http {
    async fn create(
        &self,
        _: u64,
        _: &str,
        attributes: &RoomChannelAttributes,
        guard: WriteGuard,
    ) -> Result<Channel, RoomHttpError> {
        if !guard() {
            return Err(RoomHttpError::Cancelled);
        }
        self.trace.lock().unwrap().push("create".to_owned());
        if let Some(error) = self.create_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        let id = {
            let mut id = self.next_id.lock().unwrap();
            let next = *id;
            *id += 1;
            next
        };
        self.created_attributes
            .lock()
            .unwrap()
            .push(attributes.clone());
        let mut result = channel(id, 2, attributes.parent_id);
        result.permission_overwrites = Some(attributes.overwrites.clone());
        if let Some(hook) = &self.after_create {
            hook();
        }
        Ok(result)
    }
    async fn move_member(
        &self,
        _: u64,
        member: u64,
        channel: u64,
        guard: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        if let Some(hook) = &self.before_move {
            hook();
        }
        if !guard() {
            return Err(RoomHttpError::Cancelled);
        }
        self.trace
            .lock()
            .unwrap()
            .push(format!("move:{member}:{channel}"));
        match self.move_errors.lock().unwrap().pop_front() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    async fn delete(&self, channel: u64, guard: WriteGuard) -> Result<(), RoomHttpError> {
        if let Some(hook) = &self.before_delete {
            hook();
        }
        if !guard() {
            return Err(RoomHttpError::Cancelled);
        }
        self.trace.lock().unwrap().push(format!("delete:{channel}"));
        match self.delete_errors.lock().unwrap().pop_front() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    async fn rename(&self, channel: u64, name: &str) -> Result<(), RoomHttpError> {
        self.trace
            .lock()
            .unwrap()
            .push(format!("rename:{channel}:{name}"));
        match self.rename_errors.lock().unwrap().pop_front() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

fn fixture() -> (LiveGuild, Store, Http, Trace) {
    let trace = Trace::default();
    let live = LiveGuild::new(GUILD);
    live.publish(snapshot(&[], vec![]));
    (
        live,
        Store::new(trace.clone()),
        Http::new(trace.clone()),
        trace,
    )
}

fn join(worker: &mut GuildRoomWorker<Store, Http>, member: u64) -> JoinTicket {
    let ticket = worker
        .live
        .voice_update(member, Some(CREATOR), Some(false))
        .unwrap();
    assert!(worker.accept_join(ticket, "new room".to_owned(), 7, NOW.to_owned()));
    ticket
}

async fn dispatch(worker: &mut GuildRoomWorker<Store, Http>, now: u64) {
    assert!(worker.dispatch_one(now).await);
}

#[tokio::test]
async fn worker_can_run_on_a_separate_tokio_task() {
    let (live, store, http, _) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    let task = tokio::spawn(async move {
        worker.dispatch_one(0).await;
        worker.dispatch_one(1).await;
        worker
    });
    assert_eq!(task.await.unwrap().tracked().len(), 1);
}

#[tokio::test]
async fn duplicate_voice_frames_and_duplicate_tickets_do_not_create_twice() {
    let (live, store, http, trace) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let ticket = join(&mut worker, MEMBER);
    assert!(worker
        .live
        .voice_update(MEMBER, Some(CREATOR), None)
        .is_none());
    assert!(!worker.accept_join(ticket, "duplicate".to_owned(), 8, NOW.to_owned()));
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    worker.reconcile();
    assert!(!worker.dispatch_one(2).await);
    assert_eq!(
        *trace.lock().unwrap(),
        ["create", "persist:500", "move:300:500"]
    );
    let tracked = &worker.tracked()[&500];
    assert_eq!(tracked.owner_id, tracked.original_creator_id);
    assert_eq!(tracked.name_seed, 7);
    assert_eq!(
        worker.http.created_attributes.lock().unwrap()[0].user_limit,
        8
    );
}

#[tokio::test]
async fn two_simultaneous_members_get_distinct_persisted_rooms() {
    let (live, store, http, trace) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    join(&mut worker, MEMBER + 1);
    for time in 0..4 {
        dispatch(&mut worker, time).await;
    }
    assert_eq!(
        *trace.lock().unwrap(),
        [
            "create",
            "persist:500",
            "create",
            "persist:501",
            "move:300:500",
            "move:301:501"
        ]
    );
    assert_eq!(worker.tracked().len(), 2);
}

#[tokio::test]
async fn leave_and_return_during_create_cannot_move_the_new_join_into_the_old_room() {
    let (live, store, mut http, trace) = fixture();
    let shared = live.clone();
    http.after_create = Some(Arc::new(move || {
        shared.voice_update(MEMBER, None, None);
        shared.voice_update(MEMBER, Some(CREATOR), None);
    }));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["create", "persist:500", "delete:500", "forget:500"]
    );
    assert!(worker.tracked().is_empty());
}

#[tokio::test]
async fn persistence_failure_compensates_the_exact_new_channel_and_never_moves() {
    let (live, mut store, http, trace) = fixture();
    store.persist_error = Some(StoreError::Unavailable);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["create", "persist:500", "delete:500", "forget:500"]
    );
    assert_eq!(worker.failures().len(), 1);
}

#[tokio::test]
async fn voice_change_during_persistence_compensates_instead_of_moving() {
    let (live, mut store, http, trace) = fixture();
    let shared = live.clone();
    store.after_persist = Some(Arc::new(move || {
        shared.voice_update(MEMBER, None, None);
    }));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["create", "persist:500", "delete:500", "forget:500"]
    );
}

#[tokio::test]
async fn failed_move_deletes_even_when_move_members_was_revoked() {
    let (live, store, http, trace) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    worker
        .live
        .inner
        .write()
        .unwrap()
        .bot
        .as_mut()
        .unwrap()
        .roles = vec![role(permissions() & !Permissions::MOVE_MEMBERS)];
    worker.reconcile();
    dispatch(&mut worker, 1).await;
    worker.reconcile();
    dispatch(&mut worker, 2).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["create", "persist:500", "delete:500", "forget:500"]
    );
}

#[tokio::test]
async fn stale_move_guard_is_rechecked_after_transport_wait() {
    let (live, store, mut http, trace) = fixture();
    let shared = live.clone();
    http.before_move = Some(Arc::new(move || {
        shared.voice_update(MEMBER, None, None);
    }));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    for time in 0..3 {
        dispatch(&mut worker, time).await;
    }
    assert_eq!(
        *trace.lock().unwrap(),
        ["create", "persist:500", "delete:500", "forget:500"]
    );
}

#[tokio::test]
async fn reconnect_only_prunes_tracked_empty_channels_and_counts_unknown_members_as_human() {
    let (live, store, http, trace) = fixture();
    for id in [500, 501, 502, 503] {
        store.rooms.lock().unwrap().insert(id, room(id));
    }
    live.publish(snapshot(
        &[500, 502, 503, 900],
        vec![
            VoiceMember {
                member_id: 301,
                channel_id: 500,
                bot: Some(true),
            },
            VoiceMember {
                member_id: 302,
                channel_id: 502,
                bot: None,
            },
            VoiceMember {
                member_id: 303,
                channel_id: 503,
                bot: Some(false),
            },
        ],
    ));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.reconcile();
    for time in 0..2 {
        dispatch(&mut worker, time).await;
    }
    let calls = trace.lock().unwrap();
    assert!(calls.contains(&"delete:500".to_owned()));
    assert!(calls.contains(&"forget:501".to_owned()));
    assert!(!calls.contains(&"delete:501".to_owned()));
    assert!(!calls.iter().any(|call| call.contains("900")));
    assert_eq!(worker.tracked().len(), 2);
}

#[tokio::test]
async fn not_ready_and_disconnected_snapshots_never_allow_destructive_reconciliation() {
    let (_, store, http, trace) = fixture();
    store.rooms.lock().unwrap().insert(500, room(500));
    let live = LiveGuild::new(GUILD);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.reconcile();
    assert!(!worker.dispatch_one(0).await);
    worker.live.publish(snapshot(&[500], vec![]));
    worker.reconcile();
    worker.live.disconnect();
    assert!(!worker.dispatch_one(1).await);
    worker.live.publish(snapshot(
        &[500],
        vec![VoiceMember {
            member_id: MEMBER,
            channel_id: 500,
            bot: Some(false),
        }],
    ));
    dispatch(&mut worker, 2).await;
    assert!(trace.lock().unwrap().is_empty());
    assert_eq!(worker.tracked().len(), 1);
}

#[tokio::test]
async fn occupants_arriving_during_delete_backoff_cancel_the_write() {
    let (live, store, mut http, trace) = fixture();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    let shared = live.clone();
    http.before_delete = Some(Arc::new(move || {
        shared.voice_update(MEMBER, Some(500), Some(false));
    }));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.reconcile();
    dispatch(&mut worker, 0).await;
    assert!(trace.lock().unwrap().is_empty());
    assert_eq!(worker.tracked().len(), 1);
}

#[tokio::test]
async fn delete_403_suspends_without_a_retry_storm_and_refresh_resumes() {
    let (live, store, http, trace) = fixture();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    http.delete_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::AccessDenied);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.reconcile();
    dispatch(&mut worker, 0).await;
    for time in [3000, 6000, 10000] {
        worker.reconcile();
        assert!(!worker.dispatch_one(time).await);
    }
    assert_eq!(*trace.lock().unwrap(), ["delete:500"]);
    worker.live.publish(snapshot(&[500], vec![]));
    worker.reconcile();
    dispatch(&mut worker, 10000).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["delete:500", "delete:500", "forget:500"]
    );
}

#[tokio::test]
async fn create_429_waits_exactly_and_does_not_consume_the_failure_budget() {
    let (live, store, http, trace) = fixture();
    http.create_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::RateLimited {
            retry_after_ms: 1500,
            global: false,
        });
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 100).await;
    assert!(!worker.dispatch_one(1599).await);
    dispatch(&mut worker, 1600).await;
    dispatch(&mut worker, 1601).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["create", "create", "persist:500", "move:300:500"]
    );
    assert!(worker.failures().is_empty());
}

#[tokio::test]
async fn unknown_create_is_never_retried_or_inferred_from_untracked_channels() {
    let (live, store, http, trace) = fixture();
    http.create_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::UnknownOutcome);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    worker.live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    worker.reconcile();
    assert!(!worker.dispatch_one(60000).await);
    assert_eq!(*trace.lock().unwrap(), ["create"]);
    assert!(worker.tracked().is_empty());
}

#[tokio::test]
async fn uncertain_move_waits_for_occupancy_evidence_and_last_human_leave_deletes() {
    let (live, store, http, trace) = fixture();
    http.move_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::UnknownOutcome);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    worker.reconcile();
    assert!(!worker.dispatch_one(2).await);
    worker.live.voice_update(MEMBER, Some(500), Some(false));
    worker.reconcile();
    assert!(!worker.dispatch_one(3).await);
    worker.live.voice_update(MEMBER, None, Some(false));
    worker.reconcile();
    dispatch(&mut worker, 4).await;
    assert_eq!(
        *trace.lock().unwrap(),
        [
            "create",
            "persist:500",
            "move:300:500",
            "delete:500",
            "forget:500"
        ]
    );
}

#[tokio::test]
async fn database_forget_failure_retries_only_sql_after_successful_delete() {
    let (live, store, http, trace) = fixture();
    store.rooms.lock().unwrap().insert(500, room(500));
    store
        .forget_errors
        .lock()
        .unwrap()
        .push_back(StoreError::Unavailable);
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.reconcile();
    dispatch(&mut worker, 0).await;
    assert_eq!(worker.tracked().len(), 1);
    dispatch(&mut worker, 3000).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["delete:500", "forget:500", "forget:500"]
    );
    assert!(worker.tracked().is_empty());
}

#[tokio::test]
async fn credential_refusal_stops_all_further_writes() {
    for database in [false, true] {
        let (live, mut store, http, trace) = fixture();
        if database {
            store.persist_error = Some(StoreError::CredentialRefused);
        } else {
            http.create_errors
                .lock()
                .unwrap()
                .push_back(RoomHttpError::Unauthorized);
        }
        let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
        join(&mut worker, MEMBER);
        join(&mut worker, MEMBER + 1);
        dispatch(&mut worker, 0).await;
        assert!(worker.halted());
        let before = trace.lock().unwrap().len();
        worker.reconcile();
        assert!(!worker.dispatch_one(60000).await);
        assert_eq!(trace.lock().unwrap().len(), before);
    }
}

#[tokio::test]
async fn rename_backoff_never_delays_deleting_another_room_and_keeps_latest_name() {
    let (live, store, http, trace) = fixture();
    for id in [500, 501] {
        store.rooms.lock().unwrap().insert(id, room(id));
        live.upsert_channel(channel(id, 2, Some(CATEGORY)));
    }
    live.voice_update(MEMBER, Some(500), Some(false));
    live.voice_update(MEMBER + 1, Some(501), Some(false));
    http.rename_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::RateLimited {
            retry_after_ms: 600000,
            global: false,
        });
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.propose_name(500, "old", 0);
    dispatch(&mut worker, 0).await;
    worker.propose_name(500, "latest", 1);
    worker.live.voice_update(MEMBER + 1, None, None);
    worker.reconcile();
    dispatch(&mut worker, 2).await;
    // Coalescing replaces the retry's stale name once its budget is due.
    assert!(!worker.dispatch_one(300000).await);
    dispatch(&mut worker, 600000).await;
    assert_eq!(
        *trace.lock().unwrap(),
        [
            "rename:500:old",
            "delete:501",
            "forget:501",
            "rename:500:latest"
        ]
    );
}

#[tokio::test]
async fn successive_creates_reserve_category_slots_before_gateway_echoes() {
    let (live, store, http, trace) = fixture();
    for id in 1000..1048 {
        live.upsert_channel(channel(id, 0, Some(CATEGORY)));
    }
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    join(&mut worker, MEMBER + 1);
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    dispatch(&mut worker, 2).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["create", "persist:500", "move:300:500"]
    );
    assert!(
        matches!(worker.failures().back(), Some(LifecycleFailure::CategoryFull { message, .. }) if message.contains("second creator channel in another category"))
    );
}

#[tokio::test]
async fn lacking_manage_roles_uses_category_overwrites_not_creator_overwrites() {
    use twilight_model::channel::permission_overwrite::PermissionOverwriteType;
    let (live, store, http, _) = fixture();
    let overwrite = PermissionOverwrite {
        id: Id::new(GUILD),
        kind: PermissionOverwriteType::Role,
        allow: Permissions::empty(),
        deny: Permissions::SEND_MESSAGES,
    };
    let mut category = channel(CATEGORY, 4, None);
    category.permission_overwrites = Some(vec![overwrite]);
    live.upsert_channel(category);
    live.inner.write().unwrap().bot.as_mut().unwrap().roles =
        vec![role(permissions() & !Permissions::MANAGE_ROLES)];
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    assert_eq!(
        worker.http.created_attributes.lock().unwrap()[0].overwrites,
        [overwrite]
    );
}

#[test]
fn create_channel_plan_trims_and_validates_names() {
    assert_eq!(
        decide_create_channel(CreateChannelRequest {
            guild_id: GUILD,
            name: "  lobby  ".to_owned()
        }),
        CreateChannelPlan::Create {
            guild_id: GUILD,
            name: "lobby".to_owned()
        }
    );
    assert!(matches!(
        decide_create_channel(CreateChannelRequest {
            guild_id: GUILD,
            name: "   ".to_owned()
        }),
        CreateChannelPlan::Refuse { .. }
    ));
    assert!(matches!(
        decide_create_channel(CreateChannelRequest {
            guild_id: GUILD,
            name: "x".repeat(101)
        }),
        CreateChannelPlan::Refuse { .. }
    ));
    let hundred = "y".repeat(100);
    assert_eq!(
        decide_create_channel(CreateChannelRequest {
            guild_id: GUILD,
            name: hundred.clone()
        }),
        CreateChannelPlan::Create {
            guild_id: GUILD,
            name: hundred
        }
    );
}

#[test]
fn room_name_uses_display_plus_suffix_and_truncates() {
    assert_eq!(room_name("ava"), "ava's room");
    let named = room_name(&"x".repeat(200));
    assert_eq!(named.chars().count(), 100);
    assert!(named.ends_with("'s room"));
}

#[test]
fn setup_panel_describes_empty_running_healthy_guild() {
    let panel = setup_panel(&SetupSummary {
        guild_id: GUILD,
        creators: vec![],
        tracked_rooms: 0,
        failures: vec![],
        halted: false,
    });
    assert_eq!(panel.title, "Voice rooms");
    assert!(panel.description.contains("running"));
    assert!(panel.description.contains("/create"));
    assert!(panel.description.contains("No recent failures"));
}

#[test]
fn setup_panel_lists_creators_failures_and_halt() {
    let mut creator = CreatorChannel::new(GUILD, CREATOR);
    creator.position = RoomPosition::Below;
    let panel = setup_panel(&SetupSummary {
        guild_id: GUILD,
        creators: vec![creator],
        tracked_rooms: 2,
        failures: vec!["create <#200>: rate limited".to_owned()],
        halted: true,
    });
    assert!(panel.description.contains("paused"));
    assert!(panel.description.contains(&format!("<#{CREATOR}>")));
    assert!(panel.description.contains("below"));
    assert!(panel.description.contains("Tracked rooms: 2"));
    assert!(panel.description.contains("rate limited"));
}

#[test]
fn voice_command_set_is_gated_on_two_voice() {
    let on = VoiceGates { enabled: true };
    let names: Vec<_> = voice_command_set(&on)
        .iter()
        .map(|definition| definition.name.clone())
        .collect();
    assert_eq!(names, ["create", "setup"]);
    let off = VoiceGates::from_map(&Default::default());
    assert!(voice_command_set(&off).is_empty());
}

fn test_runtime(trace: Trace) -> VoiceRuntime<Store, Http> {
    VoiceRuntime::new(
        move || (Store::new(trace.clone()), Http::new(trace.clone())),
        Duration::from_millis(10),
        true,
    )
}

#[test]
fn disabled_runtime_ignores_snapshots_and_frames() {
    let trace = Trace::default();
    let runtime = VoiceRuntime::new(
        move || (Store::new(trace.clone()), Http::new(trace.clone())),
        Duration::from_millis(10),
        false,
    );
    assert!(!runtime.publish_snapshot(GUILD, snapshot(&[], vec![])));
    assert!(!runtime.voice_frame(GUILD, MEMBER, Some(CREATOR), Some(false), "x".to_owned()));
}

#[test]
fn runtime_drops_voice_frames_before_first_snapshot() {
    let trace = Trace::default();
    let runtime = test_runtime(trace);
    assert!(!runtime.voice_frame(GUILD, MEMBER, Some(CREATOR), Some(false), "x".to_owned()));
}

#[tokio::test]
async fn runtime_remove_guild_drops_actor() {
    let trace = Trace::default();
    let runtime = test_runtime(trace);
    assert!(runtime.publish_snapshot(GUILD, snapshot(&[], vec![])));
    runtime.remove_guild(GUILD);
    assert!(!runtime.voice_frame(GUILD, MEMBER, Some(CREATOR), Some(false), "x".to_owned()));
}

#[tokio::test]
async fn runtime_actor_creates_persists_and_moves_room() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    assert!(runtime.publish_snapshot(GUILD, snapshot(&[], vec![])));
    assert!(runtime.voice_frame(
        GUILD,
        MEMBER,
        Some(CREATOR),
        Some(false),
        "ava's room".to_owned()
    ));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if trace
            .lock()
            .unwrap()
            .iter()
            .any(|entry| entry.starts_with("move:"))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "actor did not drive the lifecycle: {:?}",
            trace.lock().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        *trace.lock().unwrap(),
        ["create", "persist:500", "move:300:500"]
    );
}
