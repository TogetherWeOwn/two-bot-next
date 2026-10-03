use super::*;
use serde_json::json;
use std::sync::Mutex;

#[path = "voice_kick_tests.rs"]
mod kick;
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
            system_channel_id: None,
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
    access: Arc<Mutex<AccessControls>>,
    access_error: Option<StoreError>,
    save_access_error: Option<StoreError>,
    logging: Arc<Mutex<LoggingSettings>>,
    logging_error: Option<StoreError>,
    save_logging_error: Option<StoreError>,
    forget_errors: Mutex<VecDeque<StoreError>>,
    companion_errors: Mutex<VecDeque<StoreError>>,
    add_creator_error: Mutex<Option<StoreError>>,
    after_persist: Option<Hook>,
    config: Arc<Mutex<VoiceConfiguration>>,
    config_error: Option<StoreError>,
    save_config_error: Option<StoreError>,
}

impl Store {
    fn new(trace: Trace) -> Self {
        Self {
            trace,
            creators: Mutex::new(vec![CreatorChannel::new(GUILD, CREATOR)]),
            rooms: Mutex::new(HashMap::new()),
            companions: Mutex::new(HashMap::new()),
            persist_error: None,
            access: Arc::new(Mutex::new(AccessControls::default())),
            access_error: None,
            save_access_error: None,
            logging: Arc::new(Mutex::new(LoggingSettings::default())),
            logging_error: None,
            save_logging_error: None,
            forget_errors: Mutex::new(VecDeque::new()),
            companion_errors: Mutex::new(VecDeque::new()),
            add_creator_error: Mutex::new(None),
            after_persist: None,
            config: Arc::new(Mutex::new(empty_config())),
            config_error: None,
            save_config_error: None,
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
    async fn save_access_controls(
        &self,
        _: u64,
        controls: &AccessControls,
    ) -> Result<(), StoreError> {
        if let Some(error) = self.save_access_error {
            return Err(error);
        }
        *self.access.lock().unwrap() = controls.clone();
        Ok(())
    }
    async fn logging_settings(&self, _: u64) -> Result<LoggingSettings, StoreError> {
        match self.logging_error {
            Some(error) => Err(error),
            None => Ok(*self.logging.lock().unwrap()),
        }
    }
    async fn save_logging_settings(
        &self,
        _: u64,
        settings: &LoggingSettings,
    ) -> Result<(), StoreError> {
        if let Some(error) = self.save_logging_error {
            return Err(error);
        }
        *self.logging.lock().unwrap() = *settings;
        Ok(())
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
    async fn update_ownership(&self, room: &VoiceRoom) -> Result<bool, StoreError> {
        self.trace.lock().unwrap().push(format!(
            "update_ownership:{}:{}",
            room.channel_id, room.owner_id
        ));
        let mut rooms = self.rooms.lock().unwrap();
        if let Some(stored) = rooms.get_mut(&room.channel_id) {
            stored.owner_id = room.owner_id;
            stored.original_creator_id = room.original_creator_id;
            Ok(true)
        } else {
            Ok(false)
        }
    }
    async fn forget(&self, _: u64, channel: u64) -> Result<(), StoreError> {
        self.trace.lock().unwrap().push(format!("forget:{channel}"));
        if let Some(error) = self.forget_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        self.rooms.lock().unwrap().remove(&channel);
        Ok(())
    }
    async fn config_snapshot(&self, _: u64) -> Result<VoiceConfiguration, StoreError> {
        match self.config_error {
            Some(error) => Err(error),
            None => Ok(self.config.lock().unwrap().clone()),
        }
    }
    async fn config_apply(&self, _: u64, config: &VoiceConfiguration) -> Result<(), StoreError> {
        if let Some(error) = self.save_config_error {
            return Err(error);
        }
        self.trace.lock().unwrap().push("config_apply".to_owned());
        *self.config.lock().unwrap() = config.clone();
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

/// Scripted `/import` download queue shared with the harness.
type DownloadResults = Arc<Mutex<VecDeque<Result<Vec<u8>, RoomHttpError>>>>;

struct Http {
    trace: Trace,
    next_id: Mutex<u64>,
    create_errors: Mutex<VecDeque<RoomHttpError>>,
    move_errors: Mutex<VecDeque<RoomHttpError>>,
    delete_errors: Mutex<VecDeque<RoomHttpError>>,
    rename_errors: Mutex<VecDeque<RoomHttpError>>,
    companion_errors: Mutex<VecDeque<RoomHttpError>>,
    view_errors: Mutex<VecDeque<RoomHttpError>>,
    notices: Mutex<Vec<(NoticeTarget, String, Option<u64>)>>,
    refused_notices: Mutex<Vec<NoticeTarget>>,
    created_attributes: Mutex<Vec<RoomChannelAttributes>>,
    companion_plans: Mutex<Vec<TextChannelPlan>>,
    after_create: Option<Hook>,
    before_move: Option<Hook>,
    before_delete: Option<Hook>,
    downloaded_urls: Mutex<Vec<String>>,
    download_results: DownloadResults,
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
            notices: Mutex::new(Vec::new()),
            refused_notices: Mutex::new(Vec::new()),
            created_attributes: Mutex::new(Vec::new()),
            companion_plans: Mutex::new(Vec::new()),
            after_create: None,
            before_move: None,
            before_delete: None,
            downloaded_urls: Mutex::new(Vec::new()),
            download_results: Arc::new(Mutex::new(VecDeque::new())),
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
    async fn download_attachment(
        &self,
        url: &str,
        _max_bytes: usize,
    ) -> Result<Vec<u8>, RoomHttpError> {
        self.trace.lock().unwrap().push(format!("download:{url}"));
        self.downloaded_urls.lock().unwrap().push(url.to_owned());
        match self.download_results.lock().unwrap().pop_front() {
            Some(result) => result,
            None => Err(RoomHttpError::UnknownOutcome),
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
    async fn send_notice(
        &self,
        target: NoticeTarget,
        content: &str,
        mention_role: Option<u64>,
    ) -> Result<(), RoomHttpError> {
        if self.refused_notices.lock().unwrap().contains(&target) {
            return Err(RoomHttpError::AccessDenied);
        }
        self.notices
            .lock()
            .unwrap()
            .push((target, content.to_owned(), mention_role));
        Ok(())
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
async fn owner_leave_hands_room_to_earliest_joiner_and_persists() {
    let (live, store, http, trace) = fixture();
    store.rooms.lock().unwrap().insert(500, room(500));
    // Owner 300 alone first; 302 arrives, then 301. Caretaker is 302 by
    // earliest join time even though 301 sorts first by id.
    live.publish(snapshot(
        &[500],
        vec![VoiceMember {
            member_id: MEMBER,
            channel_id: 500,
            bot: Some(false),
        }],
    ));
    live.voice_update_at(302, Some(500), Some(false), 1_000);
    live.voice_update_at(301, Some(500), Some(false), 2_000);
    live.voice_update_at(MEMBER, None, Some(false), 3_000);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.reconcile();
    let tracked = worker.tracked().get(&500).expect("tracked room");
    assert_eq!(tracked.owner_id, 302);
    assert_eq!(tracked.original_creator_id, MEMBER);
    dispatch(&mut worker, 0).await;
    assert!(trace
        .lock()
        .unwrap()
        .contains(&"update_ownership:500:302".to_owned()));
    // Idempotent: the next tick sees the owner present and enqueues nothing.
    let queued = worker.queue.pending_counts(GUILD);
    worker.reconcile();
    assert_eq!(worker.queue.pending_counts(GUILD), queued);
}

#[tokio::test]
async fn succession_skips_present_owners_and_pending_moves() {
    let (live, store, http, trace) = fixture();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.publish(snapshot(
        &[500],
        vec![
            VoiceMember {
                member_id: MEMBER,
                channel_id: 500,
                bot: Some(false),
            },
            VoiceMember {
                member_id: 301,
                channel_id: 500,
                bot: Some(false),
            },
        ],
    ));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    // Owner present: no handoff.
    worker.reconcile();
    assert_eq!(worker.tracked().get(&500).expect("room").owner_id, MEMBER);
    // Owner leaves while their move is still in flight: succession waits
    // for the inbound owner instead of handing the room off.
    worker.moves.insert(
        500,
        JoinTicket {
            member_id: MEMBER,
            creator_id: CREATOR,
            generation: 0,
            transition: 0,
        },
    );
    worker
        .live
        .voice_update_at(MEMBER, None, Some(false), 4_000);
    worker.reconcile();
    assert_eq!(worker.tracked().get(&500).expect("room").owner_id, MEMBER);
    assert!(!trace
        .lock()
        .unwrap()
        .iter()
        .any(|call| call.starts_with("update_ownership")));
}

/// One room with two occupants: owner `MEMBER` (creator), plus 301.
fn owned_room() -> (LiveGuild, Store, Http, Trace) {
    let (live, store, http, trace) = fixture();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.publish(snapshot(
        &[500],
        vec![
            VoiceMember {
                member_id: MEMBER,
                channel_id: 500,
                bot: Some(false),
            },
            VoiceMember {
                member_id: 301,
                channel_id: 500,
                bot: Some(false),
            },
        ],
    ));
    (live, store, http, trace)
}

#[tokio::test]
async fn reclaim_hands_room_back_to_returned_creator_and_persists() {
    let (live, store, http, trace) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    // Owner leaves first, so 301 is the caretaker; the creator returns.
    worker
        .live
        .voice_update_at(MEMBER, None, Some(false), 1_000);
    worker.reconcile();
    assert_eq!(worker.tracked().get(&500).expect("room").owner_id, 301);
    worker
        .live
        .voice_update_at(MEMBER, Some(500), Some(false), 2_000);
    let text = worker.apply_ownership(MEMBER, false, OwnershipCommand::Reclaim);
    assert!(text.contains("owner of this room again"), "{text}");
    let tracked = worker.tracked().get(&500).expect("tracked room");
    assert_eq!(tracked.owner_id, MEMBER);
    assert_eq!(tracked.original_creator_id, MEMBER);
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    assert!(
        trace
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.starts_with("update_ownership"))
            .count()
            >= 1
    );
    assert!(trace
        .lock()
        .unwrap()
        .contains(&format!("update_ownership:500:{MEMBER}")));
}

#[tokio::test]
async fn reclaim_claims_room_whose_owner_is_gone() {
    let (live, store, http, _) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    // Owner leaves; the claim runs before any succession pass while the
    // owner-absent snapshot is current.
    worker
        .live
        .voice_update_at(MEMBER, None, Some(false), 1_000);
    let text = worker.apply_ownership(301, false, OwnershipCommand::Reclaim);
    assert!(text.contains("yours now"), "{text}");
    assert_eq!(worker.tracked().get(&500).expect("room").owner_id, 301);
}

#[tokio::test]
async fn reclaim_refuses_non_creator_while_owner_present() {
    let (live, store, http, trace) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let text = worker.apply_ownership(301, false, OwnershipCommand::Reclaim);
    assert!(text.contains("original creator"), "{text}");
    assert_eq!(worker.tracked().get(&500).expect("room").owner_id, MEMBER);
    assert!(!trace
        .lock()
        .unwrap()
        .iter()
        .any(|call| call.starts_with("update_ownership")));
}

#[tokio::test]
async fn reclaim_repeated_is_idempotent() {
    let (live, store, http, _) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker
        .live
        .voice_update_at(MEMBER, None, Some(false), 1_000);
    worker.reconcile();
    worker
        .live
        .voice_update_at(MEMBER, Some(500), Some(false), 2_000);
    let first = worker.apply_ownership(MEMBER, false, OwnershipCommand::Reclaim);
    assert!(first.contains("again"), "{first}");
    let second = worker.apply_ownership(MEMBER, false, OwnershipCommand::Reclaim);
    assert!(second.contains("already the owner"), "{second}");
}

#[tokio::test]
async fn reclaim_outside_voice_asks_to_join() {
    let (live, store, http, _) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let text = worker.apply_ownership(302, false, OwnershipCommand::Reclaim);
    assert!(text.contains("need to be in a voice room"), "{text}");
}

#[tokio::test]
async fn transfer_hands_room_to_occupant_and_remembers_creator() {
    let (live, store, http, trace) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let text = worker.apply_ownership(MEMBER, false, OwnershipCommand::Transfer { target_id: 301 });
    assert!(
        text.contains("Transferred") && text.contains("<@301>"),
        "{text}"
    );
    let tracked = worker.tracked().get(&500).expect("tracked room");
    assert_eq!(tracked.owner_id, 301);
    assert_eq!(tracked.original_creator_id, 301);
    dispatch(&mut worker, 0).await;
    assert!(trace
        .lock()
        .unwrap()
        .contains(&"update_ownership:500:301".to_owned()));
}

#[tokio::test]
async fn transfer_rejects_recipient_outside_room() {
    let (live, store, http, trace) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let text = worker.apply_ownership(MEMBER, false, OwnershipCommand::Transfer { target_id: 302 });
    assert!(text.contains("must be in the room"), "{text}");
    assert_eq!(worker.tracked().get(&500).expect("room").owner_id, MEMBER);
    assert!(!trace
        .lock()
        .unwrap()
        .iter()
        .any(|call| call.starts_with("update_ownership")));
}

#[tokio::test]
async fn transfer_refuses_non_owner() {
    let (live, store, http, _) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let text = worker.apply_ownership(301, false, OwnershipCommand::Transfer { target_id: MEMBER });
    assert!(text.contains("Only the room owner"), "{text}");
    assert_eq!(worker.tracked().get(&500).expect("room").owner_id, MEMBER);
}

#[tokio::test]
async fn transfer_replay_by_former_owner_fails_authorization() {
    let (live, store, http, _) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let first =
        worker.apply_ownership(MEMBER, false, OwnershipCommand::Transfer { target_id: 301 });
    assert!(first.contains("Transferred"), "{first}");
    // The former owner cannot replay the same handoff to take the room back.
    let replay = worker.apply_ownership(
        MEMBER,
        false,
        OwnershipCommand::Transfer { target_id: MEMBER },
    );
    assert!(replay.contains("Only the room owner"), "{replay}");
    assert_eq!(worker.tracked().get(&500).expect("room").owner_id, 301);
}

#[tokio::test]
async fn admin_transfer_from_outside_voice_names_occupied_room() {
    let (live, store, http, _) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    // Admin 999 is nowhere in voice but may act on any room.
    let text = worker.apply_ownership(999, true, OwnershipCommand::Transfer { target_id: 301 });
    assert!(
        text.contains("Transferred") && text.contains("<@301>"),
        "{text}"
    );
    let tracked = worker.tracked().get(&500).expect("tracked room");
    assert_eq!(tracked.owner_id, 301);
    assert_eq!(tracked.original_creator_id, 301);
}

#[tokio::test]
async fn ownership_commands_on_untracked_channel_refuse() {
    let (live, store, http, _) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker
        .live
        .voice_update_at(302, Some(999), Some(false), 1_000);
    let text = worker.apply_ownership(302, false, OwnershipCommand::Reclaim);
    assert!(text.contains("isn't a temporary room"), "{text}");
}

#[tokio::test]
async fn ownership_commands_while_halted_refuse_paused() {
    let (live, store, http, _) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.halted = true;
    let text = worker.apply_ownership(MEMBER, false, OwnershipCommand::Reclaim);
    assert!(text.contains("paused"), "{text}");
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
    assert_eq!(
        names,
        [
            "create",
            "setup",
            "ping",
            "invite",
            "textchannels",
            "access",
            "reclaim",
            "transfer",
            "logging",
            "export",
            "import"
        ]
    );
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
    user::User,
};

fn test_user(id: u64) -> User {
    User {
        accent_color: None,
        avatar: None,
        avatar_decoration: None,
        avatar_decoration_data: None,
        banner: None,
        bot: false,
        discriminator: 0,
        email: None,
        flags: None,
        global_name: None,
        id: Id::new(id),
        locale: None,
        mfa_enabled: None,
        name: "member".to_owned(),
        premium_type: None,
        primary_guild: None,
        public_flags: None,
        system: None,
        verified: None,
    }
}

/// Interaction invoked by `user_id` (guild member by default). The ownership
/// handlers authenticate the actor from `member.user`, else the top-level user.
fn voice_interaction_as(
    command: Option<CommandData>,
    permissions: Option<Permissions>,
    user_id: u64,
) -> Interaction {
    let mut interaction = voice_interaction(command, permissions, true);
    if let Some(member) = interaction.member.as_mut() {
        member.user = Some(test_user(user_id));
    } else {
        interaction.user = Some(test_user(user_id));
    }
    interaction
}

async fn wait_trace(trace: &Trace, entry: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if trace.lock().unwrap().iter().any(|value| value == entry) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "lifecycle trace deadline waiting for {entry}: {:?}",
            trace.lock().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Runtime with one tracked room (channel 500, owner/creator `MEMBER`) and a
/// second occupant 301. Drives the real actor so ownership commands run
/// against live worker state. The trailing voice frames are Discord reporting
/// both members inside the new room — live occupancy only moves on frames,
/// never on the create/move dispatch itself.
async fn ownership_room_runtime(trace: Trace) -> VoiceRuntime<Store, Http> {
    let runtime = test_runtime(trace.clone());
    assert!(runtime.publish_snapshot(GUILD, snapshot(&[], vec![])));
    assert!(runtime.voice_frame(
        GUILD,
        MEMBER,
        Some(CREATOR),
        Some(false),
        "ava's room".to_owned(),
    ));
    wait_trace(&trace, "persist:500").await;
    assert!(runtime.voice_frame(GUILD, MEMBER, Some(500), Some(false), "x".to_owned()));
    assert!(runtime.voice_frame(GUILD, 301, Some(500), Some(false), "x".to_owned()));
    runtime
}

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

async fn handle_capture<S, H>(
    runtime: &VoiceRuntime<S, H>,
    interaction: &Interaction,
) -> (bool, Option<InteractionResponse>)
where
    S: RoomPersistence + Send + 'static,
    H: RoomWrites + Send + 'static,
{
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

fn user_option(name: &str, id: u64) -> CommandDataOption {
    CommandDataOption {
        name: name.to_owned(),
        value: CommandOptionValue::User(Id::new(id)),
    }
}

#[test]
fn parse_reclaim_command() {
    let interaction = voice_interaction(Some(command_data("reclaim", Vec::new())), None, true);
    assert_eq!(
        parse_voice_command(&interaction),
        Some(VoiceCommand::Reclaim)
    );
    let guildless = voice_interaction(Some(command_data("reclaim", Vec::new())), None, false);
    assert_eq!(parse_voice_command(&guildless), None);
}

#[test]
fn parse_transfer_extracts_member_target() {
    let interaction = voice_interaction(
        Some(command_data("transfer", vec![user_option("member", 301)])),
        None,
        true,
    );
    assert_eq!(
        parse_voice_command(&interaction),
        Some(VoiceCommand::Transfer { target_id: 301 })
    );
}

#[test]
fn parse_transfer_without_member_defaults_zero_for_refusal() {
    let interaction = voice_interaction(Some(command_data("transfer", Vec::new())), None, true);
    assert_eq!(
        parse_voice_command(&interaction),
        Some(VoiceCommand::Transfer { target_id: 0 })
    );
    let wrong_type = voice_interaction(
        Some(command_data(
            "transfer",
            vec![command_option("member", "301")],
        )),
        None,
        true,
    );
    assert_eq!(
        parse_voice_command(&wrong_type),
        Some(VoiceCommand::Transfer { target_id: 0 })
    );
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
    handle_voice_interaction_with(&runtime, &interaction, Some("abc-123"), None, |response| {
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

#[tokio::test]
async fn handle_transfer_by_owner_hands_room_to_occupant() {
    let trace = Trace::default();
    let runtime = ownership_room_runtime(trace.clone()).await;
    let interaction = voice_interaction_as(
        Some(command_data("transfer", vec![user_option("member", 301)])),
        None,
        MEMBER,
    );
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(response.as_ref().expect("reply"));
    assert!(
        text.contains("Transferred") && text.contains("<@301>"),
        "{text}"
    );
    let status = tokio::time::timeout(Duration::from_secs(5), runtime.worker_status(GUILD))
        .await
        .expect("status reply")
        .expect("live actor");
    assert_eq!(status.tracked_rooms, 1);
    // Persistence lands on the actor's timer tick, not in the reply path.
    wait_trace(&trace, "update_ownership:500:301").await;
}

#[tokio::test]
async fn handle_transfer_rejects_recipient_outside_room() {
    let trace = Trace::default();
    let runtime = ownership_room_runtime(trace.clone()).await;
    let interaction = voice_interaction_as(
        Some(command_data("transfer", vec![user_option("member", 302)])),
        None,
        MEMBER,
    );
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(response.as_ref().expect("reply"));
    assert!(text.contains("must be in the room"), "{text}");
    assert!(!trace
        .lock()
        .unwrap()
        .iter()
        .any(|entry| entry.starts_with("update_ownership")));
}

#[tokio::test]
async fn handle_reclaim_by_non_creator_refuses_while_owner_present() {
    let trace = Trace::default();
    let runtime = ownership_room_runtime(trace.clone()).await;
    let interaction = voice_interaction_as(Some(command_data("reclaim", Vec::new())), None, 301);
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(response.as_ref().expect("reply"));
    assert!(text.contains("original creator"), "{text}");
    assert!(!trace
        .lock()
        .unwrap()
        .iter()
        .any(|entry| entry.starts_with("update_ownership")));
}

#[tokio::test]
async fn handle_reclaim_outside_voice_asks_to_join() {
    let trace = Trace::default();
    let runtime = ownership_room_runtime(trace.clone()).await;
    let interaction = voice_interaction_as(Some(command_data("reclaim", Vec::new())), None, 302);
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(response.as_ref().expect("reply"));
    assert!(text.contains("need to be in a voice room"), "{text}");
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

fn sub_option(name: &str, args: Vec<CommandDataOption>) -> CommandDataOption {
    CommandDataOption {
        name: name.to_owned(),
        value: CommandOptionValue::SubCommand(args),
    }
}

fn role_option(name: &str, id: u64) -> CommandDataOption {
    CommandDataOption {
        name: name.to_owned(),
        value: CommandOptionValue::Role(Id::new(id)),
    }
}

fn access_interaction(sub: CommandDataOption, admin: bool) -> Interaction {
    voice_interaction(
        Some(command_data("access", vec![sub])),
        admin.then_some(Permissions::MANAGE_CHANNELS),
        true,
    )
}

fn access_action(sub: CommandDataOption) -> AccessAction {
    match parse_voice_command(&access_interaction(sub, true)) {
        Some(VoiceCommand::Access(action)) => action,
        other => panic!("not an /access command: {other:?}"),
    }
}

#[test]
fn parse_access_subcommands() {
    assert_eq!(
        access_action(sub_option("show", vec![])),
        AccessAction::Show
    );
    assert_eq!(
        access_action(sub_option(
            "creation",
            vec![CommandDataOption {
                name: "enabled".to_owned(),
                value: CommandOptionValue::Boolean(false),
            }]
        )),
        AccessAction::Creation(false)
    );
    assert_eq!(
        access_action(sub_option("role", vec![role_option("role", 9)])),
        AccessAction::RequiredRole(Some(9))
    );
    // No role clears the requirement.
    assert_eq!(
        access_action(sub_option("role", vec![])),
        AccessAction::RequiredRole(None)
    );
    // Command names are lowercased; roles keep order and drop duplicates.
    assert_eq!(
        access_action(sub_option(
            "restrict",
            vec![
                command_option("command", " Kick "),
                role_option("role", 7),
                role_option("role2", 8),
                role_option("role3", 7),
            ]
        )),
        AccessAction::Restrict {
            command: "kick".to_owned(),
            roles: vec![7, 8]
        }
    );
    assert_eq!(
        access_action(sub_option(
            "restrict",
            vec![command_option("command", "kick")]
        )),
        AccessAction::Restrict {
            command: "kick".to_owned(),
            roles: vec![]
        }
    );
    assert_eq!(
        access_action(sub_option(
            "unrestrict",
            vec![command_option("command", "Kick")]
        )),
        AccessAction::Unrestrict("kick".to_owned())
    );
}

#[test]
fn parse_access_malformed_shapes_are_invalid() {
    assert_eq!(
        access_action(sub_option("bogus", vec![])),
        AccessAction::Invalid
    );
    assert_eq!(
        access_action(sub_option("creation", vec![])),
        AccessAction::Invalid
    );
    assert_eq!(
        access_action(sub_option("restrict", vec![])),
        AccessAction::Invalid
    );
    assert_eq!(
        access_action(sub_option("unrestrict", vec![])),
        AccessAction::Invalid
    );
    let bare = voice_interaction(Some(command_data("access", Vec::new())), None, true);
    assert_eq!(
        parse_voice_command(&bare),
        Some(VoiceCommand::Access(AccessAction::Invalid))
    );
}

fn shared_runtime(
    trace: Trace,
    shared: Arc<Mutex<AccessControls>>,
    save_error: Option<StoreError>,
) -> VoiceRuntime<Store, Http> {
    VoiceRuntime::new(
        move || {
            let mut store = Store::new(trace.clone());
            store.access = shared.clone();
            store.save_access_error = save_error;
            (store, Http::new(trace.clone()))
        },
        Duration::from_millis(10),
        true,
    )
}

#[tokio::test]
async fn access_command_needs_an_admin() {
    let shared = Arc::new(Mutex::new(AccessControls::default()));
    let runtime = shared_runtime(Trace::default(), shared.clone(), None);
    let member = access_interaction(sub_option("show", vec![]), false);
    let (owned, response) = handle_capture(&runtime, &member).await;
    assert!(owned);
    assert_eq!(
        response_text(&response.expect("denial")),
        "You need Manage Channels to use /access."
    );
}

#[tokio::test]
async fn access_command_changes_are_saved_and_shown() {
    let shared = Arc::new(Mutex::new(AccessControls::default()));
    let runtime = shared_runtime(Trace::default(), shared.clone(), None);
    let run = |sub| {
        let interaction = access_interaction(sub, true);
        let runtime = &runtime;
        async move {
            let (_, response) = handle_capture(runtime, &interaction).await;
            response_text(&response.expect("reply"))
        }
    };

    let text = run(sub_option("role", vec![role_option("role", 9)])).await;
    assert!(text.starts_with("Saved."), "{text}");
    assert!(text.contains("Required role: <@&9>"), "{text}");
    assert_eq!(shared.lock().unwrap().required_role, Some(9));

    let text = run(sub_option(
        "restrict",
        vec![command_option("command", "kick"), role_option("role", 7)],
    ))
    .await;
    assert!(text.contains("- /kick: <@&7>"), "{text}");
    // No roles at all fails closed: admins only.
    let text = run(sub_option(
        "restrict",
        vec![command_option("command", "template")],
    ))
    .await;
    assert!(text.contains("- /template: admins only"), "{text}");
    assert_eq!(shared.lock().unwrap().command_roles.len(), 2);

    let text = run(sub_option(
        "unrestrict",
        vec![command_option("command", "kick")],
    ))
    .await;
    assert!(!text.contains("/kick"), "{text}");
    let text = run(sub_option("show", vec![])).await;
    assert!(text.contains("- /template: admins only"), "{text}");
    assert!(!text.starts_with("Saved."), "{text}");

    let text = run(sub_option("role", vec![])).await;
    assert!(text.contains("Required role: none"), "{text}");
    assert_eq!(shared.lock().unwrap().required_role, None);
}

#[tokio::test]
async fn access_command_refuses_bad_input_without_changing_anything() {
    let shared = Arc::new(Mutex::new(AccessControls::default()));
    let runtime = shared_runtime(Trace::default(), shared.clone(), None);
    for sub in [
        sub_option("restrict", vec![command_option("command", "kik")]),
        sub_option("unrestrict", vec![command_option("command", "kick")]),
        sub_option("bogus", vec![]),
    ] {
        let (_, response) = handle_capture(&runtime, &access_interaction(sub, true)).await;
        let text = response_text(&response.expect("reply"));
        assert!(!text.starts_with("Saved."), "{text}");
        assert_eq!(*shared.lock().unwrap(), AccessControls::default());
    }
}

#[tokio::test]
async fn access_command_reports_store_failures_and_changes_nothing() {
    let shared = Arc::new(Mutex::new(AccessControls::default()));
    let runtime = shared_runtime(
        Trace::default(),
        shared.clone(),
        Some(StoreError::Unavailable),
    );
    let disable = || {
        access_interaction(
            sub_option(
                "creation",
                vec![CommandDataOption {
                    name: "enabled".to_owned(),
                    value: CommandOptionValue::Boolean(false),
                }],
            ),
            true,
        )
    };
    let (_, response) = handle_capture(&runtime, &disable()).await;
    let text = response_text(&response.expect("reply"));
    assert!(text.contains("Nothing was changed"), "{text}");
    assert!(shared.lock().unwrap().room_creation_enabled);

    let unreadable = gated_runtime(
        Trace::default(),
        AccessControls::default(),
        Some(StoreError::Unavailable),
    );
    let (_, response) = handle_capture(&unreadable, &disable()).await;
    assert!(response_text(&response.expect("reply")).contains("Nothing was changed"));
}

fn channel_option(name: &str, id: u64) -> CommandDataOption {
    CommandDataOption {
        name: name.to_owned(),
        value: CommandOptionValue::Channel(Id::new(id)),
    }
}

fn logging_interaction(sub: CommandDataOption, admin: bool) -> Interaction {
    voice_interaction(
        Some(command_data("logging", vec![sub])),
        admin.then_some(Permissions::MANAGE_CHANNELS),
        true,
    )
}

fn logging_action(sub: CommandDataOption) -> LoggingAction {
    match parse_voice_command(&logging_interaction(sub, true)) {
        Some(VoiceCommand::Logging(action)) => action,
        other => panic!("not a /logging command: {other:?}"),
    }
}

fn logging_runtime(
    shared: Arc<Mutex<LoggingSettings>>,
    read_error: Option<StoreError>,
    save_error: Option<StoreError>,
) -> VoiceRuntime<Store, Http> {
    let trace = Trace::default();
    VoiceRuntime::new(
        move || {
            let mut store = Store::new(trace.clone());
            store.logging = shared.clone();
            store.logging_error = read_error;
            store.save_logging_error = save_error;
            (store, Http::new(trace.clone()))
        },
        Duration::from_millis(10),
        true,
    )
}

#[test]
fn parse_logging_subcommands() {
    assert_eq!(
        logging_action(sub_option("show", vec![])),
        LoggingAction::Show
    );
    assert_eq!(
        logging_action(sub_option("level", vec![command_option("level", "Full")])),
        LoggingAction::Level("Full".to_owned())
    );
    assert_eq!(
        logging_action(sub_option("channel", vec![channel_option("channel", 5)])),
        LoggingAction::Channel(Some(5))
    );
    // No channel or role clears the setting.
    assert_eq!(
        logging_action(sub_option("channel", vec![])),
        LoggingAction::Channel(None)
    );
    assert_eq!(
        logging_action(sub_option("mention", vec![role_option("role", 9)])),
        LoggingAction::Mention(Some(9))
    );
    assert_eq!(
        logging_action(sub_option("mention", vec![])),
        LoggingAction::Mention(None)
    );
    // Malformed shapes are answered, never ignored.
    for sub in [sub_option("bogus", vec![]), sub_option("level", vec![])] {
        assert_eq!(logging_action(sub), LoggingAction::Invalid);
    }
    let bare = voice_interaction(Some(command_data("logging", Vec::new())), None, true);
    assert_eq!(
        parse_voice_command(&bare),
        Some(VoiceCommand::Logging(LoggingAction::Invalid))
    );
}

#[tokio::test]
async fn logging_command_needs_an_admin() {
    let runtime = logging_runtime(Arc::default(), None, None);
    let member = logging_interaction(sub_option("show", vec![]), false);
    let (owned, response) = handle_capture(&runtime, &member).await;
    assert!(owned);
    assert_eq!(
        response_text(&response.expect("denial")),
        "You need Manage Channels to use /logging."
    );
}

#[tokio::test]
async fn logging_command_changes_are_saved_and_shown() {
    let shared = Arc::new(Mutex::new(LoggingSettings::default()));
    let runtime = logging_runtime(shared.clone(), None, None);
    let run = |sub| {
        let interaction = logging_interaction(sub, true);
        let runtime = &runtime;
        async move {
            let (_, response) = handle_capture(runtime, &interaction).await;
            response_text(&response.expect("reply"))
        }
    };

    let text = run(sub_option("show", vec![])).await;
    assert!(text.contains("Log level: brief"), "{text}");
    assert!(!text.starts_with("Saved."), "{text}");

    let text = run(sub_option("level", vec![command_option("level", " FULL ")])).await;
    assert!(text.starts_with("Saved."), "{text}");
    assert_eq!(shared.lock().unwrap().level, DetailLevel::Full);

    let text = run(sub_option("channel", vec![channel_option("channel", 5)])).await;
    assert!(text.contains("Log channel: <#5>"), "{text}");
    let text = run(sub_option("mention", vec![role_option("role", 9)])).await;
    assert!(text.contains("Mentioned on errors: <@&9>"), "{text}");
    assert_eq!(
        *shared.lock().unwrap(),
        LoggingSettings {
            level: DetailLevel::Full,
            channel_id: Some(5),
            mention_role_id: Some(9),
        }
    );

    // Clearing the channel and mention keeps the level.
    run(sub_option("channel", vec![])).await;
    run(sub_option("mention", vec![])).await;
    let text = run(sub_option("level", vec![command_option("level", "off")])).await;
    assert!(text.contains("Log level: off"), "{text}");
    assert_eq!(
        *shared.lock().unwrap(),
        LoggingSettings {
            level: DetailLevel::Off,
            channel_id: None,
            mention_role_id: None,
        }
    );
}

#[tokio::test]
async fn logging_command_refuses_bad_input_without_changing_anything() {
    let shared = Arc::new(Mutex::new(LoggingSettings::default()));
    let runtime = logging_runtime(shared.clone(), None, None);
    for sub in [
        sub_option("level", vec![command_option("level", "loud")]),
        sub_option("bogus", vec![]),
    ] {
        let (_, response) = handle_capture(&runtime, &logging_interaction(sub, true)).await;
        let text = response_text(&response.expect("reply"));
        assert!(!text.starts_with("Saved."), "{text}");
        assert_eq!(*shared.lock().unwrap(), LoggingSettings::default());
    }
}

#[tokio::test]
async fn logging_command_reports_store_failures_and_changes_nothing() {
    let level = || {
        logging_interaction(
            sub_option("level", vec![command_option("level", "off")]),
            true,
        )
    };
    let shared = Arc::new(Mutex::new(LoggingSettings::default()));
    let unsavable = logging_runtime(shared.clone(), None, Some(StoreError::Unavailable));
    let (_, response) = handle_capture(&unsavable, &level()).await;
    assert!(response_text(&response.expect("reply")).contains("Nothing was changed"));
    assert_eq!(*shared.lock().unwrap(), LoggingSettings::default());

    let unreadable = logging_runtime(shared.clone(), Some(StoreError::Unavailable), None);
    let (_, response) = handle_capture(&unreadable, &level()).await;
    assert!(response_text(&response.expect("reply")).contains("Nothing was changed"));
    assert_eq!(*shared.lock().unwrap(), LoggingSettings::default());
}

#[tokio::test]
async fn worker_does_not_start_when_access_controls_cannot_load() {
    let (live, mut store, http, _) = fixture();
    store.access_error = Some(StoreError::Unavailable);
    assert!(GuildRoomWorker::load(live, store, http).await.is_err());
}

#[tokio::test]
async fn access_changed_refreshes_a_loaded_worker() {
    for (before, after) in [(false, true), (true, false)] {
        let (live, store, http, _) = fixture();
        *store.access.lock().unwrap() = AccessControls {
            room_creation_enabled: before,
            ..AccessControls::default()
        };
        let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
        apply_command(
            &mut worker,
            ActorCommand::AccessChanged(AccessControls {
                room_creation_enabled: after,
                ..AccessControls::default()
            }),
            0,
        );
        let ticket = worker
            .live
            .voice_update(MEMBER, Some(CREATOR), Some(false))
            .unwrap();
        assert_eq!(
            worker.accept_join(ticket, "new room".to_owned(), 7, NOW.to_owned()),
            after
        );
    }
}

async fn wait_for_trace(trace: &Trace, entry: &str) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if trace.lock().unwrap().iter().any(|line| line == entry) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

#[tokio::test]
async fn access_command_reaches_the_live_actor_without_a_restart() {
    let trace = Trace::default();
    let shared = Arc::new(Mutex::new(AccessControls::default()));
    let runtime = shared_runtime(trace.clone(), shared, None);
    assert!(runtime.publish_snapshot(GUILD, snapshot(&[], vec![])));
    // The actor is loaded once it can answer a status request.
    for _ in 0..500 {
        if runtime.worker_status(GUILD).await.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let creation = |enabled: bool| {
        access_interaction(
            sub_option(
                "creation",
                vec![CommandDataOption {
                    name: "enabled".to_owned(),
                    value: CommandOptionValue::Boolean(enabled),
                }],
            ),
            true,
        )
    };

    handle_capture(&runtime, &creation(false)).await;
    assert!(runtime.voice_frame(GUILD, MEMBER, Some(CREATOR), Some(false), "off".to_owned()));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !trace.lock().unwrap().iter().any(|line| line == "create"),
        "creation is off: {:?}",
        trace.lock().unwrap()
    );

    handle_capture(&runtime, &creation(true)).await;
    assert!(runtime.voice_frame(
        GUILD,
        MEMBER + 1,
        Some(CREATOR),
        Some(false),
        "on".to_owned()
    ));
    assert!(
        wait_for_trace(&trace, "create").await,
        "creation back on: {:?}",
        trace.lock().unwrap()
    );
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
    VoiceResponder::respond_with(&runtime, &replies, &create_interaction(), None, None).await;
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
        VoiceResponder::respond_with(&runtime, &replies, &create_interaction(), None, None).await;
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
    VoiceResponder::respond_with(&runtime, &replies, &create_interaction(), None, None).await;
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
            TextOverwriteTarget::Role(id) => Some(id),
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
    apply_command(&mut worker, ActorCommand::CreatorAdded(edited), now);
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

fn channel_with_overwrites(
    id: u64,
    kind: u8,
    parent: Option<u64>,
    overwrites: serde_json::Value,
) -> Channel {
    let mut channel = channel(id, kind, parent);
    channel.permission_overwrites = Some(serde_json::from_value(overwrites).unwrap());
    channel
}

fn health_guild(
    base: Permissions,
    category: Vec<serde_json::Value>,
    creator: Vec<serde_json::Value>,
) -> LiveGuild {
    let live = LiveGuild::new(GUILD);
    live.publish(GuildSnapshot {
        channels: vec![
            channel_with_overwrites(CREATOR, 2, Some(CATEGORY), json!(creator)),
            channel_with_overwrites(CATEGORY, 4, None, json!(category)),
        ],
        members: vec![],
        bot: BotAccess {
            member_id: 999,
            guild_owner_id: 998,
            system_channel_id: None,
            member_roles: vec![],
            roles: vec![role(base)],
        },
    });
    live
}

fn everyone_overwrite(deny: Permissions) -> serde_json::Value {
    json!({ "id": GUILD.to_string(), "type": 0, "allow": "0", "deny": deny.bits().to_string() })
}

#[test]
fn health_check_is_clean_when_every_level_grants_the_four_permissions() {
    let live = health_guild(permissions(), vec![], vec![]);
    assert!(live.permission_findings(&[CREATOR]).is_empty());
}

#[test]
fn health_check_names_a_missing_guild_level_permission() {
    let live = health_guild(permissions() - Permissions::MOVE_MEMBERS, vec![], vec![]);
    assert_eq!(
        live.permission_findings(&[CREATOR]),
        vec![PermissionFinding {
            permission: VoicePermission::MoveMembers,
            scope: VoicePermissionScope::Guild,
            category_id: None,
            channel_id: None,
        }]
    );
}

#[test]
fn health_check_names_the_category_override_that_causes_it() {
    let live = health_guild(
        permissions(),
        vec![everyone_overwrite(Permissions::MANAGE_CHANNELS)],
        vec![],
    );
    let findings = live.permission_findings(&[CREATOR]);
    assert_eq!(
        findings,
        vec![PermissionFinding {
            permission: VoicePermission::ManageChannels,
            scope: VoicePermissionScope::Category,
            category_id: Some(CATEGORY),
            channel_id: None,
        }]
    );
    assert_eq!(
        health_line(&findings[0]),
        "health: the permission override on category <#400> removes Manage Channels from the bot"
    );
}

#[test]
fn health_check_names_a_creator_channel_override() {
    let live = health_guild(
        permissions(),
        vec![],
        vec![everyone_overwrite(Permissions::VIEW_CHANNEL)],
    );
    let findings = live.permission_findings(&[CREATOR]);
    assert_eq!(
        findings,
        vec![PermissionFinding {
            permission: VoicePermission::ViewChannel,
            scope: VoicePermissionScope::Channel,
            category_id: None,
            channel_id: Some(CREATOR),
        }]
    );
    assert_eq!(
        health_line(&findings[0]),
        "health: the permission override on <#200> removes View Channel from the bot"
    );
}

#[test]
fn health_check_reports_nothing_for_incomplete_data_or_unknown_channels() {
    let never_published = LiveGuild::new(GUILD);
    assert!(never_published.permission_findings(&[CREATOR]).is_empty());
    let live = health_guild(permissions() - Permissions::MANAGE_ROLES, vec![], vec![]);
    assert!(live.permission_findings(&[12345]).is_empty());
    assert!(live.permission_findings(&[]).is_empty());
    assert_eq!(
        health_line(&live.permission_findings(&[CREATOR])[0]),
        "health: the bot lacks Manage Roles for the whole server"
    );
}

#[test]
fn health_check_dedups_a_shared_category_override() {
    let live = LiveGuild::new(GUILD);
    live.publish(GuildSnapshot {
        channels: vec![
            channel(CREATOR, 2, Some(CATEGORY)),
            channel(201, 2, Some(CATEGORY)),
            channel_with_overwrites(
                CATEGORY,
                4,
                None,
                json!([everyone_overwrite(Permissions::MOVE_MEMBERS)]),
            ),
        ],
        members: vec![],
        bot: BotAccess {
            member_id: 999,
            guild_owner_id: 998,
            system_channel_id: None,
            member_roles: vec![],
            roles: vec![role(permissions())],
        },
    });
    assert_eq!(live.permission_findings(&[CREATOR, 201]).len(), 1);
}

// --- V11c `/export` + `/import` handler tests ---------------------------------

use twilight_model::{
    application::interaction::{
        message_component::MessageComponentInteractionData, InteractionDataResolved,
    },
    channel::message::component::ComponentType,
};
use two_bot_core::voice_config as config_codec;

const UPLOADER: u64 = 302;
const OTHER_MEMBER: u64 = 303;
const ATTACHMENT_ID: u64 = 900;
const TEXT_CHANNEL: u64 = 500;
const TEMPLATE_CHANNEL: u64 = 600;
const MENTION_ROLE: u64 = 700;

fn empty_config() -> VoiceConfiguration {
    VoiceConfiguration {
        version: config_codec::VOICE_CONFIG_VERSION,
        guild_id: GUILD.to_string(),
        creators: Vec::new(),
        templates: Vec::new(),
        aliases: Vec::new(),
        lists: Vec::new(),
        logging: None,
        settings: config_codec::GuildSettings {
            creation_enabled: true,
            unique_names: false,
            no_game_label: "No game".to_owned(),
            force_single_game: false,
            count_members_without_activity: false,
            time_zone: "UTC".to_owned(),
            text_channel_name: "voice-chat".to_owned(),
            text_viewer_role_id: None,
            command_role_id: None,
            command_roles: Vec::new(),
        },
    }
}

fn full_config() -> VoiceConfiguration {
    let mut config = empty_config();
    config.creators = vec![config_codec::CreatorConfiguration {
        channel_id: CREATOR.to_string(),
        name_template: "{game}".to_owned(),
        status_template: None,
        default_limit: 0,
        always_private: false,
        text_channels: false,
        position: config_codec::RoomPosition::Above,
        first_number: 1,
        group_by_category: false,
        permission_source: config_codec::PermissionSource::Creator {},
    }];
    config.templates = vec![config_codec::ChannelTemplates {
        channel_id: TEMPLATE_CHANNEL.to_string(),
        name_template: "Lobby".to_owned(),
        status_template: None,
    }];
    config.aliases = vec![config_codec::GameAlias {
        game: "chess".to_owned(),
        alias: "Chess".to_owned(),
    }];
    config.lists = vec![config_codec::RandomList {
        name: "maps".to_owned(),
        choices: vec!["a".to_owned(), "b".to_owned()],
    }];
    config.logging = Some(config_codec::LoggingConfiguration {
        channel_id: TEXT_CHANNEL.to_string(),
        detail: config_codec::LogDetail::Errors,
        mention_member_ids: vec![MEMBER.to_string()],
        mention_role_ids: vec![MENTION_ROLE.to_string()],
    });
    config
}

fn config_inventory() -> GuildInventory {
    let guild = GUILD.to_string();
    let channel = |kind: config_codec::ChannelKind| config_codec::ChannelReference {
        guild_id: guild.clone(),
        kind,
    };
    GuildInventory {
        guild_id: guild.clone(),
        channels: [
            (
                CREATOR.to_string(),
                channel(config_codec::ChannelKind::Voice),
            ),
            (
                CATEGORY.to_string(),
                channel(config_codec::ChannelKind::Category),
            ),
            (
                TEXT_CHANNEL.to_string(),
                channel(config_codec::ChannelKind::Text),
            ),
            (
                TEMPLATE_CHANNEL.to_string(),
                channel(config_codec::ChannelKind::Voice),
            ),
        ]
        .into_iter()
        .collect(),
        roles: [
            (GUILD.to_string(), guild.clone()),
            (MENTION_ROLE.to_string(), guild.clone()),
        ]
        .into_iter()
        .collect(),
        members: [
            (MEMBER.to_string(), guild.clone()),
            (UPLOADER.to_string(), guild.clone()),
        ]
        .into_iter()
        .collect(),
    }
}

fn user(id: u64) -> User {
    serde_json::from_value(json!({
        "id": id.to_string(), "username": "fixture",
        "discriminator": "0001", "avatar": null,
    }))
    .unwrap()
}

fn with_user(mut interaction: Interaction, id: u64) -> Interaction {
    interaction.member.as_mut().expect("member").user = Some(user(id));
    interaction
}

fn manager() -> Option<Permissions> {
    Some(Permissions::MANAGE_GUILD)
}

fn import_interaction(size: u64, permissions: Option<Permissions>, member_id: u64) -> Interaction {
    let meta: twilight_model::channel::Attachment = serde_json::from_value(json!({
        "id": ATTACHMENT_ID.to_string(),
        "filename": "voice-config.json",
        "url": "https://cdn.discordapp.com/attachments/1/2/voice-config.json",
        "proxy_url": "https://media.discordapp.net/attachments/1/2/voice-config.json",
        "size": size,
    }))
    .unwrap();
    let mut data = command_data(
        "import",
        vec![CommandDataOption {
            name: "file".to_owned(),
            value: CommandOptionValue::Attachment(Id::new(ATTACHMENT_ID)),
        }],
    );
    data.resolved = Some(InteractionDataResolved {
        attachments: [(Id::new(ATTACHMENT_ID), meta)].into_iter().collect(),
        channels: HashMap::new(),
        members: HashMap::new(),
        messages: HashMap::new(),
        roles: HashMap::new(),
        users: HashMap::new(),
    });
    with_user(voice_interaction(Some(data), permissions, true), member_id)
}

fn component_interaction(
    custom_id: &str,
    permissions: Option<Permissions>,
    member_id: u64,
) -> Interaction {
    let mut interaction = with_user(voice_interaction(None, permissions, true), member_id);
    interaction.kind = InteractionType::MessageComponent;
    interaction.data = Some(InteractionData::MessageComponent(Box::new(
        MessageComponentInteractionData {
            custom_id: custom_id.to_owned(),
            component_type: ComponentType::Button,
            resolved: None,
            values: Vec::new(),
        },
    )));
    interaction
}

async fn handle_import_capture(
    runtime: &VoiceRuntime<Store, Http>,
    interaction: &Interaction,
    inventory: Option<&GuildInventory>,
) -> (bool, Option<InteractionResponse>) {
    let seen = Arc::new(Mutex::new(None::<InteractionResponse>));
    let writer = seen.clone();
    let owned = handle_voice_interaction_with(runtime, interaction, None, inventory, |response| {
        *writer.lock().unwrap() = Some(response);
        async {}
    })
    .await;
    let response = seen.lock().unwrap().clone();
    (owned, response)
}

/// Runtime sharing one configuration and one scripted download queue across
/// every store/http pair, so an upload and its Confirm see the same state.
fn import_harness(
    trace: Trace,
    config: VoiceConfiguration,
    downloads: Vec<Result<Vec<u8>, RoomHttpError>>,
) -> (VoiceRuntime<Store, Http>, Arc<Mutex<VoiceConfiguration>>) {
    let shared_config = Arc::new(Mutex::new(config));
    let shared_downloads = Arc::new(Mutex::new(downloads.into_iter().collect::<VecDeque<_>>()));
    let closure_config = shared_config.clone();
    let runtime = VoiceRuntime::new(
        move || {
            let mut store = Store::new(trace.clone());
            store.config = closure_config.clone();
            let mut http = Http::new(trace.clone());
            http.download_results = shared_downloads.clone();
            (store, http)
        },
        Duration::from_millis(10),
        true,
    );
    (runtime, shared_config)
}

fn preview_buttons(response: &InteractionResponse) -> (String, String) {
    let components = response
        .data
        .as_ref()
        .and_then(|data| data.components.as_ref())
        .expect("preview buttons");
    let mut confirm = None;
    let mut cancel = None;
    for component in components {
        let Component::ActionRow(row) = component else {
            continue;
        };
        for inner in &row.components {
            let Component::Button(button) = inner else {
                continue;
            };
            match button.label.as_deref() {
                Some("Confirm") => confirm = button.custom_id.clone(),
                Some("Cancel") => cancel = button.custom_id.clone(),
                _ => {}
            }
        }
    }
    (
        confirm.expect("confirm button"),
        cancel.expect("cancel button"),
    )
}

fn applied(trace: &Trace) -> bool {
    trace
        .lock()
        .unwrap()
        .iter()
        .any(|entry| entry == "config_apply")
}

#[test]
fn parse_export_and_import_attachment_option() {
    let export = with_user(
        voice_interaction(Some(command_data("export", Vec::new())), manager(), true),
        UPLOADER,
    );
    assert_eq!(parse_voice_command(&export), Some(VoiceCommand::Export));
    let import = import_interaction(10, manager(), UPLOADER);
    assert_eq!(
        parse_voice_command(&import),
        Some(VoiceCommand::Import {
            file_id: Some(Id::new(ATTACHMENT_ID)),
        })
    );
    // A missing or wrong-typed option is answered, never silently ignored.
    let missing = with_user(
        voice_interaction(Some(command_data("import", Vec::new())), manager(), true),
        UPLOADER,
    );
    assert_eq!(
        parse_voice_command(&missing),
        Some(VoiceCommand::Import { file_id: None })
    );
    let wrong = with_user(
        voice_interaction(
            Some(command_data("import", vec![command_option("file", "x")])),
            manager(),
            true,
        ),
        UPLOADER,
    );
    assert_eq!(
        parse_voice_command(&wrong),
        Some(VoiceCommand::Import { file_id: None })
    );
    // Other components stay untouched by the import router.
    let kick = component_interaction("two:voice:kick-yes:99", manager(), UPLOADER);
    assert_eq!(voice_import_action(&kick), None);
    let slash = import_interaction(10, manager(), UPLOADER);
    assert_eq!(voice_import_action(&slash), None);
}

#[tokio::test]
async fn export_denied_without_manage_server() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = with_user(
        voice_interaction(Some(command_data("export", Vec::new())), None, true),
        MEMBER,
    );
    let inventory = config_inventory();
    let (owned, response) = handle_import_capture(&runtime, &interaction, Some(&inventory)).await;
    assert!(owned);
    let response = response.expect("refusal");
    assert!(response_text(&response).contains("Manage Server"));
    assert!(response
        .data
        .as_ref()
        .and_then(|data| data.attachments.as_ref())
        .is_none());
    assert!(trace.lock().unwrap().is_empty());
}

#[tokio::test]
async fn export_attaches_versioned_file_ephemerally() {
    let trace = Trace::default();
    let (runtime, shared) = import_harness(trace.clone(), full_config(), Vec::new());
    let interaction = with_user(
        voice_interaction(Some(command_data("export", Vec::new())), manager(), true),
        UPLOADER,
    );
    let inventory = config_inventory();
    let (owned, response) = handle_import_capture(&runtime, &interaction, Some(&inventory)).await;
    assert!(owned);
    let response = response.expect("export reply");
    assert_eq!(
        response.data.as_ref().and_then(|data| data.flags),
        Some(MessageFlags::EPHEMERAL)
    );
    assert!(response_text(&response).contains("/import"));
    let attachments = response
        .data
        .as_ref()
        .and_then(|data| data.attachments.as_ref())
        .expect("export file");
    assert_eq!(attachments.len(), 1);
    assert_eq!(
        attachments[0].filename,
        format!("voice-config-guild-{GUILD}-v1.json")
    );
    let decoded: VoiceConfiguration =
        serde_json::from_slice(&attachments[0].file).expect("exported JSON parses");
    assert_eq!(decoded, full_config());
    assert_eq!(decoded, *shared.lock().unwrap());
    assert!(trace.lock().unwrap().is_empty());
}

#[tokio::test]
async fn export_and_import_refuse_without_guild_inventory() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let export = with_user(
        voice_interaction(Some(command_data("export", Vec::new())), manager(), true),
        UPLOADER,
    );
    let (owned, response) = handle_import_capture(&runtime, &export, None).await;
    assert!(owned);
    assert!(response_text(&response.expect("refusal")).contains("unavailable"));
    let import = import_interaction(10, manager(), UPLOADER);
    let (owned, response) = handle_import_capture(&runtime, &import, None).await;
    assert!(owned);
    assert!(response_text(&response.expect("refusal")).contains("unavailable"));
}

#[tokio::test]
async fn import_denied_without_manage_server() {
    let trace = Trace::default();
    let (runtime, _) = import_harness(trace.clone(), empty_config(), Vec::new());
    let interaction = import_interaction(10, None, MEMBER);
    let inventory = config_inventory();
    let (owned, response) = handle_import_capture(&runtime, &interaction, Some(&inventory)).await;
    assert!(owned);
    assert!(response_text(&response.expect("refusal")).contains("Manage Server"));
    assert!(trace.lock().unwrap().is_empty());
}

#[tokio::test]
async fn import_refuses_oversize_before_download() {
    let trace = Trace::default();
    let (runtime, _) = import_harness(trace.clone(), empty_config(), Vec::new());
    let interaction = import_interaction(MAX_IMPORT_BYTES as u64 + 1, manager(), UPLOADER);
    let inventory = config_inventory();
    let (owned, response) = handle_import_capture(&runtime, &interaction, Some(&inventory)).await;
    assert!(owned);
    assert!(response_text(&response.expect("refusal")).contains("too large"));
    // The size check runs before any download.
    assert!(trace.lock().unwrap().is_empty());
}

#[tokio::test]
async fn import_refuses_malformed_upload() {
    let trace = Trace::default();
    let (runtime, shared) = import_harness(
        trace.clone(),
        empty_config(),
        vec![Ok(b"not json{".to_vec())],
    );
    let interaction = import_interaction(9, manager(), UPLOADER);
    let inventory = config_inventory();
    let (owned, response) = handle_import_capture(&runtime, &interaction, Some(&inventory)).await;
    assert!(owned);
    assert!(response_text(&response.expect("refusal")).contains("malformed"));
    assert_eq!(*shared.lock().unwrap(), empty_config());
    assert!(!applied(&trace));
}

#[tokio::test]
async fn import_empty_diff_offers_no_confirm() {
    let trace = Trace::default();
    let bytes = serde_json::to_vec(&full_config()).unwrap();
    let (runtime, _) = import_harness(trace.clone(), full_config(), vec![Ok(bytes.clone())]);
    let interaction = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let inventory = config_inventory();
    let (owned, response) = handle_import_capture(&runtime, &interaction, Some(&inventory)).await;
    assert!(owned);
    let response = response.expect("empty notice");
    assert_eq!(response_text(&response), "No changes");
    assert!(response
        .data
        .as_ref()
        .and_then(|data| data.components.as_ref())
        .is_none());
    assert!(!applied(&trace));
}

#[tokio::test]
async fn import_confirm_applies() {
    let trace = Trace::default();
    let bytes = serde_json::to_vec(&full_config()).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (owned, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    assert!(owned);
    let preview = preview.expect("preview");
    let (confirm_id, _) = preview_buttons(&preview);
    assert!(!applied(&trace));

    let confirm = component_interaction(&confirm_id, manager(), UPLOADER);
    let (owned, response) = handle_import_capture(&runtime, &confirm, Some(&inventory)).await;
    assert!(owned);
    assert!(response_text(&response.expect("applied")).starts_with("Import applied:"));
    assert_eq!(*shared.lock().unwrap(), full_config());
    assert!(applied(&trace));
}

#[tokio::test]
async fn import_skips_unknown_channels_and_confirms_remainder() {
    let trace = Trace::default();
    let mut incoming = full_config();
    incoming.aliases[0].alias = "Chess Club".to_owned();
    incoming.creators.push(config_codec::CreatorConfiguration {
        channel_id: "999".to_owned(),
        name_template: "{game}".to_owned(),
        status_template: None,
        default_limit: 0,
        always_private: false,
        text_channels: false,
        position: config_codec::RoomPosition::Above,
        first_number: 1,
        group_by_category: false,
        permission_source: config_codec::PermissionSource::Creator {},
    });
    let bytes = serde_json::to_vec(&incoming).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), full_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let preview = preview.expect("preview");
    let text = response_text(&preview);
    assert!(text.contains("unknown channel"), "{text}");
    assert!(text.contains("999"), "{text}");
    let (confirm_id, _) = preview_buttons(&preview);

    let confirm = component_interaction(&confirm_id, manager(), UPLOADER);
    let (owned, response) = handle_import_capture(&runtime, &confirm, Some(&inventory)).await;
    assert!(owned);
    assert!(response_text(&response.expect("applied")).starts_with("Import applied:"));
    // The known change applied; the unknown channel never entered storage.
    let stored = shared.lock().unwrap().clone();
    assert_eq!(stored.aliases[0].alias, "Chess Club");
    assert!(stored
        .creators
        .iter()
        .all(|creator| creator.channel_id != "999"));
    assert!(applied(&trace));
}

#[tokio::test]
async fn import_unknown_channels_only_reports_without_confirm() {
    let trace = Trace::default();
    let mut incoming = full_config();
    incoming.creators.push(config_codec::CreatorConfiguration {
        channel_id: "999".to_owned(),
        name_template: "{game}".to_owned(),
        status_template: None,
        default_limit: 0,
        always_private: false,
        text_channels: false,
        position: config_codec::RoomPosition::Above,
        first_number: 1,
        group_by_category: false,
        permission_source: config_codec::PermissionSource::Creator {},
    });
    let bytes = serde_json::to_vec(&incoming).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), full_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, response) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let response = response.expect("notice");
    let text = response_text(&response);
    assert!(text.contains("unknown channel"), "{text}");
    assert!(text.contains("999"), "{text}");
    assert!(response
        .data
        .as_ref()
        .and_then(|data| data.components.as_ref())
        .is_none());
    assert_eq!(*shared.lock().unwrap(), full_config());
    assert!(!applied(&trace));
}

#[tokio::test]
async fn import_cancel_needs_no_inventory() {
    let trace = Trace::default();
    let bytes = serde_json::to_vec(&full_config()).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let preview = preview.expect("preview");
    let (_, cancel_id) = preview_buttons(&preview);

    let cancel = component_interaction(&cancel_id, manager(), UPLOADER);
    let (owned, response) = handle_import_capture(&runtime, &cancel, None).await;
    assert!(owned);
    assert!(response_text(&response.expect("cancelled")).contains("cancelled"));
    assert_eq!(*shared.lock().unwrap(), empty_config());
    assert!(!applied(&trace));
}

#[tokio::test]
async fn import_cancel_writes_nothing_and_consumes_the_preview() {
    let trace = Trace::default();
    let bytes = serde_json::to_vec(&full_config()).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let preview = preview.expect("preview");
    let (confirm_id, cancel_id) = preview_buttons(&preview);

    let cancel = component_interaction(&cancel_id, manager(), UPLOADER);
    let (owned, response) = handle_import_capture(&runtime, &cancel, Some(&inventory)).await;
    assert!(owned);
    assert!(response_text(&response.expect("cancelled")).contains("cancelled"));
    assert_eq!(*shared.lock().unwrap(), empty_config());
    assert!(!applied(&trace));

    // The consumed preview cannot be confirmed afterwards.
    let confirm = component_interaction(&confirm_id, manager(), UPLOADER);
    let (_, response) = handle_import_capture(&runtime, &confirm, Some(&inventory)).await;
    assert!(response_text(&response.expect("expired")).contains("expired"));
    assert_eq!(*shared.lock().unwrap(), empty_config());
    assert!(!applied(&trace));
}

#[tokio::test]
async fn import_stale_confirm_repreviews_instead_of_applying() {
    let trace = Trace::default();
    let bytes = serde_json::to_vec(&full_config()).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let (stale_confirm_id, _) = preview_buttons(&preview.expect("preview"));

    // A concurrent change lands before Confirm.
    let mut concurrent = full_config();
    concurrent.aliases.push(config_codec::GameAlias {
        game: "go".to_owned(),
        alias: "Go".to_owned(),
    });
    *shared.lock().unwrap() = concurrent.clone();

    let confirm = component_interaction(&stale_confirm_id, manager(), UPLOADER);
    let (owned, response) = handle_import_capture(&runtime, &confirm, Some(&inventory)).await;
    assert!(owned);
    let response = response.expect("fresh preview");
    assert!(
        response_text(&response).contains("Import preview:"),
        "{}",
        response_text(&response)
    );
    // The candidate was not applied over the concurrent change.
    assert_eq!(*shared.lock().unwrap(), concurrent);
    assert!(!applied(&trace));

    // The fresh preview confirms cleanly.
    let (fresh_confirm_id, _) = preview_buttons(&response);
    assert_ne!(fresh_confirm_id, stale_confirm_id);
    let confirm = component_interaction(&fresh_confirm_id, manager(), UPLOADER);
    let (_, response) = handle_import_capture(&runtime, &confirm, Some(&inventory)).await;
    assert!(response_text(&response.expect("applied")).starts_with("Import applied:"));
    assert_eq!(*shared.lock().unwrap(), full_config());
    assert!(applied(&trace));
}

#[tokio::test]
async fn import_confirm_rejects_foreign_member() {
    let trace = Trace::default();
    let bytes = serde_json::to_vec(&full_config()).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let (confirm_id, _) = preview_buttons(&preview.expect("preview"));

    let foreign = component_interaction(&confirm_id, manager(), OTHER_MEMBER);
    let (owned, response) = handle_import_capture(&runtime, &foreign, Some(&inventory)).await;
    assert!(owned);
    assert!(response_text(&response.expect("refusal")).contains("Only the member"));
    assert_eq!(*shared.lock().unwrap(), empty_config());
    assert!(!applied(&trace));
}

#[tokio::test]
async fn import_confirm_requires_manage_server() {
    let trace = Trace::default();
    let bytes = serde_json::to_vec(&full_config()).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let (confirm_id, _) = preview_buttons(&preview.expect("preview"));

    // Same uploader, but the Manage Server grant is gone at confirm time.
    let confirm = component_interaction(&confirm_id, None, UPLOADER);
    let (owned, response) = handle_import_capture(&runtime, &confirm, Some(&inventory)).await;
    assert!(owned);
    assert!(response_text(&response.expect("refusal")).contains("Manage Server"));
    assert_eq!(*shared.lock().unwrap(), empty_config());
    assert!(!applied(&trace));
}

#[tokio::test]
async fn import_confirm_expired_writes_nothing() {
    let trace = Trace::default();
    let bytes = serde_json::to_vec(&full_config()).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let (confirm_id, _) = preview_buttons(&preview.expect("preview"));

    runtime
        .pending_imports
        .lock()
        .expect("voice runtime lock")
        .iter_mut()
        .for_each(|(_, entry)| {
            entry.expires_at = Instant::now() - Duration::from_secs(1);
        });
    let confirm = component_interaction(&confirm_id, manager(), UPLOADER);
    let (owned, response) = handle_import_capture(&runtime, &confirm, Some(&inventory)).await;
    assert!(owned);
    assert!(response_text(&response.expect("expired")).contains("expired"));
    assert_eq!(*shared.lock().unwrap(), empty_config());
    assert!(!applied(&trace));
}

#[tokio::test]
async fn import_without_file_asks_for_attachment() {
    let trace = Trace::default();
    let (runtime, _) = import_harness(trace.clone(), empty_config(), Vec::new());
    let interaction = with_user(
        voice_interaction(Some(command_data("import", Vec::new())), manager(), true),
        UPLOADER,
    );
    let inventory = config_inventory();
    let (owned, response) = handle_import_capture(&runtime, &interaction, Some(&inventory)).await;
    assert!(owned);
    assert!(response_text(&response.expect("refusal")).contains("Attach"));
    assert!(trace.lock().unwrap().is_empty());
}

// --- V10 error notices ----------------------------------------------------

const SYSTEM_CHANNEL: u64 = 700;
const OWNER: u64 = 998;
const NOTICE_ROLE: u64 = 55;

fn notice_bot(system_channel_id: Option<u64>) -> BotAccess {
    BotAccess {
        member_id: 999,
        guild_owner_id: OWNER,
        system_channel_id,
        member_roles: vec![],
        roles: vec![role(permissions())],
    }
}

/// A worker holding exactly one tracked failure (persistence failure).
async fn failed_worker(
    settings: LoggingSettings,
    system_channel_id: Option<u64>,
) -> GuildRoomWorker<Store, Http> {
    let (live, mut store, http, _) = fixture();
    store.persist_error = Some(StoreError::Unavailable);
    *store.logging.lock().unwrap() = settings;
    live.refresh_bot(notice_bot(system_channel_id));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    assert_eq!(worker.failures().len(), 1);
    worker
}

fn sent(worker: &GuildRoomWorker<Store, Http>) -> Vec<(NoticeTarget, String, Option<u64>)> {
    worker.http.notices.lock().unwrap().clone()
}

#[tokio::test]
async fn notice_goes_to_the_system_channel_with_the_role_mention_then_repeats_are_bounded() {
    let settings = LoggingSettings {
        mention_role_id: Some(NOTICE_ROLE),
        ..LoggingSettings::default()
    };
    let mut worker = failed_worker(settings, Some(SYSTEM_CHANNEL)).await;

    assert!(worker.send_notices(0).await);
    let first = sent(&worker);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].0, NoticeTarget::Channel(SYSTEM_CHANNEL));
    assert_eq!(first[0].2, Some(NOTICE_ROLE));
    assert!(first[0].1.contains("/setup"));
    assert!(
        !first[0].1.contains("store"),
        "brief notices carry no detail"
    );

    // Too soon: nothing is sent and nothing is counted.
    assert!(!worker.send_notices(NOTICE_REPEAT_INTERVAL_MS - 1).await);
    assert_eq!(sent(&worker).len(), 1);

    // Two repeats, then silence.
    assert!(worker.send_notices(NOTICE_REPEAT_INTERVAL_MS).await);
    assert!(worker.send_notices(2 * NOTICE_REPEAT_INTERVAL_MS).await);
    assert!(!worker.send_notices(3 * NOTICE_REPEAT_INTERVAL_MS).await);
    assert!(!worker.send_notices(10 * NOTICE_REPEAT_INTERVAL_MS).await);
    assert_eq!(sent(&worker).len(), 3);
    // The failure stays listed for /setup even though notices stopped.
    assert_eq!(worker.failures().len(), 1);
}

#[tokio::test]
async fn full_level_names_the_failure() {
    let settings = LoggingSettings {
        level: DetailLevel::Full,
        ..LoggingSettings::default()
    };
    let mut worker = failed_worker(settings, Some(SYSTEM_CHANNEL)).await;
    assert!(worker.send_notices(0).await);
    assert!(sent(&worker)[0].1.contains("store <#500>"));
}

#[tokio::test]
async fn configured_channel_wins_over_the_fallback_chain() {
    let settings = LoggingSettings {
        channel_id: Some(650),
        ..LoggingSettings::default()
    };
    let mut worker = failed_worker(settings, Some(SYSTEM_CHANNEL)).await;
    assert!(worker.send_notices(0).await);
    let notices = sent(&worker);
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].0, NoticeTarget::Channel(650));
}

#[tokio::test]
async fn notices_fall_back_from_system_channel_to_owner_dm_to_creator_chat() {
    let mut worker = failed_worker(LoggingSettings::default(), Some(SYSTEM_CHANNEL)).await;
    worker
        .http
        .refused_notices
        .lock()
        .unwrap()
        .push(NoticeTarget::Channel(SYSTEM_CHANNEL));
    assert!(worker.send_notices(0).await);
    let notices = sent(&worker);
    assert_eq!(notices[0].0, NoticeTarget::DirectMessage(OWNER));
    assert_eq!(notices[0].2, None, "a DM never carries the role mention");

    worker
        .http
        .refused_notices
        .lock()
        .unwrap()
        .push(NoticeTarget::DirectMessage(OWNER));
    assert!(worker.send_notices(NOTICE_REPEAT_INTERVAL_MS).await);
    assert_eq!(sent(&worker)[1].0, NoticeTarget::Channel(CREATOR));
}

#[tokio::test]
async fn no_system_channel_goes_straight_to_the_owner_dm() {
    let mut worker = failed_worker(LoggingSettings::default(), None).await;
    assert!(worker.send_notices(0).await);
    assert_eq!(sent(&worker)[0].0, NoticeTarget::DirectMessage(OWNER));
}

#[tokio::test]
async fn an_unreachable_guild_still_stops_after_the_bound() {
    let mut worker = failed_worker(LoggingSettings::default(), Some(SYSTEM_CHANNEL)).await;
    for target in [
        NoticeTarget::Channel(SYSTEM_CHANNEL),
        NoticeTarget::DirectMessage(OWNER),
        NoticeTarget::Channel(CREATOR),
    ] {
        worker.http.refused_notices.lock().unwrap().push(target);
    }
    for step in 0..3 {
        assert!(worker.send_notices(step * NOTICE_REPEAT_INTERVAL_MS).await);
    }
    assert!(!worker.send_notices(3 * NOTICE_REPEAT_INTERVAL_MS).await);
    assert!(sent(&worker).is_empty());
}

#[tokio::test]
async fn off_level_sends_nothing_and_spends_no_budget() {
    let settings = LoggingSettings {
        level: DetailLevel::Off,
        ..LoggingSettings::default()
    };
    let mut worker = failed_worker(settings, Some(SYSTEM_CHANNEL)).await;
    assert!(!worker.send_notices(0).await);
    assert!(sent(&worker).is_empty());

    *worker.store.logging.lock().unwrap() = LoggingSettings::default();
    assert!(worker.send_notices(NOTICE_REPEAT_INTERVAL_MS).await);
    assert_eq!(sent(&worker).len(), 1);
}

#[tokio::test]
async fn unreadable_settings_send_nothing() {
    let (live, mut store, http, _) = fixture();
    store.persist_error = Some(StoreError::Unavailable);
    live.refresh_bot(notice_bot(Some(SYSTEM_CHANNEL)));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    worker.store.logging_error = Some(StoreError::Unavailable);
    assert!(!worker.send_notices(0).await);
    assert!(sent(&worker).is_empty());
}

#[tokio::test]
async fn a_halted_worker_sends_no_notices() {
    let mut worker = failed_worker(LoggingSettings::default(), Some(SYSTEM_CHANNEL)).await;
    worker.halted = true;
    assert!(!worker.send_notices(0).await);
    assert!(sent(&worker).is_empty());
}

#[test]
fn notice_text_is_bounded() {
    let failure = LifecycleFailure::CategoryFull {
        creator_id: 1,
        message: "x".repeat(5000),
    };
    assert!(notice_text(&failure, DetailLevel::Full).chars().count() <= NOTICE_MAX_CHARS);
}
