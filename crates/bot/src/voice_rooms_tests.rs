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
    creators: Mutex<Vec<CreatorChannel>>,
    rooms: Mutex<HashMap<u64, VoiceRoom>>,
    persist_error: Option<StoreError>,
    forget_errors: Mutex<VecDeque<StoreError>>,
    add_creator_error: Mutex<Option<StoreError>>,
    after_persist: Option<Hook>,
}

impl Store {
    fn new(trace: Trace) -> Self {
        Self {
            trace,
            creators: Mutex::new(vec![CreatorChannel::new(GUILD, CREATOR)]),
            rooms: Mutex::new(HashMap::new()),
            persist_error: None,
            forget_errors: Mutex::new(VecDeque::new()),
            add_creator_error: Mutex::new(None),
            after_persist: None,
        }
    }
}

impl RoomPersistence for Store {
    async fn creators(&self, _: u64) -> Result<Vec<CreatorChannel>, StoreError> {
        Ok(self.creators.lock().unwrap().clone())
    }
    async fn rooms(&self, _: u64) -> Result<Vec<VoiceRoom>, StoreError> {
        Ok(self.rooms.lock().unwrap().values().cloned().collect())
    }
    async fn add_creator(&self, creator: &CreatorChannel) -> Result<(), StoreError> {
        if let Some(error) = *self.add_creator_error.lock().unwrap() {
            return Err(error);
        }
        self.trace
            .lock()
            .unwrap()
            .push(format!("add_creator:{}", creator.channel_id));
        self.creators.lock().unwrap().push(creator.clone());
        Ok(())
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

// --- S4 `/create` + `/setup` handler tests ----------------------------------

use std::sync::atomic::{AtomicBool, Ordering};
use twilight_model::{
    application::{
        command::CommandType,
        interaction::application_command::{CommandData, CommandDataOption},
    },
    guild::{MemberFlags, PartialMember},
    oauth::ApplicationIntegrationMap,
};

fn member_with(permissions: Option<Permissions>) -> PartialMember {
    PartialMember {
        avatar: None,
        avatar_decoration_data: None,
        banner: None,
        communication_disabled_until: None,
        deaf: false,
        flags: MemberFlags::empty(),
        joined_at: None,
        mute: false,
        nick: None,
        permissions,
        premium_since: None,
        roles: Vec::new(),
        user: None,
    }
}

fn command_option(name: &str, value: &str) -> CommandDataOption {
    CommandDataOption {
        name: name.to_owned(),
        value: CommandOptionValue::String(value.to_owned()),
    }
}

fn command_data(name: &str, options: Vec<CommandDataOption>) -> CommandData {
    CommandData {
        guild_id: None,
        id: Id::new(3),
        name: name.to_owned(),
        kind: CommandType::ChatInput,
        options,
        resolved: None,
        target_id: None,
    }
}

#[allow(deprecated)]
fn voice_interaction(
    command: Option<CommandData>,
    permissions: Option<Permissions>,
    with_guild: bool,
) -> Interaction {
    Interaction {
        app_permissions: None,
        application_id: Id::new(1),
        authorizing_integration_owners: ApplicationIntegrationMap {
            guild: None,
            user: None,
        },
        channel: None,
        channel_id: None,
        context: None,
        data: command.map(|data| InteractionData::ApplicationCommand(Box::new(data))),
        entitlements: Vec::new(),
        guild: None,
        guild_id: if with_guild {
            Some(Id::new(GUILD))
        } else {
            None
        },
        guild_locale: None,
        id: Id::new(2),
        kind: InteractionType::ApplicationCommand,
        locale: None,
        member: if with_guild {
            Some(member_with(permissions))
        } else {
            None
        },
        message: None,
        token: "token".to_owned(),
        user: None,
    }
}

fn response_text(response: &InteractionResponse) -> String {
    response
        .data
        .as_ref()
        .and_then(|data| data.content.clone())
        .unwrap_or_default()
}

async fn handle_capture(
    runtime: &VoiceRuntime<Store, Http>,
    interaction: &Interaction,
) -> (bool, Option<InteractionResponse>) {
    let seen = Arc::new(Mutex::new(None::<InteractionResponse>));
    let writer = seen.clone();
    let owned = handle_voice_interaction(runtime, interaction, |response| {
        *writer.lock().unwrap() = Some(response);
        async {}
    })
    .await;
    let response = seen.lock().unwrap().clone();
    (owned, response)
}

#[test]
fn parse_create_extracts_name_option() {
    let interaction = voice_interaction(
        Some(command_data(
            "create",
            vec![command_option("name", "lobby")],
        )),
        Some(Permissions::MANAGE_CHANNELS),
        true,
    );
    assert_eq!(
        parse_voice_command(&interaction),
        Some(VoiceCommand::Create {
            name: "lobby".to_owned()
        })
    );
    assert_eq!(interaction_guild(&interaction), Some(GUILD));
}

#[test]
fn parse_create_without_name_defaults_blank_for_refusal() {
    let interaction = voice_interaction(
        Some(command_data("create", Vec::new())),
        Some(Permissions::MANAGE_CHANNELS),
        true,
    );
    assert_eq!(
        parse_voice_command(&interaction),
        Some(VoiceCommand::Create {
            name: String::new()
        })
    );
}

#[test]
fn parse_setup_command() {
    let interaction = voice_interaction(Some(command_data("setup", Vec::new())), None, true);
    assert_eq!(parse_voice_command(&interaction), Some(VoiceCommand::Setup));
}

#[test]
fn parse_ignores_unowned_commands() {
    let other = voice_interaction(Some(command_data("other", Vec::new())), None, true);
    assert_eq!(parse_voice_command(&other), None);
    let guildless = voice_interaction(Some(command_data("setup", Vec::new())), None, false);
    assert_eq!(parse_voice_command(&guildless), None);
    assert_eq!(interaction_guild(&guildless), None);
    let mut ping = voice_interaction(
        Some(command_data(
            "create",
            vec![command_option("name", "lobby")],
        )),
        None,
        true,
    );
    ping.kind = InteractionType::Ping;
    ping.data = None;
    assert_eq!(parse_voice_command(&ping), None);
}

#[test]
fn ephemeral_response_is_ephemeral_channel_message() {
    let response = ephemeral_response("hello");
    assert_eq!(
        response.kind,
        InteractionResponseType::ChannelMessageWithSource
    );
    let data = response.data.expect("ephemeral content");
    assert_eq!(data.content.as_deref(), Some("hello"));
    assert_eq!(data.flags, Some(MessageFlags::EPHEMERAL));
}

#[test]
fn may_create_accepts_admin_or_manage_channels() {
    assert!(may_create(Some(Permissions::MANAGE_CHANNELS)));
    assert!(may_create(Some(Permissions::ADMINISTRATOR)));
    assert!(may_create(Some(
        Permissions::ADMINISTRATOR | Permissions::MANAGE_CHANNELS
    )));
    assert!(!may_create(None));
    assert!(!may_create(Some(Permissions::VIEW_CHANNEL)));
}

#[test]
fn failure_line_formats_each_category() {
    assert_eq!(
        failure_line(&LifecycleFailure::CategoryFull {
            creator_id: CREATOR,
            message: "category is full".to_owned(),
        }),
        "create <#200>: category is full"
    );
    assert!(failure_line(&LifecycleFailure::Discord {
        channel_id: 500,
        error: RoomHttpError::AccessDenied,
    })
    .contains("channel <#500>"));
    assert!(failure_line(&LifecycleFailure::Persistence {
        channel_id: Some(500),
        error: StoreError::Unavailable,
    })
    .contains("store <#500>"));
    assert!(failure_line(&LifecycleFailure::Persistence {
        channel_id: None,
        error: StoreError::Unavailable,
    })
    .starts_with("store:"));
}

#[tokio::test]
async fn handle_create_refuses_without_manage_channels() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = voice_interaction(
        Some(command_data(
            "create",
            vec![command_option("name", "lobby")],
        )),
        None,
        true,
    );
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    assert!(response_text(response.as_ref().expect("reply")).contains("Manage Channels"));
    assert!(!trace.lock().unwrap().iter().any(|entry| entry == "create"));
}

#[tokio::test]
async fn handle_create_success_persists_creator() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = voice_interaction(
        Some(command_data(
            "create",
            vec![command_option("name", "lobby")],
        )),
        Some(Permissions::MANAGE_CHANNELS),
        true,
    );
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    let response = response.expect("reply");
    assert_eq!(
        response.kind,
        InteractionResponseType::ChannelMessageWithSource
    );
    assert!(response_text(&response).contains("Created <#500>"));
    assert!(trace.lock().unwrap().contains(&"create".to_owned()));
    assert!(trace
        .lock()
        .unwrap()
        .contains(&"add_creator:500".to_owned()));
}

