use super::*;
use serde_json::json;
use std::sync::Mutex;

#[path = "voice_rooms_sink_tests.rs"]
mod sink;

const GUILD: u64 = 100;
const CREATOR: u64 = 200;
const CATEGORY: u64 = 400;
const MEMBER: u64 = 300;
const NOW: &str = "2026-09-30T00:00:00.000000+00:00";
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
    role_with(GUILD, permissions)
}

fn role_with(id: u64, permissions: Permissions) -> Role {
    serde_json::from_value(json!({
        "id": id.to_string(), "name": "everyone", "color": 0, "hoist": false,
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
    companions: Mutex<HashMap<(u64, u64), TextCompanion>>,
    persist_error: Option<StoreError>,
    access: Mutex<AccessControls>,
    access_error: Option<StoreError>,
    forget_errors: Mutex<VecDeque<StoreError>>,
    companion_errors: Mutex<VecDeque<StoreError>>,
    add_creator_error: Mutex<Option<StoreError>>,
    after_persist: Option<Hook>,
}

impl Store {
    fn new(trace: Trace) -> Self {
        Self {
            trace,
            creators: Mutex::new(vec![CreatorChannel::new(GUILD, CREATOR)]),
            rooms: Mutex::new(HashMap::new()),
            companions: Mutex::new(HashMap::new()),
            persist_error: None,
            access: Mutex::new(AccessControls::default()),
            access_error: None,
            forget_errors: Mutex::new(VecDeque::new()),
            companion_errors: Mutex::new(VecDeque::new()),
            add_creator_error: Mutex::new(None),
            after_persist: None,
        }
    }
}

impl RoomPersistence for Store {
    async fn creators(&self, _: u64) -> Result<Vec<CreatorChannel>, StoreError> {
        Ok(self.creators.lock().unwrap().clone())
    }
    async fn access_controls(&self, _: u64) -> Result<AccessControls, StoreError> {
        match self.access_error {
            Some(error) => Err(error),
            None => Ok(self.access.lock().unwrap().clone()),
        }
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
        let mut creators = self.creators.lock().unwrap();
        creators.retain(|row| row.channel_id != creator.channel_id);
        creators.push(creator.clone());
        Ok(())
    }
    async fn creator_for(
        &self,
        _: u64,
        channel: u64,
    ) -> Result<Option<CreatorChannel>, StoreError> {
        Ok(self
            .creators
            .lock()
            .unwrap()
            .iter()
            .find(|row| row.channel_id == channel)
            .cloned())
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
    async fn companions(&self, _: u64) -> Result<Vec<TextCompanion>, StoreError> {
        Ok(self.companions.lock().unwrap().values().cloned().collect())
    }
    async fn add_companion(&self, companion: &TextCompanion) -> Result<bool, StoreError> {
        self.trace.lock().unwrap().push(format!(
            "add_companion:{}:{}",
            companion.room_channel_id, companion.text_channel_id
        ));
        if let Some(error) = self.companion_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        Ok(self
            .companions
            .lock()
            .unwrap()
            .insert(
                (companion.guild_id, companion.room_channel_id),
                companion.clone(),
            )
            .is_none())
    }
    async fn remove_companion(
        &self,
        guild: u64,
        room: u64,
    ) -> Result<Option<TextCompanion>, StoreError> {
        self.trace
            .lock()
            .unwrap()
            .push(format!("remove_companion:{room}"));
        Ok(self.companions.lock().unwrap().remove(&(guild, room)))
    }
}

struct Http {
    trace: Trace,
    next_id: Mutex<u64>,
    create_errors: Mutex<VecDeque<RoomHttpError>>,
    move_errors: Mutex<VecDeque<RoomHttpError>>,
    delete_errors: Mutex<VecDeque<RoomHttpError>>,
    rename_errors: Mutex<VecDeque<RoomHttpError>>,
    companion_errors: Mutex<VecDeque<RoomHttpError>>,
    view_errors: Mutex<VecDeque<RoomHttpError>>,
    created_attributes: Mutex<Vec<RoomChannelAttributes>>,
    companion_plans: Mutex<Vec<TextChannelPlan>>,
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
            companion_errors: Mutex::new(VecDeque::new()),
            view_errors: Mutex::new(VecDeque::new()),
            created_attributes: Mutex::new(Vec::new()),
            companion_plans: Mutex::new(Vec::new()),
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
    async fn disconnect(
        &self,
        guild: u64,
        member: u64,
        guard: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        if !guard() {
            return Err(RoomHttpError::Cancelled);
        }
        self.trace
            .lock()
            .unwrap()
            .push(format!("disconnect:{guild}:{member}"));
        Ok(())
    }
    async fn deny_connect(
        &self,
        channel: u64,
        member: u64,
        guard: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        if !guard() {
            return Err(RoomHttpError::Cancelled);
        }
        self.trace
            .lock()
            .unwrap()
            .push(format!("deny:{channel}:{member}"));
        Ok(())
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
    async fn create_companion(
        &self,
        plan: &TextChannelPlan,
        bot_id: Snowflake,
        guard: WriteGuard,
    ) -> Result<Channel, RoomHttpError> {
        if !guard() {
            return Err(RoomHttpError::Cancelled);
        }
        self.trace.lock().unwrap().push(format!(
            "create_companion:{}:{}:{bot_id}",
            plan.room_id, plan.name
        ));
        self.companion_plans.lock().unwrap().push(plan.clone());
        if let Some(error) = self.companion_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        let id = {
            let mut id = self.next_id.lock().unwrap();
            let next = *id;
            *id += 1;
            next
        };
        // Text channel: kind 0, parented to the room's category.
        Ok(channel(id, 0, Some(plan.category_id)))
    }
    async fn grant_companion_view(
        &self,
        text_channel: u64,
        member: u64,
        guard: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        if !guard() {
            return Err(RoomHttpError::Cancelled);
        }
        self.trace
            .lock()
            .unwrap()
            .push(format!("grant:{text_channel}:{member}"));
        match self.view_errors.lock().unwrap().pop_front() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    async fn revoke_companion_view(
        &self,
        text_channel: u64,
        member: u64,
        guard: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        if !guard() {
            return Err(RoomHttpError::Cancelled);
        }
        self.trace
            .lock()
            .unwrap()
            .push(format!("revoke:{text_channel}:{member}"));
        match self.view_errors.lock().unwrap().pop_front() {
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
async fn lacking_manage_roles_creates_without_overrides_so_the_room_syncs_to_its_category() {
    use twilight_model::channel::permission_overwrite::{
        PermissionOverwrite, PermissionOverwriteType,
    };
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
    // The bot cannot set overrides without Manage Roles: none are sent, so
    // Discord syncs the new room to the category it is created in.
    let created = worker.http.created_attributes.lock().unwrap();
    assert_eq!(created[0].parent_id, Some(CATEGORY));
    assert!(created[0].overwrites.is_empty());
}

#[tokio::test]
async fn created_rooms_carry_the_owner_override_and_a_placement_from_the_start() {
    let (live, store, http, _) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    let created = worker.http.created_attributes.lock().unwrap();
    assert!(created[0]
        .overwrites
        .iter()
        .any(|overwrite| overwrite.id.get() == MEMBER));
    assert!(created[0].position.is_some());
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
        store_error: None,
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
        store_error: None,
    });
    assert!(panel.description.contains("paused"));
    assert!(panel.description.contains(&format!("<#{CREATOR}>")));
    assert!(panel.description.contains("below"));
    assert!(panel.description.contains("Tracked rooms: 2"));
    assert!(panel.description.contains("rate limited"));
}

#[test]
fn setup_panel_surfaces_store_errors() {
    let panel = setup_panel(&SetupSummary {
        guild_id: GUILD,
        creators: vec![],
        tracked_rooms: 0,
        failures: vec![],
        halted: false,
        store_error: Some("Unavailable".to_owned()),
    });
    assert!(panel
        .description
        .contains("Could not load creator channels"));
    assert!(!panel.description.contains("No creator channels yet"));
}

#[test]
fn voice_command_set_is_gated_on_two_voice() {
    let on = VoiceGates { enabled: true };
    let names: Vec<_> = voice_command_set(&on)
        .iter()
        .map(|definition| definition.name.clone())
        .collect();
    assert_eq!(names, ["create", "setup", "ping", "invite", "textchannels"]);
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
fn parse_ping_and_invite_commands() {
    for (name, expected) in [
        ("ping", VoiceCommand::Ping),
        ("invite", VoiceCommand::Invite),
    ] {
        let interaction = voice_interaction(Some(command_data(name, Vec::new())), None, true);
        assert_eq!(parse_voice_command(&interaction), Some(expected));
        let guildless = voice_interaction(Some(command_data(name, Vec::new())), None, false);
        assert_eq!(parse_voice_command(&guildless), None);
    }
}

#[test]
fn interaction_latency_counts_from_the_snowflake_timestamp() {
    let created_ms = DISCORD_EPOCH_MS + 1_000_000;
    let id = (created_ms - DISCORD_EPOCH_MS) << 22;
    assert_eq!(interaction_latency_ms(id, created_ms + 42), 42);
    // The worker and sequence bits below the timestamp never count as time.
    assert_eq!(interaction_latency_ms(id | 0x3F_FFFF, created_ms + 42), 42);
    // A host clock behind Discord's saturates instead of underflowing.
    assert_eq!(interaction_latency_ms(id, created_ms - 5), 0);
    assert_eq!(interaction_latency_ms(u64::MAX, 0), 0);
}

#[tokio::test]
async fn ping_replies_ephemerally_without_touching_store_or_http() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = voice_interaction(Some(command_data("ping", Vec::new())), None, true);
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    let response = response.expect("ping reply");
    assert!(response_text(&response).starts_with("Pong! "));
    assert_eq!(
        response.data.as_ref().and_then(|data| data.flags),
        Some(MessageFlags::EPHEMERAL)
    );
    assert!(trace.lock().unwrap().is_empty());
}

#[tokio::test]
async fn invite_renders_the_vanity_code_or_the_fixed_notice() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = voice_interaction(Some(command_data("invite", Vec::new())), None, true);
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    assert_eq!(
        response_text(&response.expect("invite reply")),
        two_bot_core::voice_utilities::NO_INVITE_CONFIGURED
    );
    let seen = Arc::new(Mutex::new(None::<InteractionResponse>));
    let writer = seen.clone();
    handle_voice_interaction_with(&runtime, &interaction, Some("abc-123"), |response| {
        *writer.lock().unwrap() = Some(response);
        async {}
    })
    .await;
    let response = seen.lock().unwrap().clone().expect("invite reply");
    assert_eq!(
        response_text(&response),
        "Join the server: https://discord.gg/abc-123"
    );
    assert!(trace.lock().unwrap().is_empty());
}

fn gated_runtime(
    trace: Trace,
    controls: AccessControls,
    access_error: Option<StoreError>,
) -> VoiceRuntime<Store, Http> {
    VoiceRuntime::new(
        move || {
            let mut store = Store::new(trace.clone());
            *store.access.lock().unwrap() = controls.clone();
            store.access_error = access_error;
            (store, Http::new(trace.clone()))
        },
        Duration::from_millis(10),
        true,
    )
}

fn with_roles(mut interaction: Interaction, roles: &[u64]) -> Interaction {
    interaction.member.as_mut().expect("member").roles =
        roles.iter().map(|role| Id::new(*role)).collect();
    interaction
}

#[tokio::test]
async fn creation_switch_off_refuses_new_rooms() {
    let (live, store, http, _) = fixture();
    *store.access.lock().unwrap() = AccessControls {
        room_creation_enabled: false,
        ..AccessControls::default()
    };
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let ticket = worker
        .live
        .voice_update(MEMBER, Some(CREATOR), Some(false))
        .unwrap();
    assert!(!worker.accept_join(ticket, "new room".to_owned(), 7, NOW.to_owned()));
    assert!(!worker.dispatch_one(0).await);
    assert!(worker.tracked().is_empty());
}

#[tokio::test]
async fn required_role_gates_members_but_never_admins() {
    let controls = AccessControls {
        required_role: Some(9),
        ..AccessControls::default()
    };
    let runtime = gated_runtime(Trace::default(), controls, None);
    let ping = || voice_interaction(Some(command_data("ping", Vec::new())), None, true);

    let (owned, response) = handle_capture(&runtime, &ping()).await;
    assert!(owned);
    assert_eq!(
        response_text(&response.expect("denial")),
        access_denied_text(AccessDenyReason::RequiredRole)
    );

    let (_, response) = handle_capture(&runtime, &with_roles(ping(), &[9])).await;
    assert!(response_text(&response.expect("ping")).starts_with("Pong! "));

    let admin = voice_interaction(
        Some(command_data("ping", Vec::new())),
        Some(Permissions::MANAGE_CHANNELS),
        true,
    );
    let (_, response) = handle_capture(&runtime, &admin).await;
    assert!(response_text(&response.expect("ping")).starts_with("Pong! "));
}

#[tokio::test]
async fn per_command_restriction_applies_only_to_that_command() {
    let controls = AccessControls {
        command_roles: [("invite".to_owned(), vec![7])].into(),
        ..AccessControls::default()
    };
    let runtime = gated_runtime(Trace::default(), controls, None);
    let invite = || voice_interaction(Some(command_data("invite", Vec::new())), None, true);
    let ping = voice_interaction(Some(command_data("ping", Vec::new())), None, true);

    let (_, response) = handle_capture(&runtime, &ping).await;
    assert!(response_text(&response.expect("ping")).starts_with("Pong! "));
    let (_, response) = handle_capture(&runtime, &invite()).await;
    assert_eq!(
        response_text(&response.expect("denial")),
        access_denied_text(AccessDenyReason::CommandRestricted)
    );
    let (_, response) = handle_capture(&runtime, &with_roles(invite(), &[7])).await;
    assert_eq!(
        response_text(&response.expect("invite")),
        two_bot_core::voice_utilities::NO_INVITE_CONFIGURED
    );
}

#[tokio::test]
async fn unreadable_settings_fail_closed_for_members_only() {
    let runtime = gated_runtime(
        Trace::default(),
        AccessControls::default(),
        Some(StoreError::Unavailable),
    );
    let member = voice_interaction(Some(command_data("ping", Vec::new())), None, true);
    let (owned, response) = handle_capture(&runtime, &member).await;
    assert!(owned);
    assert!(response_text(&response.expect("notice")).contains("unavailable"));

    let admin = voice_interaction(
        Some(command_data("ping", Vec::new())),
        Some(Permissions::MANAGE_CHANNELS),
        true,
    );
    let (_, response) = handle_capture(&runtime, &admin).await;
    assert!(response_text(&response.expect("ping")).starts_with("Pong! "));
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

fn typed_option(name: &str, value: CommandOptionValue) -> CommandDataOption {
    CommandDataOption {
        name: name.to_owned(),
        value,
    }
}

fn textchannels_interaction(
    options: Vec<CommandDataOption>,
    permissions: Option<Permissions>,
) -> Interaction {
    voice_interaction(
        Some(command_data("textchannels", options)),
        permissions,
        true,
    )
}

#[test]
fn parse_textchannels_extracts_every_option() {
    let interaction = textchannels_interaction(
        vec![
            typed_option("channel", CommandOptionValue::Channel(Id::new(CREATOR))),
            typed_option("enabled", CommandOptionValue::Boolean(false)),
            typed_option("name", CommandOptionValue::String("lounge".to_owned())),
            typed_option("viewer-role", CommandOptionValue::Role(Id::new(42))),
        ],
        Some(Permissions::MANAGE_CHANNELS),
    );
    assert_eq!(
        parse_voice_command(&interaction),
        Some(VoiceCommand::TextChannels {
            channel_id: CREATOR,
            request: TextChannelsRequest {
                enabled: Some(false),
                name: Some("lounge".to_owned()),
                viewer_role: Some(42),
            },
        })
    );
}

#[test]
fn parse_textchannels_leaves_unset_options_unset() {
    let interaction = textchannels_interaction(
        vec![typed_option(
            "channel",
            CommandOptionValue::Channel(Id::new(CREATOR)),
        )],
        Some(Permissions::MANAGE_CHANNELS),
    );
    assert_eq!(
        parse_voice_command(&interaction),
        Some(VoiceCommand::TextChannels {
            channel_id: CREATOR,
            request: TextChannelsRequest {
                enabled: None,
                name: None,
                viewer_role: None,
            },
        })
    );
}

fn request(enabled: Option<bool>, name: Option<&str>, role: Option<u64>) -> TextChannelsRequest {
    TextChannelsRequest {
        enabled,
        name: name.map(str::to_owned),
        viewer_role: role,
    }
}

fn plan_creator(plan: TextChannelsPlan) -> CreatorChannel {
    match plan {
        TextChannelsPlan::Update(creator) => creator,
        TextChannelsPlan::Refuse { message } => panic!("unexpected refusal: {message}"),
    }
}

#[test]
fn decide_text_channels_defaults_to_on_and_keeps_unset_fields() {
    let base = CreatorChannel::new(GUILD, CREATOR);
    let creator = plan_creator(decide_text_channels(
        Some(base.clone()),
        &request(None, None, None),
    ));
    assert!(creator.text_channels, "omitted toggle turns companions on");
    assert_eq!(creator.text_channel_name, None);
    assert_eq!(creator.text_viewer_role_id, None);
    assert_eq!(creator.name_template, base.name_template);
}

#[test]
fn decide_text_channels_stores_trimmed_name_and_viewer_role() {
    let creator = plan_creator(decide_text_channels(
        Some(CreatorChannel::new(GUILD, CREATOR)),
        &request(Some(true), Some("  lounge  "), Some(42)),
    ));
    assert_eq!(creator.text_channel_name.as_deref(), Some("lounge"));
    assert_eq!(creator.text_viewer_role_id, Some(42));
    // @everyone is a valid viewer role (companion visible to all).
    let everyone = plan_creator(decide_text_channels(
        Some(CreatorChannel::new(GUILD, CREATOR)),
        &request(None, None, Some(GUILD)),
    ));
    assert_eq!(everyone.text_viewer_role_id, Some(GUILD));
}

#[test]
fn decide_text_channels_off_keeps_stored_name_and_role() {
    let mut on = CreatorChannel::new(GUILD, CREATOR);
    on.text_channels = true;
    on.text_channel_name = Some("lounge".to_owned());
    on.text_viewer_role_id = Some(42);
    let off = plan_creator(decide_text_channels(
        Some(on),
        &request(Some(false), None, None),
    ));
    assert!(!off.text_channels);
    assert_eq!(off.text_channel_name.as_deref(), Some("lounge"));
    assert_eq!(off.text_viewer_role_id, Some(42));
}

#[test]
fn decide_text_channels_refuses_bad_input() {
    let base = || Some(CreatorChannel::new(GUILD, CREATOR));
    for bad in [
        decide_text_channels(None, &request(None, None, None)),
        decide_text_channels(base(), &request(None, Some("   "), None)),
        decide_text_channels(base(), &request(None, Some(""), None)),
        decide_text_channels(
            base(),
            &request(
                None,
                Some(&"x".repeat(MAX_TEXT_CHANNEL_NAME_CHARS + 1)),
                None,
            ),
        ),
    ] {
        assert!(matches!(bad, TextChannelsPlan::Refuse { .. }), "{bad:?}");
    }
    // A name at the ceiling is accepted.
    assert!(matches!(
        decide_text_channels(
            base(),
            &request(None, Some(&"x".repeat(MAX_TEXT_CHANNEL_NAME_CHARS)), None)
        ),
        TextChannelsPlan::Update(_)
    ));
}

#[test]
fn command_names_key_the_role_restrictions() {
    let commands = [
        VoiceCommand::Create {
            name: String::new(),
        },
        VoiceCommand::Setup,
        VoiceCommand::Ping,
        VoiceCommand::Invite,
        VoiceCommand::TextChannels {
            channel_id: CREATOR,
            request: request(None, None, None),
        },
    ];
    for command in &commands {
        assert!(
            two_bot_core::voice_access::VOICE_COMMANDS.contains(&command.name()),
            "{} is not a restrictable voice command",
            command.name()
        );
    }
    assert_eq!(commands[4].name(), "textchannels");
}

#[test]
fn text_channels_summary_reports_the_stored_settings() {
    let mut creator = CreatorChannel::new(GUILD, CREATOR);
    assert!(text_channels_summary(&creator).contains("off"));
    creator.text_channels = true;
    let text = text_channels_summary(&creator);
    assert!(text.contains("on for <#200>"), "{text}");
    assert!(text.contains(DEFAULT_TEXT_CHANNEL_NAME), "{text}");
    assert!(text.contains("occupants and admins"), "{text}");
    creator.text_viewer_role_id = Some(42);
    assert!(text_channels_summary(&creator).contains("<@&42>"));
    creator.text_viewer_role_id = Some(GUILD);
    assert!(text_channels_summary(&creator).contains("everyone"));
}

#[tokio::test]
async fn handle_textchannels_refuses_without_manage_channels() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = textchannels_interaction(
        vec![typed_option(
            "channel",
            CommandOptionValue::Channel(Id::new(CREATOR)),
        )],
        Some(Permissions::VIEW_CHANNEL),
    );
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    assert!(response_text(response.as_ref().expect("reply")).contains("Manage Channels"));
    assert!(!trace
        .lock()
        .unwrap()
        .iter()
        .any(|entry| entry.starts_with("add_creator")));
}

#[tokio::test]
async fn handle_textchannels_saves_settings_for_a_creator_channel() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = textchannels_interaction(
        vec![
            typed_option("channel", CommandOptionValue::Channel(Id::new(CREATOR))),
            typed_option("name", CommandOptionValue::String("lounge".to_owned())),
            typed_option("viewer-role", CommandOptionValue::Role(Id::new(42))),
        ],
        Some(Permissions::MANAGE_CHANNELS),
    );
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(response.as_ref().expect("reply"));
    assert!(text.contains("on for <#200>"), "{text}");
    assert!(text.contains("lounge"), "{text}");
    assert!(text.contains("<@&42>"), "{text}");
    assert!(trace
        .lock()
        .unwrap()
        .contains(&format!("add_creator:{CREATOR}")));
}

#[tokio::test]
async fn handle_textchannels_refuses_a_channel_that_is_not_a_creator() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = textchannels_interaction(
        vec![typed_option(
            "channel",
            CommandOptionValue::Channel(Id::new(CREATOR + 1)),
        )],
        Some(Permissions::ADMINISTRATOR),
    );
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    assert!(response_text(response.as_ref().expect("reply")).contains("/create"));
    assert!(!trace
        .lock()
        .unwrap()
        .iter()
        .any(|entry| entry.starts_with("add_creator")));
}

#[tokio::test]
async fn handle_textchannels_surfaces_store_failures() {
    // The runtime factory builds a fresh store per command, so a failing
    // write is exercised through the pure decision + a store directly.
    let store = Store::new(Trace::default());
    *store.add_creator_error.lock().unwrap() = Some(StoreError::Unavailable);
    let creator = plan_creator(decide_text_channels(
        store.creator_for(GUILD, CREATOR).await.unwrap(),
        &request(Some(true), None, None),
    ));
    assert_eq!(
        store.add_creator(&creator).await,
        Err(StoreError::Unavailable)
    );
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
    let text = execute_create(&store, &http, GUILD, "lobby", |_, _| {}).await;
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
    let text = execute_create(&store, &http, GUILD, "lobby", |_, _| {}).await;
    assert!(text.contains("2s"));
}

#[tokio::test]
async fn execute_create_store_failure_deletes_channel_as_compensation() {
    let trace = Trace::default();
    let store = Store::new(trace.clone());
    *store.add_creator_error.lock().unwrap() = Some(StoreError::Unavailable);
    let http = Http::new(trace.clone());
    let text = execute_create(&store, &http, GUILD, "lobby", |_, _| {}).await;
    assert!(text.contains("removed"));
    assert_eq!(*trace.lock().unwrap(), ["create", "delete:500"]);
}

#[tokio::test]
async fn execute_create_store_credential_pause() {
    let trace = Trace::default();
    let store = Store::new(trace.clone());
    *store.add_creator_error.lock().unwrap() = Some(StoreError::CredentialRefused);
    let http = Http::new(trace.clone());
    let text = execute_create(&store, &http, GUILD, "lobby", |_, _| {}).await;
    assert!(text.contains("paused"));
    assert!(text.contains("removed"));
    assert_eq!(*trace.lock().unwrap(), ["create", "delete:500"]);
}

#[tokio::test]
async fn execute_create_failed_compensation_names_orphan_channel() {
    let trace = Trace::default();
    let store = Store::new(trace.clone());
    *store.add_creator_error.lock().unwrap() = Some(StoreError::Unavailable);
    let http = Http::new(trace.clone());
    http.delete_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::AccessDenied);
    let text = execute_create(&store, &http, GUILD, "lobby", |_, _| {}).await;
    assert!(!text.contains("was removed"));
    assert!(text.contains("manually"));
    assert!(text.contains("<#500>"));
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
    VoiceResponder::respond_with(&runtime, &replies, &create_interaction(), None).await;
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
        VoiceResponder::respond_with(&runtime, &replies, &create_interaction(), None).await;
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
    VoiceResponder::respond_with(&runtime, &replies, &create_interaction(), None).await;
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

// --- V9c companion lifecycle --------------------------------------------------

fn text_creator() -> CreatorChannel {
    let mut creator = CreatorChannel::new(GUILD, CREATOR);
    creator.text_channels = true;
    creator
}

fn companion_fixture() -> (LiveGuild, Store, Http, Trace) {
    let trace = Trace::default();
    let live = LiveGuild::new(GUILD);
    live.publish(snapshot(&[], vec![]));
    let store = Store::new(trace.clone());
    store.creators.lock().unwrap()[0] = text_creator();
    (live, store, Http::new(trace.clone()), trace)
}

/// Drive one full join through room create, companion create and move.
async fn join_with_companion(
    worker: &mut GuildRoomWorker<Store, Http>,
    member: u64,
    now: &mut u64,
) -> u64 {
    join(worker, member);
    // Room create.
    dispatch(worker, *now).await;
    *now += 1;
    // Companion create.
    dispatch(worker, *now).await;
    *now += 1;
    // Move.
    dispatch(worker, *now).await;
    *now += 1;
    // Simulate Discord's voice-state echo: the member is now in the room.
    worker.live.voice_update(member, Some(500), Some(false));
    worker.reconcile();
    // The companion plan was built before the move, so the creation
    // overwrites carry no occupant: the echo's reconcile queues the first
    // occupant's View grant. Drain it so callers start from an idle queue.
    dispatch(worker, *now).await;
    *now += 1;
    500
}

#[tokio::test]
async fn companion_created_with_room_when_toggle_on() {
    let (live, store, http, trace) = companion_fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let mut now = 0;
    join_with_companion(&mut worker, MEMBER, &mut now).await;
    let calls = trace.lock().unwrap().clone();
    assert!(
        calls.contains(&"create".to_owned()),
        "room created: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| call.starts_with("create_companion:500:voice-chat:999")),
        "companion POST carries room id, default name and bot id: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| call.starts_with("add_companion:500:")),
        "companion record persisted: {calls:?}"
    );
    assert_eq!(worker.companions.len(), 1);
    let companion = &worker.companions[&500];
    assert_eq!(companion.guild_id, GUILD);
    assert!(companion.settings.enabled);
}

#[tokio::test]
async fn no_companion_when_toggle_off() {
    let (live, store, http, trace) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    worker.reconcile();
    assert!(!worker.dispatch_one(2).await);
    let calls = trace.lock().unwrap().clone();
    assert!(
        !calls.iter().any(|call| call.contains("companion")),
        "{calls:?}"
    );
    assert!(worker.companions.is_empty());
}

#[tokio::test]
async fn join_grants_view_and_leave_revokes_without_deny() {
    let (live, store, http, trace) = companion_fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let mut now = 0;
    join_with_companion(&mut worker, MEMBER, &mut now).await;
    let text_id = worker.companions[&500].text_channel_id;
    // Second occupant joins the room: reconcile enqueues exactly one grant.
    worker.live.voice_update(MEMBER + 1, Some(500), Some(false));
    worker.reconcile();
    dispatch(&mut worker, now).await;
    now += 1;
    // Occupant leaves: reconcile enqueues exactly one revoke (delete of the
    // overwrite, never a deny).
    worker.live.voice_update(MEMBER + 1, None, Some(false));
    worker.reconcile();
    dispatch(&mut worker, now).await;
    let calls = trace.lock().unwrap().clone();
    assert!(
        calls.contains(&format!("grant:{text_id}:{}", MEMBER + 1)),
        "{calls:?}"
    );
    assert!(
        calls.contains(&format!("revoke:{text_id}:{}", MEMBER + 1)),
        "{calls:?}"
    );
}

#[tokio::test]
async fn companion_deleted_with_its_room_and_delete_is_idempotent() {
    let (live, store, http, trace) = companion_fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let mut now = 0;
    join_with_companion(&mut worker, MEMBER, &mut now).await;
    let text_id = worker.companions[&500].text_channel_id;
    // Everyone leaves: reconcile queues the room delete, which deletes the
    // companion first.
    worker.live.voice_update(MEMBER, None, Some(false));
    worker.reconcile();
    dispatch(&mut worker, now).await;
    let calls = trace.lock().unwrap().clone();
    assert!(
        calls.contains(&format!("delete:{text_id}")),
        "companion deleted: {calls:?}"
    );
    assert!(calls.contains(&"delete:500".to_owned()), "{calls:?}");
    assert!(
        calls.contains(&"remove_companion:500".to_owned()),
        "companion row removed: {calls:?}"
    );
    assert!(
        calls.contains(&"forget:500".to_owned()),
        "room row forgotten once the companion is gone: {calls:?}"
    );
    assert!(worker.companions.is_empty());
    assert!(worker.rooms.is_empty());
    // Re-dispatching the room delete (or a duplicate) is a no-op.
    worker.reconcile();
    assert!(!worker.dispatch_one(now + 1).await);
    let after = trace.lock().unwrap().clone();
    assert_eq!(
        after
            .iter()
            .filter(|call| *call == &format!("delete:{text_id}"))
            .count(),
        1,
        "no duplicate companion delete: {after:?}"
    );
}

#[tokio::test]
async fn companion_create_is_idempotent_when_record_already_exists() {
    let (live, store, http, trace) = companion_fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let mut now = 0;
    join(&mut worker, MEMBER);
    dispatch(&mut worker, now).await;
    now += 1;
    // A racing dispatch persisted the companion first: the queued create is
    // success without another POST.
    worker.companions.insert(
        500,
        TextCompanion {
            guild_id: GUILD,
            room_channel_id: 500,
            text_channel_id: 700,
            settings: two_bot_core::voice_text_channel::TextChannelSettings {
                enabled: true,
                configured_name: None,
                viewer_role_id: None,
            },
            created_at: NOW.to_owned(),
        },
    );
    dispatch(&mut worker, now).await;
    let calls = trace.lock().unwrap().clone();
    assert!(
        !calls
            .iter()
            .any(|call| call.starts_with("create_companion")),
        "no duplicate POST: {calls:?}"
    );
}

#[tokio::test]
async fn unknown_companion_outcome_adopts_visible_channel_without_repost() {
    let (live, store, http, trace) = companion_fixture();
    http.companion_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::UnknownOutcome);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let mut now = 0;
    join(&mut worker, MEMBER);
    dispatch(&mut worker, now).await;
    now += 1;
    // The POST outcome is unknown, but the live snapshot already shows the
    // channel the POST created (name + category match): adopt it.
    worker.live.upsert_channel(channel(600, 0, Some(CATEGORY)));
    {
        let mut live = worker.live.inner.write().unwrap();
        if let Some(created) = live.channels.get_mut(&600) {
            created.name = Some("voice-chat".to_owned());
        }
    }
    dispatch(&mut worker, now).await;
    let calls = trace.lock().unwrap().clone();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("create_companion"))
            .count(),
        1,
        "exactly one POST: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| call.starts_with("add_companion:500:600")),
        "adopted channel persisted: {calls:?}"
    );
    assert_eq!(worker.companions[&500].text_channel_id, 600);
}

// --- V9d admin roles and the settings snapshot ------------------------------

const ADMIN_ROLE: u64 = 777;
const ADMINISTRATOR_ROLE: u64 = 778;
const LATE_ADMIN_ROLE: u64 = 779;

fn snapshot_with_roles(extra_roles: Vec<Role>) -> GuildSnapshot {
    let mut snapshot = snapshot(&[], vec![]);
    snapshot.bot.roles.extend(extra_roles);
    snapshot
}

fn admin_roles() -> Vec<Role> {
    vec![
        role_with(ADMIN_ROLE, Permissions::MANAGE_CHANNELS),
        role_with(ADMINISTRATOR_ROLE, Permissions::ADMINISTRATOR),
        role_with(780, Permissions::VIEW_CHANNEL),
    ]
}

fn snapshot_companion(viewer_role_id: Option<u64>) -> TextCompanion {
    TextCompanion {
        guild_id: GUILD,
        room_channel_id: 500,
        text_channel_id: 700,
        settings: two_bot_core::voice_text_channel::TextChannelSettings {
            enabled: true,
            configured_name: None,
            viewer_role_id,
        },
        created_at: NOW.to_owned(),
    }
}

#[test]
fn admin_role_ids_keep_manage_channels_roles_only() {
    let live = LiveGuild::new(GUILD);
    live.publish(snapshot_with_roles(admin_roles()));
    let state = live.inner.read().unwrap();
    // Manage Channels role qualifies. @everyone (this snapshot gives it
    // Manage Channels), the Administrator role (bypasses overwrites) and a
    // role without the bit do not.
    assert_eq!(state.admin_role_ids(GUILD), vec![ADMIN_ROLE]);
}

#[tokio::test]
async fn companion_plan_carries_admin_roles() {
    let (live, store, http, trace) = companion_fixture();
    live.publish(snapshot_with_roles(admin_roles()));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let mut now = 0;
    join_with_companion(&mut worker, MEMBER, &mut now).await;
    let plans = worker.http.companion_plans.lock().unwrap().clone();
    assert_eq!(plans.len(), 1, "{:?}", trace.lock().unwrap());
    let roles: Vec<u64> = plans[0]
        .overwrites
        .iter()
        .filter_map(|overwrite| match overwrite.target {
            OverwriteTarget::Role(id) => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(roles, vec![ADMIN_ROLE], "{:?}", plans[0].overwrites);
}

#[tokio::test]
async fn later_promoted_admin_role_is_resolved_live_without_touching_open_rooms() {
    let (live, store, http, _) = companion_fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let mut now = 0;
    join_with_companion(&mut worker, MEMBER, &mut now).await;
    {
        let state = worker.live.inner.read().unwrap();
        assert!(state.admin_role_ids(GUILD).is_empty());
    }
    // A role is promoted to Manage Channels after the companion exists: the
    // next published snapshot carries it and the occupant is still inside.
    let mut promoted = snapshot(
        &[500, worker.companions[&500].text_channel_id],
        vec![VoiceMember {
            member_id: MEMBER,
            channel_id: 500,
            bot: Some(false),
        }],
    );
    promoted
        .bot
        .roles
        .push(role_with(LATE_ADMIN_ROLE, Permissions::MANAGE_CHANNELS));
    worker.live.publish(promoted);
    worker.reconcile();
    {
        let state = worker.live.inner.read().unwrap();
        assert_eq!(state.admin_role_ids(GUILD), vec![LATE_ADMIN_ROLE]);
    }
    assert!(
        !worker.dispatch_one(now).await,
        "no per-room overwrite update is queued for the promotion"
    );
}

#[tokio::test]
async fn protected_ids_hold_viewer_admin_roles_and_the_bot() {
    let (live, store, http, _) = companion_fixture();
    live.publish(snapshot_with_roles(admin_roles()));
    let worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let state = worker.live.inner.read().unwrap();
    let protected = worker.protected_ids(&state, &snapshot_companion(Some(42)));
    assert_eq!(protected, vec![42, ADMIN_ROLE, 999]);
    // No viewer role: admin roles and the bot stay protected.
    let protected = worker.protected_ids(&state, &snapshot_companion(None));
    assert_eq!(protected, vec![ADMIN_ROLE, 999]);
}

#[tokio::test]
async fn creator_edit_applies_to_new_rooms_only() {
    let (live, store, http, _) = companion_fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let mut now = 0;
    join_with_companion(&mut worker, MEMBER, &mut now).await;
    let before = worker.companions[&500].settings.clone();
    // `/textchannels` swaps in an edited creator row.
    let mut edited = text_creator();
    edited.text_channel_name = Some("lounge".to_owned());
    edited.text_viewer_role_id = Some(42);
    apply_command(&mut worker, ActorCommand::CreatorAdded(edited));
    assert_eq!(
        worker.companions[&500].settings, before,
        "the existing companion keeps its creation-time snapshot"
    );
    assert_eq!(
        worker.creators[&CREATOR].text_channel_name.as_deref(),
        Some("lounge")
    );
}

#[tokio::test]
async fn companion_row_write_failure_is_retried_without_a_second_post() {
    let (live, store, http, trace) = companion_fixture();
    store
        .companion_errors
        .lock()
        .unwrap()
        .push_back(StoreError::Unavailable);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    // Room create, then the companion POST succeeds but its row write fails.
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    assert!(worker.unpersisted_companions.contains(&500));
    assert!(worker.companions.contains_key(&500));
    assert!(worker.store.companions.lock().unwrap().is_empty());
    // After the backoff the same action writes only the row.
    dispatch(&mut worker, 60_000).await;
    let calls = trace.lock().unwrap().clone();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("create_companion"))
            .count(),
        1,
        "the retry never POSTs again: {calls:?}"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| *call == "add_companion:500:501")
            .count(),
        2,
        "row written on the first try and on the retry: {calls:?}"
    );
    assert!(worker
        .store
        .companions
        .lock()
        .unwrap()
        .contains_key(&(GUILD, 500)));
    assert!(worker.unpersisted_companions.is_empty());
}

