use super::*;
use serde_json::json;
use std::sync::Mutex;

#[path = "voice_kick_tests.rs"]
mod kick;
#[path = "voice_name_tests.rs"]
mod name;
#[path = "voice_private_runtime_tests.rs"]
mod private;
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

/// Guild role facts are independent of channel-effective interaction permissions.
fn command_snapshot() -> GuildSnapshot {
    let mut guild = snapshot(&[], vec![]);
    guild.bot.member_roles = vec![Id::new(900)];
    guild.bot.roles = vec![role(Permissions::empty()), role_with(900, permissions())];
    for permissions in [
        Permissions::empty(),
        Permissions::VIEW_CHANNEL,
        Permissions::MANAGE_CHANNELS,
        Permissions::ADMINISTRATOR,
        Permissions::MANAGE_GUILD,
        Permissions::VIEW_CHANNEL | Permissions::MANAGE_CHANNELS,
        Permissions::VIEW_CHANNEL | Permissions::MANAGE_GUILD,
        Permissions::MANAGE_GUILD | Permissions::MANAGE_CHANNELS,
    ] {
        guild
            .bot
            .roles
            .push(role_with(10_000 + permissions.bits(), permissions));
    }
    guild
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
    owner_intents: Mutex<HashMap<u64, OwnerGrantIntent>>,
    owner_pending: Mutex<HashSet<u64>>,
    owner_revision: AtomicU64,
    prepare_errors: Mutex<VecDeque<StoreError>>,
    ownership_errors: Mutex<VecDeque<StoreError>>,
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
    privacy: Mutex<BTreeMap<u64, PrivacyRecord>>,
    save_privacy_errors: Mutex<VecDeque<StoreError>>,
    add_creator_error: Mutex<Option<StoreError>>,
    after_persist: Option<Hook>,
    config: Arc<Mutex<VoiceConfiguration>>,
    config_error: Option<StoreError>,
    save_config_error: Option<StoreError>,
    creators_error: Option<StoreError>,
    custom_names: Mutex<HashMap<u64, String>>,
    save_custom_name_errors: Mutex<VecDeque<StoreError>>,
}

impl Store {
    fn new(trace: Trace) -> Self {
        Self {
            trace,
            creators: Mutex::new(vec![CreatorChannel::new(GUILD, CREATOR)]),
            rooms: Mutex::new(HashMap::new()),
            owner_intents: Mutex::new(HashMap::new()),
            owner_pending: Mutex::new(HashSet::new()),
            owner_revision: AtomicU64::new(0),
            prepare_errors: Mutex::new(VecDeque::new()),
            ownership_errors: Mutex::new(VecDeque::new()),
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
            privacy: Mutex::new(BTreeMap::new()),
            save_privacy_errors: Mutex::new(VecDeque::new()),
            add_creator_error: Mutex::new(None),
            after_persist: None,
            config: Arc::new(Mutex::new(empty_config())),
            config_error: None,
            save_config_error: None,
            creators_error: None,
            custom_names: Mutex::new(HashMap::new()),
            save_custom_name_errors: Mutex::new(VecDeque::new()),
        }
    }
}