#[tokio::test]
async fn handle_create_blank_name_refuses_before_rest() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = voice_interaction(
        Some(command_data("create", Vec::new())),
        Some(Permissions::MANAGE_CHANNELS),
        true,
    );
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    assert!(response_text(response.as_ref().expect("reply")).contains("a name"));
    assert!(!trace.lock().unwrap().iter().any(|entry| entry == "create"));
}

#[tokio::test]
async fn handle_setup_lists_seeded_creator() {
    let trace = Trace::default();
    let runtime = test_runtime(trace);
    let interaction = voice_interaction(Some(command_data("setup", Vec::new())), None, true);
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(response.as_ref().expect("reply"));
    assert!(text.contains("Voice rooms"));
    assert!(text.contains(&format!("<#{CREATOR}>")));
}

#[tokio::test]
async fn handle_ignores_non_voice_interactions() {
    let trace = Trace::default();
    let runtime = test_runtime(trace);
    let interaction = voice_interaction(Some(command_data("other", Vec::new())), None, true);
    let called = Arc::new(AtomicBool::new(false));
    let flag = called.clone();
    let owned = handle_voice_interaction(&runtime, &interaction, |_| {
        flag.store(true, Ordering::SeqCst);
        async {}
    })
    .await;
    assert!(!owned);
    assert!(!called.load(Ordering::SeqCst));
}