#[tokio::test]
async fn unknown_companion_outcome_never_adopts_another_rooms_companion() {
    let (live, store, http, trace) = companion_fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let mut now = 0;
    // Room A (500) and its companion (501), tracked.
    join_with_companion(&mut worker, MEMBER, &mut now).await;
    assert_eq!(worker.companions[&500].text_channel_id, 501);
    // The fake names every channel "room"; give A's companion the constant
    // default name every room's plan carries.
    worker
        .live
        .inner
        .write()
        .unwrap()
        .channels
        .get_mut(&501)
        .unwrap()
        .name = Some("voice-chat".to_owned());
    worker
        .http
        .companion_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::UnknownOutcome);
    // Room B (502) in the same category: its companion POST outcome is
    // unknown, and the only name-matching channel is A's.
    join(&mut worker, MEMBER + 1);
    while worker.dispatch_one(now).await {
        now += 1;
    }
    let calls = trace.lock().unwrap().clone();
    assert!(
        calls
            .iter()
            .any(|call| call.starts_with("create_companion:502")),
        "room B attempted its own companion: {calls:?}"
    );
    assert!(
        !worker.companions.contains_key(&502),
        "room B must not adopt room A's companion"
    );
    assert!(
        !calls
            .iter()
            .any(|call| call.starts_with("add_companion:502")),
        "{calls:?}"
    );
    assert_eq!(worker.companions[&500].text_channel_id, 501);
    assert_eq!(worker.companion_channels[&500], 501);
}