impl RoomPersistence for Store {
    async fn creators(&self, _: u64) -> Result<Vec<CreatorChannel>, StoreError> {
        match self.creators_error {
            Some(error) => Err(error),
            None => Ok(self.creators.lock().unwrap().clone()),
        }
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
    async fn pending_owner_grants(&self, _: u64) -> Result<Vec<u64>, StoreError> {
        Ok(self.owner_pending.lock().unwrap().iter().copied().collect())
    }
    async fn prepare_owner_grants(
        &self,
        room: &VoiceRoom,
        previous_owner_id: u64,
    ) -> Result<OwnerGrantIntent, StoreError> {
        if let Some(error) = self.prepare_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        let rooms = self.rooms.lock().unwrap();
        let stored = rooms.get(&room.channel_id).ok_or(StoreError::Conflict)?;
        let mut intents = self.owner_intents.lock().unwrap();
        let intent = intents
            .entry(room.channel_id)
            .or_insert_with(|| OwnerGrantIntent {
                revision: String::new(),
                members: Vec::new(),
            });
        for member in [stored.owner_id, previous_owner_id, room.owner_id] {
            if !intent.members.contains(&member) {
                intent.members.push(member);
            }
        }
        intent.members.sort_unstable();
        intent.revision = (self.owner_revision.fetch_add(1, Ordering::Relaxed) + 1).to_string();
        self.owner_pending.lock().unwrap().insert(room.channel_id);
        Ok(intent.clone())
    }
    async fn update_ownership(&self, room: &VoiceRoom, revision: &str) -> Result<bool, StoreError> {
        self.trace.lock().unwrap().push(format!(
            "update_ownership:{}:{}",
            room.channel_id, room.owner_id
        ));
        if let Some(error) = self.ownership_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        let mut rooms = self.rooms.lock().unwrap();
        let mut intents = self.owner_intents.lock().unwrap();
        let Some(intent) = intents.get_mut(&room.channel_id) else {
            return Ok(false);
        };
        if intent.revision != revision || !intent.members.contains(&room.owner_id) {
            return Ok(false);
        }
        if let Some(stored) = rooms.get_mut(&room.channel_id) {
            stored.owner_id = room.owner_id;
            stored.original_creator_id = room.original_creator_id;
            intent.members = vec![room.owner_id];
            self.owner_pending.lock().unwrap().remove(&room.channel_id);
            Ok(true)
        } else {
            Ok(false)
        }
    }
    async fn custom_names(&self, _: u64) -> Result<Vec<(u64, String)>, StoreError> {
        let mut names: Vec<_> = self
            .custom_names
            .lock()
            .unwrap()
            .iter()
            .map(|(channel, name)| (*channel, name.clone()))
            .collect();
        names.sort();
        Ok(names)
    }
    async fn save_custom_name(
        &self,
        _: u64,
        channel: u64,
        custom_name: Option<&str>,
    ) -> Result<bool, StoreError> {
        self.trace
            .lock()
            .unwrap()
            .push(format!("save_custom_name:{channel}:{custom_name:?}"));
        if let Some(error) = self.save_custom_name_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        if !self.rooms.lock().unwrap().contains_key(&channel) {
            return Ok(false);
        }
        match custom_name {
            Some(name) => self
                .custom_names
                .lock()
                .unwrap()
                .insert(channel, name.to_owned()),
            None => self.custom_names.lock().unwrap().remove(&channel),
        };
        Ok(true)
    }
    async fn forget(&self, _: u64, channel: u64) -> Result<(), StoreError> {
        self.trace.lock().unwrap().push(format!("forget:{channel}"));
        if let Some(error) = self.forget_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        self.rooms.lock().unwrap().remove(&channel);
        self.privacy.lock().unwrap().remove(&channel);
        self.owner_intents.lock().unwrap().remove(&channel);
        self.owner_pending.lock().unwrap().remove(&channel);
        Ok(())
    }
    async fn config_snapshot(&self, _: u64) -> Result<VoiceConfiguration, StoreError> {
        match self.config_error {
            Some(error) => Err(error),
            None => Ok(self.config.lock().unwrap().clone()),
        }
    }
    async fn config_apply(
        &self,
        _: u64,
        config: &VoiceConfiguration,
        expected: &VoiceConfiguration,
    ) -> Result<(), StoreError> {
        if let Some(error) = self.save_config_error {
            return Err(error);
        }
        // Compare-and-swap like `PgVoiceConfigStore::apply`: a concurrent
        // change between the Confirm re-read and the write is `Conflict`.
        if *self.config.lock().unwrap() != *expected {
            return Err(StoreError::Conflict);
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
    async fn privacy(&self, _: u64) -> Result<BTreeMap<u64, PrivacyRecord>, StoreError> {
        Ok(self.privacy.lock().unwrap().clone())
    }
    async fn save_privacy(
        &self,
        _: u64,
        room: u64,
        record: &PrivacyRecord,
    ) -> Result<bool, StoreError> {
        self.trace.lock().unwrap().push(format!(
            "save_privacy:{room}:{}:{:?}:{}",
            record.private,
            record.join_channel_id,
            record.blocked.len()
        ));
        if let Some(error) = self.save_privacy_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        if !self.rooms.lock().unwrap().contains_key(&room) {
            return Ok(false);
        }
        self.privacy.lock().unwrap().insert(room, record.clone());
        Ok(true)
    }
}

/// Scripted `/import` download queue shared with the harness.
type DownloadResults = Arc<Mutex<VecDeque<Result<Vec<u8>, RoomHttpError>>>>;

#[derive(Default)]
struct OverwriteGate {
    sent: tokio::sync::Notify,
    release: tokio::sync::Notify,
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
    overwrite_errors: Mutex<VecDeque<RoomHttpError>>,
    join_errors: Mutex<VecDeque<RoomHttpError>>,
    written_overwrites: Mutex<Vec<(u64, PermissionOverwrite)>>,
    notices: Mutex<Vec<(NoticeTarget, String, Option<u64>)>>,
    refused_notices: Mutex<Vec<NoticeTarget>>,
    created_attributes: Mutex<Vec<RoomChannelAttributes>>,
    created_names: Mutex<Vec<String>>,
    companion_plans: Mutex<Vec<TextChannelPlan>>,
    before_create: Option<Hook>,
    after_create: Option<Hook>,
    before_move: Option<Hook>,
    before_delete: Option<Hook>,
    overwrites_gate: Option<Arc<OverwriteGate>>,
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
            overwrite_errors: Mutex::new(VecDeque::new()),
            join_errors: Mutex::new(VecDeque::new()),
            written_overwrites: Mutex::new(Vec::new()),
            notices: Mutex::new(Vec::new()),
            refused_notices: Mutex::new(Vec::new()),
            created_attributes: Mutex::new(Vec::new()),
            created_names: Mutex::new(Vec::new()),
            companion_plans: Mutex::new(Vec::new()),
            before_create: None,
            after_create: None,
            before_move: None,
            before_delete: None,
            overwrites_gate: None,
            downloaded_urls: Mutex::new(Vec::new()),
            download_results: Arc::new(Mutex::new(VecDeque::new())),
        }
    }
}

impl RoomWrites for Http {
    async fn create(
        &self,
        _: u64,
        name: &str,
        attributes: &RoomChannelAttributes,
        guard: WriteGuard,
    ) -> Result<Channel, RoomHttpError> {
        if let Some(hook) = &self.before_create {
            hook();
        }
        if !guard() {
            return Err(RoomHttpError::Cancelled);
        }
        self.created_names.lock().unwrap().push(name.to_owned());
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
    async fn update_overwrites(
        &self,
        channel_id: u64,
        overwrites: &[PermissionOverwrite],
        guard: WriteGuard,
    ) -> Result<Channel, RoomHttpError> {
        if !guard() {
            return Err(RoomHttpError::Cancelled);
        }
        self.trace
            .lock()
            .unwrap()
            .push(format!("overwrites:{channel_id}"));
        if let Some(error) = self.view_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        let mut updated = channel(channel_id, 2, Some(CATEGORY));
        updated.permission_overwrites = Some(overwrites.to_vec());
        if let Some(gate) = &self.overwrites_gate {
            gate.sent.notify_one();
            gate.release.notified().await;
        }
        Ok(updated)
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
    async fn put_overwrite(
        &self,
        channel: u64,
        overwrite: PermissionOverwrite,
        guard: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        if !guard() {
            return Err(RoomHttpError::Cancelled);
        }
        self.trace.lock().unwrap().push(format!(
            "overwrite:{channel}:{}:{:?}",
            overwrite.id.get(),
            overwrite.kind
        ));
        if let Some(error) = self.overwrite_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        self.written_overwrites
            .lock()
            .unwrap()
            .push((channel, overwrite));
        Ok(())
    }
    async fn create_join_channel(
        &self,
        _: u64,
        name: &str,
        parent_id: Option<u64>,
        position: Option<u64>,
        guard: WriteGuard,
    ) -> Result<Channel, RoomHttpError> {
        if !guard() {
            return Err(RoomHttpError::Cancelled);
        }
        self.trace
            .lock()
            .unwrap()
            .push(format!("create_join:{name}:{parent_id:?}:{position:?}"));
        if let Some(error) = self.join_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        let id = {
            let mut id = self.next_id.lock().unwrap();
            let next = *id;
            *id += 1;
            next
        };
        let mut created = channel(id, 2, parent_id);
        created.name = Some(name.to_owned());
        Ok(created)
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
    assert!(worker.accept_join(ticket, "new room", 7, NOW.to_owned()));
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
    assert!(!worker.accept_join(ticket, "duplicate", 8, NOW.to_owned()));
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
    assert!(
        worker.failures().is_empty(),
        "a stale ticket is not a permission refusal"
    );
}

#[tokio::test]
async fn missing_permission_final_create_guard_records_the_lost_permission() {
    let (live, store, mut http, trace) = fixture();
    let shared = live.clone();
    http.before_create = Some(Arc::new(move || {
        shared.upsert_channel(channel_with_overwrites(
            CREATOR,
            2,
            Some(CATEGORY),
            json!([everyone_overwrite(Permissions::MANAGE_CHANNELS)]),
        ));
    }));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    assert!(
        trace.lock().unwrap().is_empty(),
        "the final guard prevents the create"
    );
    assert_eq!(
        failure_line(worker.failures().back().expect("permission refusal")),
        "create <#200>: Discord refused creating the room; the permission override on <#200> removes Manage Channels from the bot"
    );
    assert!(worker.send_notices(0).await);
    assert!(sent(&worker)[0].1.contains("Manage Channels"));
}

#[tokio::test]
async fn missing_permission_final_move_guard_records_the_lost_permission_and_compensates() {
    let (live, store, mut http, trace) = fixture();
    let shared = live.clone();
    http.before_move = Some(Arc::new(move || {
        shared.upsert_channel(channel_with_overwrites(
            500,
            2,
            Some(CATEGORY),
            json!([everyone_overwrite(Permissions::MOVE_MEMBERS)]),
        ));
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
    assert_eq!(
        failure_line(worker.failures().back().expect("permission refusal")),
        "move <#500>: Discord refused moving the member into the room; the permission override on <#500> removes Move Members from the bot"
    );
    assert!(worker.send_notices(0).await);
    assert!(sent(&worker)[0].1.contains("<#500> removes Move Members"));
}

#[tokio::test]
async fn stale_create_guard_stays_silent_even_when_permissions_also_change() {
    let (live, store, mut http, trace) = fixture();
    let shared = live.clone();
    http.before_create = Some(Arc::new(move || {
        shared.voice_update(MEMBER, None, None);
        shared.refresh_bot(BotAccess {
            roles: vec![role(Permissions::empty())],
            ..notice_bot(None)
        });
    }));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    assert!(trace.lock().unwrap().is_empty());
    assert!(worker.failures().is_empty());
    assert!(!worker.send_notices(0).await);
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
async fn untracked_live_channels_enqueue_zero_deletes() {
    // Interim-bot leftovers are live but never tracked: even an empty
    // stranger must not enqueue a delete. The tracked room stays occupied
    // so the pass enqueues nothing at all.
    let (live, store, http, trace) = fixture();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.publish(snapshot(
        &[500, 900],
        vec![VoiceMember {
            member_id: MEMBER,
            channel_id: 500,
            bot: Some(false),
        }],
    ));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.reconcile();
    // Nothing to dispatch: the occupied room needs no action and the
    // stranger must not create any. An empty queue proves zero enqueues.
    for time in 0..2 {
        assert!(!worker.dispatch_one(time).await);
    }
    let calls = trace.lock().unwrap();
    assert!(
        !calls.iter().any(|call| call.starts_with("delete:")),
        "untracked 900 must enqueue zero deletes, got {calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.contains("900")),
        "untracked 900 must stay untouched, got {calls:?}"
    );
    assert_eq!(worker.tracked().len(), 1);
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
    // The succession handoff is still queued, so the reclaim coalesces into
    // it: one grant rewrite converges on the latest owner, never the stale
    // caretaker.
    dispatch(&mut worker, 0).await;
    assert!(!worker.dispatch_one(1).await);
    assert_eq!(
        trace
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.starts_with("update_ownership"))
            .collect::<Vec<_>>(),
        [&format!("update_ownership:500:{MEMBER}")]
    );
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

fn owner_overwrite(member: u64) -> PermissionOverwrite {
    PermissionOverwrite {
        id: Id::new(member),
        kind: PermissionOverwriteType::Member,
        allow: Permissions::from_bits_retain(OWNER_ALLOW_BITS),
        deny: Permissions::empty(),
    }
}

#[tokio::test]
async fn transfer_rewrites_only_this_rooms_owner_grants_before_persistence() {
    let (live, store, http, trace) = owned_room();
    let unrelated = PermissionOverwrite {
        id: Id::new(701),
        kind: PermissionOverwriteType::Role,
        allow: Permissions::VIEW_CHANNEL,
        deny: Permissions::CONNECT,
    };
    let mut room_channel = channel(500, 2, Some(CATEGORY));
    let mut former_owner = owner_overwrite(MEMBER);
    former_owner.allow |= Permissions::EMBED_LINKS;
    former_owner.deny |= Permissions::SEND_MESSAGES;
    room_channel.permission_overwrites = Some(vec![former_owner, unrelated]);
    live.upsert_channel(room_channel);
    let untouched = channel(501, 2, Some(CATEGORY));
    live.upsert_channel(untouched.clone());
    let mut worker = GuildRoomWorker::load(live.clone(), store, http)
        .await
        .unwrap();
    assert!(worker
        .apply_ownership(MEMBER, false, OwnershipCommand::Transfer { target_id: 301 })
        .contains("Transferred"));
    dispatch(&mut worker, 0).await;
    let state = live.inner.read().unwrap();
    let overwrites = state.channels[&500].permission_overwrites.as_ref().unwrap();
    let old = overwrites.iter().find(|o| o.id.get() == MEMBER).unwrap();
    assert_eq!(old.allow, Permissions::EMBED_LINKS);
    assert_eq!(old.deny, Permissions::SEND_MESSAGES);
    assert!(overwrites.contains(&unrelated));
    assert!(overwrites.contains(&owner_overwrite(301)));
    assert_eq!(state.channels[&501], untouched);
    assert_eq!(
        *trace.lock().unwrap(),
        ["overwrites:500", "update_ownership:500:301"]
    );
}

#[tokio::test]
async fn late_owner_overwrite_response_never_replaces_newer_gateway_evidence() {
    for event in ["update", "delete", "reconnect"] {
        let (live, store, mut http, _) = owned_room();
        let mut original = channel(500, 2, Some(CATEGORY));
        original.permission_overwrites = Some(vec![owner_overwrite(MEMBER)]);
        live.upsert_channel(original.clone());
        let gate = Arc::new(OverwriteGate::default());
        http.overwrites_gate = Some(gate.clone());
        let mut worker = GuildRoomWorker::load(live.clone(), store, http)
            .await
            .unwrap();
        let deny = PermissionOverwrite {
            id: Id::new(777),
            kind: PermissionOverwriteType::Member,
            allow: Permissions::empty(),
            deny: Permissions::CONNECT,
        };
        let mutation = async {
            gate.sent.notified().await;
            match event {
                "update" => {
                    let mut newer = original.clone();
                    newer.permission_overwrites.as_mut().unwrap().push(deny);
                    newer.name = Some("newer gateway name".to_owned());
                    live.upsert_channel(newer);
                }
                "delete" => live.remove_channel(500),
                "reconnect" => {
                    live.disconnect();
                    let mut refreshed = snapshot(&[500], vec![]);
                    *refreshed
                        .channels
                        .iter_mut()
                        .find(|c| c.id.get() == 500)
                        .unwrap() = original.clone();
                    live.publish(refreshed);
                }
                _ => unreachable!(),
            }
            gate.release.notify_one();
        };
        let (result, ()) = tokio::join!(worker.rewrite_owner_grant(500, &[MEMBER], 301), mutation);
        assert_eq!(result, Err(RoomHttpError::Cancelled), "{event}");
        worker.http.overwrites_gate = None;
        if event == "delete" {
            assert!(!live.inner.read().unwrap().channels.contains_key(&500));
            assert_eq!(
                worker.rewrite_owner_grant(500, &[MEMBER], 301).await,
                Err(RoomHttpError::NotFound)
            );
        } else if event == "reconnect" {
            assert_eq!(live.inner.read().unwrap().channels[&500], original);
        } else {
            worker
                .rewrite_owner_grant(500, &[MEMBER], 301)
                .await
                .unwrap();
            let state = live.inner.read().unwrap();
            let newer = &state.channels[&500];
            assert_eq!(newer.name.as_deref(), Some("newer gateway name"));
            assert!(newer
                .permission_overwrites
                .as_ref()
                .unwrap()
                .contains(&deny));
        }
    }
}

#[tokio::test]
async fn rapid_transfers_revoke_all_former_owners_and_never_grant_a_stale_recipient() {
    let (live, store, http, trace) = owned_room();
    let mut room_channel = channel(500, 2, Some(CATEGORY));
    room_channel.permission_overwrites = Some(vec![owner_overwrite(MEMBER)]);
    live.upsert_channel(room_channel);
    live.voice_update_at(302, Some(500), Some(false), 1_000);
    let mut worker = GuildRoomWorker::load(live.clone(), store, http)
        .await
        .unwrap();
    worker.apply_ownership(MEMBER, false, OwnershipCommand::Transfer { target_id: 301 });
    worker.apply_ownership(301, false, OwnershipCommand::Transfer { target_id: 302 });
    dispatch(&mut worker, 0).await;
    assert!(!worker.dispatch_one(1).await, "handoffs coalesce per room");
    let state = live.inner.read().unwrap();
    let overwrites = state.channels[&500].permission_overwrites.as_ref().unwrap();
    for former in [MEMBER, 301] {
        assert!(!overwrites.iter().any(|o| o.id.get() == former
            && o.allow
                .intersects(Permissions::from_bits_retain(OWNER_ALLOW_BITS))));
    }
    assert!(overwrites.contains(&owner_overwrite(302)));
    assert!(!trace
        .lock()
        .unwrap()
        .contains(&"update_ownership:500:301".to_owned()));
    assert!(trace
        .lock()
        .unwrap()
        .contains(&"update_ownership:500:302".to_owned()));
}

#[tokio::test]
async fn owner_grants_are_never_issued_after_failed_journal_preparation() {
    let (live, store, http, trace) = owned_room();
    store
        .prepare_errors
        .lock()
        .unwrap()
        .push_back(StoreError::Unavailable);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.apply_ownership(MEMBER, false, OwnershipCommand::Transfer { target_id: 301 });
    dispatch(&mut worker, 0).await;
    assert!(
        trace.lock().unwrap().is_empty(),
        "no Discord grant or ownership commit"
    );
    assert!(worker.store.owner_pending.lock().unwrap().is_empty());
    assert_eq!(worker.store.rooms.lock().unwrap()[&500].owner_id, MEMBER);
}

#[tokio::test]
async fn failed_owner_commit_reload_and_later_transfer_revoke_every_issued_recipient() {
    let (live, store, http, trace) = owned_room();
    let mut original = channel(500, 2, Some(CATEGORY));
    let independent = PermissionOverwrite {
        id: Id::new(301),
        kind: PermissionOverwriteType::Member,
        allow: Permissions::EMBED_LINKS,
        deny: Permissions::SEND_MESSAGES,
    };
    original.permission_overwrites = Some(vec![owner_overwrite(MEMBER), independent]);
    live.upsert_channel(original);
    live.voice_update_at(302, Some(500), Some(false), 1_000);
    let store = Arc::new(store);
    store
        .ownership_errors
        .lock()
        .unwrap()
        .push_back(StoreError::Unavailable);
    let mut worker = GuildRoomWorker::load(live.clone(), store.clone(), http)
        .await
        .unwrap();
    worker.apply_ownership(MEMBER, false, OwnershipCommand::Transfer { target_id: 301 });
    assert!(worker.dispatch_one(0).await);
    assert_eq!(store.rooms.lock().unwrap()[&500].owner_id, MEMBER);
    assert!(store.owner_pending.lock().unwrap().contains(&500));
    assert_eq!(
        store.owner_intents.lock().unwrap()[&500].members,
        [MEMBER, 301]
    );
    assert!(live.inner.read().unwrap().channels[&500]
        .permission_overwrites
        .as_ref()
        .unwrap()
        .iter()
        .any(|o| o.id.get() == 301 && o.allow.contains(Permissions::MANAGE_CHANNELS)));
    drop(worker);

    // The ledger owner is still present, so succession does nothing. Recovery
    // must nevertheless undo the uncommitted recipient grant after reload.
    let mut restarted = GuildRoomWorker::load(live.clone(), store.clone(), Http::new(trace))
        .await
        .unwrap();
    restarted.reconcile();
    assert!(restarted.dispatch_one(0).await);
    assert!(!store.owner_pending.lock().unwrap().contains(&500));
    assert_eq!(restarted.rooms[&500].owner_id, MEMBER);
    restarted.apply_ownership(MEMBER, false, OwnershipCommand::Transfer { target_id: 302 });
    assert!(restarted.dispatch_one(1).await);
    assert_eq!(store.rooms.lock().unwrap()[&500].owner_id, 302);
    let state = live.inner.read().unwrap();
    let overwrites = state.channels[&500].permission_overwrites.as_ref().unwrap();
    for former in [MEMBER, 301] {
        assert!(!overwrites.iter().any(|o| o.id.get() == former
            && o.allow
                .intersects(Permissions::from_bits_retain(OWNER_ALLOW_BITS))));
    }
    assert!(overwrites.contains(&independent));
    assert!(overwrites.contains(&owner_overwrite(302)));
}

#[tokio::test]
async fn exhausted_owner_cleanup_is_retained_and_recovers_after_access_restoration() {
    for later_handoff in [false, true] {
        let (live, store, http, _) = owned_room();
        let mut original = channel(500, 2, Some(CATEGORY));
        original.permission_overwrites = Some(vec![owner_overwrite(MEMBER)]);
        live.upsert_channel(original);
        for member in [302, 303] {
            live.voice_update_at(member, Some(500), Some(false), 1_000);
        }
        let mut worker = GuildRoomWorker::load(live.clone(), store, http)
            .await
            .unwrap();
        worker.apply_ownership(MEMBER, false, OwnershipCommand::Transfer { target_id: 301 });
        dispatch(&mut worker, 0).await;
        let full_access = live.inner.read().unwrap().bot.clone().unwrap();
        let mut reduced = full_access.clone();
        for role in &mut reduced.roles {
            role.permissions &= !Permissions::MANAGE_ROLES;
        }
        live.refresh_bot(reduced);
        worker.apply_ownership(301, false, OwnershipCommand::Transfer { target_id: 302 });
        let mut now = 100_000;
        for _ in 0..QUEUE_MAX_ATTEMPTS {
            dispatch(&mut worker, now).await;
            now += 100_000;
        }
        assert_eq!(worker.queue.failed().len(), 1);
        assert!(worker.store.owner_pending.lock().unwrap().contains(&500));
        assert_eq!(worker.store.rooms.lock().unwrap()[&500].owner_id, 301);
        assert!(
            !worker.dispatch_one(now).await,
            "no retry storm while access is absent"
        );
        let owner = if later_handoff {
            worker.apply_ownership(302, false, OwnershipCommand::Transfer { target_id: 303 });
            dispatch(&mut worker, now).await;
            now += 100_000;
            assert_eq!(
                worker.store.rooms.lock().unwrap()[&500].owner_id,
                301,
                "missing immediate predecessor grant cannot hide the earlier grant"
            );
            303
        } else {
            302
        };
        live.refresh_bot(full_access);
        dispatch(&mut worker, now).await;
        assert_eq!(worker.store.rooms.lock().unwrap()[&500].owner_id, owner);
        assert!(!worker.store.owner_pending.lock().unwrap().contains(&500));
        let state = live.inner.read().unwrap();
        let overwrites = state.channels[&500].permission_overwrites.as_ref().unwrap();
        assert!(!overwrites.iter().any(|o| o.id.get() == 301
            && o.allow
                .intersects(Permissions::from_bits_retain(OWNER_ALLOW_BITS))));
        assert!(overwrites.contains(&owner_overwrite(owner)));
    }
}

#[tokio::test]
async fn owner_grant_rewrite_rate_limit_retries_before_saving_the_handoff() {
    let (live, store, http, trace) = owned_room();
    http.view_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::RateLimited {
            retry_after_ms: 100,
            global: false,
        });
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.apply_ownership(MEMBER, false, OwnershipCommand::Transfer { target_id: 301 });
    dispatch(&mut worker, 0).await;
    assert!(!trace
        .lock()
        .unwrap()
        .iter()
        .any(|c| c.starts_with("update_ownership")));
    assert!(!worker.dispatch_one(99).await);
    dispatch(&mut worker, 100).await;
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
    let (live, store, http, trace) = fixture();
    let mut creator = channel(CREATOR, 2, Some(CATEGORY));
    creator.permission_overwrites = Some(vec![PermissionOverwrite {
        id: Id::new(600),
        kind: PermissionOverwriteType::Member,
        allow: Permissions::MANAGE_ROLES,
        deny: Permissions::empty(),
    }]);
    live.upsert_channel(creator);
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
    dispatch(&mut worker, 1).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["create", "persist:500", "move:300:500"]
    );
    assert!(worker.failures().is_empty());
    // The bot cannot set overrides without Manage Roles: none are sent, even
    // if the source grants it to someone else. Discord syncs to the category.
    let created = worker.http.created_attributes.lock().unwrap();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].parent_id, Some(CATEGORY));
    assert!(created[0].overwrites.is_empty());
}

#[tokio::test]
async fn create_request_strips_inherited_manage_roles_from_every_overwrite() {
    use twilight_model::channel::permission_overwrite::{
        PermissionOverwrite, PermissionOverwriteType,
    };
    use two_bot_core::voice_permissions::OWNER_ALLOW_BITS;

    for source in [
        PermissionSource::Creator,
        PermissionSource::Category,
        PermissionSource::Channel(700),
    ] {
        for private in [false, true] {
            for inherited_bot in [false, true] {
                let (live, store, http, trace) = fixture();
                {
                    let mut creators = store.creators.lock().unwrap();
                    creators[0].permission_source = source;
                    creators[0].permission_channel_id = match source {
                        PermissionSource::Channel(id) => Some(id),
                        _ => None,
                    };
                    creators[0].private_default = private;
                    creators[0].validate().unwrap();
                }
                let source_id = match source {
                    PermissionSource::Creator => CREATOR,
                    PermissionSource::Category => CATEGORY,
                    PermissionSource::Channel(id) => id,
                };
                let mut source_channel = channel(
                    source_id,
                    if source_id == CATEGORY { 4 } else { 2 },
                    if source_id == CATEGORY {
                        None
                    } else {
                        Some(CATEGORY)
                    },
                );
                let mut overwrites: Vec<_> = [
                    (
                        GUILD,
                        PermissionOverwriteType::Role,
                        Permissions::VIEW_CHANNEL | Permissions::CONNECT,
                        Permissions::empty(),
                    ),
                    (
                        MEMBER,
                        PermissionOverwriteType::Member,
                        Permissions::VIEW_CHANNEL | Permissions::CONNECT,
                        Permissions::SEND_MESSAGES,
                    ),
                    (
                        600,
                        PermissionOverwriteType::Role,
                        Permissions::SPEAK,
                        Permissions::STREAM,
                    ),
                    (
                        601,
                        PermissionOverwriteType::Member,
                        Permissions::SPEAK,
                        Permissions::MANAGE_ROLES,
                    ),
                ]
                .into_iter()
                .map(|(id, kind, allow, deny)| PermissionOverwrite {
                    id: Id::new(id),
                    kind,
                    allow: allow | Permissions::MANAGE_ROLES,
                    deny,
                })
                .collect();
                if inherited_bot {
                    overwrites.push(PermissionOverwrite {
                        id: Id::new(999),
                        kind: PermissionOverwriteType::Member,
                        allow: Permissions::MANAGE_ROLES,
                        deny: Permissions::SEND_MESSAGES,
                    });
                }
                source_channel.permission_overwrites = Some(overwrites);
                live.upsert_channel(source_channel);
                let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
                join(&mut worker, MEMBER);
                dispatch(&mut worker, 0).await;
                assert!(
                    worker.failures().is_empty(),
                    "source={source:?}, private={private}, inherited_bot={inherited_bot}: {:?}",
                    worker.failures()
                );
                dispatch(&mut worker, 1).await;
                assert_eq!(
                    *trace.lock().unwrap(),
                    ["create", "persist:500", "move:300:500"]
                );
                assert!(worker.failures().is_empty());
                let created = worker.http.created_attributes.lock().unwrap();
                assert_eq!(created.len(), 1);
                let attributes = &created[0];
                assert_eq!(attributes.parent_id, Some(CATEGORY));
                assert_eq!(
                    attributes.overwrites.len(),
                    if inherited_bot || private { 5 } else { 4 }
                );
                for overwrite in &attributes.overwrites {
                    assert!(
                        !overwrite.allow.contains(Permissions::MANAGE_ROLES),
                        "{overwrite:?}"
                    );
                    assert!(!overwrite.allow.intersects(overwrite.deny), "{overwrite:?}");
                }
                let entry = |id| {
                    attributes
                        .overwrites
                        .iter()
                        .find(|overwrite| overwrite.id.get() == id)
                        .unwrap()
                };
                let owner = entry(MEMBER);
                assert_eq!(owner.allow.bits(), OWNER_ALLOW_BITS);
                assert!(!owner.allow.contains(Permissions::ADMINISTRATOR));
                assert_eq!(owner.deny, Permissions::SEND_MESSAGES);
                assert_eq!(entry(600).allow, Permissions::SPEAK);
                assert_eq!(entry(600).deny, Permissions::STREAM);
                assert_eq!(entry(601).allow, Permissions::SPEAK);
                // Manage Roles denies restrict rather than confer it, and stay intact.
                assert_eq!(entry(601).deny, Permissions::MANAGE_ROLES);
                let everyone = entry(GUILD);
                assert_eq!(everyone.deny.contains(Permissions::CONNECT), private);
                assert!(everyone.allow.contains(Permissions::VIEW_CHANNEL));
                assert_eq!(everyone.allow.contains(Permissions::CONNECT), !private);
                if private {
                    let bot = entry(999);
                    assert_eq!(bot.allow, permissions() & !Permissions::MANAGE_ROLES);
                    assert_eq!(
                        bot.deny,
                        if inherited_bot {
                            Permissions::SEND_MESSAGES
                        } else {
                            Permissions::empty()
                        }
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn lacking_manage_roles_refuses_a_private_default_before_any_create() {
    let (live, store, http, trace) = fixture();
    store.creators.lock().unwrap()[0].private_default = true;
    live.inner.write().unwrap().bot.as_mut().unwrap().roles =
        vec![role(permissions() & !Permissions::MANAGE_ROLES)];
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    assert!(worker.http.created_attributes.lock().unwrap().is_empty());
    assert!(worker.tracked().is_empty());
    assert!(!trace
        .lock()
        .unwrap()
        .iter()
        .any(|call| call == "create" || call.starts_with("persist:") || call.starts_with("move:")));
    assert!(matches!(
        worker.failures().back(),
        Some(LifecycleFailure::MissingPermission {
            write: RefusedWrite::Create,
            ..
        })
    ));
    assert!(!worker.dispatch_one(60000).await);
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

fn name_context() -> NameFilterContext {
    NameFilterContext {
        guild_id: GUILD.to_string(),
        channel_id: CREATOR.to_string(),
        user_id: MEMBER.to_string(),
    }
}

/// Fixture automod policy: one invented term, no real word list.
fn fixture_policy(words: &[&str]) -> AutomodPolicy {
    AutomodPolicy {
        bad_words: words.iter().map(ToString::to_string).collect(),
        ..AutomodPolicy::default()
    }
}

#[test]
fn room_name_uses_display_plus_suffix_and_truncates() {
    let policy = AutomodPolicy::default();
    let plain = resolve_room_name("ava", &policy, &name_context()).unwrap();
    assert_eq!(plain.name, "ava's room");
    assert!(!plain.username_stripped);
    let named = resolve_room_name(&"x".repeat(200), &policy, &name_context())
        .unwrap()
        .name;
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
            "import",
            "position",
            "group",
            "inheritpermissions",
            "defaultlimit",
            "alwaysprivate",
            "kick",
            "name",
            "private",
            "public"
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
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
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
        roles: permissions.map_or_else(Vec::new, |p| vec![Id::new(10_000 + p.bits())]),
        user: Some(test_user(MEMBER)),
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
    // Most command fixtures start after GuildCreate; publish trusted role facts
    // separately from the interaction payload. Cold-cache tests call the handler directly.
    if runtime.live_actor(GUILD).is_none() {
        assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    }
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
async fn handle_transfer_replay_by_former_owner_with_channel_manage_channels_is_refused() {
    let trace = Trace::default();
    let runtime = ownership_room_runtime(trace.clone()).await;
    let transfer = voice_interaction_as(
        Some(command_data("transfer", vec![user_option("member", 301)])),
        None,
        MEMBER,
    );
    let (_, first) = handle_capture(&runtime, &transfer).await;
    assert!(response_text(&first.unwrap()).contains("Transferred"));
    let mut replay = voice_interaction_as(
        Some(command_data(
            "transfer",
            vec![user_option("member", MEMBER)],
        )),
        Some(Permissions::MANAGE_CHANNELS),
        MEMBER,
    );
    // Discord's source-channel owner overwrite supplies Manage Channels,
    // but this member has no guild-level role with that permission.
    replay.member.as_mut().unwrap().roles.clear();
    let (_, reply) = handle_capture(&runtime, &replay).await;
    assert!(response_text(&reply.unwrap()).contains("Only the room owner"));
    wait_trace(&trace, "update_ownership:500:301").await;
    assert!(!trace
        .lock()
        .unwrap()
        .contains(&format!("update_ownership:500:{MEMBER}")));
}

#[tokio::test]
async fn channel_manage_channels_never_bypasses_voice_restrictions_or_configuration_checks() {
    let runtime = test_runtime(Trace::default());
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    for name in [
        "create",
        "access",
        "logging",
        "textchannels",
        "position",
        "group",
        "inheritpermissions",
        "defaultlimit",
        "alwaysprivate",
    ] {
        let mut interaction = voice_interaction(
            Some(command_data(name, vec![])),
            Some(Permissions::MANAGE_CHANNELS),
            true,
        );
        interaction.member.as_mut().unwrap().roles.clear();
        let (_, response) = handle_capture(&runtime, &interaction).await;
        let text = response_text(&response.unwrap());
        assert!(text.contains("You need Manage Channels"), "{name}: {text}");
    }
    let controls = AccessControls {
        required_role: Some(9),
        ..AccessControls::default()
    };
    let runtime = gated_runtime(Trace::default(), controls, None);
    let mut interaction = voice_interaction(
        Some(command_data("ping", vec![])),
        Some(Permissions::MANAGE_CHANNELS),
        true,
    );
    interaction.member.as_mut().unwrap().roles.clear();
    let (_, response) = handle_capture(&runtime, &interaction).await;
    assert!(response_text(&response.unwrap()).contains("required role"));
}

#[tokio::test]
async fn channel_manage_channels_cannot_select_another_members_room() {
    let runtime = ownership_room_runtime(Trace::default()).await;
    runtime.voice_frame(GUILD, MEMBER, Some(600), Some(false), "x".to_owned());
    let mut interaction = voice_interaction_as(
        Some(command_data("transfer", vec![user_option("member", 301)])),
        Some(Permissions::MANAGE_CHANNELS),
        MEMBER,
    );
    interaction.member.as_mut().unwrap().roles.clear();
    let (_, reply) = handle_capture(&runtime, &interaction).await;
    assert!(response_text(&reply.unwrap()).contains("isn't a temporary room"));
}

#[tokio::test]
async fn channel_manage_channels_does_not_reveal_setup_detail() {
    let runtime = failing_creators_runtime(Trace::default());
    let mut interaction = voice_interaction(
        Some(command_data("setup", Vec::new())),
        Some(Permissions::MANAGE_CHANNELS),
        true,
    );
    interaction.member.as_mut().unwrap().roles.clear();
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(response.as_ref().expect("reply"));
    assert!(text.contains("Voice rooms are running"), "{text}");
    assert!(!text.contains("Could not load creator channels"), "{text}");
}

#[tokio::test]
async fn channel_manage_channels_cannot_bypass_a_per_command_restriction_or_settings_failure() {
    for (controls, error, refusal) in [
        (
            AccessControls {
                command_roles: [("ping".to_owned(), vec![7])].into_iter().collect(),
                ..AccessControls::default()
            },
            None,
            "do not have a role",
        ),
        (
            AccessControls::default(),
            Some(StoreError::Unavailable),
            "settings are unavailable",
        ),
    ] {
        let runtime = gated_runtime(Trace::default(), controls, error);
        let mut interaction = voice_interaction(
            Some(command_data("ping", vec![])),
            Some(Permissions::MANAGE_CHANNELS),
            true,
        );
        interaction.member.as_mut().unwrap().roles.clear();
        let (_, reply) = handle_capture(&runtime, &interaction).await;
        assert!(response_text(&reply.unwrap()).contains(refusal));
    }
}

#[tokio::test]
async fn guild_role_admins_and_owner_pass_without_channel_manage_channels() {
    let runtime = test_runtime(Trace::default());
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    for (id, roles) in [
        (
            MEMBER,
            vec![Id::new(10_000 + Permissions::MANAGE_CHANNELS.bits())],
        ),
        (
            MEMBER,
            vec![Id::new(10_000 + Permissions::ADMINISTRATOR.bits())],
        ),
        (998, vec![]),
    ] {
        let mut interaction = voice_interaction_as(Some(command_data("access", vec![])), None, id);
        interaction.member.as_mut().unwrap().roles = roles;
        assert!(is_voice_admin(runtime.guild_permissions(&interaction)));
        let (_, response) = handle_capture(&runtime, &interaction).await;
        assert!(!response_text(&response.unwrap()).contains("You need Manage Channels"));
    }
}

#[tokio::test]
async fn refreshed_guild_owner_and_role_permissions_revoke_old_admin_authority() {
    let runtime = test_runtime(Trace::default());
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    let mut old_owner = voice_interaction_as(Some(command_data("access", vec![])), None, 998);
    assert!(is_voice_admin(runtime.guild_permissions(&old_owner)));
    let mut guild = command_snapshot();
    guild.bot.guild_owner_id = 997;
    runtime
        .live_actor(GUILD)
        .unwrap()
        .live
        .refresh_bot(guild.bot.clone());
    assert!(!is_voice_admin(runtime.guild_permissions(&old_owner)));
    old_owner.member.as_mut().unwrap().roles =
        vec![Id::new(10_000 + Permissions::MANAGE_CHANNELS.bits())];
    assert!(is_voice_admin(runtime.guild_permissions(&old_owner)));
    guild
        .bot
        .roles
        .iter_mut()
        .find(|r| r.id.get() == 10_000 + Permissions::MANAGE_CHANNELS.bits())
        .unwrap()
        .permissions = Permissions::VIEW_CHANNEL;
    runtime
        .live_actor(GUILD)
        .unwrap()
        .live
        .refresh_bot(guild.bot);
    assert!(!is_voice_admin(runtime.guild_permissions(&old_owner)));
}

#[tokio::test]
async fn guild_permission_snapshot_fails_closed_when_missing_incomplete_or_disconnected() {
    let runtime = test_runtime(Trace::default());
    let interaction = voice_interaction(
        Some(command_data("create", vec![])),
        Some(Permissions::ADMINISTRATOR),
        true,
    );
    assert_eq!(runtime.guild_permissions(&interaction), None);
    let mut guild = command_snapshot();
    guild
        .bot
        .roles
        .retain(|role| role.id.get() != 10_000 + Permissions::ADMINISTRATOR.bits());
    assert!(runtime.publish_snapshot(GUILD, guild));
    assert_eq!(runtime.guild_permissions(&interaction), None);
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    assert!(is_voice_admin(runtime.guild_permissions(&interaction)));
    runtime.live_actor(GUILD).unwrap().live.disconnect();
    assert_eq!(runtime.guild_permissions(&interaction), None);
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
    assert!(!worker.accept_join(ticket, "new room", 7, NOW.to_owned()));
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
            worker.accept_join(ticket, "new room", 7, NOW.to_owned()),
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
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
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

// --- V8 per-creator settings commands ------------------------------------------

fn v8_interaction(name: &str, options: Vec<CommandDataOption>) -> Interaction {
    voice_interaction(
        Some(command_data(name, options)),
        Some(Permissions::MANAGE_CHANNELS),
        true,
    )
}

#[test]
fn parse_position_extracts_side_and_first_number() {
    let interaction = v8_interaction(
        "position",
        vec![
            typed_option("channel", CommandOptionValue::Channel(Id::new(CREATOR))),
            typed_option("position", CommandOptionValue::String("below".to_owned())),
            typed_option("first-number", CommandOptionValue::Integer(5)),
        ],
    );
    assert_eq!(
        parse_voice_command(&interaction),
        Some(VoiceCommand::Position {
            channel_id: CREATOR,
            request: PositionRequest {
                position: Some(RoomPosition::Below),
                first_number: Some(5),
            },
        })
    );
}

#[test]
fn parse_position_leaves_unset_options_unset() {
    let interaction = v8_interaction(
        "position",
        vec![typed_option(
            "channel",
            CommandOptionValue::Channel(Id::new(CREATOR)),
        )],
    );
    assert_eq!(
        parse_voice_command(&interaction),
        Some(VoiceCommand::Position {
            channel_id: CREATOR,
            request: PositionRequest {
                position: None,
                first_number: None,
            },
        })
    );
}

#[test]
fn parse_group_inherit_defaultlimit_and_alwaysprivate() {
    let interaction = v8_interaction(
        "group",
        vec![
            typed_option("channel", CommandOptionValue::Channel(Id::new(CREATOR))),
            typed_option("enabled", CommandOptionValue::Boolean(false)),
        ],
    );
    assert_eq!(
        parse_voice_command(&interaction),
        Some(VoiceCommand::Group {
            channel_id: CREATOR,
            request: GroupRequest {
                enabled: Some(false),
            },
        })
    );
    let interaction = v8_interaction(
        "inheritpermissions",
        vec![
            typed_option("channel", CommandOptionValue::Channel(Id::new(CREATOR))),
            typed_option("source", CommandOptionValue::String("channel".to_owned())),
            typed_option("source-channel", CommandOptionValue::Channel(Id::new(400))),
        ],
    );
    assert_eq!(
        parse_voice_command(&interaction),
        Some(VoiceCommand::InheritPermissions {
            channel_id: CREATOR,
            request: InheritPermissionsRequest {
                source: Some("channel".to_owned()),
                source_channel: Some(400),
            },
        })
    );
    let interaction = v8_interaction(
        "defaultlimit",
        vec![
            typed_option("channel", CommandOptionValue::Channel(Id::new(CREATOR))),
            typed_option("limit", CommandOptionValue::Integer(4)),
        ],
    );
    assert_eq!(
        parse_voice_command(&interaction),
        Some(VoiceCommand::DefaultLimit {
            channel_id: CREATOR,
            request: DefaultLimitRequest { limit: Some(4) },
        })
    );
    let interaction = v8_interaction(
        "alwaysprivate",
        vec![typed_option(
            "channel",
            CommandOptionValue::Channel(Id::new(CREATOR)),
        )],
    );
    assert_eq!(
        parse_voice_command(&interaction),
        Some(VoiceCommand::AlwaysPrivate {
            channel_id: CREATOR,
            request: AlwaysPrivateRequest { enabled: None },
        })
    );
}

#[test]
fn decide_position_updates_side_and_number() {
    let base = CreatorChannel::new(GUILD, CREATOR);
    let updated = match decide_position(
        Some(base),
        &PositionRequest {
            position: Some(RoomPosition::Below),
            first_number: Some(5),
        },
    ) {
        PositionPlan::Update(creator) => creator,
        PositionPlan::Refuse { message } => panic!("unexpected refusal: {message}"),
    };
    assert_eq!(updated.position, RoomPosition::Below);
    assert_eq!(updated.first_room_number, 5);
    // A side-only edit keeps the stored number.
    let side_only = match decide_position(
        Some(CreatorChannel::new(GUILD, CREATOR)),
        &PositionRequest {
            position: Some(RoomPosition::Below),
            first_number: None,
        },
    ) {
        PositionPlan::Update(creator) => creator,
        PositionPlan::Refuse { message } => panic!("unexpected refusal: {message}"),
    };
    assert_eq!(side_only.first_room_number, 1);
}

#[test]
fn decide_position_refuses_empty_and_bad_numbers() {
    let base = || Some(CreatorChannel::new(GUILD, CREATOR));
    assert!(matches!(
        decide_position(
            None,
            &PositionRequest {
                position: Some(RoomPosition::Above),
                first_number: None,
            }
        ),
        PositionPlan::Refuse { .. }
    ));
    assert!(matches!(
        decide_position(
            base(),
            &PositionRequest {
                position: None,
                first_number: None,
            }
        ),
        PositionPlan::Refuse { .. }
    ));
    for bad in [0, -3] {
        assert!(
            matches!(
                decide_position(
                    base(),
                    &PositionRequest {
                        position: None,
                        first_number: Some(bad),
                    }
                ),
                PositionPlan::Refuse { .. }
            ),
            "first number {bad} must refuse"
        );
    }
}

#[test]
fn decide_group_defaults_to_on() {
    let on = match decide_group(
        Some(CreatorChannel::new(GUILD, CREATOR)),
        &GroupRequest { enabled: None },
    ) {
        GroupPlan::Update(creator) => creator,
        GroupPlan::Refuse { message } => panic!("unexpected refusal: {message}"),
    };
    assert!(on.group_by_category);
    let off = match decide_group(
        Some(CreatorChannel::new(GUILD, CREATOR)),
        &GroupRequest {
            enabled: Some(false),
        },
    ) {
        GroupPlan::Update(creator) => creator,
        GroupPlan::Refuse { message } => panic!("unexpected refusal: {message}"),
    };
    assert!(!off.group_by_category);
    assert!(matches!(
        decide_group(None, &GroupRequest { enabled: None }),
        GroupPlan::Refuse { .. }
    ));
}

#[test]
fn decide_inherit_permissions_stores_each_source() {
    let creator = match decide_inherit_permissions(
        Some(CreatorChannel::new(GUILD, CREATOR)),
        &InheritPermissionsRequest {
            source: Some("category".to_owned()),
            source_channel: None,
        },
    ) {
        InheritPermissionsPlan::Update(creator) => creator,
        InheritPermissionsPlan::Refuse { message } => panic!("unexpected refusal: {message}"),
    };
    assert_eq!(creator.permission_source, PermissionSource::Category);
    assert_eq!(creator.permission_channel_id, None);
    let channel = match decide_inherit_permissions(
        Some(CreatorChannel::new(GUILD, CREATOR)),
        &InheritPermissionsRequest {
            source: Some("channel".to_owned()),
            source_channel: Some(400),
        },
    ) {
        InheritPermissionsPlan::Update(creator) => creator,
        InheritPermissionsPlan::Refuse { message } => panic!("unexpected refusal: {message}"),
    };
    assert_eq!(channel.permission_source, PermissionSource::Channel(400));
    assert_eq!(channel.permission_channel_id, Some(400));
}

#[test]
fn decide_inherit_permissions_refuses_bad_input() {
    let base = || Some(CreatorChannel::new(GUILD, CREATOR));
    // No creator row, no source, unknown source.
    assert!(matches!(
        decide_inherit_permissions(
            None,
            &InheritPermissionsRequest {
                source: Some("creator".to_owned()),
                source_channel: None,
            }
        ),
        InheritPermissionsPlan::Refuse { .. }
    ));
    for bad in [
        InheritPermissionsRequest {
            source: None,
            source_channel: None,
        },
        InheritPermissionsRequest {
            source: Some("everywhere".to_owned()),
            source_channel: None,
        },
        // Channel source without a channel, or with a zero channel.
        InheritPermissionsRequest {
            source: Some("channel".to_owned()),
            source_channel: None,
        },
        InheritPermissionsRequest {
            source: Some("channel".to_owned()),
            source_channel: Some(0),
        },
        // A source channel without the channel source.
        InheritPermissionsRequest {
            source: Some("creator".to_owned()),
            source_channel: Some(400),
        },
        InheritPermissionsRequest {
            source: Some("category".to_owned()),
            source_channel: Some(400),
        },
    ] {
        assert!(
            matches!(
                decide_inherit_permissions(base(), &bad),
                InheritPermissionsPlan::Refuse { .. }
            ),
            "{bad:?}"
        );
    }
}

#[test]
fn decide_default_limit_sets_and_clears() {
    let set = match decide_default_limit(
        Some(CreatorChannel::new(GUILD, CREATOR)),
        &DefaultLimitRequest { limit: Some(4) },
    ) {
        DefaultLimitPlan::Update(creator) => creator,
        DefaultLimitPlan::Refuse { message } => panic!("unexpected refusal: {message}"),
    };
    assert_eq!(set.default_limit, Some(4));
    let unlimited = match decide_default_limit(
        Some(CreatorChannel::new(GUILD, CREATOR)),
        &DefaultLimitRequest { limit: Some(0) },
    ) {
        DefaultLimitPlan::Update(creator) => creator,
        DefaultLimitPlan::Refuse { message } => panic!("unexpected refusal: {message}"),
    };
    assert_eq!(unlimited.default_limit, Some(0));
    // An omitted limit clears back to inherit.
    let inherit = match decide_default_limit(
        Some(CreatorChannel::new(GUILD, CREATOR)),
        &DefaultLimitRequest { limit: None },
    ) {
        DefaultLimitPlan::Update(creator) => creator,
        DefaultLimitPlan::Refuse { message } => panic!("unexpected refusal: {message}"),
    };
    assert_eq!(inherit.default_limit, None);
    for bad in [100, -1] {
        assert!(
            matches!(
                decide_default_limit(
                    Some(CreatorChannel::new(GUILD, CREATOR)),
                    &DefaultLimitRequest { limit: Some(bad) }
                ),
                DefaultLimitPlan::Refuse { .. }
            ),
            "limit {bad} must refuse"
        );
    }
    assert!(matches!(
        decide_default_limit(None, &DefaultLimitRequest { limit: Some(4) }),
        DefaultLimitPlan::Refuse { .. }
    ));
}

#[test]
fn decide_always_private_defaults_to_on() {
    let on = match decide_always_private(
        Some(CreatorChannel::new(GUILD, CREATOR)),
        &AlwaysPrivateRequest { enabled: None },
    ) {
        AlwaysPrivatePlan::Update(creator) => creator,
        AlwaysPrivatePlan::Refuse { message } => panic!("unexpected refusal: {message}"),
    };
    assert!(on.private_default);
    let off = match decide_always_private(
        Some(CreatorChannel::new(GUILD, CREATOR)),
        &AlwaysPrivateRequest {
            enabled: Some(false),
        },
    ) {
        AlwaysPrivatePlan::Update(creator) => creator,
        AlwaysPrivatePlan::Refuse { message } => panic!("unexpected refusal: {message}"),
    };
    assert!(!off.private_default);
}

#[test]
fn v8_summaries_report_the_stored_settings() {
    let mut creator = CreatorChannel::new(GUILD, CREATOR);
    creator.position = RoomPosition::Below;
    creator.first_room_number = 5;
    let text = position_summary(&creator);
    assert!(text.contains("below"), "{text}");
    assert!(text.contains('5'), "{text}");
    creator.group_by_category = true;
    let grouped = group_summary(&creator);
    assert!(grouped.contains("on"), "{grouped}");
    creator.group_by_category = false;
    let ungrouped = group_summary(&creator);
    assert!(ungrouped.contains("off"), "{ungrouped}");
    creator.permission_source = PermissionSource::Category;
    let inherit = inherit_permissions_summary(&creator);
    assert!(inherit.contains("category"), "{inherit}");
    creator.default_limit = Some(4);
    let limited = default_limit_summary(&creator);
    assert!(limited.contains('4'), "{limited}");
    creator.default_limit = None;
    let inherited = default_limit_summary(&creator);
    assert!(inherited.contains("inherit"), "{inherited}");
    creator.private_default = true;
    let private = always_private_summary(&creator);
    assert!(private.contains("private"), "{private}");
}

#[test]
fn v8_command_names_key_the_role_restrictions() {
    let commands = [
        VoiceCommand::Position {
            channel_id: CREATOR,
            request: PositionRequest {
                position: None,
                first_number: Some(2),
            },
        },
        VoiceCommand::Group {
            channel_id: CREATOR,
            request: GroupRequest { enabled: None },
        },
        VoiceCommand::InheritPermissions {
            channel_id: CREATOR,
            request: InheritPermissionsRequest {
                source: Some("creator".to_owned()),
                source_channel: None,
            },
        },
        VoiceCommand::DefaultLimit {
            channel_id: CREATOR,
            request: DefaultLimitRequest { limit: Some(4) },
        },
        VoiceCommand::AlwaysPrivate {
            channel_id: CREATOR,
            request: AlwaysPrivateRequest { enabled: None },
        },
    ];
    for command in &commands {
        assert!(
            two_bot_core::voice_access::VOICE_COMMANDS.contains(&command.name()),
            "{} is not a restrictable voice command",
            command.name()
        );
    }
    assert_eq!(commands[0].name(), "position");
    assert_eq!(commands[1].name(), "group");
    assert_eq!(commands[2].name(), "inheritpermissions");
    assert_eq!(commands[3].name(), "defaultlimit");
    assert_eq!(commands[4].name(), "alwaysprivate");
}

#[tokio::test]
async fn handle_v8_commands_refuse_without_manage_channels() {
    for (name, options) in [
        (
            "position",
            vec![
                typed_option("channel", CommandOptionValue::Channel(Id::new(CREATOR))),
                typed_option("first-number", CommandOptionValue::Integer(2)),
            ],
        ),
        (
            "group",
            vec![typed_option(
                "channel",
                CommandOptionValue::Channel(Id::new(CREATOR)),
            )],
        ),
        (
            "inheritpermissions",
            vec![
                typed_option("channel", CommandOptionValue::Channel(Id::new(CREATOR))),
                typed_option("source", CommandOptionValue::String("category".to_owned())),
            ],
        ),
        (
            "defaultlimit",
            vec![
                typed_option("channel", CommandOptionValue::Channel(Id::new(CREATOR))),
                typed_option("limit", CommandOptionValue::Integer(4)),
            ],
        ),
        (
            "alwaysprivate",
            vec![typed_option(
                "channel",
                CommandOptionValue::Channel(Id::new(CREATOR)),
            )],
        ),
    ] {
        let trace = Trace::default();
        let runtime = test_runtime(trace.clone());
        let interaction = voice_interaction(
            Some(command_data(name, options)),
            Some(Permissions::VIEW_CHANNEL),
            true,
        );
        let (owned, response) = handle_capture(&runtime, &interaction).await;
        assert!(owned, "/{name} owns its interaction");
        assert!(
            response_text(response.as_ref().expect("reply")).contains("Manage Channels"),
            "/{name} refuses without Manage Channels"
        );
        assert!(
            !trace
                .lock()
                .unwrap()
                .iter()
                .any(|entry| entry.starts_with("add_creator")),
            "/{name} writes nothing without Manage Channels"
        );
    }
}

#[tokio::test]
async fn handle_position_saves_side_and_number() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = v8_interaction(
        "position",
        vec![
            typed_option("channel", CommandOptionValue::Channel(Id::new(CREATOR))),
            typed_option("position", CommandOptionValue::String("below".to_owned())),
            typed_option("first-number", CommandOptionValue::Integer(5)),
        ],
    );
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(response.as_ref().expect("reply"));
    assert!(text.contains("below"), "{text}");
    assert!(text.contains('5'), "{text}");
    assert!(trace
        .lock()
        .unwrap()
        .contains(&format!("add_creator:{CREATOR}")));
}

#[tokio::test]
async fn handle_group_saves_the_toggle() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = v8_interaction(
        "group",
        vec![
            typed_option("channel", CommandOptionValue::Channel(Id::new(CREATOR))),
            typed_option("enabled", CommandOptionValue::Boolean(true)),
        ],
    );
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    assert!(
        response_text(response.as_ref().expect("reply")).contains("on"),
        "group reports the stored toggle"
    );
    assert!(trace
        .lock()
        .unwrap()
        .contains(&format!("add_creator:{CREATOR}")));
}

#[tokio::test]
async fn handle_inheritpermissions_saves_the_source() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = v8_interaction(
        "inheritpermissions",
        vec![
            typed_option("channel", CommandOptionValue::Channel(Id::new(CREATOR))),
            typed_option("source", CommandOptionValue::String("category".to_owned())),
        ],
    );
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    assert!(
        response_text(response.as_ref().expect("reply")).contains("category"),
        "inheritpermissions reports the stored source"
    );
    assert!(trace
        .lock()
        .unwrap()
        .contains(&format!("add_creator:{CREATOR}")));
}

#[tokio::test]
async fn handle_defaultlimit_saves_the_limit() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = v8_interaction(
        "defaultlimit",
        vec![
            typed_option("channel", CommandOptionValue::Channel(Id::new(CREATOR))),
            typed_option("limit", CommandOptionValue::Integer(4)),
        ],
    );
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    assert!(
        response_text(response.as_ref().expect("reply")).contains('4'),
        "defaultlimit reports the stored limit"
    );
    assert!(trace
        .lock()
        .unwrap()
        .contains(&format!("add_creator:{CREATOR}")));
}

#[tokio::test]
async fn handle_alwaysprivate_saves_the_default() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = v8_interaction(
        "alwaysprivate",
        vec![typed_option(
            "channel",
            CommandOptionValue::Channel(Id::new(CREATOR)),
        )],
    );
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    assert!(
        response_text(response.as_ref().expect("reply")).contains("private"),
        "alwaysprivate defaults on and reports it"
    );
    assert!(trace
        .lock()
        .unwrap()
        .contains(&format!("add_creator:{CREATOR}")));
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
async fn handle_setup_lists_seeded_creator_for_admins() {
    let trace = Trace::default();
    let runtime = test_runtime(trace);
    let interaction = voice_interaction(
        Some(command_data("setup", Vec::new())),
        Some(Permissions::MANAGE_CHANNELS),
        true,
    );
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(response.as_ref().expect("reply"));
    assert!(text.contains("Voice rooms"));
    assert!(text.contains(&format!("<#{CREATOR}>")));
}

fn failing_creators_runtime(trace: Trace) -> VoiceRuntime<Store, Http> {
    VoiceRuntime::new(
        move || {
            let mut store = Store::new(trace.clone());
            store.creators_error = Some(StoreError::Unavailable);
            (store, Http::new(trace.clone()))
        },
        Duration::from_millis(10),
        true,
    )
}

#[tokio::test]
async fn handle_setup_gives_members_a_generic_status_only() {
    // A member (or one whose permissions are missing) gets running/paused and
    // nothing else: no creator ids, no store error class, no Discord detail.
    for permissions in [
        Some(Permissions::empty()),
        Some(Permissions::VIEW_CHANNEL | Permissions::MANAGE_GUILD),
        None,
    ] {
        let runtime = failing_creators_runtime(Trace::default());
        let interaction =
            voice_interaction(Some(command_data("setup", Vec::new())), permissions, true);
        let (owned, response) = handle_capture(&runtime, &interaction).await;
        assert!(owned);
        let text = response_text(response.as_ref().expect("reply"));
        assert!(text.contains("Voice rooms are running"), "{text}");
        let creator = CREATOR.to_string();
        for leaked in [
            "<#",
            creator.as_str(),
            "store",
            "Unavailable",
            "unavailable",
            "Could not load",
            "Creator channels",
            "Tracked rooms",
            "failures",
        ] {
            assert!(
                !text.contains(leaked),
                "{leaked:?} leaked to a member: {text}"
            );
        }
    }
}

#[tokio::test]
async fn handle_setup_shows_store_errors_to_admins_without_the_variant_name() {
    for permissions in [Permissions::MANAGE_CHANNELS, Permissions::ADMINISTRATOR] {
        let runtime = failing_creators_runtime(Trace::default());
        let interaction = voice_interaction(
            Some(command_data("setup", Vec::new())),
            Some(permissions),
            true,
        );
        let (_, response) = handle_capture(&runtime, &interaction).await;
        let text = response_text(response.as_ref().expect("reply"));
        assert!(text.contains("Could not load creator channels"), "{text}");
        assert!(text.contains("the store is unavailable"), "{text}");
        assert!(!text.contains("Unavailable"), "{text}");
    }
}

#[test]
fn setup_member_panel_is_generic() {
    let running = setup_member_panel(false, false);
    assert_eq!(running.title, "Voice rooms");
    assert_eq!(running.description, "Voice rooms are running.");
    let attention = setup_member_panel(false, true);
    assert!(attention
        .description
        .starts_with("Voice rooms are running."));
    assert!(attention.description.contains("Ask a server admin"));
    let paused = setup_member_panel(true, false);
    assert!(paused.description.contains("paused"));
    assert!(paused.description.contains("Ask a server admin"));
    // The credential detail the admin panel carries stays out of the member view.
    assert!(!paused.description.contains("token"));
    assert!(!paused.description.contains("credential"));
    for panel in [running, attention, paused] {
        assert!(!panel.description.contains("<#"));
        assert!(!panel.description.chars().any(|c| c.is_ascii_digit()));
    }
}

#[test]
fn store_errors_read_as_plain_words() {
    for (error, variant) in [
        (StoreError::Unavailable, "Unavailable"),
        (StoreError::CredentialRefused, "CredentialRefused"),
        (StoreError::Conflict, "Conflict"),
    ] {
        let text = error.to_string();
        assert!(!text.is_empty());
        assert!(!text.contains(variant), "{text}");
    }
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

    async fn respond(
        &self,
        _: &Interaction,
        response: InteractionResponse,
    ) -> Result<(), RoomHttpError> {
        self.trace.lock().unwrap().push("respond".to_owned());
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
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
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
async fn responder_without_guild_roles_refuses_channel_permission_claims_without_creating() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let replies = Replies::new(trace.clone());
    VoiceResponder::respond_with(&runtime, &replies, &create_interaction(), None, None).await;
    assert_eq!(*trace.lock().unwrap(), ["defer", "complete"]);
    let completed = replies.completed.lock().unwrap();
    assert_eq!(completed.len(), 1);
    assert!(response_text(&completed[0]).contains("You need Manage Channels"));
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
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
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

fn bot_overwrite(allow: Permissions, deny: Permissions) -> serde_json::Value {
    json!({ "id": "999", "type": 1, "allow": allow.bits().to_string(), "deny": deny.bits().to_string() })
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

/// Worker over a health-check guild: the creator channel and its category
/// carry the given overwrites, the bot holds `base`.
async fn health_worker(
    base: Permissions,
    category: Vec<serde_json::Value>,
    creator: Vec<serde_json::Value>,
) -> (GuildRoomWorker<Store, Http>, Trace) {
    let trace = Trace::default();
    let live = health_guild(base, category, creator);
    let worker = GuildRoomWorker::load(live, Store::new(trace.clone()), Http::new(trace.clone()))
        .await
        .unwrap();
    (worker, trace)
}

#[tokio::test]
async fn missing_permission_create_names_manage_channels_and_the_category_override() {
    // A bot-member deny survives copying the category and the bot's grant;
    // diagnose those final rows, not the healthy unsynced creator.
    let (mut worker, trace) = health_worker(
        permissions(),
        vec![bot_overwrite(
            Permissions::empty(),
            Permissions::MANAGE_CHANNELS,
        )],
        vec![],
    )
    .await;
    worker.creators.get_mut(&CREATOR).unwrap().permission_source = PermissionSource::Category;
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    assert!(trace.lock().unwrap().is_empty());
    let failure = worker.failures().back().expect("a recorded failure");
    assert_eq!(
        failure_line(failure),
        "create <#200>: Discord refused creating the room; the permission override on category <#400> removes Manage Channels from the bot"
    );
    let notice = notice_text(failure, DetailLevel::Full);
    assert!(notice.contains("removes Manage Channels from the bot"));
    assert!(notice.contains("Run /setup"));
}

#[tokio::test]
async fn missing_permission_default_brief_notice_names_permission_and_override() {
    let (mut worker, _) = health_worker(
        permissions(),
        vec![bot_overwrite(
            Permissions::empty(),
            Permissions::MANAGE_CHANNELS,
        )],
        vec![],
    )
    .await;
    worker.creators.get_mut(&CREATOR).unwrap().permission_source = PermissionSource::Category;
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    assert_eq!(
        worker.store.logging.lock().unwrap().level,
        DetailLevel::Brief
    );
    assert!(worker.send_notices(0).await);
    let notices = sent(&worker);
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].0, NoticeTarget::DirectMessage(OWNER));
    assert!(notices[0]
        .1
        .contains("category <#400> removes Manage Channels"));
    assert!(notices[0].1.contains("/setup"));
    assert!(notices[0].1.chars().count() <= NOTICE_MAX_CHARS);
}

#[tokio::test]
async fn missing_permission_create_names_move_members_and_the_creator_override() {
    // The cached creator permissions already lack Move Members, so no Discord
    // write is attempted and the refusal names the override.
    let (mut worker, trace) = health_worker(
        permissions(),
        vec![],
        vec![everyone_overwrite(Permissions::MOVE_MEMBERS)],
    )
    .await;
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    assert!(trace.lock().unwrap().is_empty());
    let failure = worker.failures().back().expect("a recorded failure");
    assert_eq!(
        failure_line(failure),
        "create <#200>: Discord refused creating the room; the permission override on <#200> removes Move Members from the bot"
    );
    assert!(notice_text(failure, DetailLevel::Full).contains("removes Move Members from the bot"));
}

#[tokio::test]
async fn missing_permission_create_names_a_guild_wide_gap_and_lists_every_missing_permission() {
    let (mut worker, _) = health_worker(
        permissions() - Permissions::MANAGE_CHANNELS - Permissions::MOVE_MEMBERS,
        vec![],
        vec![],
    )
    .await;
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    assert_eq!(
        failure_line(worker.failures().back().unwrap()),
        "create <#200>: Discord refused creating the room; the bot lacks Manage Channels for the whole server; the bot lacks Move Members for the whole server"
    );
}

#[tokio::test]
async fn missing_permission_setup_status_carries_the_named_line() {
    let trace = Trace::default();
    let runtime = VoiceRuntime::new(
        {
            let trace = trace.clone();
            move || (Store::new(trace.clone()), Http::new(trace.clone()))
        },
        Duration::from_millis(10),
        true,
    );
    let mut snap = snapshot(&[], vec![]);
    snap.bot.roles = vec![role(permissions() - Permissions::MANAGE_CHANNELS)];
    assert!(runtime.publish_snapshot(GUILD, snap));
    assert!(runtime.voice_frame(GUILD, MEMBER, Some(CREATOR), Some(false), "ava".to_owned()));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let status = runtime.worker_status(GUILD).await.expect("live actor");
        if let Some(line) = status.failures.first() {
            assert_eq!(
                line,
                "create <#200>: Discord refused creating the room; the bot lacks Manage Channels for the whole server"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no failure line before the deadline"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!trace.lock().unwrap().iter().any(|entry| entry == "create"));
}

#[tokio::test]
async fn missing_permission_unexplained_create_403_still_names_manage_channels() {
    // A clean cache (stale roles, a missing Connect) leaves no finding, but
    // the notice must still say what a refused create needs.
    let (live, store, http, trace) = fixture();
    http.create_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::AccessDenied);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    assert_eq!(*trace.lock().unwrap(), ["create"]);
    assert_eq!(
        failure_line(worker.failures().back().unwrap()),
        "create <#200>: Discord refused creating the room; the bot needs Manage Channels, Move Members, View Channel and Connect on <#200>, so check for a deny override"
    );
}

#[tokio::test]
async fn missing_permission_move_names_move_members() {
    let (live, store, http, trace) = fixture();
    http.move_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::AccessDenied);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["create", "persist:500", "move:300:500"]
    );
    assert_eq!(
        failure_line(worker.failures().back().unwrap()),
        "move <#500>: Discord refused moving the member into the room; the bot needs Manage Channels, Move Members, View Channel and Connect on <#500>, so check for a deny override"
    );
}

#[tokio::test]
async fn missing_permission_move_names_the_room_override_when_the_cache_knows_it() {
    let (live, store, http, _) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.live.upsert_channel(channel_with_overwrites(
        500,
        2,
        Some(CATEGORY),
        json!([everyone_overwrite(Permissions::MOVE_MEMBERS)]),
    ));
    worker.record_refusal(RefusedWrite::Move, 500, RoomHttpError::AccessDenied);
    assert_eq!(
        failure_line(worker.failures().back().unwrap()),
        "move <#500>: Discord refused moving the member into the room; the permission override on <#500> removes Move Members from the bot"
    );
}

#[tokio::test]
async fn missing_permission_connect_is_not_hidden_by_an_unrelated_manage_roles_gap() {
    for (base, category, creator, cause) in [
        (
            permissions() - Permissions::CONNECT - Permissions::MANAGE_ROLES,
            vec![],
            vec![],
            "the bot lacks Connect for the whole server",
        ),
        (
            permissions() - Permissions::MANAGE_ROLES,
            vec![],
            vec![everyone_overwrite(Permissions::CONNECT)],
            "the permission override on <#200> removes Connect from the bot",
        ),
        (
            permissions() - Permissions::MANAGE_ROLES,
            vec![everyone_overwrite(Permissions::CONNECT)],
            vec![],
            "the permission override on category <#400> removes Connect from the bot",
        ),
    ] {
        let (mut worker, trace) = health_worker(base, category, creator).await;
        join(&mut worker, MEMBER);
        dispatch(&mut worker, 0).await;
        assert!(trace.lock().unwrap().is_empty());
        let line = failure_line(worker.failures().back().expect("Connect refusal"));
        assert_eq!(
            line,
            format!("create <#200>: Discord refused creating the room; {cause}")
        );
        assert!(!line.contains("Manage Roles"));
        assert!(worker.send_notices(0).await);
        assert!(sent(&worker)[0].1.contains(cause));
    }
}

#[tokio::test]
async fn missing_permission_chosen_source_refusal_names_source_in_setup_and_notice() {
    const SOURCE: u64 = 850;
    for allow in [Permissions::empty(), Permissions::CONNECT] {
        let (live, store, http, trace) = fixture();
        live.upsert_channel(channel_with_overwrites(
            SOURCE,
            2,
            None,
            json!([bot_overwrite(allow, Permissions::CONNECT)]),
        ));
        let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
        let settings = worker.creators.get_mut(&CREATOR).unwrap();
        settings.permission_source = PermissionSource::Channel(SOURCE);
        settings.permission_channel_id = Some(SOURCE);
        join(&mut worker, MEMBER);
        dispatch(&mut worker, 0).await;
        assert!(
            trace.lock().unwrap().is_empty(),
            "refused plans send no create"
        );
        let failure = worker.failures().back().unwrap();
        assert_eq!(
            *failure,
            LifecycleFailure::MissingPermission {
                write: RefusedWrite::Create,
                channel_id: CREATOR,
                findings: vec![PermissionFinding {
                    permission: VoicePermission::Connect,
                    scope: VoicePermissionScope::Channel,
                    category_id: None,
                    channel_id: Some(SOURCE),
                }],
            }
        );
        let setup_line = failure_line(failure);
        assert_eq!(setup_line, "create <#200>: Discord refused creating the room; the permission override on <#850> removes Connect from the bot");
        assert!(!setup_line.contains("category <#400>"));
        assert!(worker.send_notices(0).await);
        assert!(sent(&worker)[0].1.contains("<#850> removes Connect"));
        assert!(worker.http.created_names.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn missing_permission_unknown_source_is_not_a_fabricated_permission_gap() {
    let (live, store, http, trace) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.creators.get_mut(&CREATOR).unwrap().permission_source = PermissionSource::Channel(850);
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    assert_eq!(
        worker.failures().back(),
        Some(&LifecycleFailure::Discord {
            channel_id: 850,
            error: RoomHttpError::NotFound,
        })
    );
    assert!(trace.lock().unwrap().is_empty());
}

#[tokio::test]
async fn missing_permission_other_refusals_keep_the_plain_discord_line() {
    let (live, store, http, _) = fixture();
    http.create_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::UnknownOutcome);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    assert!(matches!(
        worker.failures().back(),
        Some(LifecycleFailure::Discord {
            channel_id: CREATOR,
            error: RoomHttpError::UnknownOutcome
        })
    ));
}

/// Join as `display`, returning whether the create was queued.
fn join_as(worker: &mut GuildRoomWorker<Store, Http>, member: u64, display: &str) -> bool {
    let ticket = worker
        .live
        .voice_update(member, Some(CREATOR), Some(false))
        .unwrap();
    worker.accept_join(ticket, display, 7, NOW.to_owned())
}

async fn filtering_worker(words: &[&str]) -> (GuildRoomWorker<Store, Http>, Trace) {
    let (live, store, http, trace) = fixture();
    let worker = GuildRoomWorker::load(live, store, http)
        .await
        .unwrap()
        .with_name_policy(Arc::new(fixture_policy(words)));
    (worker, trace)
}

fn policy_with_invalid_chat_setting(words: &str, key: &str, value: &str) -> AutomodPolicy {
    let vars: HashMap<String, String> = [
        ("TWO_AUTOMOD", "0"),
        ("TWO_AUTOMOD_BAD_WORDS", words),
        ("TWO_AUTOMOD_ALLOWED_DOMAINS", "Trusted.GG"),
        (key, value),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect();
    assert!(two_bot_core::AutomodConfig::from_map(&vars).is_err());
    AutomodPolicy::name_policy_from_map(&vars)
}

#[tokio::test]
async fn name_filter_preserves_words_with_invalid_chat_config_on_both_create_paths() {
    for (key, value) in [
        ("TWO_AUTOMOD_REPEAT_COUNT", "21"),
        ("TWO_AUTOMOD_SANCTIONS", "1:banhammer"),
    ] {
        let policy = policy_with_invalid_chat_setting(" ＢＬＯＲＰ ", key, value);
        let (live, store, http, _) = fixture();
        let mut worker = GuildRoomWorker::load(live, store, http)
            .await
            .unwrap()
            .with_name_policy(Arc::new(policy.clone()));
        assert!(join_as(&mut worker, MEMBER, "Blorp"));
        dispatch(&mut worker, 0).await;
        assert_eq!(*worker.http.created_names.lock().unwrap(), ["'s room"]);

        let trace = Trace::default();
        let runtime = test_runtime(trace.clone()).with_name_policy(policy);
        let interaction = voice_interaction(
            Some(command_data(
                "create",
                vec![command_option("name", "blorp lobby")],
            )),
            Some(Permissions::MANAGE_CHANNELS),
            true,
        );
        let (owned, response) = handle_capture(&runtime, &interaction).await;
        assert!(owned);
        assert!(response_text(response.as_ref().unwrap()).contains("not allowed"));
        assert!(
            trace.lock().unwrap().is_empty(),
            "a blocked /create sends no REST call"
        );
    }
}

#[tokio::test]
async fn name_filter_invalid_chat_config_does_not_unblock_the_bare_template() {
    let (live, store, http, trace) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http)
        .await
        .unwrap()
        .with_name_policy(Arc::new(policy_with_invalid_chat_setting(
            "room",
            "TWO_AUTOMOD_REPEAT_COUNT",
            "21",
        )));
    assert!(!join_as(&mut worker, MEMBER, "Ava"));
    assert!(!worker.dispatch_one(0).await);
    assert!(trace.lock().unwrap().is_empty());
    assert!(failure_line(worker.failures().back().unwrap()).contains("name_blocked"));
}

#[tokio::test]
async fn name_filter_passes_clean_display_names_unchanged() {
    let (mut worker, trace) = filtering_worker(&["blorp"]).await;
    assert!(join_as(&mut worker, MEMBER, "Ava"));
    dispatch(&mut worker, 0).await;
    assert_eq!(*worker.http.created_names.lock().unwrap(), ["Ava's room"]);
    assert_eq!(trace.lock().unwrap()[0], "create");
}

#[tokio::test]
async fn name_filter_retries_a_blocked_display_name_without_the_username() {
    let (mut worker, trace) = filtering_worker(&["blorp"]).await;
    assert!(join_as(&mut worker, MEMBER, "Blorp"));
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    assert_eq!(*worker.http.created_names.lock().unwrap(), ["'s room"]);
    assert_eq!(
        *trace.lock().unwrap(),
        ["create", "persist:500", "move:300:500"]
    );
    assert!(worker.failures().is_empty());
}

#[tokio::test]
async fn name_filter_blocks_invite_links_in_the_display_name() {
    let (mut worker, _) = filtering_worker(&[]).await;
    assert!(join_as(&mut worker, MEMBER, "join discord.gg/abc123"));
    dispatch(&mut worker, 0).await;
    assert_eq!(*worker.http.created_names.lock().unwrap(), ["'s room"]);
}

#[tokio::test]
async fn name_filter_refuses_a_blocked_bare_template_and_never_creates() {
    // The fixture term is part of the template's own text: even the bare
    // template is blocked.
    let (mut worker, trace) = filtering_worker(&["room"]).await;
    assert!(!join_as(&mut worker, MEMBER, "Ava"));
    assert!(!worker.dispatch_one(0).await);
    assert!(trace.lock().unwrap().is_empty());
    assert!(worker.http.created_names.lock().unwrap().is_empty());
    assert!(worker.tracked().is_empty());
    let failure = worker.failures().back().expect("a recorded failure");
    assert_eq!(
        *failure,
        LifecycleFailure::NameBlocked {
            creator_id: CREATOR,
            error: NameError::Blocked {
                filter: two_bot_core::AutomodFilter::BadWords
            }
        }
    );
    assert_eq!(
        failure_line(failure),
        "create <#200>: name_blocked, no room created (the room name template is blocked by the automod name filter: bad words); fix the template or the automod policy"
    );
    // The reason carries the stable audit string and never the member's name.
    assert!(!failure_line(failure).contains("Ava"));
    assert_eq!(NAME_BLOCKED_AUDIT_REASON, "name_blocked");
}

#[tokio::test]
async fn name_filter_default_brief_notice_names_refusal_and_preserves_logging_limits() {
    let (mut worker, _) = filtering_worker(&["room"]).await;
    assert!(!join_as(&mut worker, MEMBER, "Ava"));
    worker.store.logging.lock().unwrap().level = DetailLevel::Off;
    assert!(!worker.send_notices(0).await);
    assert!(sent(&worker).is_empty());

    *worker.store.logging.lock().unwrap() = LoggingSettings::default();
    for step in 1..=3 {
        assert!(worker.send_notices(step * NOTICE_REPEAT_INTERVAL_MS).await);
        let notices = sent(&worker);
        assert_eq!(notices.len(), step as usize);
        let text = &notices.last().unwrap().1;
        assert!(text.contains("name_blocked"));
        assert!(text.contains("bad words"));
        assert!(!text.contains("Ava"));
        assert!(text.chars().count() <= NOTICE_MAX_CHARS);
    }
    assert!(!worker.send_notices(4 * NOTICE_REPEAT_INTERVAL_MS).await);
    assert!(worker.http.created_names.lock().unwrap().is_empty());
}

#[tokio::test]
async fn name_filter_does_not_retry_a_refused_join() {
    let (mut worker, trace) = filtering_worker(&["room"]).await;
    let ticket = worker
        .live
        .voice_update(MEMBER, Some(CREATOR), Some(false))
        .unwrap();
    assert!(!worker.accept_join(ticket, "Ava", 7, NOW.to_owned()));
    assert!(!worker.accept_join(ticket, "Ava", 8, NOW.to_owned()));
    assert_eq!(worker.failures().len(), 1);
    assert!(trace.lock().unwrap().is_empty());
}

#[tokio::test]
async fn name_filter_applies_the_runtime_policy_in_the_actor() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone()).with_name_policy(fixture_policy(&["room"]));
    assert!(runtime.publish_snapshot(GUILD, snapshot(&[], vec![])));
    assert!(runtime.voice_frame(GUILD, MEMBER, Some(CREATOR), Some(false), "Ava".to_owned()));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let status = runtime.worker_status(GUILD).await.expect("live actor");
        if let Some(line) = status.failures.first() {
            assert!(line.contains("name_blocked"), "{line}");
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no refusal before the deadline"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!trace.lock().unwrap().iter().any(|entry| entry == "create"));
}

#[tokio::test]
async fn name_filter_refuses_a_blocked_create_command_name_before_any_rest_call() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone()).with_name_policy(fixture_policy(&["blorp"]));
    let interaction = voice_interaction(
        Some(command_data(
            "create",
            vec![command_option("name", "blorp lobby")],
        )),
        Some(Permissions::MANAGE_CHANNELS),
        true,
    );
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(response.as_ref().expect("reply"));
    assert!(text.contains("not allowed"), "{text}");
    assert!(trace.lock().unwrap().is_empty());

    let allowed = voice_interaction(
        Some(command_data(
            "create",
            vec![command_option("name", "lobby")],
        )),
        Some(Permissions::MANAGE_CHANNELS),
        true,
    );
    let (_, response) = handle_capture(&runtime, &allowed).await;
    assert!(response_text(response.as_ref().expect("reply")).contains("Created <#500>"));
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

/// Manage Server plus Manage Channels: may import files that add creators.
fn manager() -> Option<Permissions> {
    Some(Permissions::MANAGE_GUILD | Permissions::MANAGE_CHANNELS)
}

/// Manage Server alone: the `/export` and `/import` gate, but not `/create`'s.
fn server_only() -> Option<Permissions> {
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
    if runtime.live_actor(GUILD).is_none() {
        assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    }
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
async fn import_preview_shows_template_text_without_mentions() {
    let trace = Trace::default();
    let mut incoming = full_config();
    incoming.templates[0].name_template = "@everyone lobby".to_owned();
    let bytes = serde_json::to_vec(&incoming).unwrap();
    let (runtime, _) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (owned, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    assert!(owned);
    let preview = preview.expect("preview");
    let text = response_text(&preview);
    assert!(
        text.contains("@everyone lobby"),
        "preview must show the new template text: {text:?}"
    );
    let mentions = preview
        .data
        .as_ref()
        .and_then(|data| data.allowed_mentions.as_ref())
        .expect("preview disables mentions");
    assert!(mentions.parse.is_empty());
    assert!(mentions.users.is_empty());
    assert!(mentions.roles.is_empty());
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
async fn config_apply_rejects_stale_expected() {
    // The test double mirrors `PgVoiceConfigStore::apply`: the write lands
    // only when the stored configuration still equals the snapshot the
    // preview was rendered from.
    let trace = Trace::default();
    let store = Store::new(trace.clone());
    *store.config.lock().unwrap() = empty_config();
    store
        .config_apply(GUILD, &full_config(), &empty_config())
        .await
        .expect("matching expected applies");
    assert!(applied(&trace));
    let conflict = store
        .config_apply(GUILD, &empty_config(), &empty_config())
        .await;
    assert_eq!(conflict, Err(StoreError::Conflict));
    assert_eq!(*store.config.lock().unwrap(), full_config());
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

fn positional_array_form(config: &VoiceConfiguration) -> Vec<u8> {
    let value = serde_json::to_value(config).unwrap();
    serde_json::to_vec(&json!([
        value["version"],
        value["guild_id"],
        value["creators"],
        value["templates"],
        value["aliases"],
        value["lists"],
        value["logging"],
        value["settings"],
    ]))
    .unwrap()
}

#[test]
fn plan_import_preview_uses_the_strict_decoder() {
    let positional = positional_array_form(&full_config());
    // Precondition: plain serde accepts this form, which is the gap.
    assert_eq!(
        serde_json::from_slice::<VoiceConfiguration>(&positional).unwrap(),
        full_config()
    );
    let decision = plan_import_preview(&empty_config(), &positional, &config_inventory());
    let ImportDecision::Refuse { message } = decision else {
        panic!("positional array must be refused, got {decision:?}");
    };
    assert!(
        message.contains("malformed configuration JSON"),
        "{message}"
    );
    assert!(message.contains("Nothing was changed"), "{message}");
}

#[test]
fn plan_import_preview_refuses_unlintable_templates_and_unknown_commands() {
    let refused = |config: &VoiceConfiguration| match plan_import_preview(
        &empty_config(),
        &serde_json::to_vec(config).unwrap(),
        &config_inventory(),
    ) {
        ImportDecision::Refuse { message } => message,
        other => panic!("expected a refusal, got {other:?}"),
    };
    let mut config = full_config();
    config.creators[0].name_template = "@@secret_marker@@".to_owned();
    let message = refused(&config);
    assert!(message.contains("creators[0].name_template"), "{message}");
    assert!(!message.contains("secret_marker"), "{message}");
    assert!(message.contains("Nothing was changed"), "{message}");

    let mut config = full_config();
    config.templates[0].status_template = Some("[[never closed".to_owned());
    assert!(refused(&config).contains("templates[0].status_template"));

    let mut config = full_config();
    config.settings.command_roles = vec![config_codec::CommandRoles {
        command: "ban".to_owned(),
        role_ids: Vec::new(),
    }];
    assert!(refused(&config).contains("settings.command_roles[0].command"));

    let mut config = full_config();
    config.aliases[0].alias = "x".repeat(101);
    assert!(refused(&config).contains("aliases[0]"));
}

#[tokio::test]
async fn import_without_manage_channels_cannot_add_creators() {
    let trace = Trace::default();
    let bytes = serde_json::to_vec(&full_config()).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, server_only(), UPLOADER);
    let (owned, response) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    assert!(owned);
    let response = response.expect("refusal");
    assert!(response_text(&response).contains("Manage Channels"));
    assert!(response_text(&response).contains("Nothing was changed"));
    // No Confirm button, nothing stored, nothing remembered to confirm.
    assert!(response
        .data
        .as_ref()
        .and_then(|data| data.components.as_ref())
        .is_none());
    assert_eq!(*shared.lock().unwrap(), empty_config());
    assert!(!applied(&trace));
}

#[tokio::test]
async fn import_without_manage_channels_may_edit_existing_creators() {
    let trace = Trace::default();
    let mut incoming = full_config();
    incoming.creators[0].default_limit = 5;
    let bytes = serde_json::to_vec(&incoming).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), full_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, server_only(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let (confirm_id, _) = preview_buttons(&preview.expect("preview"));
    let confirm = component_interaction(&confirm_id, server_only(), UPLOADER);
    let (_, response) = handle_import_capture(&runtime, &confirm, Some(&inventory)).await;
    assert!(response_text(&response.expect("applied")).starts_with("Import applied:"));
    assert_eq!(shared.lock().unwrap().creators[0].default_limit, 5);
    assert!(applied(&trace));
}

#[tokio::test]
async fn import_confirm_rechecks_manage_channels_for_added_creators() {
    let trace = Trace::default();
    let bytes = serde_json::to_vec(&full_config()).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let (confirm_id, _) = preview_buttons(&preview.expect("preview"));

    // Manage Channels is gone at confirm time: the creator rows are refused.
    let confirm = component_interaction(&confirm_id, server_only(), UPLOADER);
    let (owned, response) = handle_import_capture(&runtime, &confirm, Some(&inventory)).await;
    assert!(owned);
    assert!(response_text(&response.expect("refusal")).contains("Manage Channels"));
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

/// Guild-less ballot interaction: the shared builder always attaches a
/// guild, but unscoped presses must stay silent too.
#[allow(deprecated)]
fn guildless_component_interaction(custom_id: &str) -> Interaction {
    let mut interaction = voice_interaction(None, None, false);
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

#[test]
fn parse_kick_extracts_member_and_reason() {
    let interaction = voice_interaction(
        Some(command_data(
            "kick",
            vec![
                user_option("member", 303),
                command_option("reason", "too loud"),
            ],
        )),
        None,
        true,
    );
    assert_eq!(
        parse_voice_command(&interaction),
        Some(VoiceCommand::Kick {
            target: 303,
            reason: Some("too loud".to_owned()),
        })
    );
}

#[test]
fn parse_kick_accepts_moderation_shape_without_reason() {
    let interaction = voice_interaction(
        Some(command_data("kick", vec![user_option("target", 303)])),
        None,
        true,
    );
    assert_eq!(
        parse_voice_command(&interaction),
        Some(VoiceCommand::Kick {
            target: 303,
            reason: None,
        })
    );
}

#[test]
fn parse_kick_without_user_stays_silent_for_the_router() {
    let interaction = voice_interaction(
        Some(command_data("kick", vec![command_option("reason", "x")])),
        None,
        true,
    );
    assert_eq!(parse_voice_command(&interaction), None);
}

#[test]
fn parse_ballot_buttons_by_vote_id() {
    for (custom_id, expected) in [
        (
            "votekick:7000:yes",
            VoiceCommand::Ballot {
                vote_id: 7000,
                ballot: VoteBallot::Yes,
            },
        ),
        (
            "votekick:7000:no",
            VoiceCommand::Ballot {
                vote_id: 7000,
                ballot: VoteBallot::No,
            },
        ),
    ] {
        let interaction = component_interaction(custom_id, None, MEMBER);
        assert_eq!(parse_voice_command(&interaction), Some(expected));
    }
}

#[test]
fn parse_foreign_buttons_stay_silent() {
    for custom_id in [
        "self-role:1",
        "votekick:abc:yes",
        "votekick:7000:maybe",
        "votekick:7000",
        "votekick:",
    ] {
        let interaction = component_interaction(custom_id, None, MEMBER);
        assert_eq!(parse_voice_command(&interaction), None);
    }
    let guildless = guildless_component_interaction("votekick:7000:yes");
    assert_eq!(parse_voice_command(&guildless), None);
}

#[test]
fn vote_button_ids_round_trip() {
    assert_eq!(vote_button_id(7000, VoteBallot::Yes), "votekick:7000:yes");
    assert_eq!(vote_button_id(7000, VoteBallot::No), "votekick:7000:no");
}

#[test]
fn kick_and_ballot_share_the_kick_restriction_name() {
    assert_eq!(
        VoiceCommand::Kick {
            target: 303,
            reason: None,
        }
        .name(),
        "kick"
    );
    assert_eq!(
        VoiceCommand::Ballot {
            vote_id: 7000,
            ballot: VoteBallot::Yes,
        }
        .name(),
        "kick"
    );
}

fn kick_sink_interaction(target: u64, initiator: u64) -> Interaction {
    with_user(
        voice_interaction(
            Some(command_data(
                "kick",
                vec![
                    user_option("member", target),
                    command_option("reason", "too loud"),
                ],
            )),
            None,
            true,
        ),
        initiator,
    )
}

fn voice_member_in(member_id: u64, channel_id: u64) -> VoiceMember {
    VoiceMember {
        member_id,
        channel_id,
        bot: Some(false),
    }
}

const KICK_VOTER: u64 = 301;
const KICK_TARGET: u64 = 303;
const KICK_ROOM: u64 = 500;
const OTHER_ROOM: u64 = 501;

/// A runtime tracking `KICK_ROOM` and `OTHER_ROOM` with the given occupants.
fn kick_room_runtime(
    trace: &Trace,
    enabled: bool,
    members: Vec<VoiceMember>,
) -> VoiceRuntime<Store, Http> {
    let runtime = VoiceRuntime::new(
        {
            let trace = trace.clone();
            move || {
                let store = Store::new(trace.clone());
                for channel in [KICK_ROOM, OTHER_ROOM] {
                    store.rooms.lock().unwrap().insert(channel, room(channel));
                }
                (store, Http::new(trace.clone()))
            }
        },
        Duration::from_millis(10),
        enabled,
    );
    // A disabled runtime spawns no actor, so nothing is published.
    assert_eq!(
        runtime.publish_snapshot(GUILD, snapshot(&[KICK_ROOM, OTHER_ROOM], members)),
        enabled
    );
    runtime
}

#[tokio::test]
async fn kick_vote_in_a_shared_room_answers_the_public_ballot_without_defer() {
    let trace = Trace::default();
    let runtime = kick_room_runtime(
        &trace,
        true,
        vec![
            voice_member_in(KICK_VOTER, KICK_ROOM),
            voice_member_in(KICK_TARGET, KICK_ROOM),
        ],
    );
    let replies = Replies::new(trace.clone());
    let answered = VoiceResponder::start_kick_vote(
        &runtime,
        &replies,
        &kick_sink_interaction(KICK_TARGET, KICK_VOTER),
    )
    .await;
    assert!(
        answered,
        "voice owns the callback, so the router stays silent"
    );
    // No defer: the ballot goes out as the initial public callback, so every
    // occupant can see the buttons and reach quorum.
    assert_eq!(*trace.lock().unwrap(), ["respond"]);
    let completed = replies.completed.lock().unwrap();
    assert_eq!(completed.len(), 1);
    assert_eq!(
        completed[0].kind,
        InteractionResponseType::ChannelMessageWithSource
    );
    let data = completed[0].data.as_ref().expect("ballot body");
    assert_ne!(data.flags, Some(MessageFlags::EPHEMERAL));
    let content = data.content.as_deref().unwrap_or_default();
    assert!(content.contains("<@303>"), "{content}");
    assert!(content.contains("too loud"), "{content}");
    assert_eq!(data.components.as_ref().map_or(0, Vec::len), 1);
}

// Every decline sends no callback at all: the router answers instead, so a
// callback here would be a second answer to the same interaction.
#[tokio::test]
async fn kick_vote_declines_without_a_callback_unless_the_invoker_shares_the_room() {
    let occupied = vec![
        voice_member_in(KICK_VOTER, KICK_ROOM),
        voice_member_in(KICK_TARGET, KICK_ROOM),
    ];
    let cases = [
        // (case, enabled, occupants)
        ("no tracked room at all", true, Vec::new()),
        (
            "target outside any room",
            true,
            vec![voice_member_in(KICK_VOTER, KICK_ROOM)],
        ),
        (
            "invoker outside any room",
            true,
            vec![voice_member_in(KICK_TARGET, KICK_ROOM)],
        ),
        (
            "invoker in a different tracked room",
            true,
            vec![
                voice_member_in(KICK_VOTER, OTHER_ROOM),
                voice_member_in(KICK_TARGET, KICK_ROOM),
            ],
        ),
        ("voice disabled", false, occupied),
    ];
    for (case, enabled, members) in cases {
        let trace = Trace::default();
        let runtime = kick_room_runtime(&trace, enabled, members);
        let replies = Replies::new(trace.clone());
        let answered = VoiceResponder::start_kick_vote(
            &runtime,
            &replies,
            &kick_sink_interaction(KICK_TARGET, KICK_VOTER),
        )
        .await;
        assert!(!answered, "{case}: the router must answer");
        // Reconcile may still prune an empty tracked room, so look only for
        // interaction callbacks rather than an empty trace.
        let calls = trace.lock().unwrap().clone();
        assert!(
            !calls
                .iter()
                .any(|call| call.starts_with("respond") || call.starts_with("defer")),
            "{case}: no callback, got {calls:?}"
        );
        assert!(replies.completed.lock().unwrap().is_empty(), "{case}");
    }
}

// The sink never answers `/kick` on its own account, even for a room
// occupant: the router decides first (moderation gate, then `kick_vote`).
#[tokio::test]
async fn sink_never_answers_kick_without_the_router() {
    let trace = Trace::default();
    let runtime = kick_room_runtime(
        &trace,
        true,
        vec![
            voice_member_in(KICK_VOTER, KICK_ROOM),
            voice_member_in(KICK_TARGET, KICK_ROOM),
        ],
    );
    let replies = Replies::new(trace.clone());
    VoiceResponder::respond_with(
        &runtime,
        &replies,
        &kick_sink_interaction(KICK_TARGET, KICK_VOTER),
        None,
        None,
    )
    .await;
    let calls = trace.lock().unwrap().clone();
    assert!(
        !calls
            .iter()
            .any(|call| call.starts_with("respond") || call.starts_with("defer")),
        "no callback, got {calls:?}"
    );
    assert!(replies.completed.lock().unwrap().is_empty());
}

// ---- lifecycle-outcome signals (offline cutover verification) ----
//
// The worker emits fixed-cardinality outcome signals for every terminal
// create/move/delete, reconcile plan size, queue dead-letter and
// creator-orphan. These tests pin the emission wiring end to end through
// the fake store/HTTP fixtures: pure mapper coverage plus worker-driven
// metric deltas. Global counters are monotonic, so the worker tests assert
// `after >= before + expected`: safe under parallel test threads.

/// Read one counter series from the process-global metrics exposition.
fn global_series(prefix: &str) -> u64 {
    metrics::global()
        .render(None)
        .lines()
        .filter(|line| line.starts_with(prefix))
        .map(|line| {
            line.rsplit_once(' ')
                .unwrap_or_else(|| panic!("bad sample: {line}"))
                .1
                .parse::<u64>()
                .unwrap_or_else(|_| panic!("bad sample: {line}"))
        })
        .sum()
}

#[test]
fn http_error_outcomes_stay_bounded_for_cutover_queries() {
    use RoomHttpError::*;
    // Rate limits are retries, never outcomes; callers skip them before
    // reaching the mapper. Everything terminal is `discord` except a
    // stale guard, which is `cancelled`. Status/code values never leak.
    for error in [
        AccessDenied,
        NotFound,
        Unauthorized,
        UnknownOutcome,
        InvalidRequest,
        RenameDeferred,
        Rejected {
            status: 400,
            code: 50035,
        },
        Rejected {
            status: 403,
            code: 50013,
        },
    ] {
        assert_eq!(voice_outcome_from_http(&error), "discord", "{error:?}");
    }
    assert_eq!(voice_outcome_from_http(&Cancelled), "cancelled");
}

#[test]
fn store_errors_share_one_persistence_outcome() {
    for error in [
        StoreError::Unavailable,
        StoreError::CredentialRefused,
        StoreError::Conflict,
    ] {
        assert_eq!(voice_outcome_from_store(&error), "persistence");
    }
}

#[test]
fn dead_letter_families_cover_every_queue_action_shape() {
    let companion_plan = || TextChannelPlan {
        room_id: 500,
        guild_id: GUILD,
        name: "voice-chat".to_owned(),
        category_id: CATEGORY,
        overwrites: Vec::new(),
        settings: two_bot_core::voice_text_channel::TextChannelSettings::default(),
    };
    let cases: Vec<(RoomAction, &str)> = vec![
        (
            RoomAction::CreateRoom {
                creator_channel_id: CREATOR,
                owner_id: MEMBER,
                name: "room".to_owned(),
                seed: 7,
            },
            "create",
        ),
        (
            RoomAction::MoveMember {
                member_id: MEMBER,
                channel_id: 500,
            },
            "move",
        ),
        (RoomAction::DeleteRoom { channel_id: 500 }, "delete"),
        (
            RoomAction::CreateCompanion {
                room_channel_id: 500,
                plan: companion_plan(),
            },
            "companion",
        ),
        (
            RoomAction::GrantCompanionView {
                room_channel_id: 500,
                text_channel_id: 501,
                member_id: MEMBER,
            },
            "companion",
        ),
        (
            RoomAction::RevokeCompanionView {
                room_channel_id: 500,
                text_channel_id: 501,
                member_id: MEMBER,
            },
            "companion",
        ),
        (
            RoomAction::UpdateOwnership {
                channel_id: 500,
                previous_owner_id: 301,
                owner_id: MEMBER,
                original_creator_id: MEMBER,
            },
            "ownership",
        ),
        (
            RoomAction::KickMember {
                channel_id: 500,
                member_id: MEMBER,
            },
            "kick",
        ),
        (
            RoomAction::RenameRoom {
                channel_id: 500,
                name: "den".to_owned(),
            },
            "rename",
        ),
        (
            RoomAction::SetCustomName {
                channel_id: 500,
                custom_name: Some("den".to_owned()),
            },
            "rename",
        ),
    ];
    for (action, family) in &cases {
        assert_eq!(voice_dead_action(action), *family, "{action:?}");
    }
}

#[tokio::test]
async fn terminal_queue_failure_dead_letters_exactly_once_per_family() {
    let (live, store, http, _) = fixture();
    let worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    // Non-terminal attempt: requeues with backoff, emits no dead letter.
    worker.queue.enqueue(
        GUILD,
        RoomAction::RenameRoom {
            channel_id: 500,
            name: "den".to_owned(),
        },
    );
    let action = worker.queue.pop_due(GUILD, 0).expect("rename due");
    let before = global_series("two_bot_voice_dead_letters_total{action=\"rename\"}");
    assert!(worker.mark_failed_observed(action, "flaky".to_owned(), 0));
    assert_eq!(
        global_series("two_bot_voice_dead_letters_total{action=\"rename\"}"),
        before,
        "non-terminal failure must not dead-letter"
    );
    // Terminal attempt (attempts saturating at QUEUE_MAX_ATTEMPTS): one
    // dead letter under the action family, surfaced in failed().
    let mut terminal = worker.queue.pop_due(GUILD, 60_000).expect("retry due");
    terminal.attempts = QUEUE_MAX_ATTEMPTS - 1;
    // Re-mark in-flight: pop_due registered this dispatch id.
    let before = global_series("two_bot_voice_dead_letters_total{action=\"rename\"}");
    assert!(worker.mark_failed_observed(terminal, "still down".to_owned(), 61_000));
    assert_eq!(
        global_series("two_bot_voice_dead_letters_total{action=\"rename\"}"),
        before + 1,
        "terminal failure must dead-letter exactly once"
    );
    assert_eq!(worker.queue.failed().len(), 1);
    assert_eq!(worker.queue.failed()[0].action.attempts, QUEUE_MAX_ATTEMPTS);
}

#[tokio::test]
async fn reconcile_pass_reports_its_plan_sizes() {
    let (live, store, http, _) = fixture();
    store.rooms.lock().unwrap().insert(500, room(500));
    store.rooms.lock().unwrap().insert(501, room(501));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    live.upsert_channel(channel(501, 2, Some(CATEGORY)));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let resumed_before = global_series("two_bot_voice_reconcile_actions_total{action=\"resumed\"}");
    let enqueued_before =
        global_series("two_bot_voice_reconcile_actions_total{action=\"delete_enqueued\"}");
    worker.reconcile();
    // Both tracked rooms are present, accessible and empty: each pass
    // resumes its lane and enqueues its delete.
    assert!(
        global_series("two_bot_voice_reconcile_actions_total{action=\"resumed\"}")
            >= resumed_before + 2
    );
    assert!(
        global_series("two_bot_voice_reconcile_actions_total{action=\"delete_enqueued\"}")
            >= enqueued_before + 2
    );
}

#[tokio::test]
async fn failed_create_compensation_orphan_is_counted_without_a_channel_id() {
    let trace = Trace::default();
    let store = Store::new(trace.clone());
    *store.add_creator_error.lock().unwrap() = Some(StoreError::Unavailable);
    let http = Http::new(trace.clone());
    http.delete_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::AccessDenied);
    let before = global_series("two_bot_voice_orphans_total");
    let text = execute_create(&store, &http, GUILD, "lobby", |_, _| {}).await;
    assert!(text.contains("manually"), "{text}");
    assert!(
        global_series("two_bot_voice_orphans_total") > before,
        "untracked orphan must advance the counter"
    );
}