#[tokio::test]
async fn execute_create_rest_failure_never_touches_store() {
    let trace = Trace::default();
    let store = Store::new(trace.clone());
    let http = Http::new(trace.clone());
    http.create_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::AccessDenied);
    let text = execute_create(&store, &http, GUILD, "lobby").await;
    assert!(text.contains("Manage Channels"));
    assert!(!trace
        .lock()
        .unwrap()
        .iter()
        .any(|entry| entry.starts_with("add_creator")));
}

#[tokio::test]
async fn execute_create_rate_limit_reports_retry_seconds() {
    let trace = Trace::default();
    let store = Store::new(trace.clone());
    let http = Http::new(trace.clone());
    http.create_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::RateLimited {
            retry_after_ms: 1500,
            global: false,
        });
    let text = execute_create(&store, &http, GUILD, "lobby").await;
    assert!(text.contains("2s"));
}

#[tokio::test]
async fn execute_create_store_failure_deletes_channel_as_compensation() {
    let trace = Trace::default();
    let store = Store::new(trace.clone());
    *store.add_creator_error.lock().unwrap() = Some(StoreError::Unavailable);
    let http = Http::new(trace.clone());
    let text = execute_create(&store, &http, GUILD, "lobby").await;
    assert!(text.contains("removed"));
    assert_eq!(*trace.lock().unwrap(), ["create", "delete:500"]);
}

#[tokio::test]
async fn execute_create_store_credential_pause() {
    let trace = Trace::default();
    let store = Store::new(trace.clone());
    *store.add_creator_error.lock().unwrap() = Some(StoreError::CredentialRefused);
    let http = Http::new(trace.clone());
    let text = execute_create(&store, &http, GUILD, "lobby").await;
    assert!(text.contains("paused"));
    assert_eq!(*trace.lock().unwrap(), ["create", "delete:500"]);
}

#[tokio::test]
async fn runtime_worker_status_reports_live_actor() {
    let trace = Trace::default();
    let runtime = test_runtime(trace);
    assert!(runtime.publish_snapshot(GUILD, snapshot(&[], vec![])));
    let status = tokio::time::timeout(Duration::from_secs(5), runtime.worker_status(GUILD))
        .await
        .expect("status reply")
        .expect("live actor");
    assert_eq!(status.tracked_rooms, 0);
    assert!(!status.halted);
}

struct Replies {
    trace: Trace,
    defer_error: Option<RoomHttpError>,
    complete_error: Option<RoomHttpError>,
    completed: Mutex<Vec<InteractionResponse>>,
}

impl Replies {
    fn new(trace: Trace) -> Self {
        Self {
            trace,
            defer_error: None,
            complete_error: None,
            completed: Mutex::new(Vec::new()),
        }
    }
}

impl InteractionReplies for Replies {
    async fn defer(&self, _: &Interaction) -> Result<(), RoomHttpError> {
        self.trace.lock().unwrap().push("defer".to_owned());
        self.defer_error.map_or(Ok(()), Err)
    }