#[tokio::test]
async fn adopt_skips_tracked_and_older_channels_and_takes_the_oldest_remaining() {
    let (live, store, http, _) = companion_fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    for id in [450, 700, 800, 900] {
        worker.live.upsert_channel(channel(id, 0, Some(CATEGORY)));
    }
    {
        let mut live = worker.live.inner.write().unwrap();
        for id in [450, 700, 800] {
            live.channels.get_mut(&id).unwrap().name = Some("voice-chat".to_owned());
        }
    }
    let plan = TextChannelPlan {
        room_id: 500,
        guild_id: GUILD,
        name: "voice-chat".to_owned(),
        category_id: CATEGORY,
        overwrites: Vec::new(),
        settings: two_bot_core::voice_text_channel::TextChannelSettings {
            enabled: true,
            configured_name: None,
            viewer_role_id: None,
        },
    };
    // 450 predates the room, 700 is another room's companion, 900 has the
    // wrong name: 800 is the only candidate.
    worker.companion_channels.insert(600, 700);
    assert_eq!(worker.adopt_companion(&plan), Some(800));
    // A companion row loaded from the store counts as tracked too.
    worker.companions.insert(
        601,
        TextCompanion {
            guild_id: GUILD,
            room_channel_id: 601,
            text_channel_id: 800,
            settings: plan.settings.clone(),
            created_at: NOW.to_owned(),
        },
    );
    assert_eq!(worker.adopt_companion(&plan), None);
}