    async fn complete(
        &self,
        _: &Interaction,
        response: InteractionResponse,
    ) -> Result<(), RoomHttpError> {
        self.trace.lock().unwrap().push("complete".to_owned());
        self.completed.lock().unwrap().push(response);
        self.complete_error.map_or(Ok(()), Err)
    }
}

fn create_interaction() -> Interaction {
    voice_interaction(
        Some(command_data(
            "create",
            vec![command_option("name", "lobby")],
        )),
        Some(Permissions::MANAGE_CHANNELS),
        true,
    )
}

#[tokio::test]
async fn responder_defers_before_any_create_and_completes_once() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let replies = Replies::new(trace.clone());
    VoiceResponder::respond(&runtime, &replies, &create_interaction()).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["defer", "create", "add_creator:500", "complete"]
    );
    let completed = replies.completed.lock().unwrap();
    assert_eq!(completed.len(), 1);
    assert!(response_text(&completed[0]).contains("Created <#500>"));
}

#[tokio::test]
async fn responder_failed_or_ambiguous_ack_never_executes_or_retries() {
    for error in [
        RoomHttpError::UnknownOutcome,
        RoomHttpError::Unauthorized,
        RoomHttpError::RateLimited {
            retry_after_ms: 5000,
            global: false,
        },
        RoomHttpError::Rejected {
            status: 400,
            code: 40060,
        },
    ] {
        let trace = Trace::default();
        let runtime = test_runtime(trace.clone());
        let mut replies = Replies::new(trace.clone());
        replies.defer_error = Some(error);
        VoiceResponder::respond(&runtime, &replies, &create_interaction()).await;
        assert_eq!(*trace.lock().unwrap(), ["defer"]);
        assert!(replies.completed.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn responder_completion_failure_does_not_repeat_channel_creation() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let mut replies = Replies::new(trace.clone());
    replies.complete_error = Some(RoomHttpError::UnknownOutcome);
    VoiceResponder::respond(&runtime, &replies, &create_interaction()).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["defer", "create", "add_creator:500", "complete"]
    );
}

#[tokio::test]
async fn gateway_sink_routes_setup_without_blocking_and_ignores_other_commands() {
    use twilight_model::gateway::payload::incoming::InteractionCreate;
    let trace = Trace::default();
    let runtime = Arc::new(test_runtime(trace.clone()));
    let replies = Arc::new(Replies::new(trace.clone()));
    let sink = VoiceResponder::new(runtime, replies.clone());
    let cache = DefaultInMemoryCache::new();
    for (name, with_guild) in [("other", true), ("setup", false)] {
        sink.handle(
            &Event::InteractionCreate(Box::new(InteractionCreate(voice_interaction(
                Some(command_data(name, Vec::new())),
                None,
                with_guild,
            )))),
            &cache,
        );
    }
    tokio::task::yield_now().await;
    assert!(trace.lock().unwrap().is_empty());
    sink.handle(
        &Event::InteractionCreate(Box::new(InteractionCreate(voice_interaction(
            Some(command_data("setup", Vec::new())),
            None,
            true,
        )))),
        &cache,
    );
    // Work has not run synchronously on the gateway loop.
    assert!(trace.lock().unwrap().is_empty());
    tokio::time::timeout(Duration::from_secs(2), async {
        while replies.completed.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(*trace.lock().unwrap(), ["defer", "complete"]);
    assert!(response_text(&replies.completed.lock().unwrap()[0]).contains("Voice rooms"));
}

#[tokio::test]
async fn disabled_gateway_responder_does_not_acknowledge() {
    use twilight_model::gateway::payload::incoming::InteractionCreate;
    let trace = Trace::default();
    let factory_trace = trace.clone();
    let runtime = Arc::new(VoiceRuntime::new(
        move || {
            (
                Store::new(factory_trace.clone()),
                Http::new(factory_trace.clone()),
            )
        },
        Duration::from_millis(250),
        false,
    ));
    let replies = Arc::new(Replies::new(trace.clone()));
    let sink = VoiceResponder::new(runtime, replies);
    sink.handle(
        &Event::InteractionCreate(Box::new(InteractionCreate(create_interaction()))),
        &DefaultInMemoryCache::new(),
    );
    tokio::task::yield_now().await;
    assert!(trace.lock().unwrap().is_empty());
}
