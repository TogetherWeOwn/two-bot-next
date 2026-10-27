#![cfg(test)]

use super::*;
use serde_json::json;
use std::sync::atomic::AtomicUsize;
use std::sync::Mutex;

#[path = "voice_actor_inbox_backpressure_acceptance_tests.rs"]
mod actor_inbox_backpressure_acceptance;
#[path = "voice_admission_config_acceptance_tests.rs"]
mod admission_config_acceptance;
#[path = "voice_admission_spawn_tests.rs"]
mod admission_spawn;
#[path = "voice_boot_settings_acceptance_tests.rs"]
mod boot_settings_acceptance;
#[path = "voice_cold_authority_tests.rs"]
mod cold_authority;
#[path = "voice_control_plane_overflow_acceptance_tests.rs"]
mod control_plane_overflow_acceptance;
#[path = "voice_creation_receipt_acceptance_tests.rs"]
mod creation_receipt_acceptance;
#[path = "voice_delete_guard_tests.rs"]
mod delete_guards;
#[path = "voice_disconnect_loud_acceptance_tests.rs"]
mod disconnect_loud_acceptance_tests;
#[path = "voice_disconnect_poison_acceptance_tests.rs"]
mod drop_poison_acceptance_tests;
#[path = "voice_empty_grace_override_tests.rs"]
mod empty_grace_override;
#[path = "event_voice_category_acceptance_tests.rs"]
mod event_voice_category_acceptance;
#[path = "event_voice_channels_acceptance_tests.rs"]
mod event_voice_channels_acceptance;
#[path = "event_voice_channels_regression_tests.rs"]
mod event_voice_channels_regression;
#[path = "voice_actor_fair_scheduling_acceptance_tests.rs"]
mod fair_scheduling_acceptance;
#[path = "voice_invite_override_tests.rs"]
mod invite_override;
#[path = "voice_invite_responder_wiring_tests.rs"]
mod invite_responder_wiring;
#[path = "voice_join_requests_tests.rs"]
mod join;
#[path = "voice_join_grant_witness_acceptance_tests.rs"]
mod join_grant_witness_acceptance;
#[path = "voice_join_still_waiting_acceptance_tests.rs"]
mod join_still_waiting_acceptance;
#[path = "voice_kick_tests.rs"]
mod kick;
#[path = "voice_kick_ballot_delivery_tests.rs"]
mod kick_ballot_delivery;
#[path = "voice_rooms_limit_tests.rs"]
mod limit_command;
#[path = "voice_name_tests.rs"]
mod name;
#[path = "voice_name_filter_acceptance_tests.rs"]
mod name_filter_acceptance;
#[path = "voice_name_rerender_tests.rs"]
mod name_rerender;
#[path = "voice_ownership_receipt_acceptance_tests.rs"]
mod ownership_receipt_acceptance;
#[path = "voice_private_runtime_tests.rs"]
mod private;
#[path = "voice_responder_admission_acceptance_tests.rs"]
mod responder_admission_acceptance;
#[path = "voice_rooms_sink_tests.rs"]
mod sink;
#[path = "voice_snapshot_fence_acceptance_tests.rs"]
mod snapshot_fence_acceptance;
#[path = "../../core/tests/support/tracing_capture.rs"]
mod tracing_capture;
#[path = "voice_votekick_ballot_truth_acceptance_tests.rs"]
mod votekick_ballot_truth_acceptance;
#[path = "voice_worker_reply_acceptance_tests.rs"]
mod worker_reply_acceptance;

const GUILD: u64 = 100;
const CREATOR: u64 = 200;
const CATEGORY: u64 = 400;
const MEMBER: u64 = 300;
const NOW: &str = "2026-09-30T00:00:00.000000+00:00";
type Trace = Arc<Mutex<Vec<String>>>;
type Hook = Arc<dyn Fn() + Send + Sync>;
type OverwriteHook = Arc<dyn Fn(u64, u64, Option<PermissionOverwrite>) + Send + Sync>;

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
    // Ordinary members by default: every voice member carries an (empty) guild
    // role list so target-authority resolution yields Some(false). Tests for
    // privilege/unavailable override this map.
    let member_roles = members
        .iter()
        .map(|member| (member.member_id, Vec::new()))
        .collect();
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
        member_roles,
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
    creators: Arc<Mutex<Vec<CreatorChannel>>>,
    rooms: Mutex<HashMap<u64, VoiceRoom>>,
    rooms_errors: Mutex<VecDeque<StoreError>>,
    /// One-shot hang for the next `rooms()` call, consumed when it
    /// starts, so a test can prove a load attempt is deadline-bounded.
    rooms_delay: Mutex<Option<Duration>>,
    after_rooms: Option<Hook>,
    owner_intents: Mutex<HashMap<u64, OwnerGrantIntent>>,
    owner_pending: Mutex<HashSet<u64>>,
    owner_revision: AtomicU64,
    prepare_errors: Mutex<VecDeque<StoreError>>,
    ownership_errors: Mutex<VecDeque<StoreError>>,
    /// Times `update_ownership` yields before writing, consumed when the
    /// write starts, so a test can prove an expired reply budget never
    /// cancels an in-flight commit nobody is listening on.
    update_ownership_yields: AtomicUsize,
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
    /// Durable approved Connect grants, keyed by room then member.
    join_grants: Mutex<BTreeMap<u64, BTreeSet<u64>>>,
    join_grants_errors: Mutex<VecDeque<StoreError>>,
    save_join_grant_errors: Mutex<VecDeque<StoreError>>,
    remove_join_grant_errors: Mutex<VecDeque<StoreError>>,
    add_creator_error: Mutex<Option<StoreError>>,
    after_persist: Option<Hook>,
    config: Arc<Mutex<VoiceConfiguration>>,
    /// Most lifecycle fixtures edit creator rows directly. Their snapshot must
    /// describe those same rows; config-import/reload tests supply an explicit
    /// snapshot instead, including intentional creator removals.
    snapshot_uses_creator_rows: bool,
    config_error: Option<StoreError>,
    config_reads: Arc<AtomicU64>,
    save_config_error: Option<StoreError>,
    /// Scripted durable-admission verdicts, consumed per claim; an empty
    /// script admits with an incrementing reservation id.
    claim_script: Mutex<VecDeque<Result<CreateClaim, StoreError>>>,
    /// Every claim as `(user, now_secs)`, and every settle as
    /// `(reservation, bound channel)`.
    claims: Mutex<Vec<(u64, i64)>>,
    /// The `leaving` list each claim carried.
    claim_leaving: Mutex<Vec<Vec<u64>>>,
    /// The admission config each claim carried, in claim order. Shared out
    /// so runtime-level tests can pin the config spawn installed.
    claim_configs: Arc<Mutex<Vec<CreateAdmissionConfig>>>,
    settled: Mutex<Vec<(String, Option<u64>)>>,
    settle_errors: Mutex<VecDeque<StoreError>>,
    /// Claims bound to the channel Discord returned, as `(reservation, channel)`
    /// in bind order; the durable channel witness a restart reads back.
    bound: Mutex<Vec<(String, u64)>>,
    bind_errors: Mutex<VecDeque<StoreError>>,
    /// Also push `claim` / `settle` markers onto the shared trace, for tests
    /// that pin ordering against the Discord and persist calls.
    trace_claims: bool,
    creators_error: Option<StoreError>,
    /// Single-row creator reads: the join-path read-through must skip known
    /// creators, rooms, halted workers and already-missed channels.
    creator_for_reads: Arc<AtomicU64>,
    /// Scripted single-row creator read failure: while set, every
    /// `creator_for` read fails, so tests can prove the join path backs off
    /// instead of paying a read per join.
    creator_for_error: Option<StoreError>,
    /// Hanging single-row reads sleep this long before the row lookup, so a
    /// test can prove the join path bounds a slow pool at the timeout.
    creator_for_delay: Option<Duration>,
    custom_names: Mutex<HashMap<u64, String>>,
    save_custom_name_errors: Mutex<VecDeque<StoreError>>,
    /// V4 audit rows appended so far, in append order.
    kick_audit: Mutex<Vec<KickAuditRow>>,
    kick_audit_errors: Mutex<VecDeque<StoreError>>,
    /// Scripted live event-voice bindings for the guild repair; empty by
    /// default, so the repair is a no-op unless the test seeds a binding.
    ev_bindings: Mutex<Vec<EventVoiceBinding>>,
}

impl Store {
    fn new(trace: Trace) -> Self {
        Self {
            trace,
            creators: Arc::new(Mutex::new(vec![CreatorChannel::new(GUILD, CREATOR)])),
            rooms: Mutex::new(HashMap::new()),
            rooms_errors: Mutex::new(VecDeque::new()),
            rooms_delay: Mutex::new(None),
            after_rooms: None,
            owner_intents: Mutex::new(HashMap::new()),
            owner_pending: Mutex::new(HashSet::new()),
            owner_revision: AtomicU64::new(0),
            prepare_errors: Mutex::new(VecDeque::new()),
            ownership_errors: Mutex::new(VecDeque::new()),
            update_ownership_yields: AtomicUsize::new(0),
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
            join_grants: Mutex::new(BTreeMap::new()),
            join_grants_errors: Mutex::new(VecDeque::new()),
            save_join_grant_errors: Mutex::new(VecDeque::new()),
            remove_join_grant_errors: Mutex::new(VecDeque::new()),
            add_creator_error: Mutex::new(None),
            after_persist: None,
            config: Arc::new(Mutex::new(empty_config())),
            snapshot_uses_creator_rows: true,
            config_error: None,
            config_reads: Arc::new(AtomicU64::new(0)),
            save_config_error: None,
            claim_script: Mutex::new(VecDeque::new()),
            claims: Mutex::new(Vec::new()),
            claim_leaving: Mutex::new(Vec::new()),
            claim_configs: Arc::new(Mutex::new(Vec::new())),
            settled: Mutex::new(Vec::new()),
            settle_errors: Mutex::new(VecDeque::new()),
            bound: Mutex::new(Vec::new()),
            bind_errors: Mutex::new(VecDeque::new()),
            trace_claims: false,
            creators_error: None,
            creator_for_reads: Arc::new(AtomicU64::new(0)),
            creator_for_error: None,
            creator_for_delay: None,
            custom_names: Mutex::new(HashMap::new()),
            save_custom_name_errors: Mutex::new(VecDeque::new()),
            kick_audit: Mutex::new(Vec::new()),
            kick_audit_errors: Mutex::new(VecDeque::new()),
            ev_bindings: Mutex::new(Vec::new()),
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
        let delay = self.rooms_delay.lock().unwrap().take();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        if let Some(error) = self.rooms_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        let rooms = self.rooms.lock().unwrap().values().cloned().collect();
        if let Some(hook) = &self.after_rooms {
            hook();
        }
        Ok(rooms)
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
        self.creator_for_reads.fetch_add(1, Ordering::Relaxed);
        if let Some(error) = self.creator_for_error {
            return Err(error);
        }
        if let Some(delay) = self.creator_for_delay {
            tokio::time::sleep(delay).await;
        }
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
        if self.update_ownership_yields.load(Ordering::Relaxed) > 0 {
            self.update_ownership_yields.fetch_sub(1, Ordering::Relaxed);
            tokio::task::yield_now().await;
        }
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
        self.join_grants.lock().unwrap().remove(&channel);
        self.owner_intents.lock().unwrap().remove(&channel);
        self.owner_pending.lock().unwrap().remove(&channel);
        Ok(())
    }
    async fn record_event_voice_join(&self, channel: u64, member: u64) -> Result<(), StoreError> {
        self.trace
            .lock()
            .unwrap()
            .push(format!("evjoin:{channel}:{member}"));
        Ok(())
    }
    async fn record_event_voice_leave(&self, channel: u64, member: u64) -> Result<(), StoreError> {
        self.trace
            .lock()
            .unwrap()
            .push(format!("evleave:{channel}:{member}"));
        Ok(())
    }
    async fn event_voice_guild_bindings(
        &self,
        _: u64,
    ) -> Result<Vec<EventVoiceBinding>, StoreError> {
        Ok(self.ev_bindings.lock().unwrap().clone())
    }
    async fn flush_event_voice_presence(
        &self,
        event_id: &str,
        _: OffsetDateTime,
        occupants: &[u64],
    ) -> Result<u64, StoreError> {
        self.trace
            .lock()
            .unwrap()
            .push(format!("evrepair_flush:{event_id}:{}", occupants.len()));
        Ok(0)
    }
    async fn close_event_voice_spans_absent(
        &self,
        event_id: &str,
        _: OffsetDateTime,
        present: &[u64],
    ) -> Result<u64, StoreError> {
        self.trace
            .lock()
            .unwrap()
            .push(format!("evrepair_close:{event_id}:{}", present.len()));
        Ok(0)
    }
    async fn claim_create(
        &self,
        _: u64,
        user: u64,
        leaving: &[u64],
        config: &CreateAdmissionConfig,
        now_secs: i64,
    ) -> Result<CreateClaim, StoreError> {
        self.claim_leaving.lock().unwrap().push(leaving.to_vec());
        self.claim_configs.lock().unwrap().push(*config);
        if self.trace_claims {
            self.trace.lock().unwrap().push("claim".to_owned());
        }
        let mut claims = self.claims.lock().unwrap();
        claims.push((user, now_secs));
        match self.claim_script.lock().unwrap().pop_front() {
            Some(verdict) => verdict,
            None => Ok(CreateClaim::Admitted {
                reservation_id: format!("r{}", claims.len()),
            }),
        }
    }
    async fn bind_create_channel(
        &self,
        _: u64,
        reservation_id: &str,
        channel: u64,
    ) -> Result<(), StoreError> {
        if let Some(error) = self.bind_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        self.bound
            .lock()
            .unwrap()
            .push((reservation_id.to_owned(), channel));
        Ok(())
    }
    async fn orphaned_create_channels(&self, _: u64) -> Result<Vec<(String, u64)>, StoreError> {
        // Unsettled, bound, and without a tracked room: the SQL predicate.
        let settled = self.settled.lock().unwrap();
        let rooms = self.rooms.lock().unwrap();
        Ok(self
            .bound
            .lock()
            .unwrap()
            .iter()
            .filter(|(reservation, channel)| {
                !settled.iter().any(|(done, _)| done == reservation) && !rooms.contains_key(channel)
            })
            .cloned()
            .collect())
    }
    async fn persist_create(
        &self,
        reservation_id: &str,
        room: &VoiceRoom,
    ) -> Result<(), StoreError> {
        // Model a transaction: even an UPDATE-only refusal publishes no room.
        if let Some(error) = self.settle_errors.lock().unwrap().pop_front() {
            self.trace
                .lock()
                .unwrap()
                .push(format!("persist:{}", room.channel_id));
            return Err(error);
        }
        self.persist(room).await?;
        if self.trace_claims {
            self.trace.lock().unwrap().push("settle".to_owned());
        }
        self.settled
            .lock()
            .unwrap()
            .push((reservation_id.to_owned(), Some(room.channel_id)));
        Ok(())
    }
    async fn settle_create(&self, reservation_id: &str) -> Result<bool, StoreError> {
        if self.trace_claims {
            self.trace.lock().unwrap().push("settle".to_owned());
        }
        if let Some(error) = self.settle_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        self.settled
            .lock()
            .unwrap()
            .push((reservation_id.to_owned(), None));
        Ok(true)
    }
    async fn config_snapshot(&self, _: u64) -> Result<VoiceConfiguration, StoreError> {
        self.config_reads.fetch_add(1, Ordering::Relaxed);
        if let Some(error) = self.config_error {
            return Err(error);
        }
        let mut config = self.config.lock().unwrap().clone();
        if self.snapshot_uses_creator_rows {
            config.creators = self
                .creators
                .lock()
                .unwrap()
                .iter()
                .map(|creator| config_codec::CreatorConfiguration {
                    channel_id: creator.channel_id.to_string(),
                    name_template: creator.name_template.clone(),
                    status_template: None,
                    default_limit: creator.default_limit.unwrap_or(0) as u16,
                    always_private: creator.private_default,
                    text_channels: creator.text_channels,
                    position: match creator.position {
                        RoomPosition::Above => config_codec::RoomPosition::Above,
                        RoomPosition::Below => config_codec::RoomPosition::Below,
                    },
                    first_number: creator.first_room_number as u32,
                    group_by_category: creator.group_by_category,
                    permission_source: match creator.permission_source {
                        PermissionSource::Creator => config_codec::PermissionSource::Creator {},
                        PermissionSource::Category => config_codec::PermissionSource::Category {},
                        PermissionSource::Channel(id) => config_codec::PermissionSource::Channel {
                            channel_id: id.to_string(),
                        },
                    },
                })
                .collect();
        }
        Ok(config)
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
    async fn record_kick_audit(&self, rows: &[KickAuditRow]) -> Result<(), StoreError> {
        if let Some(error) = self.kick_audit_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        // Same idempotency as the table's (guild, vote, event) key.
        let mut stored = self.kick_audit.lock().unwrap();
        for row in rows {
            if !stored
                .iter()
                .any(|s| (s.guild_id, s.vote_id, s.event) == (row.guild_id, row.vote_id, row.event))
            {
                stored.push(row.clone());
            }
        }
        Ok(())
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
    async fn join_grants(&self, _: u64) -> Result<BTreeMap<u64, BTreeSet<u64>>, StoreError> {
        if let Some(error) = self.join_grants_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        Ok(self.join_grants.lock().unwrap().clone())
    }
    async fn save_join_grant(
        &self,
        _: u64,
        room: u64,
        member: u64,
    ) -> Result<JoinGrantIntent, StoreError> {
        self.trace
            .lock()
            .unwrap()
            .push(format!("save_join_grant:{room}:{member}"));
        if let Some(error) = self.save_join_grant_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        if !self.rooms.lock().unwrap().contains_key(&room) {
            return Ok(JoinGrantIntent::RoomMissing);
        }
        let inserted = self
            .join_grants
            .lock()
            .unwrap()
            .entry(room)
            .or_default()
            .insert(member);
        Ok(if inserted {
            JoinGrantIntent::Fresh
        } else {
            JoinGrantIntent::Retained
        })
    }
    async fn remove_join_grant(&self, _: u64, room: u64, member: u64) -> Result<(), StoreError> {
        self.trace
            .lock()
            .unwrap()
            .push(format!("remove_join_grant:{room}:{member}"));
        if let Some(error) = self.remove_join_grant_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        if let Some(members) = self.join_grants.lock().unwrap().get_mut(&room) {
            members.remove(&member);
        }
        Ok(())
    }
}

/// Scripted `/import` download queue shared with the harness.
type DownloadResults = Arc<Mutex<VecDeque<Result<Vec<u8>, RoomHttpError>>>>;

#[derive(Default)]
struct OverwriteGate {
    sent: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

/// Hold one `/limit` PATCH mid-flight: the fake signals `sent` when the
/// write starts and finishes only after `release`.
#[derive(Default)]
struct LimitGate {
    sent: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

/// One join-request message the fake Discord accepted.
#[derive(Clone)]
struct SentPrompt {
    channel: u64,
    content: String,
    mention: Option<u64>,
    components: Vec<Component>,
    message: MessageRef,
}

struct Http {
    trace: Trace,
    next_id: Mutex<u64>,
    create_errors: Mutex<VecDeque<RoomHttpError>>,
    move_errors: Mutex<VecDeque<RoomHttpError>>,
    delete_errors: Mutex<VecDeque<RoomHttpError>>,
    delete_channel_errors: Mutex<HashMap<u64, VecDeque<RoomHttpError>>>,
    rename_errors: Mutex<VecDeque<RoomHttpError>>,
    limit_errors: Mutex<VecDeque<RoomHttpError>>,
    companion_errors: Mutex<VecDeque<RoomHttpError>>,
    view_errors: Mutex<VecDeque<RoomHttpError>>,
    /// Scripted failures for the V4 enforcement's Connect deny.
    kick_errors: Mutex<VecDeque<RoomHttpError>>,
    overwrite_errors: Mutex<VecDeque<RoomHttpError>>,
    put_overwrite_errors: Mutex<VecDeque<RoomHttpError>>,
    after_overwrite: Option<OverwriteHook>,
    join_errors: Mutex<VecDeque<RoomHttpError>>,
    written_overwrites: Mutex<Vec<(u64, PermissionOverwrite)>>,
    notices: Mutex<Vec<(NoticeTarget, String, Option<u64>)>>,
    refused_notices: Mutex<Vec<NoticeTarget>>,
    /// Notice targets answering a per-route 429 instead of a refusal.
    rate_limited_notices: Mutex<Vec<NoticeTarget>>,
    /// V3 join-request prompts posted in a room's chat, and the scripted
    /// errors (one per attempt) the next sends fail with.
    prompts: Mutex<Vec<SentPrompt>>,
    prompt_errors: Mutex<VecDeque<RoomHttpError>>,
    component_edits: Mutex<Vec<SentPrompt>>,
    component_edit_errors: Mutex<VecDeque<RoomHttpError>>,
    next_message: Mutex<u64>,
    created_attributes: Mutex<Vec<RoomChannelAttributes>>,
    created_names: Mutex<Vec<String>>,
    companion_plans: Mutex<Vec<TextChannelPlan>>,
    before_create: Option<Hook>,
    after_create: Option<Hook>,
    before_move: Option<Hook>,
    before_delete: Option<Hook>,
    before_limit: Option<Hook>,
    overwrites_gate: Option<Arc<OverwriteGate>>,
    limit_gate: Option<Arc<LimitGate>>,
    /// Live `/limit` write start, mirroring the production in-flight REST
    /// slot. The watchdog sibling republishes it while the write is held, so
    /// a hung-dispatch test can prove the age is observable mid-flight.
    /// Tokio clock: paused-time tests advance it deterministically.
    limit_in_flight: Mutex<Option<tokio::time::Instant>>,
    /// How often the watchdog sibling republished the REST age through this
    /// writer: per-instance proof the sibling's republish call runs while a
    /// write is held (the process-global gauge is last-writer-wins and
    /// cannot be asserted exactly under parallel libtest shards).
    republish_calls: AtomicUsize,
    downloaded_urls: Mutex<Vec<String>>,
    download_results: DownloadResults,
    /// When set, renames run as detached requests answering after this
    /// many milliseconds (the production `RoomHttp` path).
    slow_renames: Mutex<Option<u64>>,
    /// When set, status writes run as detached requests answering after
    /// this many milliseconds (the production `RoomHttp` path).
    slow_status: Mutex<Option<u64>>,
    /// Scripted failures for the next status writes.
    status_errors: Mutex<VecDeque<RoomHttpError>>,
    /// Scripted outcomes for bulk channel reorders, oldest first, and the
    /// reorders sent (sorted by channel id).
    reorder_errors: Mutex<VecDeque<RoomHttpError>>,
    reorders: Mutex<Vec<Vec<(u64, u64)>>>,
    /// When set, a created channel reports the position it was created at.
    echo_positions: Mutex<bool>,
}

impl Http {
    fn new(trace: Trace) -> Self {
        Self {
            trace,
            next_id: Mutex::new(500),
            create_errors: Mutex::new(VecDeque::new()),
            move_errors: Mutex::new(VecDeque::new()),
            delete_errors: Mutex::new(VecDeque::new()),
            delete_channel_errors: Mutex::new(HashMap::new()),
            rename_errors: Mutex::new(VecDeque::new()),
            limit_errors: Mutex::new(VecDeque::new()),
            companion_errors: Mutex::new(VecDeque::new()),
            view_errors: Mutex::new(VecDeque::new()),
            kick_errors: Mutex::new(VecDeque::new()),
            overwrite_errors: Mutex::new(VecDeque::new()),
            put_overwrite_errors: Mutex::new(VecDeque::new()),
            after_overwrite: None,
            join_errors: Mutex::new(VecDeque::new()),
            written_overwrites: Mutex::new(Vec::new()),
            notices: Mutex::new(Vec::new()),
            refused_notices: Mutex::new(Vec::new()),
            rate_limited_notices: Mutex::new(Vec::new()),
            prompts: Mutex::new(Vec::new()),
            prompt_errors: Mutex::new(VecDeque::new()),
            component_edits: Mutex::new(Vec::new()),
            component_edit_errors: Mutex::new(VecDeque::new()),
            next_message: Mutex::new(9_000),
            created_attributes: Mutex::new(Vec::new()),
            created_names: Mutex::new(Vec::new()),
            companion_plans: Mutex::new(Vec::new()),
            before_create: None,
            after_create: None,
            before_move: None,
            before_delete: None,
            before_limit: None,
            overwrites_gate: None,
            limit_gate: None,
            limit_in_flight: Mutex::new(None),
            republish_calls: AtomicUsize::new(0),
            reorder_errors: Mutex::new(VecDeque::new()),
            reorders: Mutex::new(Vec::new()),
            echo_positions: Mutex::new(false),
            slow_renames: Mutex::new(None),
            slow_status: Mutex::new(None),
            status_errors: Mutex::new(VecDeque::new()),
            downloaded_urls: Mutex::new(Vec::new()),
            download_results: Arc::new(Mutex::new(VecDeque::new())),
        }
    }
}

impl RoomWrites for Http {
    fn republish_rest_age(&self) {
        // Mirror the production slot: the age of the held `/limit` write,
        // or 0 when none is live.
        self.republish_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let age_secs = self
            .limit_in_flight
            .lock()
            .unwrap()
            .map_or(0, |started| started.elapsed().as_secs());
        metrics::global().set_voice_rest_in_flight_age_secs(age_secs);
    }
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
        result.name = Some(name.to_owned());
        if *self.echo_positions.lock().unwrap() {
            result.position = attributes.position.and_then(|p| i32::try_from(p).ok());
        }
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
        if let Some(error) = self.kick_errors.lock().unwrap().pop_front() {
            return Err(error);
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
        if let Some(error) = self
            .delete_channel_errors
            .lock()
            .unwrap()
            .get_mut(&channel)
            .and_then(VecDeque::pop_front)
        {
            return Err(error);
        }
        match self.delete_errors.lock().unwrap().pop_front() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    async fn set_voice_status(&self, channel: u64, status: &str) -> Result<(), RoomHttpError> {
        self.trace
            .lock()
            .unwrap()
            .push(format!("status:{channel}:{status}"));
        match self.status_errors.lock().unwrap().pop_front() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    fn detached_voice_status(&self, channel: u64, status: &str) -> Option<DetachedWrite> {
        let delay = (*self.slow_status.lock().unwrap())?;
        let trace = self.trace.clone();
        let status = status.to_owned();
        Some(Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            trace
                .lock()
                .unwrap()
                .push(format!("status:{channel}:{status}"));
            Ok(())
        }))
    }

    async fn reorder_channels(
        &self,
        _: u64,
        positions: &[(u64, u64)],
    ) -> Result<(), RoomHttpError> {
        let mut sorted = positions.to_vec();
        sorted.sort_unstable();
        self.reorders.lock().unwrap().push(sorted);
        match self.reorder_errors.lock().unwrap().pop_front() {
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
    fn detached_rename(&self, channel: u64, name: &str) -> Option<DetachedRename> {
        let delay = (*self.slow_renames.lock().unwrap())?;
        let trace = self.trace.clone();
        let name = name.to_owned();
        let outcome = self.rename_errors.lock().unwrap().pop_front();
        Some(Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            trace
                .lock()
                .unwrap()
                .push(format!("rename:{channel}:{name}"));
            outcome.map_or(Ok(()), Err)
        }))
    }
    async fn set_user_limit(
        &self,
        channel: u64,
        user_limit: u32,
        guard: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        if let Some(hook) = &self.before_limit {
            hook();
        }
        if !guard() {
            return Err(RoomHttpError::Cancelled);
        }
        self.trace
            .lock()
            .unwrap()
            .push(format!("limit:{channel}:{user_limit}"));
        // Mark the write live before any gate: the watchdog sibling reads
        // this slot while the PATCH is held mid-flight.
        *self.limit_in_flight.lock().unwrap() = Some(tokio::time::Instant::now());
        if let Some(gate) = &self.limit_gate {
            gate.sent.notify_one();
            gate.release.notified().await;
        }
        let outcome = match self.limit_errors.lock().unwrap().pop_front() {
            Some(error) => Err(error),
            None => Ok(()),
        };
        self.limit_in_flight.lock().unwrap().take();
        outcome
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
        if let Some(gate) = &self.overwrites_gate {
            gate.sent.notify_one();
            gate.release.notified().await;
        }
        if let Some(error) = self
            .put_overwrite_errors
            .lock()
            .unwrap()
            .pop_front()
            .or_else(|| self.overwrite_errors.lock().unwrap().pop_front())
        {
            return Err(error);
        }
        self.written_overwrites
            .lock()
            .unwrap()
            .push((channel, overwrite));
        if let Some(hook) = &self.after_overwrite {
            hook(channel, overwrite.id.get(), Some(overwrite));
        }
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
    async fn delete_overwrite(
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
            .push(format!("delete_overwrite:{channel}:{member}"));
        if let Some(gate) = &self.overwrites_gate {
            gate.sent.notify_one();
            gate.release.notified().await;
        }
        if let Some(error) = self.overwrite_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        if let Some(hook) = &self.after_overwrite {
            hook(channel, member, None);
        }
        Ok(())
    }
    async fn send_component_message(
        &self,
        channel: u64,
        content: &str,
        mention_user: Option<u64>,
        components: &[Component],
    ) -> Result<MessageRef, RoomHttpError> {
        self.trace.lock().unwrap().push(format!("prompt:{channel}"));
        if let Some(error) = self.prompt_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        let message_id = {
            let mut id = self.next_message.lock().unwrap();
            let next = *id;
            *id += 1;
            next
        };
        let message = MessageRef {
            channel_id: channel,
            message_id,
        };
        self.prompts.lock().unwrap().push(SentPrompt {
            channel,
            content: content.to_owned(),
            mention: mention_user,
            components: components.to_vec(),
            message,
        });
        Ok(message)
    }
    async fn edit_component_message(
        &self,
        message: MessageRef,
        content: &str,
        components: &[Component],
    ) -> Result<(), RoomHttpError> {
        self.trace.lock().unwrap().push(format!(
            "edit_prompt:{}:{}:{}:{}",
            message.channel_id,
            message.message_id,
            content,
            components.len()
        ));
        self.component_edits.lock().unwrap().push(SentPrompt {
            channel: message.channel_id,
            content: content.to_owned(),
            mention: None,
            components: components.to_vec(),
            message,
        });
        self.component_edit_errors
            .lock()
            .unwrap()
            .pop_front()
            .map_or(Ok(()), Err)
    }
    async fn send_notice(
        &self,
        target: NoticeTarget,
        content: &str,
        mention_role: Option<u64>,
    ) -> Result<(), RoomHttpError> {
        if self.rate_limited_notices.lock().unwrap().contains(&target) {
            return Err(RoomHttpError::RateLimited {
                retry_after_ms: 1_000,
                global: false,
            });
        }
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

/// `snapshot` re-keyed to `guild`: channel guild ids and the @everyone
/// role id follow the guild, so `LiveGuild::publish` accepts it.
fn snapshot_for_guild(guild: u64, extra: &[u64], members: Vec<VoiceMember>) -> GuildSnapshot {
    let mut snap = snapshot(extra, members);
    for channel in &mut snap.channels {
        channel.guild_id = Some(Id::new(guild));
    }
    snap.bot.roles = vec![role_with(guild, permissions())];
    snap
}

/// `fixture` on a caller-chosen guild id. Tests that read one actor's
/// process-global stall contribution use a unique guild per test, so
/// parallel test threads running actors on `GUILD` cannot create,
/// overwrite or retire the entry under test.
fn fixture_for_guild(guild: u64) -> (LiveGuild, Store, Http, Trace) {
    let trace = Trace::default();
    let live = LiveGuild::new(guild);
    live.publish(snapshot_for_guild(guild, &[], vec![]));
    (
        live,
        Store::new(trace.clone()),
        Http::new(trace.clone()),
        trace,
    )
}

fn room_for_guild(guild: u64, channel_id: u64) -> VoiceRoom {
    VoiceRoom::from_spec(
        NewRoomSpec {
            guild_id: guild,
            creator_channel_id: CREATOR,
            owner_id: MEMBER,
            seed: 7,
            created_at: NOW.to_owned(),
        },
        channel_id,
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

#[derive(Clone)]
struct JoinLogWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for JoinLogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture_join_logs() -> (Arc<Mutex<Vec<u8>>>, tracing::subscriber::DefaultGuard) {
    // Share the reconcile tests' global subscriber to keep callsite interest
    // stable; the local JSON writer captures only this runtime's events.
    // Admit DEBUG: benign duplicate refusals log at debug, and the default
    // fmt max is INFO, so without this the duplicate refusal line is dropped.
    // No other capture user filters on a field a debug line sets, so their
    // exact counts are unaffected.
    let _global = tracing_capture::global();
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let output = Arc::clone(&bytes);
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || JoinLogWriter(Arc::clone(&output)))
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (bytes, guard)
}

#[tokio::test(start_paused = true)]
async fn creator_join_logs_one_terminal_with_receipt_dwell_and_retry_wait() {
    let (bytes, _subscriber) = capture_join_logs();
    let (live, store, http, _) = fixture();
    http.move_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::RateLimited {
            retry_after_ms: 1_000,
            global: true,
        });
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let received = tokio::time::Instant::now();
    tokio::time::advance(Duration::from_millis(20)).await;
    let ticket = worker
        .live
        .voice_update(MEMBER, Some(CREATOR), Some(false))
        .unwrap();
    assert!(worker.accept_join_received(ticket, "new room", 7, NOW.to_owned(), received));
    tokio::time::advance(Duration::from_millis(30)).await;
    dispatch(&mut worker, 50).await;
    dispatch(&mut worker, 50).await;
    assert!(worker.move_timings.contains_key(&500));
    assert!(!worker.dispatch_one(999).await);
    tokio::time::advance(Duration::from_secs(1)).await;
    dispatch(&mut worker, 2_000).await;
    assert!(worker.move_timings.is_empty());
    let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    let joins: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|event| event["fields"]["voice_event"] == "voice_join")
        .collect();
    assert_eq!(joins.len(), 1, "{text}");
    let fields = &joins[0]["fields"];
    assert_eq!(fields["outcome"], "success");
    assert_eq!(fields["queue_dwell_seconds"], 0.02);
    assert_eq!(fields["wait_before_dispatch_seconds"], 1.03);
    assert_eq!(fields["elapsed_seconds"], 1.05);
    assert_eq!(fields["reorder_seconds"], 0.0);
    for field in [
        "claim_seconds",
        "post_seconds",
        "persist_seconds",
        "move_seconds",
    ] {
        assert!(fields[field].is_number());
    }
}

#[tokio::test]
async fn every_accepted_join_finishes_its_log_on_halt_or_shutdown() {
    for halt in [true, false] {
        let (bytes, _subscriber) = capture_join_logs();
        let (live, store, http, _) = fixture();
        if halt {
            store
                .claim_script
                .lock()
                .unwrap()
                .push_back(Err(StoreError::CredentialRefused));
        }
        let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
        join(&mut worker, MEMBER);
        join(&mut worker, MEMBER + 1);
        if halt {
            dispatch(&mut worker, 0).await;
            assert!(worker.halted());
            assert!(!worker.dispatch_one(1).await);
        }
        drop(worker);
        let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        let joins: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|event| event["fields"]["voice_event"] == "voice_join")
            .collect();
        assert_eq!(joins.len(), 2, "halt={halt}: {text}");
        assert_eq!(
            joins
                .iter()
                .filter(|event| event["fields"]["outcome"] == "cancelled")
                .count(),
            if halt { 1 } else { 2 }
        );
    }
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
    dispatch(&mut worker, 2).await;
    assert!(!worker.dispatch_one(3).await);
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
async fn refused_creator_joins_log_channel_and_creator_map_state() {
    // Stall triage needs the refusing channel and the creator-map state on
    // every refusal line: the channel id, the ticket's map generation versus
    // the live one, whether the channel is a known creator, and the map size.
    let (bytes, _subscriber) = capture_join_logs();
    let (live, store, http, _) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let ticket = join(&mut worker, MEMBER);
    // Same ticket twice: the repeat is refused as a benign duplicate while
    // the channel stays a known creator.
    assert!(!worker.accept_join(ticket, "duplicate", 8, NOW.to_owned()));
    // A join into a channel the creator map never held.
    let unknown = JoinTicket {
        creator_id: 999,
        ..ticket
    };
    assert!(!worker.accept_join(unknown, "new room", 9, NOW.to_owned()));
    // A stale ticket on a gateway that has not re-readied: flip ready on the
    // same worker and re-offer the queued ticket. The stale_ticket check runs
    // before the duplicate check, so the reason is stale_ticket with
    // ready=false, proving the field varies instead of being a constant.
    worker.live.write_state().ready = false;
    assert!(!worker.accept_join(ticket, "new room", 10, NOW.to_owned()));
    // A halted worker refuses joins until restart: seed a credential refusal
    // so the first dispatch halts it, then join again and check the halted
    // line carries the same channel and map-state fields.
    let (live, store, http, _) = fixture();
    store
        .claim_script
        .lock()
        .unwrap()
        .push_back(Err(StoreError::CredentialRefused));
    let mut halted_worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut halted_worker, MEMBER);
    dispatch(&mut halted_worker, 0).await;
    assert!(halted_worker.halted());
    let halted_ticket = halted_worker
        .live
        .voice_update(MEMBER + 1, Some(CREATOR), Some(false))
        .unwrap();
    assert!(!halted_worker.accept_join(halted_ticket, "new room", 10, NOW.to_owned()));
    // A name the automod filter blocks: refused as name_blocked with the
    // same channel and map-state fields.
    let (live, store, http, _) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http)
        .await
        .unwrap()
        .with_name_policy(Arc::new(fixture_policy(&["room"])));
    assert!(!join_as(&mut worker, MEMBER, "Ava"));
    let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    let refusals: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|event| event["fields"]["message"] == "voice creator join refused before queueing")
        .collect();
    assert_eq!(refusals.len(), 5, "{text}");
    let fields = &refusals[0]["fields"];
    assert_eq!(fields["voice_event"], "voice_join_received");
    assert_eq!(fields["reason"], "duplicate");
    assert_eq!(fields["channel"], CREATOR.to_string());
    assert_eq!(fields["ticket_generation"], ticket.generation);
    assert_eq!(fields["live_generation"], ticket.generation);
    assert_eq!(fields["known"], true);
    assert_eq!(fields["creators_len"], 1);
    assert_eq!(fields["halted"], false);
    assert_eq!(fields["ready"], true);
    let fields = &refusals[1]["fields"];
    assert_eq!(fields["voice_event"], "voice_join_received");
    assert_eq!(fields["reason"], "unknown_creator");
    assert_eq!(fields["channel"], "999");
    assert_eq!(fields["ticket_generation"], ticket.generation);
    assert_eq!(fields["live_generation"], ticket.generation);
    assert_eq!(fields["known"], false);
    assert_eq!(fields["creators_len"], 1);
    assert_eq!(fields["halted"], false);
    assert_eq!(fields["ready"], true);
    let fields = &refusals[2]["fields"];
    assert_eq!(fields["voice_event"], "voice_join_received");
    assert_eq!(fields["reason"], "stale_ticket");
    assert_eq!(fields["channel"], CREATOR.to_string());
    assert_eq!(fields["ticket_generation"], ticket.generation);
    assert_eq!(fields["live_generation"], ticket.generation);
    assert_eq!(fields["known"], true);
    assert_eq!(fields["creators_len"], 1);
    assert_eq!(fields["halted"], false);
    assert_eq!(fields["ready"], false);
    let fields = &refusals[3]["fields"];
    assert_eq!(fields["voice_event"], "voice_join_received");
    assert_eq!(fields["reason"], "halted");
    assert_eq!(fields["channel"], CREATOR.to_string());
    assert_eq!(fields["ticket_generation"], halted_ticket.generation);
    assert_eq!(fields["live_generation"], halted_ticket.generation);
    assert_eq!(fields["known"], true);
    assert_eq!(fields["creators_len"], 1);
    assert_eq!(fields["halted"], true);
    assert_eq!(fields["ready"], true);
    let fields = &refusals[4]["fields"];
    assert_eq!(fields["voice_event"], "voice_join_received");
    assert_eq!(fields["reason"], "name_blocked");
    assert_eq!(fields["channel"], CREATOR.to_string());
    assert_eq!(fields["ticket_generation"], ticket.generation);
    assert_eq!(fields["live_generation"], ticket.generation);
    assert_eq!(fields["known"], true);
    assert_eq!(fields["creators_len"], 1);
    assert_eq!(fields["halted"], false);
    assert_eq!(fields["ready"], true);
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
            "move:300:500",
            "create",
            "persist:501",
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

/// Worker-level pin for the legacy "only creator channels create rooms" rule
/// (the core `decide_room_join` test does not exercise `GuildRoomWorker`).
/// Mutation: drop the `creators.contains_key` gate from `accept_join`.
#[tokio::test]
async fn joining_a_channel_that_is_not_a_creator_never_creates_a_room() {
    const LOBBY: u64 = 777;
    let (live, store, http, trace) = fixture();
    live.publish(snapshot(&[LOBBY], vec![]));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    // The gateway still hands out a ticket for any voice channel; the worker
    // is what refuses it.
    let ticket = worker
        .live
        .voice_update(MEMBER, Some(LOBBY), Some(false))
        .expect("ticket for a joined channel");
    assert_eq!(ticket.creator_id, LOBBY);
    assert!(!worker.accept_join(ticket, "lobby room", 7, NOW.to_owned()));
    assert!(!worker.dispatch_one(0).await);
    assert!(worker.creations.is_empty());
    assert!(worker.tracked().is_empty());
    assert!(worker.failures().is_empty());
    assert!(trace.lock().unwrap().is_empty());
    // The configured creator still works for the same member afterwards.
    worker.live.voice_update(MEMBER, None, Some(false));
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 1).await;
    assert_eq!(*trace.lock().unwrap(), ["create", "persist:500"]);
}

/// A guild with no creator row configured creates nothing, even when a member
/// sits in a voice channel. Mutation: as above.
#[tokio::test]
async fn a_guild_without_creator_channels_never_creates_a_room() {
    let (live, store, http, trace) = fixture();
    store.creators.lock().unwrap().clear();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let ticket = worker
        .live
        .voice_update(MEMBER, Some(CREATOR), Some(false))
        .expect("ticket for a joined channel");
    assert!(!worker.accept_join(ticket, "new room", 7, NOW.to_owned()));
    assert!(!worker.dispatch_one(0).await);
    assert!(worker.tracked().is_empty());
    assert!(worker.failures().is_empty());
    assert!(trace.lock().unwrap().is_empty());
}

/// The overwrite/permission preflight (`prepare`) must refuse before the
/// channel POST, at worker level. Mutation: run `prepare()` after `create`
/// (the trace then contains `create` for the refused plan).
#[tokio::test]
async fn refused_creation_plan_records_a_failure_before_any_channel_is_created() {
    // Bot lacks Manage Channels on the creator channel.
    let (live, store, http, trace) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker
        .live
        .inner
        .write()
        .unwrap()
        .bot
        .as_mut()
        .unwrap()
        .roles = vec![role(permissions() & !Permissions::MANAGE_CHANNELS)];
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    assert_eq!(
        worker.failures().back(),
        Some(&LifecycleFailure::MissingPermission {
            write: RefusedWrite::Create,
            channel_id: CREATOR,
            findings: vec![PermissionFinding {
                permission: VoicePermission::ManageChannels,
                scope: VoicePermissionScope::Guild,
                category_id: None,
                channel_id: None,
            }],
        })
    );
    assert!(worker.creations.is_empty());
    assert!(worker.tracked().is_empty());
    assert!(!worker.dispatch_one(1).await);
    assert!(trace.lock().unwrap().is_empty());

    // A private-by-default creator without Manage Roles: `plan_room` refuses
    // instead of silently making a public room.
    let (live, store, http, trace) = fixture();
    store.creators.lock().unwrap()[0].private_default = true;
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker
        .live
        .inner
        .write()
        .unwrap()
        .bot
        .as_mut()
        .unwrap()
        .roles = vec![role(permissions() & !Permissions::MANAGE_ROLES)];
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    assert!(matches!(
        worker.failures().back(),
        Some(LifecycleFailure::MissingPermission {
            write: RefusedWrite::Create,
            channel_id: CREATOR,
            ..
        })
    ));
    assert!(worker.creations.is_empty());
    assert!(worker.tracked().is_empty());
    assert!(!worker.dispatch_one(1).await);
    assert!(trace.lock().unwrap().is_empty());
    assert!(worker.http.created_attributes.lock().unwrap().is_empty());

    // A creator-channel overwrite denying the bot Connect refuses the room at
    // the creator-channel guard, before any `create` call: `prepare` checks
    // the bot's channel permissions first, so the plan (and its grant) never
    // runs for this fixture. The guild grant holds Connect, but the
    // creator-channel overwrite removes it at the guard.
    let (live, store, http, trace) = fixture();
    live.upsert_channel(channel_with_overwrites(
        CREATOR,
        2,
        Some(CATEGORY),
        json!([bot_overwrite(Permissions::empty(), Permissions::CONNECT)]),
    ));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    assert!(matches!(
        worker.failures().back(),
        Some(LifecycleFailure::MissingPermission {
            write: RefusedWrite::Create,
            channel_id: CREATOR,
            findings,
        }) if findings.iter().any(|finding| finding.permission == VoicePermission::Connect
            && finding.scope == VoicePermissionScope::Channel
            && finding.channel_id == Some(CREATOR))
    ));
    assert!(worker.creations.is_empty());
    assert!(worker.tracked().is_empty());
    assert!(!worker.dispatch_one(1).await);
    assert!(trace.lock().unwrap().is_empty());
    assert!(worker.http.created_attributes.lock().unwrap().is_empty());
}

/// A failed compensation delete must keep the room (and its provenance)
/// tracked so a later sweep can remove it, never strand an untracked orphan.
/// Mutation: drop the room from `rooms` after the refused compensation delete.
#[tokio::test]
async fn failed_rollback_keeps_the_room_tracked_until_a_later_sweep_removes_it() {
    let (live, mut store, http, trace) = fixture();
    store.persist_error = Some(StoreError::Unavailable);
    http.delete_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::AccessDenied);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["create", "persist:500", "delete:500"]
    );
    // Retained with its creation provenance and flagged for compensation.
    let kept = worker.tracked().get(&500).expect("room stays tracked");
    assert_eq!(kept.owner_id, MEMBER);
    assert_eq!(kept.original_creator_id, MEMBER);
    assert_eq!(kept.creator_channel_id, CREATOR);
    assert!(worker.compensation.contains(&500));
    assert!(worker.deletes.contains(&500));
    // The refused delete is not retried against unchanged permissions.
    for time in [3000, 6000, 10000] {
        worker.reconcile();
        assert!(!worker.dispatch_one(time).await);
    }
    assert_eq!(
        *trace.lock().unwrap(),
        ["create", "persist:500", "delete:500"]
    );
    assert!(worker.tracked().contains_key(&500));
    // A permission refresh lets the next sweep retry the delete and forget it.
    worker.live.publish(snapshot(&[500], vec![]));
    worker.reconcile();
    dispatch(&mut worker, 10000).await;
    assert_eq!(
        *trace.lock().unwrap(),
        [
            "create",
            "persist:500",
            "delete:500",
            "delete:500",
            "forget:500"
        ]
    );
    assert!(worker.tracked().is_empty());
    assert!(!worker.compensation.contains(&500));
    assert!(!worker.deletes.contains(&500));
}

/// In-memory restart proof for "retain provenance after a failed rollback":
/// the failed persist writes no room row, but the `bound` witness is durable
/// state (the SQL `bound_create_channels` rows), so it crosses the simulated
/// restart like `rooms` does and `load` rediscovers the channel through
/// `orphaned_create_channels`. The disposable-database test
/// `crates/cutover/tests/voice_create_admission_store.rs::bound_channel_witness_survives_restart_and_holds_the_slot_until_settled`
/// proves the same path against a real pool.
#[tokio::test]
async fn a_restarted_worker_rediscovers_a_channel_whose_rollback_failed() {
    let (live, mut store, http, _) = fixture();
    store.persist_error = Some(StoreError::Unavailable);
    http.delete_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::AccessDenied);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    assert!(worker.tracked().contains_key(&500), "same-worker retention");

    // Restart: a fresh worker, store connection and live snapshot built only
    // from what the first worker's store persisted. Nothing in memory crosses.
    let (live, fresh_store, http, _) = fixture();
    live.publish(snapshot(&[500], vec![]));
    *fresh_store.rooms.lock().unwrap() = worker.store.rooms.lock().unwrap().clone();
    *fresh_store.bound.lock().unwrap() = worker.store.bound.lock().unwrap().clone();
    drop(worker);
    let restarted = GuildRoomWorker::load(live, fresh_store, http)
        .await
        .unwrap();
    assert!(
        restarted.tracked().contains_key(&500) || restarted.compensation.contains(&500),
        "the channel whose rollback failed must be rediscoverable after a restart"
    );
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

#[tokio::test(start_paused = true)]
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
    tokio::time::advance(EMPTY_ROOM_GRACE).await;
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

#[test]
fn join_reports_human_occupancy_at_join_before_insert() {
    // Seeding TEST: occupancy-at-join counts the humans already present in
    // the target channel, excluding the joiner.
    let live = LiveGuild::new(GUILD);
    live.publish(snapshot(
        &[500],
        vec![VoiceMember {
            member_id: MEMBER,
            channel_id: 500,
            bot: Some(false),
        }],
    ));
    let (_, occupancy) = live
        .voice_update_at_with_occupancy(301, Some(500), Some(false), 1_000)
        .expect("ticket");
    assert_eq!(occupancy, Some(1));
    let (_, occupancy) = live
        .voice_update_at_with_occupancy(302, Some(500), Some(false), 2_000)
        .expect("ticket");
    assert_eq!(occupancy, Some(2));
    // An empty creator sees zero.
    let (_, occupancy) = live
        .voice_update_at_with_occupancy(303, Some(CREATOR), Some(false), 3_000)
        .expect("ticket");
    assert_eq!(occupancy, Some(0));
    // Leaves and same-channel updates are not join samples.
    assert!(live
        .voice_update_at_with_occupancy(MEMBER, None, Some(false), 4_000)
        .is_none());
    assert!(live
        .voice_update_at_with_occupancy(301, Some(500), Some(false), 5_000)
        .is_none());
}

#[test]
fn mechanical_generator_and_move_echo_carry_no_occupancy_sample() {
    // Review finding: one room creation emitted two occupancy-0 samples
    // (the generator join and the bot's move echo into the new room),
    // skewing the 0 bucket. Both keep their ticket so room creation is
    // unaffected, but carry no sample.
    const GENERATOR: u64 = 900;
    let live = LiveGuild::new(GUILD);
    live.publish(snapshot(
        &[500],
        vec![VoiceMember {
            member_id: MEMBER,
            channel_id: 500,
            bot: Some(false),
        }],
    ));
    live.protect_creators([GENERATOR]);
    // Joining the creator-tracked generator keeps the ticket, samples nothing.
    let (_, sample) = live
        .voice_update_at_with_occupancy(301, Some(GENERATOR), Some(false), 1_000)
        .expect("generator join keeps its ticket");
    assert_eq!(sample, None);
    // The move echo out of the generator into the new room also samples
    // nothing, while an organic join into the same room still samples.
    let (_, sample) = live
        .voice_update_at_with_occupancy(301, Some(500), Some(false), 2_000)
        .expect("move echo keeps its ticket");
    assert_eq!(sample, None);
    let (_, sample) = live
        .voice_update_at_with_occupancy(302, Some(500), Some(false), 3_000)
        .expect("organic join keeps its sample");
    assert_eq!(sample, Some(2));
}

#[test]
fn protected_lobby_and_lounge_joins_still_sample_occupancy() {
    // Review finding: the mechanical filter tested the whole protected set,
    // so organic joins into the session lobby and permanent protected
    // channels (lounges) emitted no sample. Only creator/generator channels
    // are mechanical; lobby and lounge joins are organic choices and sample.
    const LOBBY: u64 = 600;
    const LOUNGE: u64 = 700;
    let live = LiveGuild::new(GUILD);
    live.publish(snapshot(
        &[500, LOBBY, LOUNGE],
        vec![VoiceMember {
            member_id: MEMBER,
            channel_id: LOUNGE,
            bot: Some(false),
        }],
    ));
    live.protect_channels([LOBBY, LOUNGE]);
    // Organic joins into protected-but-not-creator channels still sample.
    let (_, sample) = live
        .voice_update_at_with_occupancy(301, Some(LOUNGE), Some(false), 1_000)
        .expect("lounge join keeps its ticket");
    assert_eq!(sample, Some(1));
    let (_, sample) = live
        .voice_update_at_with_occupancy(302, Some(LOBBY), Some(false), 2_000)
        .expect("lobby join keeps its ticket");
    assert_eq!(sample, Some(0));
    // A move out of the lounge into a plain channel is organic too.
    let (_, sample) = live
        .voice_update_at_with_occupancy(MEMBER, Some(500), Some(false), 3_000)
        .expect("move out of lounge keeps its ticket");
    assert_eq!(sample, Some(0));
}

#[tokio::test]
async fn store_creators_reach_creator_channels_via_load() {
    // Review finding: five store-creator sites feed `creator_channels`;
    // reverting `GuildRoomWorker::load` to `protect_channels` reintroduced
    // the two spurious occupancy-0 samples with no test failing. The
    // fixture store holds CREATOR, so a join into it must carry no sample
    // while an organic join still samples.
    let (live, store, http, _) = fixture();
    let worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let (_, sample) = worker
        .live
        .voice_update_with_occupancy(301, Some(CREATOR), Some(false))
        .expect("store creator join keeps its ticket");
    assert_eq!(
        sample, None,
        "store creator join carries no occupancy sample"
    );
    let (_, sample) = worker
        .live
        .voice_update_with_occupancy(302, Some(500), Some(false))
        .expect("organic join keeps its sample");
    assert_eq!(sample, Some(0));
}

#[tokio::test]
async fn runtime_creator_added_and_updated_feed_creator_channels() {
    // Review finding: `creator_added` and `creator_updated` are the live
    // paths for newly saved creators; reverting either to
    // `protect_channels` left all lib tests green. Both update the actor's
    // `creator_channels` synchronously, so the seam check below is
    // deterministic (no global counter, no background worker timing).
    const ADDED: u64 = 901;
    const UPDATED: u64 = 902;
    let trace = Trace::default();
    let runtime = test_runtime(trace);
    assert!(runtime.publish_snapshot(GUILD, snapshot(&[], vec![])));
    let added = CreatorChannel::new(GUILD, ADDED);
    runtime.creator_added(&added, channel(ADDED, 2, Some(CATEGORY)));
    let updated = CreatorChannel::new(GUILD, UPDATED);
    runtime.creator_updated(&updated);
    let actor = runtime.live_actor(GUILD).expect("live actor");
    let (_, sample) = actor
        .live
        .voice_update_with_occupancy(301, Some(ADDED), Some(false))
        .expect("added creator join keeps its ticket");
    assert_eq!(
        sample, None,
        "added creator join carries no occupancy sample"
    );
    let (_, sample) = actor
        .live
        .voice_update_with_occupancy(302, Some(UPDATED), Some(false))
        .expect("updated creator join keeps its ticket");
    assert_eq!(
        sample, None,
        "updated creator join carries no occupancy sample"
    );
    let (_, sample) = actor
        .live
        .voice_update_with_occupancy(303, Some(500), Some(false))
        .expect("organic join keeps its sample");
    assert_eq!(sample, Some(0));
    runtime.remove_guild(GUILD);
}

#[tokio::test]
async fn actor_creator_added_command_feeds_creator_channels() {
    // Review finding: the `ActorCommand::CreatorAdded` worker branch is the
    // fourth store-creator path (the name-panel refresh shares this seam and
    // needs no separate test). `apply_command` runs it synchronously.
    const COMMANDED: u64 = 903;
    let (live, store, http, _) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    apply_command(
        &mut worker,
        ActorCommand::CreatorAdded(CreatorChannel::new(GUILD, COMMANDED)),
        0,
        &ActorInboxBackpressure::default(),
    );
    let (_, sample) = worker
        .live
        .voice_update_with_occupancy(301, Some(COMMANDED), Some(false))
        .expect("commanded creator join keeps its ticket");
    assert_eq!(
        sample, None,
        "commanded creator join carries no occupancy sample"
    );
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

/// `owned_room` on a caller-chosen guild id (see `fixture_for_guild`).
fn owned_room_for_guild(guild: u64) -> (LiveGuild, Store, Http, Trace) {
    let (live, store, http, trace) = fixture_for_guild(guild);
    store
        .rooms
        .lock()
        .unwrap()
        .insert(500, room_for_guild(guild, 500));
    live.publish(snapshot_for_guild(
        guild,
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

#[tokio::test(start_paused = true)]
async fn suspended_room_transfer_reports_finalizing_and_keeps_the_handoff_queued() {
    let (live, store, http, _) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    // Suspend the room so the queued handoff is never due: the staged reply
    // must answer from the pump deadline, not hang on the queue.
    assert!(worker.queue.suspend(GUILD, 500));
    let (reply, inbox) = oneshot::channel();
    apply_command(
        &mut worker,
        ActorCommand::Ownership {
            actor_id: MEMBER,
            is_admin: false,
            command: OwnershipCommand::Transfer { target_id: 301 },
            reply,
        },
        0,
        &ActorInboxBackpressure::default(),
    );
    worker.drive_staged_ownership(0).await;
    assert_eq!(inbox.await.unwrap(), OWNERSHIP_FINALIZING);
    assert!(
        worker.ownership_queued.contains(&500),
        "an unconfirmed handoff stays queued for retry"
    );
}

#[tokio::test(start_paused = true)]
async fn held_owner_grant_reports_finalizing_before_the_write_finishes() {
    let (live, store, mut http, trace) = owned_room();
    let gate = Arc::new(OverwriteGate::default());
    http.overwrites_gate = Some(gate.clone());
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let (reply, inbox) = oneshot::channel();
    apply_command(
        &mut worker,
        ActorCommand::Ownership {
            actor_id: MEMBER,
            is_admin: false,
            command: OwnershipCommand::Transfer { target_id: 301 },
            reply,
        },
        0,
        &ActorInboxBackpressure::default(),
    );
    let observe = async {
        gate.sent.notified().await;
        let response = tokio::time::timeout(Duration::from_millis(2_000), inbox).await;
        // Release even on failure so the queued action can finish, rather
        // than hiding a dropped in-flight dispatch behind test shutdown.
        gate.release.notify_one();
        let text = response
            .expect("reply before the held grant finishes")
            .unwrap();
        assert!(
            text.contains("Recorded") && text.contains("finalized"),
            "{text}"
        );
        assert!(!text.contains("Transferred"), "{text}");
        assert!(
            !trace
                .lock()
                .unwrap()
                .iter()
                .any(|call| call.starts_with("update_ownership:")),
            "the reply must precede the held grant's durable completion"
        );
    };
    tokio::join!(worker.drive_staged_ownership(0), observe);
    assert!(
        trace
            .lock()
            .unwrap()
            .contains(&"update_ownership:500:301".to_owned()),
        "the in-flight handoff must finish after its reply, not be cancelled"
    );
}

#[tokio::test(start_paused = true)]
async fn held_owner_grant_with_slow_store_still_commits_after_finalizing() {
    let (live, store, mut http, trace) = owned_room();
    let gate = Arc::new(OverwriteGate::default());
    http.overwrites_gate = Some(gate.clone());
    // A real Postgres write pends on its first poll: the expired reply
    // budget must not cancel the in-flight commit once nobody listens.
    store.update_ownership_yields.store(1, Ordering::Relaxed);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let (reply, inbox) = oneshot::channel();
    apply_command(
        &mut worker,
        ActorCommand::Ownership {
            actor_id: MEMBER,
            is_admin: false,
            command: OwnershipCommand::Transfer { target_id: 301 },
            reply,
        },
        0,
        &ActorInboxBackpressure::default(),
    );
    let observe = async {
        gate.sent.notified().await;
        let response = tokio::time::timeout(Duration::from_millis(2_000), inbox).await;
        // Release even on failure so the queued action can finish, rather
        // than hiding a dropped in-flight dispatch behind test shutdown.
        gate.release.notify_one();
        let text = response
            .expect("reply before the held grant finishes")
            .unwrap();
        assert!(
            text.contains("Recorded") && text.contains("finalized"),
            "{text}"
        );
        assert!(!text.contains("Transferred"), "{text}");
    };
    tokio::join!(worker.drive_staged_ownership(0), observe);
    assert!(
        trace
            .lock()
            .unwrap()
            .contains(&"update_ownership:500:301".to_owned()),
        "a Pending-first store write must run to completion after its reply, not be cancelled by the expired budget"
    );
    assert!(
        !worker.ownership_queued.contains(&500),
        "the committed handoff leaves the retry queue instead of failing back into it"
    );
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
    let (live, store, mut http, trace) = owned_room();
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
    let echo_live = live.clone();
    http.after_overwrite = Some(Arc::new(move |channel_id, member_id, overwrite| {
        let mut echoed = echo_live
            .read_state()
            .channels
            .get(&channel_id)
            .cloned()
            .expect("echoed channel exists");
        let overwrites = echoed.permission_overwrites.as_mut().unwrap();
        overwrites.retain(|existing| {
            !(existing.kind == PermissionOverwriteType::Member && existing.id.get() == member_id)
        });
        if let Some(overwrite) = overwrite {
            overwrites.push(overwrite);
        }
        echo_live.upsert_channel(echoed);
    }));
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
        [
            format!("overwrite:500:{MEMBER}:Member"),
            "overwrite:500:301:Member".to_owned(),
            "update_ownership:500:301".to_owned(),
        ]
    );
}

#[test]
fn owner_overwrite_delta_orders_trimmed_grants_before_the_incoming_owner() {
    let mut former_owner = owner_overwrite(MEMBER);
    former_owner.allow |= Permissions::EMBED_LINKS;
    let mut incoming_owner = PermissionOverwrite {
        id: Id::new(301),
        kind: PermissionOverwriteType::Member,
        allow: Permissions::SEND_MESSAGES,
        deny: Permissions::empty(),
    };
    let before = vec![incoming_owner, former_owner];
    former_owner.allow = Permissions::EMBED_LINKS;
    incoming_owner.allow |= Permissions::from_bits_retain(OWNER_ALLOW_BITS);
    let after = vec![incoming_owner, former_owner];

    let (deletes, puts) = owner_overwrite_delta(&before, &after, 301);

    assert!(deletes.is_empty());
    assert_eq!(
        puts.iter()
            .map(|overwrite| overwrite.id.get())
            .collect::<Vec<_>>(),
        [MEMBER, 301],
        "revoke the former owner's bits before granting the incoming owner"
    );
}

#[tokio::test]
async fn denied_incoming_grant_after_successful_revoke_remains_owed_without_access() {
    let (live, store, http, _) = owned_room();
    let mut room_channel = channel(500, 2, Some(CATEGORY));
    // The incoming owner joined through an approved join request, so they
    // already hold a Connect-only overwrite (Connect is an owner bit).
    // Partial bits must not count as a completed grant.
    let connect_only_joiner = PermissionOverwrite {
        id: Id::new(301),
        kind: PermissionOverwriteType::Member,
        allow: Permissions::CONNECT,
        deny: Permissions::empty(),
    };
    room_channel.permission_overwrites = Some(vec![owner_overwrite(MEMBER), connect_only_joiner]);
    live.upsert_channel(room_channel);
    http.put_overwrite_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::AccessDenied);
    let worker = GuildRoomWorker::load(live.clone(), store, http)
        .await
        .unwrap();

    assert_eq!(
        worker.rewrite_owner_grant(500, &[MEMBER], 301).await,
        Err(RoomHttpError::AccessDenied)
    );
    let access = live.inner.read().unwrap().bot.clone().unwrap();
    let mut without_manage_roles = access.clone();
    for role in &mut without_manage_roles.roles {
        role.permissions &= !Permissions::MANAGE_ROLES;
    }
    live.refresh_bot(without_manage_roles);

    assert_eq!(
        worker.rewrite_owner_grant(500, &[MEMBER], 301).await,
        Err(RoomHttpError::AccessDenied),
        "the denied grant must not let the ownership journal close"
    );
    assert!(worker.grants_owed.lock().unwrap().contains(&500));
    let state = live.inner.read().unwrap();
    let overwrites = state.channels[&500].permission_overwrites.as_ref().unwrap();
    let owner_mask = Permissions::from_bits_retain(OWNER_ALLOW_BITS);
    assert!(!overwrites
        .iter()
        .any(|overwrite| overwrite.id.get() == MEMBER
            && overwrite.allow.contains(owner_mask & !overwrite.deny)));
    // The incoming owner still holds only Connect: partial bits are not a grant.
    assert!(!overwrites.iter().any(|overwrite| overwrite.id.get() == 301
        && overwrite.allow.contains(owner_mask & !overwrite.deny)));
    assert!(overwrites.iter().any(
        |overwrite| overwrite.id.get() == 301 && overwrite.allow.contains(Permissions::CONNECT)
    ));
}

#[tokio::test]
async fn transient_grant_failure_after_revoke_remains_owed_without_access() {
    let (live, store, http, _) = owned_room();
    let mut room_channel = channel(500, 2, Some(CATEGORY));
    // Same Connect-only joiner setup as the 403 case: partial bits must not
    // count as a completed grant.
    let connect_only_joiner = PermissionOverwrite {
        id: Id::new(301),
        kind: PermissionOverwriteType::Member,
        allow: Permissions::CONNECT,
        deny: Permissions::empty(),
    };
    room_channel.permission_overwrites = Some(vec![owner_overwrite(MEMBER), connect_only_joiner]);
    live.upsert_channel(room_channel);
    // The former owner's revoke succeeds; the incoming owner's PUT hits a
    // transient failure, not a 403.
    http.put_overwrite_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::UnknownOutcome);
    let worker = GuildRoomWorker::load(live.clone(), store, http)
        .await
        .unwrap();

    assert_eq!(
        worker.rewrite_owner_grant(500, &[MEMBER], 301).await,
        Err(RoomHttpError::UnknownOutcome)
    );
    let access = live.inner.read().unwrap().bot.clone().unwrap();
    let mut without_manage_roles = access.clone();
    for role in &mut without_manage_roles.roles {
        role.permissions &= !Permissions::MANAGE_ROLES;
    }
    live.refresh_bot(without_manage_roles);

    assert_eq!(
        worker.rewrite_owner_grant(500, &[MEMBER], 301).await,
        Err(RoomHttpError::AccessDenied),
        "a half-applied handoff must not close the journal without the grant"
    );
    assert!(worker.grants_owed.lock().unwrap().contains(&500));
    let state = live.inner.read().unwrap();
    let overwrites = state.channels[&500].permission_overwrites.as_ref().unwrap();
    let owner_mask = Permissions::from_bits_retain(OWNER_ALLOW_BITS);
    assert!(!overwrites
        .iter()
        .any(|overwrite| overwrite.id.get() == MEMBER
            && overwrite.allow.contains(owner_mask & !overwrite.deny)));
    assert!(!overwrites.iter().any(|overwrite| overwrite.id.get() == 301
        && overwrite.allow.contains(owner_mask & !overwrite.deny)));
    assert!(overwrites.iter().any(
        |overwrite| overwrite.id.get() == 301 && overwrite.allow.contains(Permissions::CONNECT)
    ));
}

#[tokio::test]
async fn denied_handoff_falls_back_to_revoke_only_then_retries_the_grant() {
    let (live, store, http, trace) = owned_room();
    let mut room_channel = channel(500, 2, Some(CATEGORY));
    room_channel.permission_overwrites = Some(vec![owner_overwrite(MEMBER)]);
    live.upsert_channel(room_channel);
    // The combined revoke+grant PATCH is refused; the revoke-only fallback
    // must still clear the former owner's bits and defer the grant.
    http.overwrite_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::AccessDenied);
    let worker = GuildRoomWorker::load(live.clone(), store, http)
        .await
        .unwrap();
    assert_eq!(
        worker.rewrite_owner_grant(500, &[MEMBER], 301).await,
        Err(RoomHttpError::AccessDenied)
    );
    let owner_mask = Permissions::from_bits_retain(OWNER_ALLOW_BITS);
    {
        let state = live.inner.read().unwrap();
        let overwrites = state.channels[&500].permission_overwrites.as_ref().unwrap();
        assert!(
            !overwrites
                .iter()
                .any(|o| o.id.get() == MEMBER && o.allow.intersects(owner_mask)),
            "former owner bits must be revoked even when the grant is denied"
        );
        assert!(
            !overwrites
                .iter()
                .any(|o| o.id.get() == 301 && o.allow.intersects(owner_mask)),
            "new owner grant must wait for the retry"
        );
    }
    assert_eq!(
        trace
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.as_str() == format!("delete_overwrite:500:{MEMBER}"))
            .count(),
        2,
        "combined attempt plus the revoke-only fallback"
    );
    // The retry converges: the already-revoked former owner stays revoked
    // and the new owner is granted.
    worker
        .rewrite_owner_grant(500, &[MEMBER], 301)
        .await
        .unwrap();
    let state = live.inner.read().unwrap();
    let overwrites = state.channels[&500].permission_overwrites.as_ref().unwrap();
    assert!(!overwrites
        .iter()
        .any(|o| o.id.get() == MEMBER && o.allow.intersects(owner_mask)));
    assert!(overwrites.contains(&owner_overwrite(301)));
}

#[tokio::test]
async fn revoked_fallback_keeps_handoff_pending_until_access_returns() {
    let (live, store, http, trace) = owned_room();
    let mut room_channel = channel(500, 2, Some(CATEGORY));
    room_channel.permission_overwrites = Some(vec![owner_overwrite(MEMBER)]);
    live.upsert_channel(room_channel);
    // The combined revoke+grant PATCH is refused; the revoke-only fallback
    // clears the former owner and defers the new-owner grant.
    http.overwrite_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::AccessDenied);
    let mut worker = GuildRoomWorker::load(live.clone(), store, http)
        .await
        .unwrap();
    worker.apply_ownership(MEMBER, false, OwnershipCommand::Transfer { target_id: 301 });
    dispatch(&mut worker, 0).await;
    let owner_mask = Permissions::from_bits_retain(OWNER_ALLOW_BITS);
    {
        let state = live.inner.read().unwrap();
        let overwrites = state.channels[&500].permission_overwrites.as_ref().unwrap();
        assert!(
            !overwrites
                .iter()
                .any(|o| o.id.get() == MEMBER && o.allow.intersects(owner_mask)),
            "fallback must revoke the former owner"
        );
        assert!(
            !overwrites
                .iter()
                .any(|o| o.id.get() == 301 && o.allow.intersects(owner_mask)),
            "new owner grant waits for the retry"
        );
    }
    assert!(worker.store.owner_pending.lock().unwrap().contains(&500));
    assert_eq!(worker.store.rooms.lock().unwrap()[&500].owner_id, MEMBER);
    // Lose Manage Roles before the retry: the handoff must stay pending even
    // though no stranded grant remains, or the new owner would never be
    // granted after access returns.
    let full_access = live.inner.read().unwrap().bot.clone().unwrap();
    let mut reduced = full_access.clone();
    for role in &mut reduced.roles {
        role.permissions &= !Permissions::MANAGE_ROLES;
    }
    live.refresh_bot(reduced);
    dispatch(&mut worker, 100_000).await;
    assert!(
        worker.store.owner_pending.lock().unwrap().contains(&500),
        "revoked-but-ungranted handoff stays pending without access"
    );
    assert_eq!(worker.store.rooms.lock().unwrap()[&500].owner_id, MEMBER);
    assert!(
        !trace
            .lock()
            .unwrap()
            .iter()
            .any(|call| call == "update_ownership:500:301"),
        "no commit while the grant is still owed"
    );
    // Restore access: the retry converges on the new owner.
    live.refresh_bot(full_access);
    dispatch(&mut worker, 200_000).await;
    assert!(!worker.store.owner_pending.lock().unwrap().contains(&500));
    assert_eq!(worker.store.rooms.lock().unwrap()[&500].owner_id, 301);
    let state = live.inner.read().unwrap();
    let overwrites = state.channels[&500].permission_overwrites.as_ref().unwrap();
    assert!(!overwrites
        .iter()
        .any(|o| o.id.get() == MEMBER && o.allow.intersects(owner_mask)));
    assert!(overwrites.contains(&owner_overwrite(301)));
}

#[tokio::test]
async fn category_synced_transfer_commits_without_minting_overwrites_while_access_is_absent() {
    // Rooms created without Manage Roles are category-synced and carry no
    // bot-issued owner overwrite ("Do not invent one"). A transfer there
    // while the bot still lacks Manage Roles must commit the owner change
    // with no Discord write — the opposite of a revoked-but-ungranted room,
    // which stays pending until the owed grant lands.
    let (live, store, http, trace) = owned_room();
    let mut room_channel = channel(500, 2, Some(CATEGORY));
    room_channel.permission_overwrites = Some(vec![]);
    live.upsert_channel(room_channel);
    let full_access = live.inner.read().unwrap().bot.clone().unwrap();
    let mut reduced = full_access.clone();
    for role in &mut reduced.roles {
        role.permissions &= !Permissions::MANAGE_ROLES;
    }
    live.refresh_bot(reduced);
    let mut worker = GuildRoomWorker::load(live.clone(), store, http)
        .await
        .unwrap();
    worker.apply_ownership(MEMBER, false, OwnershipCommand::Transfer { target_id: 301 });
    dispatch(&mut worker, 0).await;
    assert!(
        trace
            .lock()
            .unwrap()
            .contains(&"update_ownership:500:301".to_owned()),
        "category-synced ownership change commits without access"
    );
    assert!(
        !trace
            .lock()
            .unwrap()
            .iter()
            .any(|call| call == "overwrites:500"),
        "no overwrite is minted for a category-synced room"
    );
    assert!(!worker.store.owner_pending.lock().unwrap().contains(&500));
    assert_eq!(worker.store.rooms.lock().unwrap()[&500].owner_id, 301);
}

#[tokio::test]
async fn fallback_patch_propagates_rate_limit_and_unauthorized() {
    let rate_limited = RoomHttpError::RateLimited {
        retry_after_ms: 100,
        global: false,
    };
    for (fallback_error, expected) in [
        (rate_limited, rate_limited),
        (RoomHttpError::Unauthorized, RoomHttpError::Unauthorized),
        (RoomHttpError::Cancelled, RoomHttpError::AccessDenied),
        (RoomHttpError::NotFound, RoomHttpError::AccessDenied),
    ] {
        let (live, store, http, _) = owned_room();
        let mut room_channel = channel(500, 2, Some(CATEGORY));
        room_channel.permission_overwrites = Some(vec![owner_overwrite(MEMBER)]);
        live.upsert_channel(room_channel);
        http.overwrite_errors
            .lock()
            .unwrap()
            .push_back(RoomHttpError::AccessDenied);
        http.overwrite_errors
            .lock()
            .unwrap()
            .push_back(fallback_error);
        let worker = GuildRoomWorker::load(live.clone(), store, http)
            .await
            .unwrap();
        assert_eq!(
            worker.rewrite_owner_grant(500, &[MEMBER], 301).await,
            Err(expected),
            "fallback {fallback_error:?}"
        );
    }
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
        // The writer handle is shared with the watchdog sibling, so replace
        // the gate through the sole owner instead of a direct field write.
        Arc::get_mut(&mut worker.http)
            .expect("sole writer handle")
            .overwrites_gate = None;
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
async fn owner_repair_is_dropped_when_the_room_row_is_deleted_out_of_band() {
    let (live, store, http, trace) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    // The worker still tracks the room, but its database row was deleted
    // out-of-band, so journal preparation reports a conflict.
    worker.store.rooms.lock().unwrap().remove(&500);
    worker.apply_ownership(MEMBER, false, OwnershipCommand::Transfer { target_id: 301 });
    dispatch(&mut worker, 0).await;
    assert!(
        trace.lock().unwrap().is_empty(),
        "no Discord grant or ownership commit without a journal row"
    );
    assert!(
        !worker.ownership_queued.contains(&500),
        "the repair is dropped, not re-queued"
    );
    assert!(
        !worker.ownership_repairs.contains_key(&500),
        "the repair is dropped, not re-armed after the cooldown"
    );
    assert!(
        !worker.grants_owed.lock().unwrap().contains(&500),
        "no owed grant outlives its repair"
    );
    // Past the repair cooldown the dropped repair must not come back.
    worker.enqueue_owner_repairs(OWNER_REPAIR_COOLDOWN_MS + 1);
    assert!(!worker.ownership_queued.contains(&500));
    assert!(!worker.ownership_repairs.contains_key(&500));
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
    http.overwrite_errors
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
    // Past the 429 deadline with slack: the wait starts at the real
    // response time (`elapsed_ms`), so the exact boundary races loaded
    // runners. Core queue tests pin the exact deadline arithmetic.
    dispatch(&mut worker, 1_100).await;
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

#[tokio::test(start_paused = true)]
async fn not_ready_and_disconnected_snapshots_never_allow_destructive_reconciliation() {
    let (_, store, http, trace) = fixture();
    store.rooms.lock().unwrap().insert(500, room(500));
    let live = LiveGuild::new(GUILD);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.reconcile();
    assert!(!worker.dispatch_one(0).await);
    worker.live.publish(snapshot(&[500], vec![]));
    tokio::time::advance(EMPTY_ROOM_GRACE).await;
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

#[tokio::test(start_paused = true)]
async fn occupants_arriving_during_delete_backoff_cancel_the_write() {
    let (live, store, mut http, trace) = fixture();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    let shared = live.clone();
    http.before_delete = Some(Arc::new(move || {
        shared.voice_update(MEMBER, Some(500), Some(false));
    }));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    tokio::time::advance(EMPTY_ROOM_GRACE).await;
    worker.reconcile();
    dispatch(&mut worker, 0).await;
    assert!(trace.lock().unwrap().is_empty());
    assert_eq!(worker.tracked().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn delete_403_suspends_without_a_retry_storm_and_refresh_resumes() {
    let (live, store, http, trace) = fixture();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    http.delete_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::AccessDenied);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    tokio::time::advance(EMPTY_ROOM_GRACE).await;
    worker.reconcile();
    dispatch(&mut worker, 0).await;
    for time in [3000, 6000, 10000] {
        worker.reconcile();
        assert!(!worker.dispatch_one(time).await);
    }
    assert_eq!(*trace.lock().unwrap(), ["delete:500"]);
    worker.live.publish(snapshot(&[500], vec![]));
    tokio::time::advance(EMPTY_ROOM_GRACE).await;
    worker.reconcile();
    dispatch(&mut worker, 10000).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["delete:500", "delete:500", "forget:500"]
    );
}

/// Prime one eligible delete before reconciling both rooms, so each sweep
/// visitation order is deterministic despite the tracked-room HashMap.
async fn two_empty_rooms(
    first: u64,
    failures: VecDeque<RoomHttpError>,
) -> (GuildRoomWorker<Store, Http>, Trace) {
    let (live, store, http, trace) = fixture();
    for id in [500, 501] {
        store.rooms.lock().unwrap().insert(id, room(id));
        live.upsert_channel(channel(id, 2, Some(CATEGORY)));
    }
    http.delete_channel_errors
        .lock()
        .unwrap()
        .insert(500, failures);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    // These scenarios predate the ordinary empty-room grace and assert the
    // delete is queued at once: shorten the grace to zero via the
    // fixture-only seam so reconcile behaves as on main. Production keeps
    // the 60 s default.
    worker.live.set_empty_grace(Duration::ZERO);
    worker.queue_delete(first, false);
    worker.reconcile();
    assert_eq!(worker.queue.pending_counts(GUILD), (2, 0));
    (worker, trace)
}

#[tokio::test]
async fn transient_delete_isolated_in_either_sweep_order_and_retried_after_backoff() {
    for first in [500, 501] {
        let (mut worker, trace) =
            two_empty_rooms(first, VecDeque::from([RoomHttpError::UnknownOutcome])).await;
        dispatch(&mut worker, 0).await;
        dispatch(&mut worker, 1).await;
        let expected = if first == 500 {
            vec!["delete:500", "delete:501", "forget:501"]
        } else {
            vec!["delete:501", "forget:501", "delete:500"]
        };
        assert_eq!(*trace.lock().unwrap(), expected, "first room {first}");
        assert_eq!(worker.tracked().get(&500), Some(&room(500)));
        assert!(!worker.tracked().contains_key(&501));
        assert_eq!(
            worker.store.rooms.lock().unwrap().get(&500),
            Some(&room(500))
        );
        assert!(!worker.store.rooms.lock().unwrap().contains_key(&501));
        assert!(worker
            .live
            .inner
            .read()
            .unwrap()
            .channels
            .contains_key(&500));
        worker.reconcile();
        assert_eq!(worker.queue.pending_counts(GUILD), (1, 0));
        for time in [2, 100, 1_999] {
            assert!(
                !worker.dispatch_one(time).await,
                "retry must wait for its backoff"
            );
        }
        assert_eq!(*trace.lock().unwrap(), expected);
        dispatch(&mut worker, 3_000).await;
        assert!(worker.tracked().is_empty());
        assert!(worker.store.rooms.lock().unwrap().is_empty());
        assert!(worker.queue.failed().is_empty());
        assert_eq!(
            &trace.lock().unwrap()[expected.len()..],
            ["delete:500", "forget:500"]
        );
    }
}

#[tokio::test]
async fn transient_delete_dead_letter_retains_provenance_without_requeue_storms() {
    let (mut worker, trace) = two_empty_rooms(
        500,
        VecDeque::from(vec![
            RoomHttpError::UnknownOutcome;
            QUEUE_MAX_ATTEMPTS as usize
        ]),
    )
    .await;
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    let mut previous = 0;
    for attempt in 1..QUEUE_MAX_ATTEMPTS {
        worker.reconcile();
        let before_due = previous + two_bot_core::voice_rooms::fail_backoff_ms(attempt) - 1;
        assert!(!worker.dispatch_one(before_due).await);
        let due = u64::from(attempt) * 61_000;
        dispatch(&mut worker, due).await;
        previous = due;
    }
    let failed = worker.queue.failed();
    assert_eq!(failed.len(), 1);
    assert_eq!(
        failed[0].action.action,
        RoomAction::DeleteRoom { channel_id: 500 }
    );
    assert_eq!(failed[0].action.attempts, QUEUE_MAX_ATTEMPTS);
    assert_eq!(worker.tracked().get(&500), Some(&room(500)));
    assert_eq!(
        worker.store.rooms.lock().unwrap().get(&500),
        Some(&room(500))
    );
    assert!(worker
        .live
        .inner
        .read()
        .unwrap()
        .channels
        .contains_key(&500));
    for time in [previous + 1, previous + 60_000, previous + 600_000] {
        worker.reconcile();
        assert!(!worker.dispatch_one(time).await);
    }
    assert_eq!(
        trace
            .lock()
            .unwrap()
            .iter()
            .filter(|call| *call == "delete:500")
            .count(),
        QUEUE_MAX_ATTEMPTS as usize
    );
    assert!(!trace.lock().unwrap().contains(&"forget:500".to_owned()));
    // A new eligible room still progresses after the earlier failure is terminal.
    worker.store.rooms.lock().unwrap().insert(502, room(502));
    worker.rooms.insert(502, room(502));
    worker.live.upsert_channel(channel(502, 2, Some(CATEGORY)));
    worker.reconcile();
    dispatch(&mut worker, previous + 600_001).await;
    assert!(!worker.tracked().contains_key(&502));
    assert!(trace.lock().unwrap().contains(&"forget:502".to_owned()));
    assert_eq!(worker.queue.failed(), failed);
}

#[tokio::test]
async fn refused_delete_isolated_in_either_sweep_order_without_retry_storms() {
    for first in [500, 501] {
        let (mut worker, trace) =
            two_empty_rooms(first, VecDeque::from([RoomHttpError::AccessDenied])).await;
        dispatch(&mut worker, 0).await;
        dispatch(&mut worker, 1).await;
        assert_eq!(worker.tracked().get(&500), Some(&room(500)));
        assert!(!worker.tracked().contains_key(&501));
        assert!(worker.queue.is_suspended(GUILD, 500));
        for time in [3_000, 60_000, 600_000] {
            worker.reconcile();
            assert!(!worker.dispatch_one(time).await);
        }
        let calls = trace.lock().unwrap();
        assert_eq!(calls.iter().filter(|call| *call == "delete:500").count(), 1);
        assert!(calls.contains(&"forget:501".to_owned()));
        assert!(!calls.contains(&"forget:500".to_owned()));
    }
}

#[tokio::test]
async fn transient_delete_retry_rechecks_occupancy_permissions_and_readiness() {
    for changed in ["occupancy", "permissions", "readiness"] {
        let (mut worker, trace) =
            two_empty_rooms(500, VecDeque::from([RoomHttpError::UnknownOutcome])).await;
        dispatch(&mut worker, 0).await;
        dispatch(&mut worker, 1).await;
        match changed {
            "occupancy" => {
                worker.live.voice_update(MEMBER, Some(500), Some(false));
            }
            "permissions" => {
                worker
                    .live
                    .inner
                    .write()
                    .unwrap()
                    .bot
                    .as_mut()
                    .unwrap()
                    .roles = vec![role(Permissions::VIEW_CHANNEL)];
            }
            "readiness" => worker.live.disconnect(),
            _ => unreachable!(),
        }
        // No reconcile is needed to invalidate the previously queued write.
        // Dispatch and its last-moment guard must recheck current live facts.
        worker.dispatch_one(3_000).await;
        assert_eq!(
            *trace.lock().unwrap(),
            ["delete:500", "delete:501", "forget:501"]
        );
        assert_eq!(worker.tracked().get(&500), Some(&room(500)));
        assert_eq!(
            worker.store.rooms.lock().unwrap().get(&500),
            Some(&room(500))
        );
    }
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
    // Past the 429 deadline with slack: the wait starts at the real
    // response time (`elapsed_ms`), so the exact boundary races loaded
    // runners. Core queue tests pin the exact deadline arithmetic.
    dispatch(&mut worker, 2600).await;
    dispatch(&mut worker, 2601).await;
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
async fn admission_claims_before_discord_and_binds_the_created_room() {
    let (live, mut store, http, trace) = fixture();
    store.trace_claims = true;
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["claim", "create", "persist:500", "settle", "move:300:500"]
    );
    let claims = worker.store.claims.lock().unwrap().clone();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].0, MEMBER);
    // The claim is stamped with the wall clock, not the actor's monotonic one.
    let wall = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    assert!((wall - claims[0].1).abs() < 60);
    assert_eq!(
        *worker.store.settled.lock().unwrap(),
        [("r1".to_owned(), Some(500))]
    );
    assert!(worker.failures().is_empty());
}

#[tokio::test]
async fn admission_refusal_records_the_stable_code_and_never_calls_discord() {
    for (reason, code, text) in [
        (
            RefusalReason::UserCap,
            "user_cap",
            "You already have a temporary voice channel.",
        ),
        (
            RefusalReason::GuildCap,
            "guild_cap",
            "This server has reached its temporary voice channel limit. Try again shortly.",
        ),
        (
            RefusalReason::Cooldown,
            "cooldown",
            "Please wait 30 seconds between creating channels.",
        ),
        (
            RefusalReason::UserBurst,
            "user_burst",
            "Temporary voice rate limit: 3 creates per 60 seconds. Please try again shortly.",
        ),
        (
            RefusalReason::GuildBurst,
            "guild_burst",
            "This server's temporary voice rate limit is 10 creates per 60 seconds. Please try again shortly.",
        ),
    ] {
        let (live, store, http, trace) = fixture();
        store
            .claim_script
            .lock()
            .unwrap()
            .push_back(Ok(CreateClaim::Refused(reason)));
        let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
        // The legacy limits, so every refusal text (incl. the 30 s cooldown)
        // stays pinned; the temp-voice default has no cooldown.
        worker.set_admission_config(CreateAdmissionConfig::default());
        join(&mut worker, MEMBER);
        dispatch(&mut worker, 0).await;
        assert!(trace.lock().unwrap().is_empty(), "{code}: no Discord call");
        assert!(worker.tracked().is_empty());
        assert!(worker.store.settled.lock().unwrap().is_empty());
        let failure = worker.failures().back().cloned();
        assert_eq!(
            failure,
            Some(LifecycleFailure::CreateRefused {
                creator_id: CREATOR,
                reason,
                message: text.to_owned(),
            })
        );
        let line = failure_line(&failure.unwrap());
        assert!(line.contains(&format!("({code})")) && line.contains(text), "{line}");
        // The refused create is gone: nothing retries it.
        assert!(!worker.dispatch_one(60_000).await);
        assert_eq!(worker.store.claims.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn admission_store_failure_never_calls_discord_and_credential_refusal_halts() {
    for (error, halted) in [
        (StoreError::Unavailable, false),
        (StoreError::CredentialRefused, true),
    ] {
        let (live, store, http, trace) = fixture();
        store.claim_script.lock().unwrap().push_back(Err(error));
        let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
        join(&mut worker, MEMBER);
        dispatch(&mut worker, 0).await;
        assert!(trace.lock().unwrap().is_empty());
        assert_eq!(worker.halted(), halted);
        assert_eq!(
            worker.failures().back(),
            Some(&LifecycleFailure::Persistence {
                channel_id: None,
                error
            })
        );
        assert!(worker.store.settled.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn a_429_requeue_keeps_one_claim_and_settles_it_once() {
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
    assert!(worker.store.settled.lock().unwrap().is_empty());
    // Past the 429 deadline with slack (`elapsed_ms` starts the wait at
    // the real response time, so the exact boundary races loaded runners).
    dispatch(&mut worker, 2600).await;
    assert_eq!(*trace.lock().unwrap(), ["create", "create", "persist:500"]);
    assert_eq!(worker.store.claims.lock().unwrap().len(), 1);
    assert_eq!(
        *worker.store.settled.lock().unwrap(),
        [("r1".to_owned(), Some(500))]
    );
}

#[tokio::test]
async fn unknown_create_outcome_keeps_capacity_and_never_reposts() {
    let (live, store, http, trace) = fixture();
    http.create_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::UnknownOutcome);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    assert!(worker.store.settled.lock().unwrap().is_empty());
    assert!(!worker.dispatch_one(86_400_000).await);
    assert_eq!(*trace.lock().unwrap(), ["create"]);
    assert_eq!(worker.store.claims.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn delayed_compensation_keeps_capacity_until_the_delete_succeeds() {
    let (live, mut store, http, trace) = fixture();
    store.persist_error = Some(StoreError::Unavailable);
    http.delete_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::RateLimited {
            retry_after_ms: 600_000,
            global: false,
        });
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    assert!(worker.store.settled.lock().unwrap().is_empty());
    assert_eq!(
        worker
            .compensation_reservations
            .get(&500)
            .map(String::as_str),
        Some("r1")
    );
    dispatch(&mut worker, 1).await;
    assert!(worker.store.settled.lock().unwrap().is_empty());
    // The 429 retry is scheduled from the 429 response time (dispatch start
    // plus wall-clock dispatch cost), so the lower bound pins
    // not_before >= now + retry_after while the retry itself is awaited with
    // wall-clock margin: asserting the exact 600_001 instant flakes whenever
    // dispatch costs >= 1 ms under load.
    assert!(!worker.dispatch_one(300_001).await);
    dispatch(&mut worker, 610_000).await;
    assert_eq!(
        *worker.store.settled.lock().unwrap(),
        [("r1".to_owned(), None)]
    );
    assert!(worker.compensation_reservations.is_empty());
    assert_eq!(
        *trace.lock().unwrap(),
        [
            "create",
            "persist:500",
            "delete:500",
            "delete:500",
            "forget:500"
        ]
    );
}

#[tokio::test]
async fn denied_or_occupied_compensation_never_refunds_capacity() {
    for occupied in [false, true] {
        let (live, mut store, http, _) = fixture();
        store.persist_error = Some(StoreError::Unavailable);
        if !occupied {
            http.delete_errors
                .lock()
                .unwrap()
                .push_back(RoomHttpError::AccessDenied);
        }
        let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
        join(&mut worker, MEMBER);
        dispatch(&mut worker, 0).await;
        if occupied {
            worker.live.voice_update(MEMBER + 1, Some(500), Some(false));
        }
        dispatch(&mut worker, 1).await;
        assert!(worker.store.settled.lock().unwrap().is_empty());
        assert_eq!(
            worker
                .compensation_reservations
                .get(&500)
                .map(String::as_str),
            Some("r1")
        );
    }
}

#[tokio::test]
async fn created_channel_is_bound_to_its_claim_before_the_room_is_persisted() {
    let (live, store, http, _) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    assert_eq!(
        *worker.store.bound.lock().unwrap(),
        [("r1".to_owned(), 500)]
    );
    assert_eq!(
        *worker.store.settled.lock().unwrap(),
        [("r1".to_owned(), Some(500))]
    );
    assert!(worker.compensation_reservations.is_empty());
}

#[tokio::test]
async fn bind_failure_compensates_the_channel_and_keeps_the_hold_until_the_delete() {
    for error in [StoreError::Unavailable, StoreError::CredentialRefused] {
        let (live, store, http, trace) = fixture();
        store.bind_errors.lock().unwrap().push_back(error);
        let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
        join(&mut worker, MEMBER);
        dispatch(&mut worker, 0).await;
        // The channel exists but is unwitnessed: persist never ran, the hold is
        // kept and the exact channel is queued for compensation.
        assert!(worker.store.settled.lock().unwrap().is_empty());
        assert_eq!(
            worker
                .compensation_reservations
                .get(&500)
                .map(String::as_str),
            Some("r1")
        );
        assert_eq!(
            worker.failures().back(),
            Some(&LifecycleFailure::Persistence {
                channel_id: Some(500),
                error,
            })
        );
        assert_eq!(worker.halted(), error == StoreError::CredentialRefused);
        assert_eq!(*trace.lock().unwrap(), ["create"]);
    }
}

#[tokio::test]
async fn restart_after_a_refused_rollback_rediscovers_the_bound_channel_and_holds_the_slot() {
    let (live, mut store, http, trace) = fixture();
    store.persist_error = Some(StoreError::Unavailable);
    http.delete_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::AccessDenied);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    assert!(worker.store.settled.lock().unwrap().is_empty());
    assert_eq!(
        *trace.lock().unwrap(),
        ["create", "persist:500", "delete:500"]
    );

    // A fresh worker over the same durable state knows the channel and still
    // holds its cap slot: nothing was settled and no room row exists.
    let mut store = std::mem::replace(&mut worker.store, Store::new(trace.clone()));
    drop(worker);
    store.persist_error = None;
    let live = LiveGuild::new(GUILD);
    live.publish(snapshot(&[500], vec![]));
    let mut worker = GuildRoomWorker::load(live, store, Http::new(trace.clone()))
        .await
        .unwrap();
    assert_eq!(
        worker
            .compensation_reservations
            .get(&500)
            .map(String::as_str),
        Some("r1")
    );
    assert!(worker.compensation.contains(&500));
    assert!(worker.tracked().is_empty());
    assert!(worker.store.settled.lock().unwrap().is_empty());

    // The first authoritative reconcile deletes it; only then is the slot freed.
    worker.reconcile();
    dispatch(&mut worker, 2).await;
    assert_eq!(
        *trace.lock().unwrap(),
        [
            "create",
            "persist:500",
            "delete:500",
            "delete:500",
            "forget:500"
        ]
    );
    assert_eq!(
        *worker.store.settled.lock().unwrap(),
        [("r1".to_owned(), None)]
    );
    assert!(worker.compensation_reservations.is_empty());
}

#[tokio::test]
async fn restart_confirms_absence_of_a_bound_channel_before_freeing_the_slot() {
    let trace = Trace::default();
    let store = Store::new(trace.clone());
    store.bound.lock().unwrap().push(("r9".to_owned(), 500));
    // Not ready: no authoritative snapshot has been published yet.
    let live = LiveGuild::new(GUILD);
    let mut worker = GuildRoomWorker::load(live, store, Http::new(trace.clone()))
        .await
        .unwrap();
    worker.reconcile();
    assert!(!worker.dispatch_one(0).await);
    assert!(worker.store.settled.lock().unwrap().is_empty());
    // The snapshot lacks the channel: Discord no longer has it, so no delete is
    // sent and the hold is released.
    worker.live.publish(snapshot(&[], vec![]));
    worker.reconcile();
    dispatch(&mut worker, 1).await;
    assert!(trace
        .lock()
        .unwrap()
        .iter()
        .all(|call| call != "delete:500"));
    assert_eq!(
        *worker.store.settled.lock().unwrap(),
        [("r9".to_owned(), None)]
    );
    assert!(worker.compensation_reservations.is_empty());
}

#[tokio::test]
async fn compensation_settlement_errors_are_reported_and_credentials_halt() {
    for error in [StoreError::Unavailable, StoreError::CredentialRefused] {
        let (live, mut store, http, trace) = fixture();
        store.persist_error = Some(StoreError::Unavailable);
        let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
        join(&mut worker, MEMBER);
        dispatch(&mut worker, 0).await;
        worker.store.settle_errors.lock().unwrap().push_back(error);
        dispatch(&mut worker, 1).await;
        assert!(worker.store.settled.lock().unwrap().is_empty());
        assert_eq!(
            worker.failures().back(),
            Some(&LifecycleFailure::Persistence {
                channel_id: Some(500),
                error,
            })
        );
        assert_eq!(worker.halted(), error == StoreError::CredentialRefused);
        let before = trace.lock().unwrap().clone();
        if worker.halted() {
            // A halted worker refuses new joins and writes nothing further.
            let ticket = worker
                .live
                .voice_update(MEMBER + 1, Some(CREATOR), Some(false))
                .unwrap();
            assert!(!worker.accept_join(ticket, "new room", 7, NOW.to_owned()));
            worker.reconcile();
            assert!(!worker.dispatch_one(600_000).await);
            assert_eq!(*trace.lock().unwrap(), before);
        } else {
            dispatch(&mut worker, 3000).await;
            assert_eq!(
                *worker.store.settled.lock().unwrap(),
                [("r1".to_owned(), None)]
            );
            assert_eq!(
                *trace.lock().unwrap(),
                ["create", "persist:500", "delete:500", "forget:500"]
            );
        }
    }
}

#[tokio::test]
async fn atomic_create_settlement_refusal_halts_without_publishing_a_partial_room() {
    let (live, store, http, trace) = fixture();
    store
        .settle_errors
        .lock()
        .unwrap()
        .push_back(StoreError::CredentialRefused);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    join(&mut worker, MEMBER + 1);
    dispatch(&mut worker, 0).await;
    assert!(worker.halted());
    assert!(worker.store.rooms.lock().unwrap().is_empty());
    assert!(worker.store.settled.lock().unwrap().is_empty());
    assert_eq!(
        worker.failures().back(),
        Some(&LifecycleFailure::Persistence {
            channel_id: Some(500),
            error: StoreError::CredentialRefused,
        })
    );
    assert!(!worker.dispatch_one(600_000).await);
    assert_eq!(*trace.lock().unwrap(), ["create", "persist:500"]);
}

#[tokio::test]
async fn long_repeated_429s_reuse_one_durable_capacity_hold() {
    let (live, store, http, _) = fixture();
    for _ in 0..2 {
        http.create_errors
            .lock()
            .unwrap()
            .push_back(RoomHttpError::RateLimited {
                retry_after_ms: 350_000,
                global: false,
            });
    }
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    // Past each 429 deadline with slack (`elapsed_ms` starts the wait at
    // the real response time, so the exact boundary races loaded runners).
    dispatch(&mut worker, 351_000).await;
    assert!(worker.store.settled.lock().unwrap().is_empty());
    dispatch(&mut worker, 702_000).await;
    assert_eq!(worker.store.claims.lock().unwrap().len(), 1);
    assert_eq!(
        *worker.store.settled.lock().unwrap(),
        [("r1".to_owned(), Some(500))]
    );
}

#[tokio::test]
async fn a_429_requeue_with_a_stale_join_releases_its_confirmed_absent_claim() {
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
    worker.live.voice_update(MEMBER, None, Some(false));
    // Past the 429 deadline with slack (`elapsed_ms` starts the wait at
    // the real response time, so the exact boundary races loaded runners).
    dispatch(&mut worker, 2600).await;
    assert_eq!(*trace.lock().unwrap(), ["create"]);
    assert_eq!(
        *worker.store.settled.lock().unwrap(),
        [("r1".to_owned(), None)]
    );
    assert_eq!(worker.store.claims.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cancelled_create_settlement_refusal_is_reported_and_halts_on_credentials() {
    for error in [StoreError::Unavailable, StoreError::CredentialRefused] {
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
        dispatch(&mut worker, 0).await;
        worker.live.voice_update(MEMBER, None, Some(false));
        worker.store.settle_errors.lock().unwrap().push_back(error);
        // Past the 429 deadline with slack (`elapsed_ms` starts the wait
        // at the real response time, so the exact boundary races runners).
        dispatch(&mut worker, 2500).await;
        assert_eq!(
            worker.failures().back(),
            Some(&LifecycleFailure::Persistence {
                channel_id: None,
                error
            })
        );
        assert_eq!(worker.halted(), error == StoreError::CredentialRefused);
        assert!(worker.store.settled.lock().unwrap().is_empty());
        assert_eq!(*trace.lock().unwrap(), ["create"]);
        assert!(!worker.dispatch_one(600_000).await);
    }
}

#[tokio::test(start_paused = true)]
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
    tokio::time::advance(EMPTY_ROOM_GRACE).await;
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

#[tokio::test(start_paused = true)]
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
    tokio::time::advance(EMPTY_ROOM_GRACE).await;
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

#[tokio::test(start_paused = true)]
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
    tokio::time::advance(EMPTY_ROOM_GRACE).await;
    worker.reconcile();
    dispatch(&mut worker, 2).await;
    // Coalescing replaces the retry's stale name once its budget is due.
    assert!(!worker.dispatch_one(300000).await);
    // Leave room for the synchronous request's measured wall-clock duration.
    dispatch(&mut worker, 601000).await;
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
    {
        let created = worker.http.created_attributes.lock().unwrap();
        assert!(created[0]
            .overwrites
            .iter()
            .any(|overwrite| overwrite.id.get() == MEMBER));
        // Deferred respacing: the create carries no position when the slot
        // needs a gap (here, above the creator); the post-move repair places
        // the room.
        assert_eq!(created[0].position, None);
    }
    for now in 1..4 {
        worker.dispatch_one(now).await;
    }
    // The room takes the slot above its creator; the creator itself rides
    // the suffix below it, keeping its relative order.
    assert_eq!(
        worker.http.reorders.lock().unwrap().as_slice(),
        [[(200, 31), (500, 15)]],
    );
    let live = worker.live.read_state();
    assert_eq!(live.channels[&500].position, Some(15));
    assert_eq!(live.channels[&200].position, Some(31));
    drop(live);
    assert_eq!(worker.queue.pending_counts(GUILD), (0, 0));
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
fn possessive_matches_auto_voice_for_names_ending_in_s() {
    // Auto-Voice renders a literal `'s` (`@@owner@@'s room`) whatever the
    // name ends with, so "PisnRzrs" is "PisnRzrs's room" there too.
    let policy = AutomodPolicy::default();
    for (display, expected) in [("PisnRzrs", "PisnRzrs's room"), ("JAMES", "JAMES's room")] {
        assert_eq!(
            resolve_room_name(display, &policy, &name_context())
                .unwrap()
                .name,
            expected
        );
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
    // The Setup arm stores `StoreError::to_string()`; pin each Display
    // variant's exact line so a rewording fails here, not silently.
    for (error, display) in [
        (StoreError::Unavailable, "the store is unavailable"),
        (
            StoreError::CredentialRefused,
            "the store refused the bot credential",
        ),
        (StoreError::Conflict, "the store reported a conflict"),
    ] {
        assert_eq!(error.to_string(), display);
        let panel = setup_panel(&SetupSummary {
            guild_id: GUILD,
            creators: vec![],
            tracked_rooms: 0,
            failures: vec![],
            halted: false,
            store_error: Some(error.to_string()),
        });
        assert_eq!(
            panel.description,
            format!(
                "Voice rooms are running.\n\
                 Could not load creator channels ({display}); the worker state below is current.\n\
                 Tracked rooms: 0.\n\
                 No recent failures."
            ),
            "store-error line drifted for {display:?}"
        );
    }
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
            "public",
            "limit",
            "unlimit"
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
async fn voice_frame_counts_occupancy_without_per_event_log() {
    // Seeding TEST AC(1): organic VOICE_STATE_UPDATE joins increment the
    // occupancy counter. VOICE_STATE_UPDATE is hot per
    // docs/log-volume-guard.md, so no per-event log line is emitted.
    // Joins still dispatch their ticket; leaves dispatch none. The counter
    // reads below are the pin: replacing the `voice_join_occupancy` call in
    // the frame handler with `let _ = occupancy` fails the growth assert.
    const GENERATOR: u64 = 900;
    let (bytes, _subscriber) = capture_join_logs();
    let trace = Trace::default();
    let runtime = VoiceRuntime::new(
        move || (Store::new(trace.clone()), Http::new(trace.clone())),
        Duration::from_millis(10),
        true,
    )
    .with_creator_channels(HashSet::from([GENERATOR]));
    assert!(runtime.publish_snapshot(
        GUILD,
        snapshot(
            &[500],
            vec![VoiceMember {
                member_id: MEMBER,
                channel_id: 500,
                bot: Some(false),
            }],
        ),
    ));
    let series = "two_bot_voice_join_occupancy_total";
    let before = global_series(series);
    assert!(runtime.voice_frame(GUILD, 301, Some(500), Some(false), "x".to_owned()));
    assert!(runtime.voice_frame(GUILD, 302, Some(500), Some(false), "x".to_owned()));
    assert!(runtime.voice_frame(GUILD, MEMBER, None, Some(false), "x".to_owned()));
    // Global counters are shared with parallel tests, so growth asserts a
    // lower bound (see `global_series` convention); our own two organic joins
    // guarantee it.
    assert!(
        global_series(series) >= before + 2,
        "two organic joins increment the occupancy counter"
    );
    // The mechanical generator join keeps its ticket but emits no sample,
    // pinned at the actor seam: the global counter is shared with parallel
    // tests, so exact-equality reads flake (14/60 full-lib runs at 32
    // threads). The seam check is per-actor and deterministic.
    let actor = runtime.live_actor(GUILD).expect("live actor");
    let (_, sample) = actor
        .live
        .voice_update_with_occupancy(401, Some(GENERATOR), Some(false))
        .expect("generator join keeps its ticket");
    assert_eq!(sample, None, "generator join carries no occupancy sample");
    // The runtime path keeps the ticket too, so room creation is unaffected.
    assert!(runtime.voice_frame(GUILD, 402, Some(GENERATOR), Some(false), "x".to_owned()));
    let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    let joins: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|event| event["fields"]["voice_event"] == "voice_join_occupancy")
        .collect();
    assert!(joins.is_empty(), "{text}");
    runtime.remove_guild(GUILD);
}

#[tokio::test]
async fn runtime_remove_guild_drops_actor() {
    let trace = Trace::default();
    let runtime = test_runtime(trace);
    assert!(runtime.publish_snapshot(GUILD, snapshot(&[], vec![])));
    runtime.remove_guild(GUILD);
    assert!(!runtime.voice_frame(GUILD, MEMBER, Some(CREATOR), Some(false), "x".to_owned()));
}

#[tokio::test(start_paused = true)]
async fn creator_join_moves_without_waiting_for_the_actor_tick() {
    let trace = Trace::default();
    let runtime = VoiceRuntime::new(
        {
            let trace = trace.clone();
            move || (Store::new(trace.clone()), Http::new(trace.clone()))
        },
        Duration::from_millis(250),
        true,
    );
    assert!(runtime.publish_snapshot(GUILD, snapshot(&[], vec![])));
    // Let the initial interval tick and bootstrap finish before the join.
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    let received = tokio::time::Instant::now();
    for member in [MEMBER, MEMBER + 1] {
        assert!(runtime.voice_frame(GUILD, member, Some(CREATOR), Some(false), "room".to_owned()));
    }
    // Yield, never sleep/advance: Tokio cannot auto-advance the paused clock.
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert_eq!(received.elapsed(), Duration::ZERO);
    // The presence hook records each join ahead of room handling.
    assert_eq!(
        *trace.lock().unwrap(),
        [
            "evjoin:200:300",
            "evjoin:200:301",
            "create",
            "persist:500",
            "move:300:500",
            "create",
            "persist:501",
            "move:301:501",
        ]
    );
    runtime.remove_guild(GUILD);
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
    // The join is recorded twice: once by the buffered transition, once by
    // the bootstrap occupant flush (the store join is idempotent, so the
    // second write is a no-op in production).
    assert_eq!(
        *trace.lock().unwrap(),
        [
            "evjoin:200:300",
            "evjoin:200:300",
            "create",
            "persist:500",
            "move:300:500"
        ]
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

/// Wait for any trace entry starting with `prefix`. The actor path runs the
/// deferred post-move respace concurrently with ownership commands, so a
/// Join created after the respace carries the respaced room position
/// (`Some(16)` with `POSITION_STEP = 16`) while one created before it
/// carries none. Pinning the exact position makes the test racy; the name
/// and category prefix is the stable assertion.
async fn wait_trace_prefix(trace: &Trace, prefix: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if trace
            .lock()
            .unwrap()
            .iter()
            .any(|value| value.starts_with(prefix))
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "lifecycle trace deadline waiting for prefix {prefix}: {:?}",
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

#[tokio::test]
async fn registered_voice_identity_drives_parsing_and_refused_ids_have_no_effects() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let identities = two_bot_discord::command_identity::CommandIdentities::default();
    let mut commands = two_bot_discord::publish_commands(&[
        CommandDefinition::new("setup", "Synthetic setup."),
        CommandDefinition::new("help", "Synthetic help."),
        CommandDefinition::new("create", "Synthetic create."),
    ]);
    for (index, command) in commands.iter_mut().enumerate() {
        command.id = Some(Id::new(9000 + index as u64));
        command.application_id = Some(Id::new(1));
        command.guild_id = Some(Id::new(GUILD));
    }
    identities.replace_guild(1, GUILD, &commands).unwrap();
    runtime.set_command_identities(identities.clone());
    let mut data = command_data("not-setup", Vec::new());
    data.id = Id::new(9000);
    data.guild_id = Some(Id::new(GUILD));
    assert_eq!(
        runtime.parse_voice_command(&voice_interaction(Some(data), None, true)),
        Some(VoiceCommand::Setup)
    );
    for id in [9001, 9999] {
        let mut data = command_data("create", vec![command_option("name", "lobby")]);
        data.id = Id::new(id);
        data.guild_id = Some(Id::new(GUILD));
        let interaction = voice_interaction(Some(data), Some(Permissions::MANAGE_CHANNELS), true);
        assert_eq!(runtime.parse_voice_command(&interaction), None);
        let seen = Arc::new(Mutex::new(None::<InteractionResponse>));
        let writer = seen.clone();
        let owned = handle_voice_interaction(&runtime, &interaction, move |response| {
            *writer.lock().unwrap() = Some(response);
            async {}
        })
        .await;
        assert!(!owned);
        assert!(seen.lock().unwrap().is_none());
        assert!(
            trace.lock().unwrap().is_empty(),
            "refusal makes no store/Discord call"
        );
    }
    identities.replace_guild(1, GUILD, &[]).unwrap();
    let mut data = command_data("setup", Vec::new());
    data.id = Id::new(9000);
    data.guild_id = Some(Id::new(GUILD));
    assert_eq!(
        runtime.parse_voice_command(&voice_interaction(Some(data), None, true)),
        None
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
async fn guild_timeout_active_removes_role_derived_administrative_authority() {
    use twilight_model::util::Timestamp;

    let runtime = test_runtime(Trace::default());
    let mut guild = command_snapshot();
    let readable_admin = Permissions::VIEW_CHANNEL
        | Permissions::READ_MESSAGE_HISTORY
        | Permissions::MANAGE_CHANNELS
        | Permissions::MANAGE_GUILD;
    guild.bot.roles.push(role_with(12_345, readable_admin));
    assert!(runtime.publish_snapshot(GUILD, guild));
    let now = Timestamp::parse(NOW).unwrap().as_micros();
    for (role_id, expected) in [
        (
            10_000 + Permissions::MANAGE_CHANNELS.bits(),
            Permissions::empty(),
        ),
        (
            10_000 + Permissions::MANAGE_GUILD.bits(),
            Permissions::empty(),
        ),
        (
            10_000 + (Permissions::MANAGE_CHANNELS | Permissions::MANAGE_GUILD).bits(),
            Permissions::empty(),
        ),
        (
            12_345,
            Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY,
        ),
    ] {
        let mut interaction = voice_interaction(
            Some(command_data("access", vec![])),
            Some(Permissions::ADMINISTRATOR),
            true,
        );
        let member = interaction.member.as_mut().unwrap();
        member.roles = vec![Id::new(role_id)];
        member.communication_disabled_until = Some(Timestamp::from_micros(now + 1).unwrap());
        let permissions = runtime.guild_permissions_at(&interaction, i128::from(now));
        assert_eq!(permissions, Some(expected), "role {role_id}");
        assert!(!is_voice_admin(permissions));
        assert!(!may_manage_server(permissions));
    }
}

#[tokio::test]
async fn guild_timeout_absent_expired_and_boundary_leave_authority_unchanged() {
    use twilight_model::util::Timestamp;

    let runtime = test_runtime(Trace::default());
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    let now = Timestamp::parse(NOW).unwrap().as_micros();
    let expected = Permissions::MANAGE_CHANNELS | Permissions::MANAGE_GUILD;
    for until in [None, Some(now - 1), Some(now)] {
        let mut interaction = voice_interaction(Some(command_data("access", vec![])), None, true);
        let member = interaction.member.as_mut().unwrap();
        member.roles = vec![Id::new(10_000 + expected.bits())];
        member.communication_disabled_until = until.map(|at| Timestamp::from_micros(at).unwrap());
        let permissions = runtime.guild_permissions_at(&interaction, i128::from(now));
        assert_eq!(permissions, Some(expected));
        assert!(is_voice_admin(permissions));
        assert!(may_manage_server(permissions));
    }
}

#[tokio::test]
async fn guild_timeout_preserves_discord_owner_and_administrator_exemptions() {
    use twilight_model::util::Timestamp;

    let runtime = test_runtime(Trace::default());
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    let now = Timestamp::parse(NOW).unwrap().as_micros();
    for (id, roles) in [
        (
            MEMBER,
            vec![Id::new(10_000 + Permissions::ADMINISTRATOR.bits())],
        ),
        (998, vec![]),
    ] {
        let mut interaction = voice_interaction_as(Some(command_data("access", vec![])), None, id);
        let member = interaction.member.as_mut().unwrap();
        member.roles = roles;
        member.communication_disabled_until = Some(Timestamp::from_micros(now + 1).unwrap());
        let permissions = runtime.guild_permissions_at(&interaction, i128::from(now));
        assert_eq!(permissions, Some(Permissions::all()));
        assert!(is_voice_admin(permissions));
        assert!(may_manage_server(permissions));
    }
}

#[tokio::test]
async fn guild_timeout_does_not_supply_missing_authority_facts() {
    use twilight_model::util::Timestamp;

    let runtime = test_runtime(Trace::default());
    let now = Timestamp::parse(NOW).unwrap().as_micros();
    let mut interaction = voice_interaction(
        Some(command_data("access", vec![])),
        Some(Permissions::ADMINISTRATOR),
        true,
    );
    interaction
        .member
        .as_mut()
        .unwrap()
        .communication_disabled_until = Some(Timestamp::from_micros(now + 1).unwrap());
    assert_eq!(
        runtime.guild_permissions_at(&interaction, i128::from(now)),
        None
    );
    let mut guild = command_snapshot();
    guild
        .bot
        .roles
        .retain(|role| role.id.get() != 10_000 + Permissions::ADMINISTRATOR.bits());
    assert!(runtime.publish_snapshot(GUILD, guild));
    assert_eq!(
        runtime.guild_permissions_at(&interaction, i128::from(now)),
        None
    );
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    let mut missing_user = interaction.clone();
    missing_user.member.as_mut().unwrap().user = None;
    assert_eq!(
        runtime.guild_permissions_at(&missing_user, i128::from(now)),
        None
    );
    let mut missing_member = interaction.clone();
    missing_member.member = None;
    assert_eq!(
        runtime.guild_permissions_at(&missing_member, i128::from(now)),
        None
    );
    let mut missing_guild = interaction.clone();
    missing_guild.guild_id = None;
    assert_eq!(
        runtime.guild_permissions_at(&missing_guild, i128::from(now)),
        None
    );
    runtime.live_actor(GUILD).unwrap().live.disconnect();
    assert_eq!(
        runtime.guild_permissions_at(&interaction, i128::from(now)),
        None
    );
}

#[tokio::test]
async fn guild_timeout_admin_commands_are_refused_before_effects() {
    use twilight_model::util::Timestamp;

    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    let until = time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000
        + 28 * 24 * 60 * 60 * 1_000_000;
    for (name, permissions, refusal) in [
        (
            "access",
            Permissions::MANAGE_CHANNELS,
            "You need Manage Channels",
        ),
        (
            "export",
            Permissions::MANAGE_GUILD,
            "You need Manage Server",
        ),
    ] {
        let options = if name == "access" {
            vec![sub_option("role", vec![role_option("role", 9)])]
        } else {
            vec![]
        };
        let mut interaction =
            voice_interaction(Some(command_data(name, options)), Some(permissions), true);
        interaction
            .member
            .as_mut()
            .unwrap()
            .communication_disabled_until =
            Some(Timestamp::from_micros(i64::try_from(until).unwrap()).unwrap());
        let inventory = config_inventory();
        let (_, response) = handle_import_capture(&runtime, &interaction, Some(&inventory)).await;
        let text = response_text(&response.unwrap());
        assert!(text.contains(refusal), "{name}: {text}");
    }
    assert!(trace.lock().unwrap().is_empty());
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

// Exact-text pins for reclaim/transfer outcome copy. Each `ownership_refusal`
// arm plus the reclaim/transfer confirmation lines is asserted with exact
// equality through the real reply path. Reachable cases go through
// `handle_capture` (the full interaction reply); succession cases use
// `worker.apply_ownership` (the exact string the handler replies with);
// defensive arms that consistent live state cannot produce are pinned via the
// refusal helper directly and noted as such. A one-word swap in any pinned
// literal must fail its test; scratch-mutant runs are recorded on the card.

#[tokio::test]
async fn ownership_copy_reclaim_outside_voice_exact() {
    let runtime = ownership_room_runtime(Trace::default()).await;
    let interaction = voice_interaction_as(Some(command_data("reclaim", Vec::new())), None, 302);
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    assert_eq!(
        response_text(response.as_ref().expect("reply")),
        "You need to be in a voice room to use /reclaim."
    );
}

#[tokio::test]
async fn ownership_copy_transfer_outside_voice_exact() {
    let runtime = ownership_room_runtime(Trace::default()).await;
    let interaction = voice_interaction_as(
        Some(command_data("transfer", vec![user_option("member", 301)])),
        None,
        302,
    );
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    assert_eq!(
        response_text(response.as_ref().expect("reply")),
        "You need to be in a voice room to use /transfer."
    );
}

#[tokio::test]
async fn ownership_copy_transfer_missing_member_exact() {
    // A transfer with no member option parses to target 0, which the handler
    // refuses before touching the worker (not the InvalidMemberId arm).
    let runtime = ownership_room_runtime(Trace::default()).await;
    let interaction =
        voice_interaction_as(Some(command_data("transfer", Vec::new())), None, MEMBER);
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    assert_eq!(
        response_text(response.as_ref().expect("reply")),
        "Choose a member in the room to transfer to."
    );
}

#[tokio::test]
async fn ownership_copy_reclaim_already_owner_exact() {
    let runtime = ownership_room_runtime(Trace::default()).await;
    let interaction = voice_interaction_as(Some(command_data("reclaim", Vec::new())), None, MEMBER);
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    assert_eq!(
        response_text(response.as_ref().expect("reply")),
        "You're already the owner of this room."
    );
}

#[tokio::test]
async fn ownership_copy_transfer_already_with_exact() {
    let runtime = ownership_room_runtime(Trace::default()).await;
    let interaction = voice_interaction_as(
        Some(command_data(
            "transfer",
            vec![user_option("member", MEMBER)],
        )),
        None,
        MEMBER,
    );
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    assert_eq!(
        response_text(response.as_ref().expect("reply")),
        format!("Ownership is already with <@{MEMBER}>.")
    );
}

#[tokio::test]
async fn ownership_copy_transfer_success_exact() {
    let trace = Trace::default();
    let runtime = ownership_room_runtime(trace.clone()).await;
    let interaction = voice_interaction_as(
        Some(command_data("transfer", vec![user_option("member", 301)])),
        None,
        MEMBER,
    );
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    assert_eq!(
        response_text(response.as_ref().expect("reply")),
        "Transferred ownership of this room to <@301>."
    );
    wait_trace(&trace, "update_ownership:500:301").await;
}

#[tokio::test]
async fn ownership_copy_reclaim_creator_return_exact() {
    let (live, store, http, _) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker
        .live
        .voice_update_at(MEMBER, None, Some(false), 1_000);
    worker.reconcile();
    assert_eq!(worker.tracked().get(&500).expect("room").owner_id, 301);
    worker
        .live
        .voice_update_at(MEMBER, Some(500), Some(false), 2_000);
    assert_eq!(
        worker.apply_ownership(MEMBER, false, OwnershipCommand::Reclaim),
        "You're the owner of this room again."
    );
}

#[tokio::test]
async fn ownership_copy_reclaim_owner_gone_exact() {
    let (live, store, http, _) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker
        .live
        .voice_update_at(MEMBER, None, Some(false), 1_000);
    assert_eq!(
        worker.apply_ownership(301, false, OwnershipCommand::Reclaim),
        "The room's owner is gone, so it's yours now."
    );
}

#[tokio::test]
async fn ownership_copy_refusal_not_original_creator_exact() {
    let trace = Trace::default();
    let runtime = ownership_room_runtime(trace.clone()).await;
    let interaction = voice_interaction_as(Some(command_data("reclaim", Vec::new())), None, 301);
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    assert_eq!(
        response_text(response.as_ref().expect("reply")),
        "Only the original creator can reclaim this room while its owner is still here."
    );
    assert!(!trace
        .lock()
        .unwrap()
        .iter()
        .any(|entry| entry.starts_with("update_ownership")));
}

#[tokio::test]
async fn ownership_copy_refusal_not_owner_exact() {
    let trace = Trace::default();
    let runtime = ownership_room_runtime(trace.clone()).await;
    let interaction = voice_interaction_as(
        Some(command_data(
            "transfer",
            vec![user_option("member", MEMBER)],
        )),
        None,
        301,
    );
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    assert_eq!(
        response_text(response.as_ref().expect("reply")),
        "Only the room owner or an admin can transfer this room."
    );
    assert!(!trace
        .lock()
        .unwrap()
        .iter()
        .any(|entry| entry.starts_with("update_ownership")));
}

#[tokio::test]
async fn ownership_copy_refusal_target_not_in_room_exact() {
    let trace = Trace::default();
    let runtime = ownership_room_runtime(trace.clone()).await;
    let interaction = voice_interaction_as(
        Some(command_data("transfer", vec![user_option("member", 302)])),
        None,
        MEMBER,
    );
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    assert_eq!(
        response_text(response.as_ref().expect("reply")),
        "The transfer recipient must be in the room."
    );
    assert!(!trace
        .lock()
        .unwrap()
        .iter()
        .any(|entry| entry.starts_with("update_ownership")));
}

#[tokio::test]
async fn ownership_copy_refusal_bot_actor_exact() {
    let trace = Trace::default();
    let runtime = ownership_room_runtime(trace.clone()).await;
    assert!(runtime.voice_frame(GUILD, 302, Some(500), Some(true), "bot".to_owned()));
    let interaction = voice_interaction_as(Some(command_data("reclaim", Vec::new())), None, 302);
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    assert_eq!(
        response_text(response.as_ref().expect("reply")),
        "Bots can't own temporary rooms."
    );
    assert!(!trace
        .lock()
        .unwrap()
        .iter()
        .any(|entry| entry.starts_with("update_ownership")));
}

#[tokio::test]
async fn ownership_copy_refusal_bot_target_exact() {
    let trace = Trace::default();
    let runtime = ownership_room_runtime(trace.clone()).await;
    assert!(runtime.voice_frame(GUILD, 302, Some(500), Some(true), "bot".to_owned()));
    let interaction = voice_interaction_as(
        Some(command_data("transfer", vec![user_option("member", 302)])),
        None,
        MEMBER,
    );
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    assert_eq!(
        response_text(response.as_ref().expect("reply")),
        "Bots can't own temporary rooms."
    );
    assert!(!trace
        .lock()
        .unwrap()
        .iter()
        .any(|entry| entry.starts_with("update_ownership")));
}

#[tokio::test]
async fn ownership_copy_corrupt_target_zero_exact() {
    // `Transfer { target_id: 0 }` reaches the worker only via the direct call
    // (the interaction handler refuses a missing member first). The worker
    // maps the resulting `InvalidMemberId` through the shared corrupt-data arm.
    let (live, store, http, trace) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    assert_eq!(
        worker.apply_ownership(MEMBER, false, OwnershipCommand::Transfer { target_id: 0 }),
        "Something's off with this room's membership data — leave and rejoin, then try again."
    );
    assert!(!trace
        .lock()
        .unwrap()
        .iter()
        .any(|call| call.starts_with("update_ownership")));
}

#[test]
fn ownership_copy_refusal_actor_not_in_room_exact() {
    // `ActorNotInRoom` is defensive: the worker resolves the room from the
    // actor's own voice state under one lock, so consistent live state always
    // yields the "voice room" or "temporary room" replies before `decide`
    // could report it. Pin the literal so copy drift still fails.
    assert_eq!(
        super::ownership_refusal(OwnershipError::ActorNotInRoom),
        "You need to be in the room to use this command."
    );
}

#[test]
fn ownership_copy_refusal_corrupt_variants_exact() {
    // All four corrupt-snapshot variants share one arm and one literal.
    const EXPECTED: &str =
        "Something's off with this room's membership data — leave and rejoin, then try again.";
    assert_eq!(
        super::ownership_refusal(OwnershipError::InvalidMemberId),
        EXPECTED
    );
    assert_eq!(
        super::ownership_refusal(OwnershipError::DuplicateMember(2)),
        EXPECTED
    );
    assert_eq!(
        super::ownership_refusal(OwnershipError::BotOwnership),
        EXPECTED
    );
    assert_eq!(
        super::ownership_refusal(OwnershipError::EmptyRoom),
        EXPECTED
    );
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

// These tests share the process-global log capture. Serialize every admin
// store-failure emitter, including the two reply-only tests, so each capture
// window contains only its own operation's warning. Hold the guard across await.
static ADMIN_STORE_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn access_command_reports_store_failures_and_changes_nothing() {
    let _guard = ADMIN_STORE_TEST_LOCK.lock().await;
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

#[tokio::test]
async fn access_read_store_failure_warns_and_keeps_reply() {
    let _guard = ADMIN_STORE_TEST_LOCK.lock().await;
    let runtime = gated_runtime(
        Trace::default(),
        AccessControls::default(),
        Some(StoreError::Unavailable),
    );
    let global = tracing_capture::global();
    let before = global.text().len();
    let (_, response) = handle_capture(
        &runtime,
        &access_interaction(sub_option("show", vec![]), true),
    )
    .await;
    assert_eq!(
        response_text(&response.expect("reply")),
        "Could not read the voice-room settings. Nothing was changed; try again."
    );
    let log = global.text()[before..].to_owned();
    let lines: Vec<_> = log
        .lines()
        .filter(|line| line.contains("op=\"access_read\""))
        .collect();
    assert_eq!(lines.len(), 1, "expected one access_read warn in:\n{log}");
    let line = lines[0];
    assert!(line.starts_with("WARN "), "{line}");
    assert!(line.contains("voice_admin_store"), "{line}");
    assert!(line.contains("reason=\"unavailable\""), "{line}");
    assert!(line.contains("guild_id=\"100\""), "{line}");
    assert!(line.contains("voice admin store failed"), "{line}");
    assert!(!line.contains("the store"), "{line}");
}

#[tokio::test]
async fn access_save_store_failure_warns_and_keeps_reply() {
    let _guard = ADMIN_STORE_TEST_LOCK.lock().await;
    let shared = Arc::new(Mutex::new(AccessControls::default()));
    let runtime = shared_runtime(
        Trace::default(),
        shared.clone(),
        Some(StoreError::CredentialRefused),
    );
    let disable = access_interaction(
        sub_option(
            "creation",
            vec![CommandDataOption {
                name: "enabled".to_owned(),
                value: CommandOptionValue::Boolean(false),
            }],
        ),
        true,
    );
    let global = tracing_capture::global();
    let before = global.text().len();
    let (_, response) = handle_capture(&runtime, &disable).await;
    assert_eq!(
        response_text(&response.expect("reply")),
        "Could not save the voice-room settings. Nothing was changed; try again."
    );
    assert_eq!(*shared.lock().unwrap(), AccessControls::default());
    let log = global.text()[before..].to_owned();
    let lines: Vec<_> = log
        .lines()
        .filter(|line| line.contains("op=\"access_save\""))
        .collect();
    assert_eq!(lines.len(), 1, "expected one access_save warn in:\n{log}");
    let line = lines[0];
    assert!(line.starts_with("WARN "), "{line}");
    assert!(line.contains("voice_admin_store"), "{line}");
    assert!(line.contains("reason=\"credential_refused\""), "{line}");
    assert!(line.contains("guild_id=\"100\""), "{line}");
    assert!(line.contains("voice admin store failed"), "{line}");
    assert!(!line.contains("the store"), "{line}");
}

fn channel_option(name: &str, id: u64) -> CommandDataOption {
    CommandDataOption {
        name: name.to_owned(),
        value: CommandOptionValue::Channel(Id::new(id)),
    }
}

fn logging_interaction(sub: CommandDataOption, admin: bool) -> Interaction {
    let mut interaction = voice_interaction_as(
        Some(command_data("logging", vec![sub])),
        admin.then_some(Permissions::MANAGE_CHANNELS),
        MEMBER,
    );
    interaction.member.as_mut().unwrap().roles = vec![Id::new(10)];
    interaction
}

fn logging_snapshot() -> GuildSnapshot {
    let mut snapshot = snapshot(&[], vec![]);
    snapshot.channels.push(channel(5, 0, None));
    snapshot.bot.roles[0].permissions |= Permissions::SEND_MESSAGES;
    let mut target = role_with(9, Permissions::empty());
    target.position = 1;
    target.mentionable = true;
    let mut actor = role_with(10, Permissions::MANAGE_CHANNELS);
    actor.position = 2;
    snapshot.bot.roles.extend([target, actor]);
    snapshot
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
    for sub in [
        sub_option("bogus", vec![]),
        sub_option("level", vec![]),
        sub_option("mention", vec![channel_option("role", 9)]),
        sub_option("channel", vec![role_option("channel", 5)]),
    ] {
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
    // Ready guild-role facts, not cold ones: command_snapshot's @everyone
    // carries no permissions and the member holds no roles, so authority
    // resolves to a known non-admin and the denial is permanent.
    // (logging_snapshot's @everyone carries MANAGE_CHANNELS, and a held
    // role missing from the snapshot is the cold warming-up case covered
    // by the cold-authority tests.)
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    let mut member = logging_interaction(sub_option("show", vec![]), false);
    member.member.as_mut().unwrap().roles.clear();
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
    assert!(runtime.publish_snapshot(GUILD, logging_snapshot()));
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
async fn logging_mention_refuses_unsafe_roles_without_saving() {
    for case in [
        "everyone",
        "managed",
        "higher",
        "same",
        "tie",
        "unmentionable",
        "unknown",
        "missing-held",
    ] {
        let initial = LoggingSettings {
            channel_id: Some(5),
            mention_role_id: Some(9),
            ..LoggingSettings::default()
        };
        let shared = Arc::new(Mutex::new(initial));
        let runtime = logging_runtime(shared.clone(), None, None);
        let mut snapshot = logging_snapshot();
        let mut target = 9;
        match case {
            "everyone" => {
                target = GUILD;
                snapshot.bot.roles[0].mentionable = true;
            }
            "managed" => snapshot.bot.roles[1].managed = true,
            "higher" => snapshot.bot.roles[1].position = 3,
            "same" => {
                target = 10;
                snapshot.bot.roles[2].mentionable = true;
            }
            "tie" => snapshot.bot.roles[1].position = 2, // Lower ID wins the tie.
            "unmentionable" => snapshot.bot.roles[1].mentionable = false,
            "unknown" => target = 99,
            "missing-held" => {
                snapshot.bot.roles.pop();
            }
            _ => unreachable!(),
        }
        assert!(runtime.publish_snapshot(GUILD, snapshot));
        let (_, response) = handle_capture(
            &runtime,
            &logging_interaction(
                sub_option("mention", vec![role_option("role", target)]),
                true,
            ),
        )
        .await;
        let response = response.expect("refusal");
        let text = response_text(&response);
        if case == "missing-held" {
            // The actor's own role is missing from cache, so guild-role
            // facts are incomplete: a cold authority state. The reply is
            // the retryable warming-up text, not a permanent denial, and
            // nothing is saved.
            let lower = text.to_ascii_lowercase();
            assert!(
                lower.contains("warm") && (lower.contains("try again") || lower.contains("retry")),
                "{case}: expected warming-up + retry reply, got: {text}"
            );
            assert!(
                !text.contains("You need Manage Channels to use /logging."),
                "{case}: cold reply must not be a permanent denial, got: {text}"
            );
        } else {
            assert!(text.contains("Nothing was changed"), "{case}: {text}");
        }
        assert_eq!(
            response.data.as_ref().unwrap().flags,
            Some(MessageFlags::EPHEMERAL)
        );
        assert_eq!(*shared.lock().unwrap(), initial, "{case}");
    }
}

#[tokio::test]
async fn logging_role_hierarchy_uses_the_highest_role_and_owner_exception() {
    for owner in [false, true] {
        let shared = Arc::new(Mutex::new(LoggingSettings::default()));
        let runtime = logging_runtime(shared.clone(), None, None);
        let mut snapshot = logging_snapshot();
        snapshot.bot.roles[1].position = 2;
        snapshot.bot.roles[1].id = Id::new(11); // Higher ID is lower at equal position.
        if owner {
            snapshot.bot.roles[1].position = 3;
        }
        assert!(runtime.publish_snapshot(GUILD, snapshot));
        let mut interaction =
            logging_interaction(sub_option("mention", vec![role_option("role", 11)]), true);
        interaction
            .member
            .as_mut()
            .unwrap()
            .roles
            .insert(0, Id::new(GUILD));
        if owner {
            interaction.member.as_mut().unwrap().user = Some(test_user(OWNER));
        }
        let (_, response) = handle_capture(&runtime, &interaction).await;
        assert!(response_text(&response.unwrap()).starts_with("Saved."));
        assert_eq!(shared.lock().unwrap().mention_role_id, Some(11));
    }
}

#[tokio::test]
async fn logging_channel_refuses_non_text_unknown_and_hidden_targets() {
    use twilight_model::channel::permission_overwrite::{
        PermissionOverwrite, PermissionOverwriteType,
    };
    for case in [
        "voice",
        "stage",
        "category",
        "thread",
        "announcement",
        "dm",
        "unknown",
        "hidden",
        "read-only",
        "missing-held",
    ] {
        let initial = LoggingSettings {
            channel_id: Some(5),
            mention_role_id: Some(9),
            ..LoggingSettings::default()
        };
        let shared = Arc::new(Mutex::new(initial));
        let runtime = logging_runtime(shared.clone(), None, None);
        let mut snapshot = logging_snapshot();
        let target = if case == "unknown" { 99 } else { 5 };
        let channel = snapshot.channels.last_mut().unwrap();
        match case {
            "voice" => channel.kind = ChannelType::GuildVoice,
            "stage" => channel.kind = ChannelType::GuildStageVoice,
            "category" => channel.kind = ChannelType::GuildCategory,
            "thread" => channel.kind = ChannelType::PublicThread,
            "announcement" => channel.kind = ChannelType::GuildAnnouncement,
            "dm" => channel.kind = ChannelType::Private,
            "hidden" | "read-only" => {
                channel.permission_overwrites = Some(vec![PermissionOverwrite {
                    id: Id::new(MEMBER),
                    kind: PermissionOverwriteType::Member,
                    allow: Permissions::empty(),
                    deny: if case == "hidden" {
                        Permissions::VIEW_CHANNEL
                    } else {
                        Permissions::SEND_MESSAGES
                    },
                }])
            }
            "missing-held" => {
                snapshot.bot.roles.pop();
            }
            "unknown" => {}
            _ => unreachable!(),
        }
        assert!(runtime.publish_snapshot(GUILD, snapshot));
        let (_, response) = handle_capture(
            &runtime,
            &logging_interaction(
                sub_option("channel", vec![channel_option("channel", target)]),
                true,
            ),
        )
        .await;
        let text = response_text(&response.unwrap());
        if case == "missing-held" {
            // The actor's own role is missing from cache, so guild-role
            // facts are incomplete: a cold authority state. The reply is
            // the retryable warming-up text, not a permanent denial, and
            // nothing is saved.
            let lower = text.to_ascii_lowercase();
            assert!(
                lower.contains("warm") && (lower.contains("try again") || lower.contains("retry")),
                "{case}: expected warming-up + retry reply, got: {text}"
            );
            assert!(
                !text.contains("You need Manage Channels to use /logging."),
                "{case}: cold reply must not be a permanent denial, got: {text}"
            );
        } else {
            assert!(text.contains("Nothing was changed"), "{case}: {text}");
        }
        assert_eq!(*shared.lock().unwrap(), initial, "{case}");
    }
}

#[tokio::test]
async fn logging_targets_fail_closed_without_target_evidence_but_clearing_still_works() {
    let initial = LoggingSettings {
        channel_id: Some(5),
        mention_role_id: Some(9),
        ..LoggingSettings::default()
    };
    let shared = Arc::new(Mutex::new(initial));
    let runtime = logging_runtime(shared.clone(), None, None);
    // The actor stays admin through the live cache, but neither target has
    // cache evidence, so validation refuses while clearing still saves.
    let mut snapshot = logging_snapshot();
    snapshot.channels.clear();
    snapshot.bot.roles.retain(|role| role.id.get() != 9);
    assert!(runtime.publish_snapshot(GUILD, snapshot));
    for sub in [
        sub_option("channel", vec![channel_option("channel", 5)]),
        sub_option("mention", vec![role_option("role", 9)]),
    ] {
        let (_, response) = handle_capture(&runtime, &logging_interaction(sub, true)).await;
        assert!(response_text(&response.unwrap()).contains("Nothing was changed"));
        assert_eq!(*shared.lock().unwrap(), initial);
    }
    for sub in [sub_option("channel", vec![]), sub_option("mention", vec![])] {
        let (_, response) = handle_capture(&runtime, &logging_interaction(sub, true)).await;
        assert!(response_text(&response.unwrap()).starts_with("Saved."));
    }
    assert_eq!(*shared.lock().unwrap(), LoggingSettings::default());
}

#[tokio::test]
async fn logging_rejects_foreign_channel_and_disconnected_or_unidentified_actor() {
    let live = LiveGuild::new(GUILD);
    assert!(live.publish(logging_snapshot()));
    let interaction = logging_interaction(
        sub_option("channel", vec![channel_option("channel", 5)]),
        true,
    );
    let mut foreign = channel(5, 0, None);
    foreign.guild_id = Some(Id::new(101));
    // Inject inconsistent cache evidence, not an interaction's resolved map.
    live.inner.write().unwrap().channels.insert(5, foreign);
    assert!(live
        .validate_logging(&interaction, &LoggingAction::Channel(Some(5)))
        .is_err());
    assert!(live.publish(logging_snapshot()));
    let mut anonymous = interaction.clone();
    anonymous.member.as_mut().unwrap().user = None;
    assert!(live
        .validate_logging(&anonymous, &LoggingAction::Channel(Some(5)))
        .is_err());
    live.disconnect();
    assert!(live
        .validate_logging(&interaction, &LoggingAction::Mention(Some(9)))
        .is_err());
}

#[tokio::test]
async fn logging_command_reports_store_failures_and_changes_nothing() {
    let _guard = ADMIN_STORE_TEST_LOCK.lock().await;
    let level = || {
        logging_interaction(
            sub_option("level", vec![command_option("level", "off")]),
            true,
        )
    };
    let shared = Arc::new(Mutex::new(LoggingSettings::default()));
    let unsavable = logging_runtime(shared.clone(), None, Some(StoreError::Unavailable));
    assert!(unsavable.publish_snapshot(GUILD, logging_snapshot()));
    let (_, response) = handle_capture(&unsavable, &level()).await;
    assert!(response_text(&response.expect("reply")).contains("Nothing was changed"));
    assert_eq!(*shared.lock().unwrap(), LoggingSettings::default());

    let unreadable = logging_runtime(shared.clone(), Some(StoreError::Unavailable), None);
    assert!(unreadable.publish_snapshot(GUILD, logging_snapshot()));
    let (_, response) = handle_capture(&unreadable, &level()).await;
    assert!(response_text(&response.expect("reply")).contains("Nothing was changed"));
    assert_eq!(*shared.lock().unwrap(), LoggingSettings::default());
}

#[tokio::test]
async fn logging_read_store_failure_warns_and_keeps_reply() {
    let _guard = ADMIN_STORE_TEST_LOCK.lock().await;
    let shared = Arc::new(Mutex::new(LoggingSettings::default()));
    let runtime = logging_runtime(shared.clone(), Some(StoreError::Conflict), None);
    assert!(runtime.publish_snapshot(GUILD, logging_snapshot()));
    let level = logging_interaction(
        sub_option("level", vec![command_option("level", "off")]),
        true,
    );
    let global = tracing_capture::global();
    let before = global.text().len();
    let (_, response) = handle_capture(&runtime, &level).await;
    assert_eq!(
        response_text(&response.expect("reply")),
        "Could not read the logging settings. Nothing was changed; try again."
    );
    assert_eq!(*shared.lock().unwrap(), LoggingSettings::default());
    let log = global.text()[before..].to_owned();
    let lines: Vec<_> = log
        .lines()
        .filter(|line| line.contains("op=\"logging_read\""))
        .collect();
    assert_eq!(lines.len(), 1, "expected one logging_read warn in:\n{log}");
    let line = lines[0];
    assert!(line.starts_with("WARN "), "{line}");
    assert!(line.contains("voice_admin_store"), "{line}");
    assert!(line.contains("reason=\"conflict\""), "{line}");
    assert!(line.contains("guild_id=\"100\""), "{line}");
    assert!(line.contains("voice admin store failed"), "{line}");
    assert!(!line.contains("the store"), "{line}");
}

#[tokio::test]
async fn logging_save_store_failure_warns_and_keeps_reply() {
    let _guard = ADMIN_STORE_TEST_LOCK.lock().await;
    let shared = Arc::new(Mutex::new(LoggingSettings::default()));
    let runtime = logging_runtime(shared.clone(), None, Some(StoreError::Unavailable));
    assert!(runtime.publish_snapshot(GUILD, logging_snapshot()));
    let level = logging_interaction(
        sub_option("level", vec![command_option("level", "off")]),
        true,
    );
    let global = tracing_capture::global();
    let before = global.text().len();
    let (_, response) = handle_capture(&runtime, &level).await;
    assert_eq!(
        response_text(&response.expect("reply")),
        "Could not save the logging settings. Nothing was changed; try again."
    );
    assert_eq!(*shared.lock().unwrap(), LoggingSettings::default());
    let log = global.text()[before..].to_owned();
    let lines: Vec<_> = log
        .lines()
        .filter(|line| line.contains("op=\"logging_save\""))
        .collect();
    assert_eq!(lines.len(), 1, "expected one logging_save warn in:\n{log}");
    let line = lines[0];
    assert!(line.starts_with("WARN "), "{line}");
    assert!(line.contains("voice_admin_store"), "{line}");
    assert!(line.contains("reason=\"unavailable\""), "{line}");
    assert!(line.contains("guild_id=\"100\""), "{line}");
    assert!(line.contains("voice admin store failed"), "{line}");
    assert!(!line.contains("the store"), "{line}");
}

#[test]
fn admin_store_error_tokens_cover_all_variants() {
    assert_eq!(
        admin_store_error_token(StoreError::Unavailable),
        "unavailable"
    );
    assert_eq!(
        admin_store_error_token(StoreError::CredentialRefused),
        "credential_refused"
    );
    assert_eq!(admin_store_error_token(StoreError::Conflict), "conflict");
}

#[test]
fn admin_store_failure_logs_one_warn_with_op_and_reason() {
    tracing_capture::global();
    let capture = tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || {
        log_admin_store_failure("access_read", GUILD, StoreError::Unavailable);
    });
    let log = capture.text();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 1, "{log}");
    let line = lines[0];
    assert!(line.starts_with("WARN "), "{line}");
    assert!(line.contains("voice_event=\"voice_admin_store\""), "{line}");
    assert!(line.contains("op=\"access_read\""), "{line}");
    assert!(line.contains("reason=\"unavailable\""), "{line}");
    assert!(line.contains("guild_id=\"100\""), "{line}");
    assert!(line.contains("voice admin store failed"), "{line}");
    assert!(!line.contains("the store"), "{line}");
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
            &ActorInboxBackpressure::default(),
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

#[tokio::test(flavor = "current_thread")]
async fn access_changed_delivers_past_a_full_inbox() {
    // Mutation: dropping runtime.access_changed, or making its send lossy,
    // leaves the worker enforcing the opposite policy after a saved update.
    for (before, after) in [(false, true), (true, false)] {
        let (live, store, http, trace) = fixture();
        *store.access.lock().unwrap() = AccessControls {
            room_creation_enabled: before,
            ..AccessControls::default()
        };
        let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
        let runtime = test_runtime(trace);
        let (tx, mut rx) = mpsc::unbounded_channel();
        runtime.actors.lock().unwrap().insert(
            GUILD,
            GuildActor {
                live: worker.live.clone(),
                tx,
                backpressure: ActorInboxBackpressure::default(),
            },
        );
        let actor = runtime.ensure_actor(GUILD).expect("registered actor");
        // Hold the receiver: all sends below are synchronous and the ledger
        // is exactly full before the sole policy update enters the channel.
        for member in 0..VOICE_ACTOR_INBOX_CAP {
            assert!(actor.try_send_command(ActorCommand::Display {
                member_id: 900_000 + member as u64,
                display: "x".to_owned(),
            }));
        }
        assert_eq!(actor.inbox_depth(), VOICE_ACTOR_INBOX_CAP);
        assert!(!actor.try_send_command(ActorCommand::Reconcile));
        let dropped = actor.inbox_overflow_dropped();
        runtime.access_changed(
            GUILD,
            AccessControls {
                room_creation_enabled: after,
                ..AccessControls::default()
            },
        );
        assert_eq!(actor.inbox_overflow_dropped(), dropped);
        // Drain through the real worker command handler; send success alone
        // cannot satisfy this test, nor can an earlier direct lossless send.
        while let Ok(command) = rx.try_recv() {
            actor.backpressure.release_for(&command);
            apply_command(&mut worker, command, 0, &actor.backpressure);
        }
        assert_eq!(actor.inbox_depth(), 0);
        let ticket = worker
            .live
            .voice_update(MEMBER, Some(CREATOR), Some(false))
            .expect("fresh creator join");
        assert_eq!(
            worker.accept_join(ticket, "new room", 7, NOW.to_owned()),
            after,
            "join acceptance must use the policy delivered through runtime.access_changed"
        );
        assert_eq!(worker.queue.pending_counts(GUILD), (usize::from(after), 0));
        runtime.remove_guild(GUILD);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_confirmed_import_hands_its_settings_to_the_worker_past_a_full_inbox() {
    // Full-inbox sibling of `a_confirmed_import_hands_its_settings_to_the_worker`:
    // `/import` already wrote the store and replied "Saved", so the saved
    // settings must reach the worker even when fire-and-forget sends refuse.
    let trace = Trace::default();
    let bytes = serde_json::to_vec(&full_config()).unwrap();
    let (runtime, _) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let (probe_store, _) = runtime.make_pair();
    let reads = probe_store.config_reads.clone();
    let initial_reads = reads.load(Ordering::Relaxed);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let (confirm_id, _) = preview_buttons(&preview.expect("preview"));
    // Preview reads shared storage and ensure_actor only spawns. Await a
    // worker-applied probe before the save, otherwise a lost notification
    // escapes through the worker's first read of the NEW durable settings.
    let (before, _) = runtime
        .naming_probe(GUILD)
        .await
        .expect("loaded worker probe");
    assert_eq!(before, NameSettings::from_config(&empty_config()));
    assert_ne!(before, NameSettings::from_config(&full_config()));
    // The first timer maintenance is also a read-through escape: let it
    // finish on OLD state before the save. This harness has one preview
    // read, one load read, then the first paced worker reload.
    tokio::time::timeout(Duration::from_secs(5), async {
        while reads.load(Ordering::Relaxed) < initial_reads + 3 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("old-state worker maintenance read before the save");
    let (before, _) = runtime
        .naming_probe(GUILD)
        .await
        .expect("warm worker probe");
    assert_eq!(before, NameSettings::from_config(&empty_config()));
    // Current-thread execution: there is no yield between filling this gate
    // and the synchronous persistence/notification in the Confirm harness.
    let actor = runtime.ensure_actor(GUILD).expect("live actor");
    let mut refused = false;
    for member in 0..10_000u64 {
        if !actor.try_send_command(ActorCommand::Display {
            member_id: 900_000 + member,
            display: "x".to_owned(),
        }) {
            refused = true;
            break;
        }
    }
    assert!(refused, "burst must fill the inbox gate to refusal");
    assert_eq!(actor.inbox_depth(), VOICE_ACTOR_INBOX_CAP);
    let confirm = component_interaction(&confirm_id, manager(), UPLOADER);
    let (_, response) = handle_import_capture(&runtime, &confirm, Some(&inventory)).await;
    assert!(response_text(&response.expect("applied")).starts_with("Import applied:"));
    // The probe queues behind the flood and the saved update, so its answer
    // proves the update applied despite the full gate.
    let (settings, _) = runtime
        .naming_probe(GUILD)
        .await
        .expect("probe answers after a full-gate save");
    assert_eq!(settings, NameSettings::from_config(&full_config()));
    runtime.remove_guild(GUILD);
}

#[tokio::test]
async fn display_seed_batch_applies_every_entry() {
    // Startup seeding travels as one batch so a guild with more cached
    // members than inbox slots still seeds every name (owners included).
    let (live, store, http, _) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    apply_command(
        &mut worker,
        ActorCommand::DisplaySeed(vec![
            (MEMBER, "Alex".to_owned()),
            (MEMBER + 1, "Blair".to_owned()),
        ]),
        0,
        &ActorInboxBackpressure::default(),
    );
    assert!(
        worker.name_directory.knows(MEMBER),
        "seed batch must carry every entry"
    );
    assert!(
        worker.name_directory.knows(MEMBER + 1),
        "seed batch must carry every entry"
    );
}

#[test]
fn display_seed_buffers_while_actor_initializes() {
    // The seed batch is fire-and-forget state: it must survive the init
    // window instead of failing fast like reply-bearing commands.
    let mut buffered = std::collections::VecDeque::new();
    buffer_init_command(
        &mut buffered,
        ActorCommand::DisplaySeed(vec![(MEMBER, "Alex".to_owned())]),
    );
    assert_eq!(buffered.len(), 1, "seed batch must buffer during init");
    purge_unanswerable_buffered(&mut buffered);
    assert_eq!(buffered.len(), 1, "seed batch must survive a failure purge");
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
fn decide_text_channels_absent_toggle_keeps_the_stored_value() {
    let base = CreatorChannel::new(GUILD, CREATOR);
    let creator = plan_creator(decide_text_channels(
        Some(base.clone()),
        &request(None, None, None),
    ));
    assert!(
        !creator.text_channels,
        "absent toggle preserves stored off, never enables"
    );
    assert_eq!(creator.text_channel_name, None);
    assert_eq!(creator.text_viewer_role_id, None);
    assert_eq!(creator.name_template, base.name_template);
    let mut on = CreatorChannel::new(GUILD, CREATOR);
    on.text_channels = true;
    let kept = plan_creator(decide_text_channels(Some(on), &request(None, None, None)));
    assert!(kept.text_channels, "absent toggle preserves stored on");
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
async fn handle_textchannels_preserves_stored_toggle_on_rename_only() {
    let trace = Trace::default();
    let runtime = VoiceRuntime::new(
        {
            let trace = trace.clone();
            move || {
                let store = Store::new(trace.clone());
                store.creators.lock().unwrap()[0].text_channels = true;
                (store, Http::new(trace.clone()))
            }
        },
        Duration::from_millis(10),
        true,
    );
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
    for bad in [0, -3, 4_294_967_296, i64::MAX] {
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
    // The export codec bound itself still stores.
    let capped = match decide_position(
        base(),
        &PositionRequest {
            position: None,
            first_number: Some(4_294_967_295),
        },
    ) {
        PositionPlan::Update(creator) => creator,
        PositionPlan::Refuse { message } => panic!("unexpected refusal: {message}"),
    };
    assert_eq!(capped.first_room_number, 4_294_967_295);
}

#[test]
fn decide_group_refuses_without_a_toggle() {
    let GroupPlan::Refuse { message } = decide_group(
        Some(CreatorChannel::new(GUILD, CREATOR)),
        &GroupRequest { enabled: None },
    ) else {
        panic!("absent toggle refuses, never enables by default");
    };
    assert!(message.contains("group"), "{message}");
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
fn inherit_source_refusal_needs_guild_kind_and_view() {
    // Unknown id (not in this guild) refuses.
    assert!(inherit_source_refusal(None, Some(permissions())).is_some());
    // Copyable kinds (voice 2, stage 13, text 0, category 4) with View pass.
    for kind in [0u8, 2u8, 4u8, 13u8] {
        let source = channel(400, kind, None);
        assert!(
            inherit_source_refusal(Some(&source), Some(permissions())).is_none(),
            "kind {kind} must pass"
        );
    }
    // Anything else (DM 1, announcement 5, thread 10) refuses.
    for kind in [1u8, 3u8, 5u8, 10u8, 15u8] {
        let source = channel(400, kind, None);
        assert!(
            inherit_source_refusal(Some(&source), Some(permissions())).is_some(),
            "kind {kind} must refuse"
        );
    }
    // Incomplete snapshots fail closed; a viewer without View refuses.
    let voice = channel(400, 2, None);
    assert!(inherit_source_refusal(Some(&voice), None).is_some());
    assert!(inherit_source_refusal(Some(&voice), Some(Permissions::empty())).is_some());
    assert!(inherit_source_refusal(Some(&voice), Some(Permissions::VIEW_CHANNEL)).is_none());
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
fn decide_always_private_refuses_without_a_toggle() {
    let AlwaysPrivatePlan::Refuse { message } = decide_always_private(
        Some(CreatorChannel::new(GUILD, CREATOR)),
        &AlwaysPrivateRequest { enabled: None },
    ) else {
        panic!("absent toggle refuses, never enables by default");
    };
    assert!(message.contains("private"), "{message}");
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
async fn handle_alwaysprivate_refuses_without_enabled() {
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
        response_text(response.as_ref().expect("reply"))
            .contains("Choose whether new rooms start private, then try again."),
        "alwaysprivate refuses an absent toggle instead of defaulting on"
    );
    assert!(
        !trace
            .lock()
            .unwrap()
            .iter()
            .any(|entry| entry.starts_with("add_creator")),
        "a refused toggle writes nothing"
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

/// Two-second ceiling shared by the sibling worker-wait regression tests
/// (`/name`, join-request decisions, `/setup` status): mirrors the protected
/// worker-reply suite's guard so an unbounded production future fails fast
/// instead of hanging the test.
const SIBLING_ACK_BOUND: Duration = Duration::from_millis(2_000);
const SIBLING_TEST_GUARD: Duration = Duration::from_millis(2_100);

/// A runtime whose guild actor never answers: the test holds the inbox
/// receiver without polling it, so every worker wait must time out.
fn stalled_worker_runtime() -> (
    VoiceRuntime<Store, Http>,
    mpsc::UnboundedReceiver<ActorCommand>,
) {
    let runtime = test_runtime(Trace::default());
    let live = LiveGuild::new(GUILD);
    live.publish(command_snapshot());
    let (tx, rx) = mpsc::unbounded_channel();
    runtime.actors.lock().unwrap().insert(
        GUILD,
        GuildActor {
            live,
            tx,
            backpressure: ActorInboxBackpressure::default(),
        },
    );
    (runtime, rx)
}

async fn bounded_sibling_wait<T>(future: impl Future<Output = T>) -> T {
    let start = tokio::time::Instant::now();
    let result = tokio::time::timeout(SIBLING_TEST_GUARD, future)
        .await
        .expect("sibling worker wait must return within the two-second /limit budget");
    assert!(
        start.elapsed() <= SIBLING_ACK_BOUND,
        "sibling worker wait exceeded its documented bound"
    );
    result
}

fn assert_sibling_worker_lost_reply(response: &InteractionResponse) {
    let text = response_text(response);
    let lower = text.to_lowercase();
    assert!(
        !lower.contains("warm") && !lower.contains("syncing"),
        "a known worker lost its reply, not its cache: {text}"
    );
    assert!(
        [
            "timed out",
            "unavailable",
            "not responding",
            "couldn't",
            "stopped"
        ]
        .iter()
        .any(|reason| lower.contains(reason)),
        "state the worker failure honestly: {text}"
    );
    assert!(
        ["try again", "retry", "check", "wait"]
            .iter()
            .any(|next| lower.contains(next)),
        "worker failure needs a next action: {text}"
    );
    assert_eq!(
        response.kind,
        InteractionResponseType::ChannelMessageWithSource
    );
    assert_eq!(
        response.data.as_ref().and_then(|data| data.flags),
        Some(MessageFlags::EPHEMERAL)
    );
}

#[tokio::test(start_paused = true)]
async fn status_wait_is_bounded_with_a_live_unanswered_inbox() {
    let (runtime, _rx) = stalled_worker_runtime();
    assert!(bounded_sibling_wait(runtime.worker_status(GUILD))
        .await
        .is_none());
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
    let text = response_text(&completed[0]);
    let lower = text.to_ascii_lowercase();
    assert!(
        lower.contains("warm") && (lower.contains("try again") || lower.contains("retry")),
        "cold denial must say warming up and ask to retry, got: {text:?}"
    );
    assert!(
        !text.contains("You need Manage Channels"),
        "cold denial must not report a permanent denial, got: {text:?}"
    );
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

/// Production wires `VoiceResponder`, not the bare runtime: a presence fed
/// to the responder must reach the live guild's naming facts.
#[tokio::test]
async fn gateway_responder_forwards_presence_to_the_runtime() {
    let trace = Trace::default();
    let runtime = Arc::new(test_runtime(trace.clone()));
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    let replies = Arc::new(Replies::new(trace));
    let sink = VoiceResponder::new(Arc::clone(&runtime), replies);
    let update: twilight_model::gateway::payload::incoming::PresenceUpdate =
        serde_json::from_value(serde_json::json!({
            "user": {"id": MEMBER.to_string()},
            "guild_id": GUILD.to_string(),
            "status": "online",
            "activities": [{"type": 0, "name": "Apex Legends"}],
            "client_status": {"desktop": "online"}
        }))
        .expect("presence");
    VoiceEventSink::presence(&sink, &update);
    let actor = runtime.live_actor(GUILD).expect("live actor");
    let live = actor.live.read_state();
    assert!(
        live.presences.contains_key(&MEMBER),
        "the responder dropped the presence update"
    );
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

/// Drive one full join through room create, move and companion create.
async fn join_with_companion(
    worker: &mut GuildRoomWorker<Store, Http>,
    member: u64,
    now: &mut u64,
) -> u64 {
    join(worker, member);
    // Room create.
    dispatch(worker, *now).await;
    *now += 1;
    // Creator move.
    dispatch(worker, *now).await;
    *now += 1;
    // Companion create.
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
    dispatch(&mut worker, 2).await;
    assert!(!worker.dispatch_one(3).await);
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

#[tokio::test(start_paused = true)]
async fn companion_deleted_with_its_room_and_delete_is_idempotent() {
    let (live, store, http, trace) = companion_fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let mut now = 0;
    join_with_companion(&mut worker, MEMBER, &mut now).await;
    let text_id = worker.companions[&500].text_channel_id;
    // Everyone leaves: after the grace, reconcile queues the room delete and
    // its companion cleanup.
    worker.live.voice_update(MEMBER, None, Some(false));
    tokio::time::advance(EMPTY_ROOM_GRACE).await;
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
    dispatch(&mut worker, now).await; // Creator move has priority.
    dispatch(&mut worker, now + 1).await; // Exercise the companion action.
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
    dispatch(&mut worker, now).await; // Creator move has priority.
    dispatch(&mut worker, now + 1).await; // Exercise the companion action.
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
    while worker.dispatch_one(now).await {
        now += 1;
    }
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
    apply_command(
        &mut worker,
        ActorCommand::CreatorAdded(edited),
        now,
        &ActorInboxBackpressure::default(),
    );
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
    // Room create and move precede the companion POST whose row write fails.
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    dispatch(&mut worker, 2).await;
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
        member_roles: HashMap::new(),
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
        member_roles: HashMap::new(),
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
    let config_reads = Arc::new(AtomicU64::new(0));
    let runtime = VoiceRuntime::new(
        move || {
            let mut store = Store::new(trace.clone());
            store.config = closure_config.clone();
            store.config_reads = config_reads.clone();
            store.snapshot_uses_creator_rows = false;
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

fn applied_count(trace: &Trace) -> usize {
    trace
        .lock()
        .unwrap()
        .iter()
        .filter(|entry| entry.as_str() == "config_apply")
        .count()
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
async fn export_refuses_a_configuration_import_would_reject_for_size() {
    let trace = Trace::default();
    let mut config = full_config();
    // Valid under every field bound, yet far above the import cap once exported.
    config.lists = (0..config_codec::MAX_LISTS)
        .map(|list| config_codec::RandomList {
            name: format!("list-{list:03}"),
            choices: vec![
                "x".repeat(config_codec::MAX_LIST_TEXT_CHARS);
                config_codec::MAX_LIST_CHOICES
            ],
        })
        .collect();
    let (runtime, _) = import_harness(trace.clone(), config, Vec::new());
    let interaction = with_user(
        voice_interaction(Some(command_data("export", Vec::new())), manager(), true),
        UPLOADER,
    );
    let inventory = config_inventory();
    let (owned, response) = handle_import_capture(&runtime, &interaction, Some(&inventory)).await;
    assert!(owned);
    let response = response.expect("refusal");
    let text = response_text(&response);
    assert!(text.contains("Could not export"), "{text}");
    assert!(text.contains(&MAX_IMPORT_BYTES.to_string()), "{text}");
    assert!(text.contains("Nothing was sent"), "{text}");
    assert_eq!(
        response.data.as_ref().and_then(|data| data.flags),
        Some(MessageFlags::EPHEMERAL)
    );
    assert!(response
        .data
        .as_ref()
        .and_then(|data| data.attachments.as_ref())
        .is_none());
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

fn config_failure_runtime(trace: Trace, error: StoreError) -> VoiceRuntime<Store, Http> {
    VoiceRuntime::new(
        move || {
            let mut store = Store::new(trace.clone());
            store.config_error = Some(error);
            (store, Http::new(trace.clone()))
        },
        Duration::from_millis(10),
        true,
    )
}

#[tokio::test]
async fn export_read_store_failure_warns_and_keeps_reply() {
    let _guard = ADMIN_STORE_TEST_LOCK.lock().await;
    let trace = Trace::default();
    let runtime = config_failure_runtime(trace.clone(), StoreError::CredentialRefused);
    let interaction = with_user(
        voice_interaction(Some(command_data("export", Vec::new())), manager(), true),
        UPLOADER,
    );
    let inventory = config_inventory();
    let global = tracing_capture::global();
    let before = global.text().len();
    let (owned, response) = handle_import_capture(&runtime, &interaction, Some(&inventory)).await;
    assert!(owned);
    assert_eq!(
        response_text(&response.expect("reply")),
        "Could not read the voice configuration. Nothing was sent; try again."
    );
    assert!(trace.lock().unwrap().is_empty());
    let log = global.text()[before..].to_owned();
    let lines: Vec<_> = log
        .lines()
        .filter(|line| line.contains("op=\"export_read\""))
        .collect();
    assert_eq!(lines.len(), 1, "expected one export_read warn in:\n{log}");
    let line = lines[0];
    assert!(line.starts_with("WARN "), "{line}");
    assert!(line.contains("voice_admin_store"), "{line}");
    assert!(line.contains("reason=\"credential_refused\""), "{line}");
    assert!(line.contains("guild_id=\"100\""), "{line}");
    assert!(line.contains("voice admin store failed"), "{line}");
    assert!(!line.contains("the store"), "{line}");
}

#[tokio::test]
async fn import_read_store_failure_warns_and_keeps_reply() {
    let _guard = ADMIN_STORE_TEST_LOCK.lock().await;
    let trace = Trace::default();
    let runtime = config_failure_runtime(trace.clone(), StoreError::Conflict);
    let interaction = import_interaction(10, manager(), UPLOADER);
    let inventory = config_inventory();
    let global = tracing_capture::global();
    let before = global.text().len();
    let (owned, response) = handle_import_capture(&runtime, &interaction, Some(&inventory)).await;
    assert!(owned);
    assert_eq!(
        response_text(&response.expect("reply")),
        "Could not read the current voice configuration. Nothing was changed; try again."
    );
    assert!(trace.lock().unwrap().is_empty());
    let log = global.text()[before..].to_owned();
    let lines: Vec<_> = log
        .lines()
        .filter(|line| line.contains("op=\"import_read\""))
        .collect();
    assert_eq!(lines.len(), 1, "expected one import_read warn in:\n{log}");
    let line = lines[0];
    assert!(line.starts_with("WARN "), "{line}");
    assert!(line.contains("voice_admin_store"), "{line}");
    assert!(line.contains("reason=\"conflict\""), "{line}");
    assert!(line.contains("guild_id=\"100\""), "{line}");
    assert!(line.contains("voice admin store failed"), "{line}");
    assert!(!line.contains("the store"), "{line}");
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

/// `/setup` then `/import` confirm round-trip through constructed interactions.
///
/// An admin reads the status panel, uploads a configuration file, previews
/// the diff, and confirms: the stored configuration equals the upload and
/// the apply lands exactly once, with the panel still answering after.
/// Drives the real `handle_voice_interaction_with` arms with constructed
/// slash and component inputs; no guild, network, or database.
#[tokio::test]
async fn setup_then_import_confirm_round_trip_through_interactions() {
    let trace = Trace::default();
    let bytes = serde_json::to_vec(&full_config()).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    // Baseline: /setup answers the admin panel before the import.
    let setup = with_user(
        voice_interaction(Some(command_data("setup", Vec::new())), manager(), true),
        UPLOADER,
    );
    let (owned, panel) = handle_import_capture(&runtime, &setup, Some(&inventory)).await;
    assert!(owned, "voice owns the /setup callback");
    assert!(!response_text(&panel.expect("setup panel")).is_empty());

    // Upload: /import previews the diff with confirm/cancel buttons.
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (owned, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    assert!(owned, "voice owns the /import callback");
    let preview = preview.expect("preview");
    assert!(!applied(&trace), "preview writes nothing");
    let (confirm_id, _) = preview_buttons(&preview);

    // Confirm: the component press applies the upload exactly once.
    let confirm = component_interaction(&confirm_id, manager(), UPLOADER);
    let (owned, response) = handle_import_capture(&runtime, &confirm, Some(&inventory)).await;
    assert!(owned, "voice owns the confirm callback");
    assert!(response_text(&response.expect("applied")).starts_with("Import applied:"));
    assert_eq!(*shared.lock().unwrap(), full_config());
    assert_eq!(applied_count(&trace), 1, "one apply per confirm");

    // Replay: pressing Confirm again expires instead of applying twice.
    let (owned, response) = handle_import_capture(&runtime, &confirm, Some(&inventory)).await;
    assert!(owned, "voice owns the replayed confirm callback");
    let replayed = response_text(&response.expect("expired replay"));
    assert!(replayed.contains("expired"), "{replayed}");
    assert_eq!(applied_count(&trace), 1, "no second apply");
    assert_eq!(*shared.lock().unwrap(), full_config());

    // The panel still answers with content after the import lands.
    let (owned, panel) = handle_import_capture(&runtime, &setup, Some(&inventory)).await;
    assert!(owned, "voice still owns /setup after the import");
    assert!(!response_text(&panel.expect("post-import panel")).is_empty());
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
    let mut notice_role = role_with(NOTICE_ROLE, Permissions::empty());
    notice_role.mentionable = true;
    BotAccess {
        member_id: 999,
        guild_owner_id: OWNER,
        system_channel_id,
        member_roles: vec![],
        roles: vec![role(permissions()), notice_role],
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
    live.upsert_channel(channel(650, 0, None));
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
async fn notice_suppresses_everyone_unknown_managed_and_unmentionable_roles() {
    for case in ["everyone", "unknown", "managed", "unmentionable"] {
        let target = match case {
            "everyone" => GUILD,
            "unknown" => 99,
            _ => NOTICE_ROLE,
        };
        let settings = LoggingSettings {
            mention_role_id: Some(target),
            ..LoggingSettings::default()
        };
        let mut worker = failed_worker(settings, Some(SYSTEM_CHANNEL)).await;
        let mut bot = notice_bot(Some(SYSTEM_CHANNEL));
        if case == "managed" {
            bot.roles[1].managed = true;
        }
        if case == "unmentionable" {
            bot.roles[1].mentionable = false;
        }
        bot.roles[0].mentionable = true;
        worker.live.refresh_bot(bot);
        assert!(worker.send_notices(0).await);
        assert_eq!(sent(&worker)[0].2, None, "{case}");
    }
}

#[tokio::test]
async fn notice_role_drift_is_rechecked_before_a_repeat() {
    let settings = LoggingSettings {
        mention_role_id: Some(NOTICE_ROLE),
        ..LoggingSettings::default()
    };
    let mut worker = failed_worker(settings, Some(SYSTEM_CHANNEL)).await;
    assert!(worker.send_notices(0).await);
    assert_eq!(sent(&worker)[0].2, Some(NOTICE_ROLE));
    let mut bot = notice_bot(Some(SYSTEM_CHANNEL));
    bot.roles[1].mentionable = false;
    worker.live.refresh_bot(bot);
    assert!(worker.send_notices(NOTICE_REPEAT_INTERVAL_MS).await);
    assert_eq!(sent(&worker)[1].2, None);
}

#[tokio::test]
async fn rejected_everyone_logging_command_cannot_add_a_notice_mention() {
    let shared = Arc::new(Mutex::new(LoggingSettings::default()));
    let runtime = logging_runtime(shared.clone(), None, None);
    assert!(runtime.publish_snapshot(GUILD, logging_snapshot()));
    let (_, response) = handle_capture(
        &runtime,
        &logging_interaction(
            sub_option("mention", vec![role_option("role", GUILD)]),
            true,
        ),
    )
    .await;
    assert!(response_text(&response.unwrap()).contains("Nothing was changed"));
    let settings = *shared.lock().unwrap();
    let mut worker = failed_worker(settings, Some(SYSTEM_CHANNEL)).await;
    assert!(worker.send_notices(0).await);
    assert_eq!(sent(&worker)[0].2, None);
}

#[tokio::test]
async fn notice_skips_a_stored_non_text_channel() {
    let settings = LoggingSettings {
        channel_id: Some(CREATOR),
        ..LoggingSettings::default()
    };
    let mut worker = failed_worker(settings, Some(SYSTEM_CHANNEL)).await;
    assert!(worker.send_notices(0).await);
    assert_eq!(sent(&worker)[0].0, NoticeTarget::Channel(SYSTEM_CHANNEL));
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
async fn configured_channel_still_wins_while_reconnecting() {
    // `disconnect` clears readiness but keeps the cached channels and roles.
    // Delivery filters last-known evidence, so a due notice still reaches the
    // configured channel with its mention instead of leaking to the system
    // channel and consuming a repeat.
    let settings = LoggingSettings {
        channel_id: Some(650),
        mention_role_id: Some(NOTICE_ROLE),
        ..LoggingSettings::default()
    };
    let mut worker = failed_worker(settings, Some(SYSTEM_CHANNEL)).await;
    worker.live.disconnect();
    assert!(worker.send_notices(0).await);
    let notices = sent(&worker);
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].0, NoticeTarget::Channel(650));
    assert_eq!(notices[0].2, Some(NOTICE_ROLE));
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
async fn configured_channel_failure_warns_then_falls_back() {
    let settings = LoggingSettings {
        channel_id: Some(650),
        mention_role_id: Some(NOTICE_ROLE),
        ..LoggingSettings::default()
    };
    let mut worker = failed_worker(settings, Some(SYSTEM_CHANNEL)).await;

    // A working configured channel stays silent: no fallback warn.
    let global = tracing_capture::global();
    let before = global.text().len();
    assert!(worker.send_notices(0).await);
    assert_eq!(sent(&worker)[0].0, NoticeTarget::Channel(650));
    let quiet = global.text()[before..].to_owned();
    assert!(
        !quiet.contains("voice notice configured channel failed; trying fallback"),
        "success stays silent: {quiet}"
    );

    // A rate limit is a retry, not a misconfiguration: the fallback still
    // delivers and no warn fires. Kept in this test because the capture is
    // process-wide and only this test emits the warn, so a sibling's line
    // cannot land in these silent windows.
    worker
        .http
        .rate_limited_notices
        .lock()
        .unwrap()
        .push(NoticeTarget::Channel(650));
    let before = global.text().len();
    assert!(worker.send_notices(NOTICE_REPEAT_INTERVAL_MS).await);
    let notices = sent(&worker);
    assert_eq!(notices.len(), 2);
    assert_eq!(notices[1].0, NoticeTarget::Channel(SYSTEM_CHANNEL));
    let throttled = global.text()[before..].to_owned();
    assert!(
        !throttled.contains("voice notice configured channel failed; trying fallback"),
        "a rate limit stays silent: {throttled}"
    );

    // A failing configured channel warns once, then the fallback delivers
    // with the mention intact.
    worker.http.rate_limited_notices.lock().unwrap().clear();
    worker
        .http
        .refused_notices
        .lock()
        .unwrap()
        .push(NoticeTarget::Channel(650));
    let before = global.text().len();
    assert!(worker.send_notices(2 * NOTICE_REPEAT_INTERVAL_MS).await);
    let notices = sent(&worker);
    assert_eq!(notices.len(), 3);
    assert_eq!(notices[2].0, NoticeTarget::Channel(SYSTEM_CHANNEL));
    assert_eq!(notices[2].2, Some(NOTICE_ROLE));
    let logged = global.text()[before..].to_owned();
    let line = logged
        .lines()
        .find(|line| line.contains("voice notice configured channel failed; trying fallback"))
        .expect("a configured-channel failure warns before falling back");
    assert!(line.starts_with("WARN "), "{line}");
    assert!(line.contains(&format!("guild=\"{GUILD}\"")), "{line}");
    assert!(line.contains("op=\"send_notice\""), "{line}");
    assert!(line.contains("outcome=\"discord\""), "{line}");
    assert!(
        !line.contains("/setup"),
        "the warn carries no notice text: {line}"
    );
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
    // Reconcile may still prune the empty OTHER_ROOM, so look only for
    // interaction callbacks rather than an exact trace.
    let calls = trace.lock().unwrap().clone();
    assert!(
        !calls
            .iter()
            .any(|call| call.as_str() == "defer" || call.as_str() == "complete"),
        "no defer, got {calls:?}"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.as_str() == "respond")
            .count(),
        1,
        "one public ballot, got {calls:?}"
    );
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

/// Every ballot button's custom ID plus its disabled state, in render order.
fn ballot_button_states(response: &InteractionResponse) -> Vec<(String, bool)> {
    let mut states = Vec::new();
    let Some(data) = response.data.as_ref() else {
        return states;
    };
    let Some(components) = data.components.as_ref() else {
        return states;
    };
    for component in components {
        let Component::ActionRow(row) = component else {
            continue;
        };
        for inner in &row.components {
            let Component::Button(button) = inner else {
                continue;
            };
            states.push((
                button.custom_id.clone().unwrap_or_default(),
                button.disabled,
            ));
        }
    }
    states
}

/// Vote-kick open/vote/close round-trip through constructed interactions.
///
/// Drives the real `handle_voice_interaction` arms with a `/kick` slash
/// command followed by Yes-ballot component presses: the open renders the
/// public ballot, the second Yes reaches strict majority (2/3) and closes
/// the vote Passed with disabled buttons, and a further ballot replays the
/// terminal Passed instead of reopening. The queued room-scoped
/// deny/disconnect enforcement is pinned at worker level by
/// `strict_majority_passes_and_enforces_on_that_room_only`; no guild,
/// network, or database here.
#[tokio::test]
async fn vote_kick_open_vote_close_round_trip_through_interactions() {
    const VOTER_B: u64 = 302;
    const VOTER_C: u64 = 304;
    let trace = Trace::default();
    let runtime = kick_room_runtime(
        &trace,
        true,
        vec![
            voice_member_in(KICK_VOTER, KICK_ROOM),
            voice_member_in(VOTER_B, KICK_ROOM),
            voice_member_in(KICK_TARGET, KICK_ROOM),
            voice_member_in(VOTER_C, KICK_ROOM),
        ],
    );

    // Open: /kick from an occupant renders the public ballot.
    let open = kick_sink_interaction(KICK_TARGET, KICK_VOTER);
    let vote_id = open.id.get();
    let (owned, ballot) = handle_capture(&runtime, &open).await;
    assert!(owned, "voice owns the /kick callback");
    let ballot = ballot.expect("ballot reply");
    assert_eq!(
        ballot.kind,
        InteractionResponseType::ChannelMessageWithSource
    );
    let content = response_text(&ballot);
    assert!(content.contains("<@303>"), "{content}");
    assert!(content.contains("too loud"), "{content}");
    assert!(content.contains("2/3 needed"), "{content}");
    assert_eq!(
        ballot_button_states(&ballot),
        vec![
            (vote_button_id(vote_id, VoteBallot::Yes), false),
            (vote_button_id(vote_id, VoteBallot::No), false),
        ]
    );

    // First Yes: still active, buttons live.
    let first = component_interaction(&vote_button_id(vote_id, VoteBallot::Yes), None, KICK_VOTER);
    let (owned, response) = handle_capture(&runtime, &first).await;
    assert!(owned, "voice owns the ballot callback");
    let response = response.expect("first ballot update");
    assert_eq!(response.kind, InteractionResponseType::UpdateMessage);
    let content = response_text(&response);
    assert!(content.contains("2/3 needed"), "{content}");
    assert_eq!(
        ballot_button_states(&response),
        vec![
            (vote_button_id(vote_id, VoteBallot::Yes), false),
            (vote_button_id(vote_id, VoteBallot::No), false),
        ],
        "active ballot keeps both live buttons"
    );

    // Second Yes: strict majority closes the vote Passed, buttons disabled.
    let second = component_interaction(&vote_button_id(vote_id, VoteBallot::Yes), None, VOTER_B);
    let (owned, response) = handle_capture(&runtime, &second).await;
    assert!(owned, "voice owns the closing ballot callback");
    let response = response.expect("closing ballot update");
    assert_eq!(response.kind, InteractionResponseType::UpdateMessage);
    let content = response_text(&response);
    assert!(content.contains("Vote passed"), "{content}");
    assert_eq!(
        ballot_button_states(&response),
        vec![
            (vote_button_id(vote_id, VoteBallot::Yes), true),
            (vote_button_id(vote_id, VoteBallot::No), true),
        ],
        "closed ballot keeps both buttons, disabled"
    );

    // Replay: a further ballot stays terminal instead of reopening.
    let replay = component_interaction(&vote_button_id(vote_id, VoteBallot::Yes), None, VOTER_C);
    let (owned, response) = handle_capture(&runtime, &replay).await;
    assert!(owned, "voice owns the replay ballot callback");
    let response = response.expect("replay update");
    assert_eq!(response.kind, InteractionResponseType::UpdateMessage);
    let content = response_text(&response);
    assert!(content.contains("Vote passed"), "{content}");
    assert_eq!(
        ballot_button_states(&response),
        vec![
            (vote_button_id(vote_id, VoteBallot::Yes), true),
            (vote_button_id(vote_id, VoteBallot::No), true),
        ],
        "replay keeps the closed Passed shell"
    );
}

// ---- lifecycle-outcome signals (offline cutover verification) ----
//
// The worker emits fixed-cardinality outcome signals for every terminal
// create/move/delete, reconcile plan size, queue dead-letter and
// creator-orphan. These tests pin the emission wiring end to end through
// the fake store/HTTP fixtures: pure mapper coverage plus worker-driven
// metric deltas. Global counters are monotonic, so the worker tests assert
// `after >= before + expected`: safe under parallel test threads.

/// Serializes the tests that fire the process-global stuck watchdog with
/// exact `+1` asserts, so their windows never interleave under parallel
/// test threads. Held across the whole test; an async mutex stays silent
/// under clippy while held across awaits.
static STUCK_COUNTER_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
                vote_id: 7_000,
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
        (
            RoomAction::SetUserLimit {
                channel_id: 500,
                user_limit: 4,
            },
            "limit",
        ),
        // Post-move respace repairs share the unknown-shape family so the
        // bounded metric label set stays unchanged (never a create/move/
        // delete, and never failed, so never dead-lettered).
        (
            RoomAction::RespaceChannels {
                room_channel_id: 500,
            },
            "other",
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

fn reconcile_signals(worker: &mut GuildRoomWorker<Store, Http>) -> Vec<String> {
    // Keep callsite interest stable while other test threads also reconcile.
    tracing_capture::global();
    let capture = tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || worker.reconcile());
    capture
        .text()
        .lines()
        .filter(|line| line.contains("voice_event=\"voice_reconcile\""))
        .map(str::to_owned)
        .collect()
}

fn assert_reconcile_signal(signals: &[String], counts: [u64; 4]) {
    assert_eq!(signals.len(), 1, "{signals:?}");
    for (name, count) in [
        "delete_enqueued",
        "suspended",
        "resumed",
        "succession_enqueued",
    ]
    .into_iter()
    .zip(counts)
    {
        assert!(
            signals[0].contains(&format!(" {name}={count}")),
            "{signals:?}"
        );
    }
}

#[tokio::test]
async fn reconcile_reports_access_transitions_once_and_keeps_stable_rooms_quiet() {
    let (live, store, http, _) = owned_room();
    let mut worker = GuildRoomWorker::load(live.clone(), store, http)
        .await
        .unwrap();
    for _ in 0..3 {
        assert!(reconcile_signals(&mut worker).is_empty());
    }

    live.refresh_bot(BotAccess {
        roles: vec![role(Permissions::empty())],
        ..snapshot(&[], vec![]).bot
    });
    let suspended_before =
        global_series("two_bot_voice_reconcile_actions_total{action=\"suspended\"}");
    assert_reconcile_signal(&reconcile_signals(&mut worker), [0, 1, 0, 0]);
    assert!(worker.queue.is_suspended(GUILD, 500));
    assert!(
        global_series("two_bot_voice_reconcile_actions_total{action=\"suspended\"}")
            > suspended_before
    );
    for _ in 0..3 {
        assert!(reconcile_signals(&mut worker).is_empty());
    }

    live.refresh_bot(snapshot(&[], vec![]).bot);
    let resumed_before = global_series("two_bot_voice_reconcile_actions_total{action=\"resumed\"}");
    assert_reconcile_signal(&reconcile_signals(&mut worker), [0, 0, 1, 0]);
    assert!(!worker.queue.is_suspended(GUILD, 500));
    assert!(
        global_series("two_bot_voice_reconcile_actions_total{action=\"resumed\"}") > resumed_before
    );
    for _ in 0..3 {
        assert!(reconcile_signals(&mut worker).is_empty());
    }
}

#[tokio::test]
async fn reconcile_missing_room_only_resumes_a_suspended_lane() {
    for was_suspended in [false, true] {
        let (live, store, http, _) = fixture();
        store.rooms.lock().unwrap().insert(500, room(500));
        let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
        if was_suspended {
            worker.queue.suspend(GUILD, 500);
        }
        assert_reconcile_signal(
            &reconcile_signals(&mut worker),
            [1, 0, u64::from(was_suspended), 0],
        );
        assert!(!worker.queue.is_suspended(GUILD, 500));
        assert!(reconcile_signals(&mut worker).is_empty());
        assert_eq!(worker.queue.pending_counts(GUILD), (1, 0));
    }
}

#[tokio::test]
async fn reconcile_still_reports_succession_without_an_access_transition() {
    let (live, store, http, _) = owned_room();
    live.voice_update(MEMBER, None, Some(false));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    assert_reconcile_signal(&reconcile_signals(&mut worker), [0, 0, 0, 1]);
    assert_eq!(worker.tracked()[&500].owner_id, 301);
    assert!(reconcile_signals(&mut worker).is_empty());
}

#[tokio::test(start_paused = true)]
async fn reconcile_reports_only_new_delete_enqueues() {
    let (live, store, http, _) = fixture();
    store.rooms.lock().unwrap().insert(500, room(500));
    store.rooms.lock().unwrap().insert(501, room(501));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    live.upsert_channel(channel(501, 2, Some(CATEGORY)));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    tokio::time::advance(EMPTY_ROOM_GRACE).await;
    let enqueued_before =
        global_series("two_bot_voice_reconcile_actions_total{action=\"delete_enqueued\"}");
    // Both rooms are accessible and empty, but neither lane was suspended.
    assert_reconcile_signal(&reconcile_signals(&mut worker), [2, 0, 0, 0]);
    assert!(
        global_series("two_bot_voice_reconcile_actions_total{action=\"delete_enqueued\"}")
            >= enqueued_before + 2
    );
    assert!(reconcile_signals(&mut worker).is_empty());
    assert_eq!(worker.queue.pending_counts(GUILD), (2, 0));
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

#[tokio::test]
async fn a_new_room_is_created_with_its_template_name_and_never_renamed() {
    let (live, store, http, trace) = fixture();
    store.creators.lock().unwrap()[0].name_template = "@@owner@@'s den".to_owned();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    worker.live.voice_update(MEMBER, Some(500), Some(false));
    for now in 2..6 {
        worker.refresh_template_names(now);
        worker.dispatch_one(now).await;
    }
    assert_eq!(
        *worker.http.created_names.lock().unwrap(),
        ["new room's den"]
    );
    assert_eq!(
        *trace.lock().unwrap(),
        ["create", "persist:500", "move:300:500"]
    );
}

#[tokio::test]
async fn a_blank_template_creates_the_room_with_its_v1_name() {
    let (live, store, http, _) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    assert_eq!(
        *worker.http.created_names.lock().unwrap(),
        ["new room's room"]
    );
}

#[test]
fn room_playtime_adds_up_member_minutes_per_game() {
    let minute = 60_000;
    let mut playtime = name_panel::RoomPlaytime::default();
    playtime.observe(Some("Apex"), 2, 0);
    assert_eq!(playtime.minutes("Apex", 10 * minute), 20);
    playtime.observe(None, 0, 10 * minute);
    assert_eq!(playtime.minutes("Apex", 60 * minute), 20);
    playtime.observe(Some("Apex"), 1, 60 * minute);
    assert_eq!(playtime.minutes("Apex", 70 * minute), 30);
    assert_eq!(playtime.minutes("Valorant", 70 * minute), 0);
}

thread_local! {
    static TEST_WALL_MS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn test_wall_clock() -> u64 {
    TEST_WALL_MS.get()
}

async fn playtime_worker(template: &str, games: &[&str]) -> GuildRoomWorker<Store, Http> {
    TEST_WALL_MS.set(0);
    let (live, store, http, _) = fixture();
    store.creators.lock().unwrap()[0].name_template = template.to_owned();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    for (index, game) in games.iter().enumerate() {
        let member = MEMBER + index as u64;
        live.voice_update(member, Some(500), Some(false));
        live.set_presence(
            member,
            MemberPresence {
                game: Some((*game).to_owned()),
                ..Default::default()
            },
        );
    }
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.wall_clock = test_wall_clock;
    worker.name_directory.insert(MEMBER, "Alex".to_owned());
    worker
}

#[tokio::test]
async fn room_playtime_counts_only_players_of_the_selected_games() {
    let template = "@@game_name@@ @@game_minutes@@ @@game_tier@@";
    for (games, alias, force_single, shown, expected) in [
        (vec!["Apex", "Apex", "Valorant"], false, false, "Apex", 20),
        (
            vec!["Apex Legends", "apex legends", "Valorant"],
            true,
            false,
            "Apex",
            20,
        ),
        (
            vec!["Apex", "Apex", "Valorant", "Valorant", "Chess"],
            false,
            false,
            "Apex & Valorant",
            40,
        ),
        (
            vec!["Apex", "Valorant", "Valorant"],
            false,
            true,
            "Apex",
            10,
        ),
    ] {
        let mut worker = playtime_worker(template, &games).await;
        if alias {
            worker
                .name_settings
                .aliases
                .push(("Apex Legends".to_owned(), "Apex".to_owned()));
        }
        worker.name_settings.force_single_game = force_single;
        worker.refresh_template_names(0);
        TEST_WALL_MS.set(10 * 60_000);
        worker.refresh_template_names(1);
        assert_eq!(
            worker.playtime[&500].minutes(shown, test_wall_clock()),
            expected
        );
        if !force_single {
            let tier = two_bot_core::voice_naming::minutes_tier(expected);
            assert_eq!(
                worker.desired_names[&500],
                format!("{shown} {expected} {tier}")
            );
        }
    }
}

#[tokio::test]
async fn room_playtime_keeps_tracking_when_automatic_names_are_skipped() {
    let template = "@@game_name@@ @@game_minutes@@ @@game_tier@@";
    for skip in ["override", "blank", "name wait"] {
        let mut worker = playtime_worker(template, &["Apex", "Apex"]).await;
        worker.refresh_template_names(0);
        TEST_WALL_MS.set(3 * 60_000);
        worker.refresh_template_names(1);
        match skip {
            "override" => {
                worker.custom_names.insert(500, "custom".to_owned());
            }
            "blank" => worker
                .creators
                .get_mut(&CREATOR)
                .unwrap()
                .name_template
                .clear(),
            _ => worker.name_directory = NameDirectory::default(),
        }
        TEST_WALL_MS.set(4 * 60_000);
        worker.live.set_presence(MEMBER, MemberPresence::default());
        worker
            .live
            .set_presence(MEMBER + 1, MemberPresence::default());
        worker.refresh_template_names(2);
        TEST_WALL_MS.set(124 * 60_000);
        worker.refresh_template_names(3);
        assert_eq!(
            worker.playtime[&500].minutes("Apex", test_wall_clock()),
            8,
            "{skip}"
        );
        worker.custom_names.remove(&500);
        worker.creators.get_mut(&CREATOR).unwrap().name_template = template.to_owned();
        worker.name_directory.insert(MEMBER, "Alex".to_owned());
        worker.live.set_presence(
            MEMBER,
            MemberPresence {
                game: Some("Apex".to_owned()),
                ..Default::default()
            },
        );
        worker.refresh_template_names(4);
        TEST_WALL_MS.set(127 * 60_000);
        worker.refresh_template_names(5);
        assert_eq!(
            worker.playtime[&500].minutes("Apex", test_wall_clock()),
            11,
            "{skip}"
        );
    }
}

#[tokio::test]
async fn room_playtime_discards_unobserved_gateway_and_halt_gaps() {
    for gap in ["halt", "gateway", "reconnect between passes"] {
        let mut worker = playtime_worker("@@game_minutes@@ @@game_tier@@", &["Apex"]).await;
        worker.refresh_template_names(0);
        TEST_WALL_MS.set(10 * 60_000);
        worker.refresh_template_names(1);
        TEST_WALL_MS.set(11 * 60_000);
        if gap == "halt" {
            worker.halted = true;
        } else {
            worker.live.disconnect();
        }
        if gap != "reconnect between passes" {
            worker.refresh_template_names(2);
        }
        TEST_WALL_MS.set(131 * 60_000);
        worker.halted = false;
        worker.live.write_state().ready = true;
        worker.refresh_template_names(3);
        assert_eq!(worker.playtime[&500].minutes("Apex", test_wall_clock()), 10);
        TEST_WALL_MS.set(137 * 60_000);
        worker.refresh_template_names(4);
        assert_eq!(worker.desired_names[&500], "16 1");
        assert_eq!(worker.playtime[&500].minutes("Apex", test_wall_clock()), 16);
    }
}

#[tokio::test]
async fn a_room_tier_change_renames_the_room_once() {
    let created = two_bot_core::funnel::parse_iso_millis(NOW).unwrap() as u64;
    let minute = 60_000;
    let (live, store, http, trace) = fixture();
    store.creators.lock().unwrap()[0].name_template =
        "{{@@room_tier@@ >= 1 ?? veterans // fresh}}".to_owned();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    live.voice_update(MEMBER, Some(500), Some(false));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.wall_clock = test_wall_clock;
    worker.name_directory.insert(MEMBER, "Alex".to_owned());
    TEST_WALL_MS.set(created + minute);
    worker.refresh_template_names(0);
    dispatch(&mut worker, 0).await;
    // Minutes pass inside the tier: no new name.
    TEST_WALL_MS.set(created + 10 * minute);
    worker.refresh_template_names(1);
    assert!(!worker.dispatch_one(1).await);
    TEST_WALL_MS.set(created + 16 * minute);
    worker.refresh_template_names(2);
    assert_eq!(worker.desired_names[&500], "veterans");
    dispatch(&mut worker, 600_000).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["rename:500:fresh", "rename:500:veterans"]
    );
}

fn first_names(worker: &GuildRoomWorker<Store, Http>, template: &str) -> Vec<String> {
    let mut creations: Vec<_> = worker.creations.iter().collect();
    creations.sort_unstable_by_key(|(id, _)| **id);
    creations
        .into_iter()
        .map(|(_, creation)| {
            let context = two_bot_core::voice_naming::RoomContext {
                seed: creation.spec.seed,
                ..Default::default()
            };
            two_bot_core::voice_naming::render_str(template, &context)
        })
        .collect()
}

#[tokio::test]
async fn consecutive_rooms_never_repeat_a_recent_first_name() {
    let template = "[[a/b/c/d/e/f]]";
    let (live, store, http, _) = fixture();
    store.creators.lock().unwrap()[0].name_template = template.to_owned();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    // Every join draws the same seed, so only the fresh-name pick can
    // tell these rooms apart.
    for member in 0..12 {
        join(&mut worker, MEMBER + member);
    }
    let names = first_names(&worker, template);
    assert_eq!(names.len(), 12);
    for (index, name) in names.iter().enumerate() {
        let recent = &names[index.saturating_sub(name_panel::RECENT_NAME_MEMORY)..index];
        assert!(!recent.contains(name), "{index}: {name} repeats {recent:?}");
    }
}

#[tokio::test]
async fn a_pool_too_small_to_vary_keeps_the_drawn_seed() {
    let (live, store, http, _) = fixture();
    store.creators.lock().unwrap()[0].name_template = "[[only]]".to_owned();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    join(&mut worker, MEMBER + 1);
    let seeds: Vec<u64> = worker.creations.values().map(|c| c.spec.seed).collect();
    assert_eq!(seeds, [7, 7]);
}

#[tokio::test]
async fn voice_name_reload_rerenders_unchanged_facts_after_list_edit() {
    let (live, mut store, http, trace) = fixture();
    store.snapshot_uses_creator_rows = false;
    let mut config = full_config();
    config.creators[0].name_template = "[[list:maps]]".to_owned();
    config.lists[0].choices = vec!["old".to_owned()];
    store.creators.lock().unwrap()[0].name_template = config.creators[0].name_template.clone();
    *store.config.lock().unwrap() = config;
    slow_rename_room(&live, &store);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.wall_clock = || 0;
    worker.name_directory.insert(MEMBER, "Alex".to_owned());
    worker.reload_name_settings(0).await;
    worker.refresh_template_names(0);
    dispatch(&mut worker, 0).await;
    let signatures = worker.name_signatures.clone();

    worker.store.config.lock().unwrap().lists[0].choices = vec!["new".to_owned()];
    worker.reload_name_settings(299_999).await;
    worker.refresh_template_names(299_999);
    assert_eq!(worker.desired_names[&700], "old", "reload remains paced");
    worker.reload_name_settings(300_000).await;
    worker.refresh_template_names(300_000);
    assert_eq!(
        worker.name_signatures, signatures,
        "room facts did not change"
    );
    assert_eq!(worker.desired_names[&700], "new");
    dispatch(&mut worker, 300_000).await;
    assert_eq!(renames_sent(&trace), ["rename:700:old", "rename:700:new"]);

    // An equal snapshot must not invalidate a successfully rendered room.
    worker.reload_name_settings(600_000).await;
    assert_eq!(worker.name_signatures, signatures);
    worker.refresh_template_names(600_000);
    assert!(!worker.dispatch_one(600_000).await);
}

#[tokio::test]
async fn voice_name_reload_refreshes_creator_rows_from_the_same_snapshot() {
    let (live, mut store, http, trace) = fixture();
    store.snapshot_uses_creator_rows = false;
    let mut config = full_config();
    config.creators[0].name_template = "old".to_owned();
    store.creators.lock().unwrap()[0].name_template = "old".to_owned();
    store.creators.lock().unwrap()[0].text_channel_name = Some("keep-chat".to_owned());
    store.creators.lock().unwrap()[0].text_viewer_role_id = Some(123);
    *store.config.lock().unwrap() = config;
    slow_rename_room(&live, &store);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.wall_clock = || 0;
    worker.name_directory.insert(MEMBER, "Alex".to_owned());
    worker.reload_name_settings(0).await;
    worker.refresh_template_names(0);
    dispatch(&mut worker, 0).await;
    {
        let mut config = worker.store.config.lock().unwrap();
        let creator = &mut config.creators[0];
        creator.name_template = "new".to_owned();
        creator.status_template = Some("new status".to_owned());
        creator.position = config_codec::RoomPosition::Below;
        creator.permission_source = config_codec::PermissionSource::Channel {
            channel_id: CATEGORY.to_string(),
        };
        creator.always_private = true;
        creator.text_channels = true;
        creator.group_by_category = true;
        let mut added = creator.clone();
        added.channel_id = "201".to_owned();
        added.default_limit = 5;
        added.first_number = 10;
        config.creators.push(added);
    }
    worker.reload_name_settings(300_000).await;
    worker.refresh_template_names(300_000);
    assert_eq!(
        worker.desired_names[&700], "new",
        "template-only edit must render"
    );
    assert_eq!(worker.pending_status[&700], "new status");
    let creator = &worker.creators[&CREATOR];
    assert_eq!(
        creator.position,
        two_bot_core::voice_rooms::RoomPosition::Below
    );
    assert_eq!(
        creator.permission_source,
        PermissionSource::Channel(CATEGORY)
    );
    assert_eq!(creator.permission_channel_id, Some(CATEGORY));
    assert!(creator.private_default && creator.text_channels && creator.group_by_category);
    assert_eq!(
        creator.default_limit, None,
        "the codec's zero preserves inheritance"
    );
    assert_eq!(creator.text_channel_name.as_deref(), Some("keep-chat"));
    assert_eq!(creator.text_viewer_role_id, Some(123));
    assert_eq!(worker.creators[&201].default_limit, Some(5));
    assert_eq!(worker.creators[&201].first_room_number, 10);
    assert!(worker.live.read_state().protected_channels.contains(&201));
    dispatch(&mut worker, 300_000).await;
    assert_eq!(renames_sent(&trace), ["rename:700:old", "rename:700:new"]);

    worker.store.config.lock().unwrap().creators.clear();
    worker.reload_name_settings(600_000).await;
    assert!(
        worker.creators.is_empty(),
        "removed creators stop admitting joins"
    );
}

#[tokio::test]
async fn voice_name_settings_notification_invalidates_cached_renders() {
    let (live, store, http, trace) = fixture();
    store.creators.lock().unwrap()[0].name_template = "[[list:maps]]".to_owned();
    store.config.lock().unwrap().lists = vec![config_codec::RandomList {
        name: "maps".to_owned(),
        choices: vec!["old".to_owned()],
    }];
    slow_rename_room(&live, &store);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.wall_clock = || 0;
    worker.name_directory.insert(MEMBER, "Alex".to_owned());
    worker.refresh_template_names(0);
    dispatch(&mut worker, 0).await;
    let mut settings = worker.name_settings.clone();
    settings
        .lists
        .insert("maps".to_owned(), vec!["new".to_owned()]);
    apply_command(
        &mut worker,
        ActorCommand::NameSettingsChanged(settings),
        1,
        &ActorInboxBackpressure::default(),
    );
    worker.refresh_template_names(1);
    assert_eq!(worker.desired_names[&700], "new");
    assert!(
        !worker.dispatch_one(1).await,
        "normal rename pacing still applies"
    );
    dispatch(&mut worker, RENAME_MIN_INTERVAL_MS).await;
    assert_eq!(renames_sent(&trace), ["rename:700:old", "rename:700:new"]);
}

#[tokio::test]
async fn voice_name_import_refreshes_creator_template_without_waiting_for_reload() {
    let (live, mut store, http, trace) = fixture();
    store.snapshot_uses_creator_rows = false;
    let mut config = full_config();
    config.creators[0].name_template = "old".to_owned();
    store.creators.lock().unwrap()[0].name_template = "old".to_owned();
    *store.config.lock().unwrap() = config;
    slow_rename_room(&live, &store);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.wall_clock = || 0;
    worker.name_directory.insert(MEMBER, "Alex".to_owned());
    worker.reload_name_settings(0).await;
    worker.refresh_template_names(0);
    dispatch(&mut worker, 0).await;
    assert_eq!(worker.desired_names[&700], "old");

    // Simulate a successful in-bot /import that changes only the creator template.
    worker.store.config.lock().unwrap().creators[0].name_template = "new".to_owned();
    let settings = {
        let config = worker.store.config.lock().unwrap().clone();
        NameSettings::from_config(&config)
    };
    apply_command(
        &mut worker,
        ActorCommand::NameSettingsChanged(settings),
        100_000,
        &ActorInboxBackpressure::default(),
    );
    assert_eq!(
        worker.name_settings_read_ms, None,
        "import must not push the creator reload a full interval out"
    );

    // The next tick must pick up the creator edit, not wait for the reload interval.
    worker.reload_name_settings(100_001).await;
    worker.refresh_template_names(100_001);
    assert_eq!(
        worker.creators[&CREATOR].name_template, "new",
        "imported creator template must refresh on the next tick"
    );
    assert_eq!(worker.desired_names[&700], "new");
    assert!(
        !worker.dispatch_one(100_001).await,
        "normal rename pacing still applies"
    );
    dispatch(&mut worker, 400_000).await;
    assert_eq!(renames_sent(&trace), ["rename:700:old", "rename:700:new"]);
}

#[tokio::test]
async fn voice_name_creator_notification_invalidates_cached_renders() {
    let (live, store, http, trace) = fixture();
    store.creators.lock().unwrap()[0].name_template = "old".to_owned();
    slow_rename_room(&live, &store);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.wall_clock = || 0;
    worker.name_directory.insert(MEMBER, "Alex".to_owned());
    worker.refresh_template_names(0);
    dispatch(&mut worker, 0).await;
    let mut creator = worker.creators[&CREATOR].clone();
    creator.name_template = "new".to_owned();
    apply_command(
        &mut worker,
        ActorCommand::CreatorAdded(creator),
        1,
        &ActorInboxBackpressure::default(),
    );
    worker.refresh_template_names(1);
    assert_eq!(worker.desired_names[&700], "new");
    assert!(!worker.dispatch_one(1).await);
    dispatch(&mut worker, RENAME_MIN_INTERVAL_MS).await;
    assert_eq!(renames_sent(&trace), ["rename:700:old", "rename:700:new"]);
}

#[tokio::test]
async fn template_names_rerender_only_when_their_facts_change() {
    let (live, store, http, trace) = fixture();
    store.creators.lock().unwrap()[0].name_template = "@@owner@@ · @@num@@".to_owned();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    live.voice_update(MEMBER, Some(500), Some(false));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.name_directory.insert(MEMBER, "Alex".to_owned());
    worker.refresh_template_names(0);
    dispatch(&mut worker, 0).await;
    // Nothing changed: no new proposal, nothing to dispatch.
    worker.refresh_template_names(1);
    assert!(!worker.dispatch_one(1).await);
    worker.live.voice_update(MEMBER + 1, Some(500), Some(false));
    worker.refresh_template_names(2);
    assert_eq!(worker.desired_names[&500], "Alex · 2");
    // The rename lane paces the second rename within Discord's budget.
    dispatch(&mut worker, 600_000).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["rename:500:Alex · 1", "rename:500:Alex · 2"]
    );
}

#[tokio::test]
async fn a_name_override_or_a_blank_template_is_never_replaced() {
    let (live, store, http, trace) = fixture();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    live.voice_update(MEMBER, Some(500), Some(false));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    // The fixture creator has a blank template: the room keeps its name.
    worker.refresh_template_names(0);
    assert!(!worker.dispatch_one(0).await);
    let mut creator = worker.creators[&CREATOR].clone();
    creator.name_template = "@@owner@@'s den".to_owned();
    worker.creators.insert(CREATOR, creator);
    worker.custom_names.insert(500, "my room".to_owned());
    worker.refresh_template_names(1);
    assert!(!worker.dispatch_one(1).await);
    assert!(trace.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_voice_display_update_reaches_the_owner_token() {
    let (live, store, http, trace) = fixture();
    store.creators.lock().unwrap()[0].name_template = "@@owner@@".to_owned();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    live.voice_update(MEMBER, Some(500), Some(false));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    apply_command(
        &mut worker,
        ActorCommand::Display {
            member_id: MEMBER,
            display: "Sam".to_owned(),
        },
        0,
        &ActorInboxBackpressure::default(),
    );
    worker.refresh_template_names(0);
    dispatch(&mut worker, 0).await;
    assert_eq!(*trace.lock().unwrap(), ["rename:500:Sam"]);
}

#[tokio::test]
async fn a_restarted_worker_waits_for_the_owner_name_instead_of_renaming_to_member() {
    let (live, store, http, trace) = fixture();
    store.creators.lock().unwrap()[0].name_template = "@@owner@@'s den".to_owned();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    live.voice_update(MEMBER, Some(500), Some(false));
    // A fresh worker has no display names yet.
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.refresh_template_names(0);
    assert!(!worker.dispatch_one(0).await);
    // The GuildCreate seeding (or the owner's next voice event) supplies it.
    apply_command(
        &mut worker,
        ActorCommand::Display {
            member_id: MEMBER,
            display: "Alex".to_owned(),
        },
        1,
        &ActorInboxBackpressure::default(),
    );
    worker.refresh_template_names(1);
    dispatch(&mut worker, 1).await;
    assert_eq!(*trace.lock().unwrap(), ["rename:500:Alex's den"]);
}

#[tokio::test]
async fn written_settings_reach_automatic_names() {
    let (live, store, http, _trace) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let mut config = empty_config();
    config.settings.no_game_label = "Hangout".to_owned();
    apply_command(
        &mut worker,
        ActorCommand::NameSettingsChanged(NameSettings::from_config(&config)),
        5,
        &ActorInboxBackpressure::default(),
    );
    assert_eq!(worker.name_settings.no_game_label, "Hangout");
    assert!(worker.name_settings_loaded);
    // The stamp stays clear so the next tick re-reads creators too; an
    // /import must not push their refresh a full interval out.
    assert_eq!(worker.name_settings_read_ms, None);
}

#[tokio::test]
async fn an_unknown_original_creator_name_waits_instead_of_rendering_member() {
    let (live, store, http, trace) = fixture();
    store.creators.lock().unwrap()[0].name_template =
        "@@owner@@ from @@ORIGINAL_CREATOR@@".to_owned();
    let mut handed = room(500);
    handed.owner_id = MEMBER + 1;
    store.rooms.lock().unwrap().insert(500, handed);
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    live.voice_update(MEMBER + 1, Some(500), Some(false));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.name_directory.insert(MEMBER + 1, "Sam".to_owned());
    worker.refresh_template_names(0);
    assert!(!worker.dispatch_one(0).await);
    worker.name_directory.insert(MEMBER, "Alex".to_owned());
    worker.refresh_template_names(1);
    dispatch(&mut worker, 1).await;
    assert_eq!(*trace.lock().unwrap(), ["rename:500:Sam from Alex"]);
}

#[tokio::test]
async fn a_confirmed_import_hands_its_settings_to_the_worker() {
    let trace = Trace::default();
    let bytes = serde_json::to_vec(&full_config()).unwrap();
    let (runtime, _) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let (confirm_id, _) = preview_buttons(&preview.expect("preview"));
    // The worker has loaded and done its first settings read before the
    // write, so only the handoff can deliver the imported settings now (the
    // periodic reload is minutes away).
    let (before, _) = runtime.naming_probe(GUILD).await.expect("probe");
    assert_eq!(before, NameSettings::from_config(&empty_config()));
    let confirm = component_interaction(&confirm_id, manager(), UPLOADER);
    let (_, response) = handle_import_capture(&runtime, &confirm, Some(&inventory)).await;
    assert!(response_text(&response.expect("applied")).starts_with("Import applied:"));
    let (settings, _) = runtime.naming_probe(GUILD).await.expect("probe");
    assert_eq!(settings, NameSettings::from_config(&full_config()));
}

#[tokio::test]
async fn an_unknown_owner_name_waits_only_a_bounded_time() {
    let (live, store, http, trace) = fixture();
    store.creators.lock().unwrap()[0].name_template = "@@owner@@'s den".to_owned();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    live.voice_update(MEMBER + 1, Some(500), Some(false));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.refresh_template_names(0);
    assert!(!worker.dispatch_one(0).await);
    worker.refresh_template_names(name_panel::NAME_WAIT_MS);
    dispatch(&mut worker, name_panel::NAME_WAIT_MS).await;
    assert_eq!(*trace.lock().unwrap(), ["rename:500:member's den"]);
}

#[tokio::test]
async fn a_game_or_stream_change_rerenders_the_room_name() {
    use two_bot_core::voice_presence::MemberPresence;
    let (live, store, http, trace) = fixture();
    store.creators.lock().unwrap()[0].name_template =
        "{{PLAYING ?? @@game_name@@ // Hangout}}{{ANY_LIVE ?? 🔴}}".to_owned();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    live.voice_update(MEMBER, Some(500), Some(false));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.name_directory.insert(MEMBER, "Alex".to_owned());
    worker.name_settings.no_game_label = "Hangout".to_owned();
    worker.refresh_template_names(0);
    dispatch(&mut worker, 0).await;
    // The owner starts a game: the presence frame reports the room.
    let room = worker.live.set_presence(
        MEMBER,
        MemberPresence {
            game: Some("Apex Legends".to_owned()),
            ..MemberPresence::default()
        },
    );
    assert_eq!(room, Some(500));
    worker.room_facts_changed(500, 1);
    tracing_capture::global();
    let capture = tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || worker.refresh_template_names(1));
    let decision_log = capture.text();
    // The decision line carries counts and booleans only: the detected
    // game title and the rendered name are member-derived text and must
    // never reach the logs.
    assert!(!decision_log.contains("Apex"), "{decision_log}");
    assert!(decision_log.contains("playing_members=1"), "{decision_log}");
    assert!(
        decision_log.contains("activity_detected=true"),
        "{decision_log}"
    );
    assert!(
        decision_log.contains("decision=\"queued\""),
        "{decision_log}"
    );
    assert_eq!(worker.desired_names[&500], "Apex Legends");
    // Going live through Discord adds the live marker on the next render.
    assert_eq!(worker.live.set_self_stream(MEMBER, true), Some(500));
    worker.room_facts_changed(500, 2);
    worker.refresh_template_names(2);
    assert_eq!(worker.desired_names[&500], "Apex Legends 🔴");
    // The rename lane coalesces both changes into one paced rename.
    dispatch(&mut worker, 600_000).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["rename:500:Hangout", "rename:500:Apex Legends 🔴"]
    );
}

#[test]
fn rename_decision_logs_carry_bounded_fields_at_each_level() {
    fn line<'a>(log: &'a str, decision: &str) -> &'a str {
        log.lines()
            .find(|line| line.contains(&format!("decision=\"{decision}\"")))
            .unwrap_or_else(|| panic!("missing decision {decision} in:\n{log}"))
    }
    tracing_capture::global();
    let capture = tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || {
        log_rename_decision(GUILD, 500, "attempted", "", 0, false);
        log_rename_decision(
            GUILD,
            500,
            "permission_denied",
            "rename_no_access",
            0,
            false,
        );
        log_rename_decision(GUILD, 500, "rate_limited", "", 1_500, true);
        log_template_name_decision(GUILD, 500, 2, "queued");
        log_template_name_decision(GUILD, 500, 0, "channel_unseen");
        log_template_name_decision(GUILD, 500, 1, "template_blocked");
    });
    let log = capture.text();
    let attempted = line(&log, "attempted");
    assert!(attempted.starts_with("INFO "), "{attempted}");
    assert!(attempted.contains("guild=100 channel=500"), "{attempted}");
    let denied = line(&log, "permission_denied");
    assert!(denied.starts_with("INFO "), "{denied}");
    assert!(denied.contains("reason=\"rename_no_access\""), "{denied}");
    let limited = line(&log, "rate_limited");
    assert!(limited.starts_with("WARN "), "{limited}");
    assert!(limited.contains("retry_after_ms=1500"), "{limited}");
    let queued = line(&log, "queued");
    assert!(queued.starts_with("INFO "), "{queued}");
    assert!(
        queued.contains("playing_members=2 activity_detected=true"),
        "{queued}"
    );
    let unseen = line(&log, "channel_unseen");
    assert!(unseen.starts_with("DEBUG "), "{unseen}");
    assert!(
        unseen.contains("playing_members=0 activity_detected=false"),
        "{unseen}"
    );
    let blocked = line(&log, "template_blocked");
    assert!(blocked.starts_with("INFO "), "{blocked}");
    assert!(
        blocked.contains("playing_members=1 activity_detected=true"),
        "{blocked}"
    );
}

#[tokio::test]
async fn unseen_template_channel_logs_channel_unseen_at_debug() {
    let (live, store, http, _trace) = fixture();
    store.creators.lock().unwrap()[0].name_template = "@@owner@@'s den".to_owned();
    store.rooms.lock().unwrap().insert(500, room(500));
    // No live channel 500: the snapshot cannot see it yet, so the naming
    // pass retries on a later pass.
    live.voice_update(MEMBER, Some(500), Some(false));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.name_directory.insert(MEMBER, "Alex".to_owned());
    tracing_capture::global();
    let capture = tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || worker.refresh_template_names(0));
    let decision_log = capture.text();
    assert!(decision_log.contains("DEBUG "), "{decision_log}");
    assert!(
        decision_log.contains("decision=\"channel_unseen\""),
        "{decision_log}"
    );
    assert!(!decision_log.contains("Alex"), "{decision_log}");
    assert!(worker.name_inputs.is_none());
}

#[tokio::test]
async fn blocked_template_logs_template_blocked_without_member_text() {
    let (live, store, http, _trace) = fixture();
    store.creators.lock().unwrap()[0].name_template = "nasty den".to_owned();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    live.voice_update(MEMBER, Some(500), Some(false));
    let mut worker = GuildRoomWorker::load(live, store, http)
        .await
        .unwrap()
        .with_name_policy(Arc::new(fixture_policy(&["nasty"])));
    worker.name_directory.insert(MEMBER, "Alex".to_owned());
    tracing_capture::global();
    let capture = tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || worker.refresh_template_names(0));
    let decision_log = capture.text();
    assert!(
        decision_log.contains("decision=\"template_blocked\""),
        "{decision_log}"
    );
    assert!(!decision_log.contains("Alex"), "{decision_log}");
    assert!(worker.name_signatures.contains_key(&500));
}

#[tokio::test]
async fn unchanged_template_name_logs_unchanged() {
    let (live, store, http, _trace) = fixture();
    store.creators.lock().unwrap()[0].name_template =
        "{{PLAYING ?? @@game_name@@ // Hangout}}".to_owned();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    live.voice_update(MEMBER, Some(500), Some(false));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.name_directory.insert(MEMBER, "Alex".to_owned());
    worker
        .live
        .write_state()
        .channels
        .get_mut(&500)
        .unwrap()
        .name = Some("Hangout".to_owned());
    tracing_capture::global();
    let capture = tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || worker.refresh_template_names(0));
    let decision_log = capture.text();
    assert!(
        decision_log.contains("decision=\"unchanged\""),
        "{decision_log}"
    );
    assert!(!decision_log.contains("Alex"), "{decision_log}");
    assert_eq!(worker.desired_names[&500], "Hangout");
}

#[tokio::test]
async fn rename_without_access_logs_permission_denied() {
    let (live, store, http, trace) = fixture();
    slow_rename_room(&live, &store);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.live.upsert_channel(channel_with_overwrites(
        700,
        2,
        Some(CATEGORY),
        json!([everyone_overwrite(Permissions::MANAGE_CHANNELS)]),
    ));
    worker.desired_names.insert(700, "B".to_owned());
    worker.queue.enqueue(
        GUILD,
        RoomAction::RenameRoom {
            channel_id: 700,
            name: "B".to_owned(),
        },
    );
    let global = tracing_capture::global();
    let before = global.text().len();
    dispatch(&mut worker, 0).await;
    let decision_log = global.text()[before..].to_owned();
    assert!(
        decision_log.contains("decision=\"permission_denied\""),
        "{decision_log}"
    );
    assert!(
        decision_log.contains("reason=\"rename_no_access\""),
        "{decision_log}"
    );
    assert!(renames_sent(&trace).is_empty());
}

#[tokio::test]
async fn inline_rename_errors_log_warn_decisions() {
    for (error, decision, extra) in [
        (
            RoomHttpError::RateLimited {
                retry_after_ms: 5_000,
                global: false,
            },
            "rate_limited",
            "retry_after_ms=5000",
        ),
        (RoomHttpError::RenameDeferred, "outcome_deferred", ""),
        (RoomHttpError::UnknownOutcome, "outcome_unknown", ""),
        (RoomHttpError::AccessDenied, "permission_denied", ""),
        (RoomHttpError::Unauthorized, "discord_error", ""),
    ] {
        let (live, store, http, trace) = fixture();
        slow_rename_room(&live, &store);
        http.rename_errors.lock().unwrap().push_back(error);
        let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
        worker.propose_name(700, "B", 0);
        let global = tracing_capture::global();
        let before = global.text().len();
        dispatch(&mut worker, 0).await;
        let decision_log = global.text()[before..].to_owned();
        let expected = format!("decision=\"{decision}\"");
        assert!(decision_log.contains(&expected), "{decision_log}");
        let line = decision_log
            .lines()
            .find(|line| line.contains(&expected))
            .unwrap();
        assert!(line.starts_with("WARN "), "{line}");
        if !extra.is_empty() {
            assert!(line.contains(extra), "{line}");
        }
        assert_eq!(renames_sent(&trace), ["rename:700:B"]);
    }
}

#[tokio::test(start_paused = true)]
async fn detached_rename_errors_log_warn_decisions_on_collect() {
    for (error, decision) in [
        (
            RoomHttpError::RateLimited {
                retry_after_ms: 5_000,
                global: false,
            },
            "rate_limited",
        ),
        (RoomHttpError::UnknownOutcome, "outcome_unknown"),
        (RoomHttpError::AccessDenied, "permission_denied"),
        (RoomHttpError::Unauthorized, "discord_error"),
    ] {
        let (live, store, http, trace) = fixture();
        slow_rename_room(&live, &store);
        *http.slow_renames.lock().unwrap() = Some(5_000);
        http.rename_errors.lock().unwrap().push_back(error);
        let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
        worker.propose_name(700, "B", 0);
        dispatch(&mut worker, 0).await;
        finish_detached_after(Duration::from_secs(5)).await;
        let global = tracing_capture::global();
        let before = global.text().len();
        worker.dispatch_one(6_000).await;
        let decision_log = global.text()[before..].to_owned();
        let expected = format!("decision=\"{decision}\"");
        assert!(decision_log.contains(&expected), "{decision_log}");
        let line = decision_log
            .lines()
            .find(|line| line.contains(&expected))
            .unwrap();
        assert!(line.starts_with("WARN "), "{line}");
        assert_eq!(renames_sent(&trace), ["rename:700:B"]);
    }
}

#[tokio::test]
async fn a_rename_that_timed_out_retries_within_seconds_not_minutes() {
    let (live, store, http, trace) = fixture();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    live.voice_update(MEMBER, Some(500), Some(false));
    http.rename_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::RenameDeferred);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.propose_name(500, "templated", 0);
    dispatch(&mut worker, 0).await;
    assert!(!worker.dispatch_one(RENAME_DEFERRED_RETRY_MS - 1).await);
    // The inline path adds the call's real duration to the deadline.
    dispatch(&mut worker, RENAME_DEFERRED_RETRY_MS + 1_000).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["rename:500:templated", "rename:500:templated"]
    );
}

fn slow_rename_room(live: &LiveGuild, store: &Store) {
    store.rooms.lock().unwrap().insert(700, room(700));
    live.upsert_channel(channel(700, 2, Some(CATEGORY)));
    live.voice_update(MEMBER, Some(700), Some(false));
}

fn renames_sent(trace: &Trace) -> Vec<String> {
    trace
        .lock()
        .unwrap()
        .iter()
        .filter(|entry| entry.starts_with("rename:"))
        .cloned()
        .collect()
}

// Start owned HTTP futures before advancing their deadlines, then let both
// the request and its outcome relay finish without introducing actor delays.
async fn finish_detached_after(delay: Duration) {
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(delay).await;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn voice_name_obsolete_detached_retry_cannot_replace_a_newer_queued_name() {
    let (live, store, http, trace) = fixture();
    slow_rename_room(&live, &store);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.desired_names.insert(700, "A".to_owned());
    // Deliver a detached timeout through the real completion path.
    worker
        .rename_outcomes
        .0
        .send(RenameOutcome {
            channel_id: 700,
            name: "A".to_owned(),
            result: Err(RoomHttpError::RenameDeferred),
        })
        .unwrap();
    assert!(!worker.dispatch_one(0).await);
    assert_eq!(worker.rename_retries[&700].1, RENAME_DEFERRED_RETRY_MS);

    worker.propose_name(700, "B", 1);
    // B has left the coalescer but waits in the deferred queue behind backoff.
    for (channel_id, name) in worker.renames.take_due(1) {
        worker
            .queue
            .enqueue(GUILD, RoomAction::RenameRoom { channel_id, name });
    }
    let queued = worker.queue.pop_due(GUILD, 1).unwrap();
    worker.queue.mark_rate_limited(GUILD, 30_000, 1, queued);
    assert!(!worker.dispatch_one(RENAME_DEFERRED_RETRY_MS).await);
    assert_eq!(
        worker.queue.queued_rename_for(GUILD, 700).as_deref(),
        Some("B")
    );
    assert!(!worker.rename_retries.contains_key(&700));
    dispatch(&mut worker, 30_001).await;
    assert_eq!(renames_sent(&trace), ["rename:700:B"]);
}

#[tokio::test]
async fn voice_name_a_b_a_cancellation_sends_no_needless_http() {
    let (live, store, http, trace) = fixture();
    slow_rename_room(&live, &store);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker
        .live
        .write_state()
        .channels
        .get_mut(&700)
        .unwrap()
        .name = Some("A".to_owned());
    worker.propose_name(700, "B", 0);
    for (channel_id, name) in worker.renames.take_due(0) {
        worker
            .queue
            .enqueue(GUILD, RoomAction::RenameRoom { channel_id, name });
    }
    worker.rename_retries.insert(700, ("A".to_owned(), 1));
    assert_eq!(
        worker.propose_name(700, "A", 1),
        Some(ProposeOutcome::Unchanged)
    );
    dispatch(&mut worker, 1).await;
    assert!(!worker.dispatch_one(RENAME_MIN_INTERVAL_MS).await);
    assert!(renames_sent(&trace).is_empty());
}

#[tokio::test]
async fn voice_name_dispatch_distinguishes_gone_superseded_and_already_applied() {
    for (reason, tracked, desired, current) in [
        ("rename_room_gone", false, "B", "A"),
        ("rename_superseded", true, "C", "A"),
        ("rename_already_applied", true, "B", "B"),
    ] {
        let (live, store, http, trace) = fixture();
        slow_rename_room(&live, &store);
        let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
        if !tracked {
            worker.rooms.remove(&700);
        }
        worker
            .live
            .write_state()
            .channels
            .get_mut(&700)
            .unwrap()
            .name = Some(current.to_owned());
        worker.desired_names.insert(700, desired.to_owned());
        worker.queue.enqueue(
            GUILD,
            RoomAction::RenameRoom {
                channel_id: 700,
                name: "B".to_owned(),
            },
        );
        let series = format!("two_bot_voice_names_total{{outcome=\"{reason}\"}}");
        let before = global_series(&series);
        dispatch(&mut worker, 0).await;
        assert!(global_series(&series) > before, "missing outcome: {reason}");
        assert!(renames_sent(&trace).is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn detached_rename_timeouts_retry_in_seconds_and_back_off() {
    let (live, store, http, trace) = fixture();
    slow_rename_room(&live, &store);
    // Every attempt takes Discord's full 10 s; the first two time out.
    *http.slow_renames.lock().unwrap() = Some(10_000);
    for _ in 0..2 {
        http.rename_errors
            .lock()
            .unwrap()
            .push_back(RoomHttpError::RenameDeferred);
    }
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.propose_name(700, "slow", 0);
    dispatch(&mut worker, 0).await;
    finish_detached_after(Duration::from_secs(10)).await;
    // Collect the completed timeout: first retry 15 s later.
    assert!(!worker.dispatch_one(11_000).await);
    assert!(
        !worker
            .dispatch_one(11_000 + RENAME_DEFERRED_RETRY_MS - 1)
            .await
    );
    dispatch(&mut worker, 11_000 + RENAME_DEFERRED_RETRY_MS).await;
    finish_detached_after(Duration::from_secs(10)).await;
    // The second timeout doubles the wait.
    let back = 11_000 + RENAME_DEFERRED_RETRY_MS + 11_000;
    assert!(!worker.dispatch_one(back).await);
    assert!(
        !worker
            .dispatch_one(back + 2 * RENAME_DEFERRED_RETRY_MS - 1)
            .await
    );
    dispatch(&mut worker, back + 2 * RENAME_DEFERRED_RETRY_MS).await;
    finish_detached_after(Duration::from_secs(10)).await;
    worker
        .dispatch_one(back + 2 * RENAME_DEFERRED_RETRY_MS + 11_000)
        .await;
    assert_eq!(renames_sent(&trace).len(), 3);
    assert_eq!(
        worker.live.read_state().channels[&700].name.as_deref(),
        Some("slow")
    );
}

#[tokio::test(start_paused = true)]
async fn a_newer_name_waits_for_the_rename_in_flight() {
    let (live, store, http, trace) = fixture();
    slow_rename_room(&live, &store);
    *http.slow_renames.lock().unwrap() = Some(400_000);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.propose_name(700, "first", 0);
    dispatch(&mut worker, 0).await;
    worker.propose_name(700, "second", 1);
    // The rename budget releases the newer name while the first request
    // is still running: it is held, not sent.
    dispatch(&mut worker, RENAME_MIN_INTERVAL_MS).await;
    assert!(renames_sent(&trace).is_empty());
    finish_detached_after(Duration::from_secs(400)).await;
    assert_eq!(renames_sent(&trace), ["rename:700:first"]);
    // The first outcome lands, then the held name goes out.
    dispatch(&mut worker, 401_000).await;
    assert_eq!(
        worker.live.read_state().channels[&700].name.as_deref(),
        Some("first")
    );
    assert_eq!(
        worker.renames_in_flight.get(&700).map(String::as_str),
        Some("second")
    );
}

#[tokio::test(start_paused = true)]
async fn a_slow_rename_releases_the_actor_and_still_lands() {
    let (live, store, http, trace) = fixture();
    *http.slow_renames.lock().unwrap() = Some(5_000);
    store.rooms.lock().unwrap().insert(700, room(700));
    live.upsert_channel(channel(700, 2, Some(CATEGORY)));
    live.voice_update(MEMBER, Some(700), Some(false));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.propose_name(700, "slow", 0);
    let started = tokio::time::Instant::now();
    dispatch(&mut worker, 0).await;
    // Cosmetic HTTP never consumes any of the creator join's latency budget.
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert!(trace.lock().unwrap().is_empty());
    // A creator joins and moves before the cosmetic request completes.
    join(&mut worker, MEMBER + 1);
    dispatch(&mut worker, 1).await;
    dispatch(&mut worker, 1).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["create", "persist:500", "move:301:500"]
    );
    assert_eq!(started.elapsed(), Duration::ZERO);
    finish_detached_after(Duration::from_secs(5)).await;
    worker.dispatch_one(2).await;
    let renames: Vec<String> = trace
        .lock()
        .unwrap()
        .iter()
        .filter(|entry| entry.starts_with("rename:"))
        .cloned()
        .collect();
    assert_eq!(renames, ["rename:700:slow"]);
    assert_eq!(
        worker.live.read_state().channels[&700].name.as_deref(),
        Some("slow")
    );
}

#[tokio::test]
async fn the_creator_status_template_sets_and_updates_the_voice_status() {
    let (live, store, http, trace) = fixture();
    store.creators.lock().unwrap()[0].name_template = "@@owner@@".to_owned();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    live.voice_update(MEMBER, Some(500), Some(false));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.name_directory.insert(MEMBER, "Alex".to_owned());
    worker
        .name_settings
        .status_templates
        .insert(CREATOR, "@@num@@ <<person/people>>".to_owned());
    worker.refresh_template_names(0);
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    // Same facts: nothing new to write.
    worker.refresh_template_names(2);
    assert!(!worker.dispatch_one(2).await);
    worker.live.voice_update(MEMBER + 1, Some(500), Some(false));
    worker.refresh_template_names(3);
    // The status write for this room waits for its interval.
    assert!(!worker.dispatch_one(3).await);
    dispatch(&mut worker, 1 + name_panel::STATUS_MIN_INTERVAL_MS).await;
    let trace = trace.lock().unwrap().clone();
    assert!(
        trace.contains(&"status:500:1 person".to_owned()),
        "{trace:?}"
    );
    assert!(trace.contains(&"rename:500:Alex".to_owned()), "{trace:?}");
    assert_eq!(trace.last().unwrap(), "status:500:2 people");
}

fn status_worker_parts() -> (LiveGuild, Store, Http, Trace) {
    let (live, store, http, trace) = fixture();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    live.voice_update(MEMBER, Some(500), Some(false));
    (live, store, http, trace)
}

async fn status_worker(
    live: LiveGuild,
    store: Store,
    http: Http,
    template: &str,
) -> GuildRoomWorker<Store, Http> {
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.name_directory.insert(MEMBER, "Alex".to_owned());
    worker
        .name_settings
        .status_templates
        .insert(CREATOR, template.to_owned());
    worker
}

#[tokio::test]
async fn a_status_line_waits_behind_room_creation() {
    let (live, store, http, trace) = status_worker_parts();
    let mut worker = status_worker(live, store, http, "@@num@@ here").await;
    worker.refresh_template_names(0);
    join(&mut worker, MEMBER + 1);
    dispatch(&mut worker, 0).await;
    assert_eq!(trace.lock().unwrap()[0], "create");
}

#[tokio::test]
async fn a_status_line_follows_rooms_without_a_name_template_or_with_an_override() {
    // The fixture creator's name template is blank.
    let (live, store, http, trace) = status_worker_parts();
    let mut worker = status_worker(live, store, http, "@@num@@ here").await;
    worker.refresh_template_names(0);
    dispatch(&mut worker, 0).await;
    worker.custom_names.insert(500, "my room".to_owned());
    worker.live.voice_update(MEMBER + 1, Some(500), Some(false));
    worker.refresh_template_names(1);
    dispatch(&mut worker, name_panel::STATUS_MIN_INTERVAL_MS).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["status:500:1 here", "status:500:2 here"]
    );
}

#[tokio::test]
async fn a_status_line_keeps_up_to_500_characters() {
    let template = format!("{} @@num@@", "x".repeat(150));
    let (live, store, http, trace) = status_worker_parts();
    let mut worker = status_worker(live, store, http, &template).await;
    worker.refresh_template_names(0);
    dispatch(&mut worker, 0).await;
    assert_eq!(
        trace.lock().unwrap()[0],
        format!("status:500:{} 1", "x".repeat(150))
    );
}

#[tokio::test]
async fn a_suspended_lifecycle_write_does_not_hold_back_status_lines() {
    let (live, store, http, trace) = status_worker_parts();
    let mut worker = status_worker(live, store, http, "@@num@@ here").await;
    worker
        .queue
        .enqueue(GUILD, RoomAction::DeleteRoom { channel_id: 999 });
    worker.queue.suspend(GUILD, 999);
    worker.refresh_template_names(0);
    dispatch(&mut worker, 0).await;
    assert_eq!(*trace.lock().unwrap(), ["status:500:1 here"]);
}

#[tokio::test]
async fn removing_the_status_template_clears_the_line() {
    let (live, store, http, trace) = status_worker_parts();
    let mut worker = status_worker(live, store, http, "@@num@@ here").await;
    worker.refresh_template_names(0);
    dispatch(&mut worker, 0).await;
    worker.name_settings.status_templates.clear();
    worker.live.voice_update(MEMBER + 1, Some(500), Some(false));
    worker.refresh_template_names(1);
    dispatch(&mut worker, name_panel::STATUS_MIN_INTERVAL_MS).await;
    assert_eq!(*trace.lock().unwrap(), ["status:500:1 here", "status:500:"]);
    assert!(!worker.room_status.contains_key(&500) || worker.room_status[&500].is_empty());
}

#[tokio::test]
async fn a_failed_clear_is_retried_even_when_the_next_render_is_empty() {
    let (live, store, http, trace) = status_worker_parts();
    let mut worker = status_worker(live, store, http, "@@num@@ here").await;
    worker.refresh_template_names(0);
    dispatch(&mut worker, 0).await;
    worker.name_settings.status_templates.clear();
    worker
        .http
        .status_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::UnknownOutcome);
    worker.live.voice_update(MEMBER + 1, Some(500), Some(false));
    worker.refresh_template_names(1);
    let interval = name_panel::STATUS_MIN_INTERVAL_MS;
    dispatch(&mut worker, interval).await;
    // Another change renders empty again: the clear stays queued.
    worker.live.voice_update(MEMBER + 2, Some(500), Some(false));
    worker.refresh_template_names(2);
    dispatch(&mut worker, 2 * interval).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["status:500:1 here", "status:500:", "status:500:"]
    );
    assert!(!worker.status_unknown.contains(&500));
}

#[tokio::test]
async fn status_bookkeeping_is_dropped_with_the_room() {
    let (live, store, http, _) = status_worker_parts();
    let mut worker = status_worker(live, store, http, "@@num@@ here").await;
    worker.refresh_template_names(0);
    dispatch(&mut worker, 0).await;
    assert!(worker.room_status.contains_key(&500));
    worker.rooms.remove(&500);
    worker.name_inputs = None;
    worker.refresh_template_names(1);
    assert!(worker.room_status.is_empty());
    assert!(worker.status_not_before_ms.is_empty());
    assert!(worker.pending_status.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_slow_status_write_releases_the_actor_and_still_lands() {
    let (live, store, http, trace) = status_worker_parts();
    *http.slow_status.lock().unwrap() = Some(5_000);
    *http.next_id.lock().unwrap() = 501;
    let mut worker = status_worker(live, store, http, "@@num@@ here").await;
    worker.refresh_template_names(0);
    let started = tokio::time::Instant::now();
    dispatch(&mut worker, 0).await;
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert!(trace.lock().unwrap().is_empty());
    join(&mut worker, MEMBER + 1);
    dispatch(&mut worker, 1).await;
    dispatch(&mut worker, 1).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["create", "persist:501", "move:301:501"]
    );
    assert_eq!(started.elapsed(), Duration::ZERO);
    finish_detached_after(Duration::from_secs(5)).await;
    worker.dispatch_one(1).await;
    assert_eq!(trace.lock().unwrap().last().unwrap(), "status:500:1 here");
    assert_eq!(worker.room_status[&500], "1 here");
}

#[test]
fn creator_status_templates_come_from_the_configuration() {
    let mut config = empty_config();
    config
        .creators
        .push(two_bot_core::voice_config::CreatorConfiguration {
            channel_id: CREATOR.to_string(),
            name_template: "Room".to_owned(),
            status_template: Some("@@num@@ here".to_owned()),
            default_limit: 0,
            always_private: false,
            text_channels: false,
            position: two_bot_core::voice_config::RoomPosition::Below,
            first_number: 1,
            group_by_category: false,
            permission_source: two_bot_core::voice_config::PermissionSource::Creator {},
        });
    let settings = NameSettings::from_config(&config);
    assert_eq!(
        settings.status_templates.get(&CREATOR).map(String::as_str),
        Some("@@num@@ here")
    );
}

#[test]
fn empty_grace_config_accepts_zero_to_ten_minutes_and_refuses_garbage() {
    assert_eq!(configured_empty_grace(None), Ok(EMPTY_ROOM_GRACE));
    assert_eq!(configured_empty_grace(Some(" ")), Ok(EMPTY_ROOM_GRACE));
    assert_eq!(configured_empty_grace(Some("0")), Ok(Duration::ZERO));
    assert_eq!(
        configured_empty_grace(Some("600")),
        Ok(Duration::from_secs(600))
    );
    assert_eq!(configured_empty_grace(Some("601")), Err(InvalidEmptyGrace));
    assert_eq!(configured_empty_grace(Some("-1")), Err(InvalidEmptyGrace));
    assert_eq!(configured_empty_grace(Some("1m")), Err(InvalidEmptyGrace));
}

/// Regression for the stored-grace fallback bug: a guild that spawns with a
/// stored `0` must return to the true boot grace once the row is deleted
/// (the documented undo path), not keep sweeping instantly. Driven through
/// the real `ensure_actor` spawn path, so the boot-fallback restore line in
/// `ensure_actor` is what makes this pass: with that line removed the final
/// assert sees `Some(ZERO)` instead of `Some(EMPTY_ROOM_GRACE)`.
#[tokio::test]
async fn spawn_with_stored_zero_then_row_removal_restores_boot_grace() {
    use two_bot_core::settings::{live_channel, SettingRow, SettingsSnapshot};

    let trace = Trace::default();
    let (mut writer, reader) = live_channel();
    writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: vec![SettingRow {
            guild_id: GUILD.to_string(),
            key: EMPTY_GRACE_KEY.to_owned(),
            value: json!(0),
            version: 1,
        }],
    });
    let runtime = VoiceRuntime::new(
        {
            let trace = trace.clone();
            move || (Store::new(trace.clone()), Http::new(trace.clone()))
        },
        Duration::from_millis(10),
        true,
    )
    .with_live_source(move || Some(reader.clone()));
    let actor = runtime.ensure_actor(GUILD).expect("spawn the guild actor");
    for _ in 0..1000 {
        if actor.live.read_state().empty_grace == Some(Duration::ZERO) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(
        actor.live.read_state().empty_grace,
        Some(Duration::ZERO),
        "spawn with a stored 0 applies the stored override first"
    );
    writer.publish(&SettingsSnapshot {
        revision: 2,
        rows: vec![],
    });
    for _ in 0..1000 {
        if actor.live.read_state().empty_grace == Some(EMPTY_ROOM_GRACE) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(
        actor.live.read_state().empty_grace,
        Some(EMPTY_ROOM_GRACE),
        "deleting the stored row must restore the boot grace, never a stale zero"
    );
    runtime.remove_guild(GUILD);
}

/// Same fallback bug via the out-of-range path: a stored `0` at spawn
/// followed by a `601` edit must fail closed to the boot grace. Driven
/// through the real `ensure_actor` spawn path, like the row-removal test
/// above: with the boot-fallback restore line removed the final assert
/// sees `Some(ZERO)` instead of `Some(EMPTY_ROOM_GRACE)`.
#[tokio::test]
async fn spawn_with_stored_zero_then_out_of_range_restores_boot_grace() {
    use two_bot_core::settings::{live_channel, SettingRow, SettingsSnapshot};

    let trace = Trace::default();
    let (mut writer, reader) = live_channel();
    writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: vec![SettingRow {
            guild_id: GUILD.to_string(),
            key: EMPTY_GRACE_KEY.to_owned(),
            value: json!(0),
            version: 1,
        }],
    });
    let runtime = VoiceRuntime::new(
        {
            let trace = trace.clone();
            move || (Store::new(trace.clone()), Http::new(trace.clone()))
        },
        Duration::from_millis(10),
        true,
    )
    .with_live_source(move || Some(reader.clone()));
    let actor = runtime.ensure_actor(GUILD).expect("spawn the guild actor");
    for _ in 0..1000 {
        if actor.live.read_state().empty_grace == Some(Duration::ZERO) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(
        actor.live.read_state().empty_grace,
        Some(Duration::ZERO),
        "spawn with a stored 0 applies the stored override first"
    );
    writer.publish(&SettingsSnapshot {
        revision: 2,
        rows: vec![SettingRow {
            guild_id: GUILD.to_string(),
            key: EMPTY_GRACE_KEY.to_owned(),
            value: json!(601),
            version: 2,
        }],
    });
    for _ in 0..1000 {
        if actor.live.read_state().empty_grace == Some(EMPTY_ROOM_GRACE) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(
        actor.live.read_state().empty_grace,
        Some(EMPTY_ROOM_GRACE),
        "an out-of-range edit after a stored zero must fail closed to boot"
    );
    runtime.remove_guild(GUILD);
}

#[test]
fn synthetic_human_config_takes_up_to_four_snowflakes_and_refuses_garbage() {
    assert_eq!(configured_synthetic_humans(None), Ok(HashSet::new()));
    assert_eq!(configured_synthetic_humans(Some(" ")), Ok(HashSet::new()));
    assert_eq!(
        configured_synthetic_humans(Some(" 301, 302 ,")),
        Ok(HashSet::from([301, 302]))
    );
    assert_eq!(
        configured_synthetic_humans(Some("1,2,3,4")),
        Ok(HashSet::from([1, 2, 3, 4]))
    );
    let overflow = format!("{}0", u64::MAX);
    for bad in [
        "1,2,3,4,5",
        "0",
        "-1",
        "+5",
        "12a",
        "1;2",
        overflow.as_str(),
    ] {
        assert_eq!(
            configured_synthetic_humans(Some(bad)),
            Err(InvalidSyntheticHumans),
            "{bad}"
        );
    }
}

#[test]
fn synthetic_humans_never_reach_the_live_guild() {
    let ids = HashSet::from([301]);
    let live_guild: Snowflake = two_bot_core::backup::guild_config::LIVE_GUILD_ID
        .parse()
        .unwrap();
    assert!(synthetic_humans_for(live_guild, &ids).is_empty());
    assert_eq!(synthetic_humans_for(GUILD, &ids), ids);
}

#[test]
fn generator_channel_config_takes_one_id_and_refuses_garbage() {
    let channel = |value: &str| {
        configured_generator_channel(|key| {
            (key == "TWO_TEMP_VOICE_GENERATOR_CHANNEL_ID").then(|| value.to_owned())
        })
    };
    assert_eq!(channel("900"), Ok(Some(900)));
    assert_eq!(channel(""), Ok(None));
    assert_eq!(channel("   "), Ok(None));
    for bad in ["900,901", "abc", "0", "-1", "0900", "9 00"] {
        assert!(channel(bad).is_err(), "{bad}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_synthetic_human_bot_keeps_its_room_while_other_bots_leave_rooms_empty() {
    let (live, store, http, trace) = fixture();
    live.set_synthetic_humans(HashSet::from([301]));
    for id in [500, 502] {
        store.rooms.lock().unwrap().insert(id, room(id));
    }
    live.publish(snapshot(
        &[500, 502],
        vec![
            VoiceMember {
                member_id: 301,
                channel_id: 500,
                bot: Some(true),
            },
            VoiceMember {
                member_id: 304,
                channel_id: 502,
                bot: Some(true),
            },
        ],
    ));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    tokio::time::advance(EMPTY_ROOM_GRACE).await;
    worker.reconcile();
    for time in 0..2 {
        dispatch(&mut worker, time).await;
    }
    let calls = trace.lock().unwrap();
    assert!(!calls.contains(&"delete:500".to_owned()));
    assert!(calls.contains(&"delete:502".to_owned()));
}

#[tokio::test(start_paused = true)]
async fn without_synthetic_humans_a_bot_only_room_is_still_empty() {
    let (live, store, http, trace) = fixture();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.publish(snapshot(
        &[500],
        vec![VoiceMember {
            member_id: 301,
            channel_id: 500,
            bot: Some(true),
        }],
    ));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    tokio::time::advance(EMPTY_ROOM_GRACE).await;
    worker.reconcile();
    dispatch(&mut worker, 0).await;
    assert!(trace.lock().unwrap().contains(&"delete:500".to_owned()));
}

fn two_creator_worker_parts(a_position: i32, b_position: i32) -> (LiveGuild, Store, Http, Trace) {
    let (live, store, http, trace) = fixture();
    {
        let mut creators = store.creators.lock().unwrap();
        creators.push(CreatorChannel::new(GUILD, 210));
        for creator in creators.iter_mut() {
            creator.position = two_bot_core::voice_rooms::RoomPosition::Below;
        }
    }
    let mut a = channel(CREATOR, 2, Some(CATEGORY));
    a.position = Some(a_position);
    live.upsert_channel(a);
    let mut b = channel(210, 2, Some(CATEGORY));
    b.position = Some(b_position);
    live.upsert_channel(b);
    *http.echo_positions.lock().unwrap() = true;
    (live, store, http, trace)
}

fn join_creator(worker: &mut GuildRoomWorker<Store, Http>, member: u64, creator: u64) {
    let ticket = worker
        .live
        .voice_update(member, Some(creator), Some(false))
        .unwrap();
    assert!(worker.accept_join(ticket, "new room", 7, NOW.to_owned()));
}

fn created_positions(worker: &GuildRoomWorker<Store, Http>) -> Vec<Option<u64>> {
    worker
        .http
        .created_attributes
        .lock()
        .unwrap()
        .iter()
        .map(|attributes| attributes.position)
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_post_move_respace_repairs_only_the_suffix() {
    // Deferred respacing: creators at 2 and 3, so the first room needs a
    // gap. No PATCH runs before either POST; the post-move repair touches
    // only the room and the suffix below its slot while the prefix keeps
    // its exact positions.
    let (live, store, http, _) = two_creator_worker_parts(2, 3);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join_creator(&mut worker, MEMBER, CREATOR);
    dispatch(&mut worker, 0).await;
    // No pre-create reorder: the room takes the fallback tie with its creator.
    assert!(worker.http.reorders.lock().unwrap().is_empty());
    assert_eq!(created_positions(&worker), [Some(2)]);
    join_creator(&mut worker, MEMBER + 1, 210);
    for now in 1..8 {
        worker.dispatch_one(now).await;
    }
    // The second create also waited on no reorder and found the free gap.
    assert_eq!(created_positions(&worker), [Some(2), Some(4)]);
    // One deferred PATCH, recomputed after both moves: the room first, then
    // the suffix below its slot in current relative order.
    assert_eq!(
        worker.http.reorders.lock().unwrap().as_slice(),
        [[(210, 34), (500, 18), (501, 50)]],
    );
    let live = worker.live.read_state();
    assert_eq!(live.channels[&CREATOR].position, Some(2));
    assert_eq!(live.channels[&500].position, Some(18));
    assert_eq!(live.channels[&210].position, Some(34));
    assert_eq!(live.channels[&501].position, Some(50));
    assert_eq!(worker.queue.pending_counts(GUILD), (0, 0));
}

#[tokio::test(start_paused = true)]
async fn a_post_move_respace_never_reports_a_lifecycle_outcome() {
    // Telemetry guard: the deferred post-move repair is cosmetic, not a
    // room lifecycle outcome. It used to feed `voice_operation` with
    // op="respace", which `bounded_index` folds into the `delete` series,
    // so every gapped join added one phantom delete. Log capture is
    // thread-local, so these assertions stay exact under parallel tests
    // (the shared global counters cannot pin "unchanged" exactly).
    let (bytes, _subscriber) = capture_join_logs();
    let (live, store, http, _) = two_creator_worker_parts(2, 3);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join_creator(&mut worker, MEMBER, CREATOR);
    dispatch(&mut worker, 0).await;
    join_creator(&mut worker, MEMBER + 1, 210);
    for now in 1..8 {
        worker.dispatch_one(now).await;
    }
    // The repair ran exactly once and nothing lingers queued.
    assert_eq!(worker.http.reorders.lock().unwrap().len(), 1);
    assert_eq!(worker.queue.pending_counts(GUILD), (0, 0));
    // It reported exactly one respace line, and no lifecycle outcome line
    // ever carried op="respace".
    let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    let events: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect();
    let respaces: Vec<&serde_json::Value> = events
        .iter()
        .filter(|event| event["fields"]["voice_event"] == "voice_respace")
        .collect();
    assert_eq!(respaces.len(), 1, "{text}");
    assert_eq!(respaces[0]["fields"]["outcome"], "success");
    assert!(
        events
            .iter()
            .all(|event| event["fields"]["op"] != "respace"),
        "{text}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_second_join_moves_while_a_respace_is_pending() {
    // Two members join packed creators back to back. Only the first create
    // needs respacing, and B's create and move both run before any PATCH:
    // B never waits behind A's deferred repair.
    let (live, store, http, _) = two_creator_worker_parts(2, 3);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join_creator(&mut worker, MEMBER, CREATOR);
    dispatch(&mut worker, 0).await;
    join_creator(&mut worker, MEMBER + 1, 210);
    dispatch(&mut worker, 1).await;
    dispatch(&mut worker, 2).await;
    dispatch(&mut worker, 3).await;
    // Both members created and moved with no PATCH sent yet.
    assert_eq!(created_positions(&worker), [Some(2), Some(4)]);
    assert!(worker.moves.is_empty());
    assert_eq!(worker.tracked().len(), 2);
    assert!(worker.http.reorders.lock().unwrap().is_empty());
    for now in 4..8 {
        worker.dispatch_one(now).await;
    }
    // Exactly one deferred repair afterwards; nothing lingers queued.
    assert_eq!(worker.http.reorders.lock().unwrap().len(), 1);
    assert_eq!(worker.queue.pending_counts(GUILD), (0, 0));
}

#[tokio::test(start_paused = true)]
async fn a_respace_plans_from_dispatch_time_state() {
    // Plan and guard share one snapshot at dispatch: a sibling the gateway
    // moved after the move still lands in the repair at its new position,
    // and the prefix is never rewritten.
    let (live, store, http, _) = two_creator_worker_parts(2, 3);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join_creator(&mut worker, MEMBER, CREATOR);
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    let mut sibling = worker.live.read_state().channels[&210].clone();
    sibling.position = Some(7);
    worker.live.upsert_channel(sibling);
    for now in 2..6 {
        worker.dispatch_one(now).await;
    }
    // An enqueue-time plan would have patched the sibling to 19; the
    // dispatch-time repair tracks its reported position 7 instead.
    assert_eq!(
        worker.http.reorders.lock().unwrap().as_slice(),
        [[(210, 34), (500, 18)]],
    );
    let live = worker.live.read_state();
    assert_eq!(live.channels[&CREATOR].position, Some(2));
    assert_eq!(live.channels[&500].position, Some(18));
    assert_eq!(live.channels[&210].position, Some(34));
}

#[tokio::test(start_paused = true)]
async fn a_respace_drops_when_the_slot_converges_first() {
    // Gateway CHANNEL_UPDATEs converge the slot before the queued repair
    // dispatches, so no PATCH is sent at all.
    let (live, store, http, _) = two_creator_worker_parts(2, 3);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join_creator(&mut worker, MEMBER, CREATOR);
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    assert_eq!(worker.queue.pending_counts(GUILD), (0, 1));
    let mut room = worker.live.read_state().channels[&500].clone();
    room.position = Some(18);
    worker.live.upsert_channel(room);
    let mut sibling = worker.live.read_state().channels[&210].clone();
    sibling.position = Some(19);
    worker.live.upsert_channel(sibling);
    for now in 2..6 {
        worker.dispatch_one(now).await;
    }
    assert!(worker.http.reorders.lock().unwrap().is_empty());
    assert_eq!(worker.queue.pending_counts(GUILD), (0, 0));
}

#[tokio::test(start_paused = true)]
async fn a_grouped_repair_sends_no_patch_when_the_create_found_a_free_slot() {
    // Mixed grouped/ungrouped creators sharing one category: the grouped
    // block is every room in the category, so a min-rank plus seniority
    // count assumes a contiguous block and repairs a room the create had
    // already placed. The repair must reuse the create slot (block edge).
    let (live, store, http, _) = two_creator_worker_parts(10, 20);
    {
        let mut creators = store.creators.lock().unwrap();
        creators
            .iter_mut()
            .find(|creator| creator.channel_id == CREATOR)
            .expect("CREATOR present")
            .group_by_category = true;
    }
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join_creator(&mut worker, MEMBER, CREATOR);
    for now in 0..6 {
        worker.dispatch_one(now).await;
    }
    assert!(worker.http.reorders.lock().unwrap().is_empty());
    join_creator(&mut worker, MEMBER + 1, 210);
    for now in 6..12 {
        worker.dispatch_one(now).await;
    }
    assert!(worker.http.reorders.lock().unwrap().is_empty());
    join_creator(&mut worker, MEMBER + 2, CREATOR);
    for now in 12..20 {
        worker.dispatch_one(now).await;
    }
    assert_eq!(created_positions(&worker), [Some(11), Some(21), Some(22)]);
    assert!(
        worker.http.reorders.lock().unwrap().is_empty(),
        "free-slot create needs no post-move PATCH"
    );
    let live = worker.live.read_state();
    assert_eq!(live.channels[&CREATOR].position, Some(10));
    assert_eq!(live.channels[&500].position, Some(11));
    assert_eq!(live.channels[&210].position, Some(20));
    assert_eq!(live.channels[&501].position, Some(21));
    assert_eq!(live.channels[&502].position, Some(22));
    assert_eq!(worker.queue.pending_counts(GUILD), (0, 0));
}

#[tokio::test(start_paused = true)]
async fn a_burst_grouped_repair_keeps_the_room_above_its_creator() {
    // A grouped Above repair must not anchor on a newer room from another
    // creator queued before the repair runs: both joins land correctly, so
    // the burst must converge on the sequential order, keeping the grouped
    // room above its creator.
    let (live, store, http, _) = two_creator_worker_parts(1, 2);
    {
        let mut creators = store.creators.lock().unwrap();
        for creator in creators.iter_mut() {
            creator.position = two_bot_core::voice_rooms::RoomPosition::Above;
            if creator.channel_id == CREATOR {
                creator.group_by_category = true;
            }
        }
    }
    let mut extra = channel(450, 2, Some(CATEGORY));
    extra.position = Some(0);
    live.upsert_channel(extra);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join_creator(&mut worker, MEMBER, CREATOR);
    join_creator(&mut worker, MEMBER + 1, 210);
    for now in 0..12 {
        worker.dispatch_one(now).await;
    }
    let live = worker.live.read_state();
    let mut order = [450, 500, 200, 501, 210];
    order.sort_by_key(|id| (live.channels[id].position.unwrap_or(0), *id));
    assert_eq!(order, [450, 500, 200, 501, 210]);
    assert_eq!(worker.queue.pending_counts(GUILD), (0, 0));
}

#[tokio::test(start_paused = true)]
async fn a_respace_429_holds_the_route_while_joins_flow() {
    // A 429 on one room's reorder holds every pending reorder on the route,
    // while creates and moves for other rooms keep flowing past it.
    let (live, store, http, _) = two_creator_worker_parts(2, 3);
    http.reorder_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::RateLimited {
            retry_after_ms: 100,
            global: false,
        });
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join_creator(&mut worker, MEMBER, CREATOR);
    dispatch(&mut worker, 0).await;
    // Pack B's slot so its move also queues a repair.
    join_creator(&mut worker, MEMBER + 1, 210);
    dispatch(&mut worker, 1).await;
    dispatch(&mut worker, 2).await;
    let mut created = worker.live.read_state().channels[&501].clone();
    created.position = Some(3);
    worker.live.upsert_channel(created);
    dispatch(&mut worker, 3).await;
    // A's repair sends once and hits the scripted 429.
    dispatch(&mut worker, 4).await;
    assert_eq!(
        worker.http.reorders.lock().unwrap().as_slice(),
        [[(210, 34), (500, 18), (501, 50)]],
    );
    // A third join flows through the held route: created and moved, with no
    // second PATCH while backed off.
    join_creator(&mut worker, MEMBER + 2, 210);
    for now in 50..56 {
        worker.dispatch_one(now).await;
    }
    assert_eq!(created_positions(&worker), [Some(2), Some(4), Some(4)]);
    assert_eq!(worker.queue.pending_counts(GUILD), (0, 2));
    assert!(worker.moves.is_empty());
    assert_eq!(worker.http.reorders.lock().unwrap().len(), 1);
    // Past retry-after, the queued repairs converge on fresh state: one more
    // PATCH, then every room renders at its slot and nothing lingers.
    for now in 100..108 {
        worker.dispatch_one(now).await;
    }
    let reorders = worker.http.reorders.lock().unwrap();
    assert_eq!(reorders.len(), 2);
    assert_eq!(reorders[0], [(210, 34), (500, 18), (501, 50)]);
    assert_eq!(reorders[1], [(210, 34), (500, 18), (501, 50), (502, 66)]);
    drop(reorders);
    let live = worker.live.read_state();
    assert_eq!(live.channels[&CREATOR].position, Some(2));
    assert_eq!(live.channels[&500].position, Some(18));
    assert_eq!(live.channels[&210].position, Some(34));
    assert_eq!(live.channels[&501].position, Some(50));
    assert_eq!(live.channels[&502].position, Some(66));
    assert_eq!(worker.queue.pending_counts(GUILD), (0, 0));
}

#[tokio::test(start_paused = true)]
async fn a_failed_post_move_respace_keeps_the_fallback_tie() {
    // The create never waits on a reorder, so scripted reorder failures
    // cannot change its position; a refused or unknown post-move repair
    // drops without retrying and the room keeps its fallback tie.
    let (live, store, http, _) = two_creator_worker_parts(2, 3);
    http.reorder_errors.lock().unwrap().extend([
        RoomHttpError::Rejected {
            status: 403,
            code: 50013,
        },
        RoomHttpError::UnknownOutcome,
    ]);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join_creator(&mut worker, MEMBER, CREATOR);
    dispatch(&mut worker, 0).await;
    assert!(worker.http.reorders.lock().unwrap().is_empty());
    assert_eq!(created_positions(&worker), [Some(2)]);
    for now in 1..6 {
        worker.dispatch_one(now).await;
    }
    // Refused: one PATCH attempted, then dropped; the tie stands.
    assert_eq!(
        worker.http.reorders.lock().unwrap().as_slice(),
        [[(210, 34), (500, 18)]],
    );
    assert_eq!(worker.live.read_state().channels[&500].position, Some(2));
    // Pack the second block: its repair meets the unknown outcome.
    let mut room = worker.live.read_state().channels[&500].clone();
    room.position = Some(4);
    worker.live.upsert_channel(room);
    let mut b = channel(210, 2, Some(CATEGORY));
    b.position = Some(3);
    worker.live.upsert_channel(b);
    join_creator(&mut worker, MEMBER + 1, 210);
    for now in 6..12 {
        worker.dispatch_one(now).await;
    }
    // Unknown: the repair may have landed, so local positions stay as the
    // gateway last reported them and nothing retries.
    let reorders = worker.http.reorders.lock().unwrap();
    assert_eq!(reorders.len(), 2);
    assert_eq!(reorders[1], [(500, 35), (501, 19)]);
    drop(reorders);
    assert_eq!(created_positions(&worker), [Some(2), Some(3)]);
    assert_eq!(worker.live.read_state().channels[&501].position, Some(3));
    assert_eq!(worker.queue.pending_counts(GUILD), (0, 0));
}

#[tokio::test(start_paused = true)]
async fn sustained_dispatches_yield_to_due_settings_maintenance() {
    let (live, store, http, _) = fixture();
    let reads = Arc::clone(&store.config_reads);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    for id in 10_000..11_000 {
        worker
            .queue
            .enqueue(GUILD, RoomAction::DeleteRoom { channel_id: id });
    }
    let (sender, receiver) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        run_actor(&mut worker, receiver, Duration::from_millis(250)).await;
        worker
    });
    for _ in 0..50 {
        if reads.load(Ordering::Relaxed) >= 2 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        reads.load(Ordering::Relaxed) >= 2,
        "maintenance ran while work remained"
    );
    drop(sender);
    let worker = task.await.unwrap();
    assert!(
        worker.queue.pending_counts(GUILD).0 > 0,
        "maintenance did not wait for all 1,000 actions"
    );
}

#[tokio::test]
async fn time_tokens_follow_the_guild_time_zone() {
    // 2026-10-11 02:30 UTC is 22:30 the previous evening in New York (EDT).
    let wall =
        two_bot_core::funnel::parse_iso_millis("2026-10-11T02:30:00.000000+00:00").unwrap() as u64;
    let (live, store, http, _) = fixture();
    store.creators.lock().unwrap()[0].name_template = "@@hour@@ @@daypart@@".to_owned();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    live.voice_update(MEMBER, Some(500), Some(false));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.wall_clock = test_wall_clock;
    worker.name_directory.insert(MEMBER, "Alex".to_owned());
    TEST_WALL_MS.set(wall);
    worker.name_settings.time_zone = "America/New_York".to_owned();
    worker.refresh_template_names(0);
    assert_eq!(worker.desired_names[&500], "22 night");
    // UTC (the default) renders the UTC hour.
    worker.name_settings.time_zone = "UTC".to_owned();
    worker.refresh_template_names(1);
    assert_eq!(worker.desired_names[&500], "2 late night");
}

#[test]
fn name_settings_carry_the_configured_time_zone() {
    let mut config = empty_config();
    config.settings.time_zone = "Europe/Berlin".to_owned();
    assert_eq!(
        NameSettings::from_config(&config).time_zone,
        "Europe/Berlin"
    );
}

#[test]
fn import_preview_and_confirm_refuse_a_new_unsupported_time_zone() {
    let mut config = full_config();
    config.settings.time_zone = "Mars/Olympus".to_owned();
    let bytes = serde_json::to_vec(&config).unwrap();
    match plan_import_preview(&empty_config(), &bytes, &config_inventory()) {
        ImportDecision::Refuse { message } => {
            assert!(message.contains("settings.time_zone"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    match plan_import_confirm(
        &empty_config(),
        &config,
        &config_inventory(),
        "0000000000000000",
    ) {
        ImportDecision::Refuse { message } => {
            assert!(message.contains("settings.time_zone"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    // The zone already stored is kept as is, so a guild can re-import its export.
    let mut current = empty_config();
    current.settings.time_zone = "Mars/Olympus".to_owned();
    assert!(!matches!(
        plan_import_preview(&current, &bytes, &config_inventory()),
        ImportDecision::Refuse { .. }
    ));
}

#[tokio::test]
async fn a_member_hopping_from_their_own_room_to_the_creator_claims_without_it() {
    let (live, store, http, _) = fixture();
    store.rooms.lock().unwrap().insert(500, room(500));
    store.rooms.lock().unwrap().insert(501, room(501));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    live.upsert_channel(channel(501, 2, Some(CATEGORY)));
    // Room 501 (also the member's) still has someone in it; 500 is the room
    // they are leaving.
    live.voice_update(OTHER_MEMBER, Some(501), Some(false));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    assert_eq!(worker.rooms[&500].owner_id, MEMBER);
    assert_eq!(worker.rooms[&501].owner_id, MEMBER);
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    assert_eq!(*worker.store.claim_leaving.lock().unwrap(), vec![vec![500]]);
}

#[test]
fn temp_voice_admission_has_no_cooldown() {
    let config = temp_voice_admission();
    assert_eq!(config.max_per_user(), 1);
    assert_eq!(config.cooldown_secs(), 0);
}

// ---- stall-observability regressions (TOG prod stall follow-up) ----
//
// Received-vs-handled accounting: every delivered creator-channel join bumps
// `two_bot_voice_joins_received_total`, accepted or refused. Refusals carry a
// stable reason label. Global counters are monotonic, so these tests assert
// `after == before + expected` on the exact series.

fn refused_with_reason(reason: &str) -> u64 {
    global_series(&format!(
        "two_bot_voice_join_refused_total{{reason=\"{reason}\"}}"
    ))
}

fn join_command(ticket: JoinTicket) -> ActorCommand {
    ActorCommand::Join {
        ticket,
        display: "new room".to_owned(),
        seed: 7,
        created_at: NOW.to_owned(),
        received: tokio::time::Instant::now(),
    }
}

#[tokio::test]
async fn join_commands_count_received_on_accept_and_on_refusal() {
    let (live, store, http, _) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let received_before = global_series("two_bot_voice_joins_received_total");
    let duplicate_before = refused_with_reason("duplicate");
    let ticket = worker
        .live
        .voice_update(MEMBER, Some(CREATOR), Some(false))
        .unwrap();
    assert!(apply_command(
        &mut worker,
        join_command(ticket),
        0,
        &ActorInboxBackpressure::default()
    ));
    // Same ticket twice: the second delivery is refused as a duplicate, but
    // both deliveries count as received. Lower bounds, not exact diffs: these
    // counters are process-global and sink tests deliver joins through the
    // same Join arm concurrently, so concurrent runs can only add.
    assert!(!apply_command(
        &mut worker,
        join_command(ticket),
        1,
        &ActorInboxBackpressure::default()
    ));
    assert!(
        global_series("two_bot_voice_joins_received_total") >= received_before + 2,
        "both deliveries count as received"
    );
    assert!(
        refused_with_reason("duplicate") > duplicate_before,
        "the repeat delivery refuses as a duplicate"
    );
}

#[tokio::test]
async fn silent_join_refusals_carry_stable_reason_labels() {
    // Refusal counters are process-global and other tests refuse joins
    // concurrently, so every assert below is a lower bound: our own refusal
    // guarantees at least +1 and concurrent runs can only add.
    // Halted latch.
    let (live, store, http, _) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.halted = true;
    let halted_before = refused_with_reason("halted");
    let ticket = worker
        .live
        .voice_update(MEMBER, Some(CREATOR), Some(false))
        .unwrap();
    assert!(!worker.accept_join_received(
        ticket,
        "new room",
        7,
        NOW.to_owned(),
        tokio::time::Instant::now()
    ));
    assert!(
        refused_with_reason("halted") > halted_before,
        "halted joins refuse with the halted label"
    );

    // Access gate off.
    let (live, store, http, _) = fixture();
    *store.access.lock().unwrap() = AccessControls {
        room_creation_enabled: false,
        ..AccessControls::default()
    };
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let access_before = refused_with_reason("access");
    let ticket = worker
        .live
        .voice_update(MEMBER, Some(CREATOR), Some(false))
        .unwrap();
    assert!(!worker.accept_join_received(
        ticket,
        "new room",
        7,
        NOW.to_owned(),
        tokio::time::Instant::now()
    ));
    assert!(
        refused_with_reason("access") > access_before,
        "gated joins refuse with the access label"
    );

    // No creator row for the channel.
    let (live, store, http, _) = fixture();
    store.creators.lock().unwrap().clear();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let unknown_before = refused_with_reason("unknown_creator");
    let ticket = worker
        .live
        .voice_update(MEMBER, Some(CREATOR), Some(false))
        .expect("ticket for a joined channel");
    assert!(!worker.accept_join_received(
        ticket,
        "new room",
        7,
        NOW.to_owned(),
        tokio::time::Instant::now()
    ));
    assert!(
        refused_with_reason("unknown_creator") > unknown_before,
        "creator-less joins refuse with the unknown_creator label"
    );

    // Blocked room name.
    let (mut worker, _) = filtering_worker(&["room"]).await;
    let blocked_before = refused_with_reason("name_blocked");
    assert!(!join_as(&mut worker, MEMBER, "Ava"));
    assert!(
        refused_with_reason("name_blocked") > blocked_before,
        "blocked names refuse with the name_blocked label"
    );
}

#[tokio::test]
async fn stale_creator_view_revalidates_from_the_store_before_refusing() {
    // The runtime creator view went stale while the voice_creators row
    // existed, so joins refused as unknown_creator until restart. A stale
    // view must re-read the store row for the joined channel instead of
    // refusing it.
    let (live, store, http, _) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    // Stale view only: the store row stays, the worker map loses it.
    worker.creators.clear();
    let ticket = worker
        .live
        .voice_update(MEMBER, Some(CREATOR), Some(false))
        .expect("ticket for a joined channel");
    assert!(
        !worker.creators.contains_key(&CREATOR),
        "stale view under test"
    );
    assert!(
        worker.revalidate_creator(CREATOR).await,
        "a voice_creators row must revalidate the stale view"
    );
    assert!(worker.creators.contains_key(&CREATOR));
    assert!(
        worker.accept_join_received(
            ticket,
            "new room",
            7,
            NOW.to_owned(),
            tokio::time::Instant::now()
        ),
        "the revalidated join is accepted, not refused as unknown_creator"
    );
}

#[tokio::test]
async fn revalidate_ignores_channels_with_no_creator_row() {
    // A join to a channel the store never heard of still refuses: the
    // revalidation only admits channels that are voice_creators rows.
    let (live, store, http, _) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    const STRANGER: u64 = 999;
    assert!(!worker.revalidate_creator(STRANGER).await);
    assert!(!worker.creators.contains_key(&STRANGER));
}

#[tokio::test]
async fn creator_template_edit_reaches_existing_rooms_through_the_refresh() {
    // reload_creators runs just before reload_name_settings on the same
    // maintenance tick. When it swaps a changed row it must clear the name
    // caches like CreatorAdded does: NameSignature carries no template, so
    // once the edited row is stored, reload_name_settings sees an unchanged
    // map and skips its own clear, and the edit never re-renders.
    let (live, store, http, _) = fixture();
    store.creators.lock().unwrap()[0].name_template = "old".to_owned();
    slow_rename_room(&live, &store);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.wall_clock = || 0;
    worker.name_directory.insert(MEMBER, "Alex".to_owned());
    worker.reload_name_settings(0).await;
    worker.refresh_template_names(0);
    dispatch(&mut worker, 0).await;
    assert_eq!(worker.desired_names[&700], "old");
    worker.store.creators.lock().unwrap()[0].name_template = "new".to_owned();
    worker.reload_creators(300_000).await;
    worker.reload_name_settings(300_000).await;
    worker.refresh_template_names(300_000);
    assert_eq!(
        worker.desired_names[&700], "new",
        "a template-only row edit merged by reload_creators must re-render existing rooms"
    );
}

#[tokio::test]
async fn revalidate_skips_the_store_read_while_halted() {
    // A halted worker refuses every join as `halted` anyway; the read-through
    // must not spend store reads (or bypass the halt re-probe pacing) first.
    let (live, store, http, _) = fixture();
    let reads = Arc::clone(&store.creator_for_reads);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.halted = true;
    const STRANGER: u64 = 555;
    assert!(!worker.revalidate_creator(STRANGER).await);
    assert_eq!(
        reads.load(Ordering::Relaxed),
        0,
        "halted joins must not cost a store read"
    );
}

#[tokio::test]
async fn revalidate_remembers_channels_with_no_creator_row() {
    // Joins into ordinary voice channels (permanent, AFK, lobby) arrive as
    // Join commands too. Without a miss cache each one costs a store read;
    // the merge clears misses, so a later creator row still converges.
    let (live, store, http, _) = fixture();
    let reads = Arc::clone(&store.creator_for_reads);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    const STRANGER: u64 = 999;
    assert!(!worker.revalidate_creator(STRANGER).await);
    assert!(!worker.revalidate_creator(STRANGER).await);
    assert_eq!(
        reads.load(Ordering::Relaxed),
        1,
        "repeat joins to a non-creator channel must reuse the miss cache"
    );
}

#[tokio::test]
async fn revalidate_backs_off_while_the_store_is_degraded() {
    // One failed read must not cost a read per later join: while the store
    // is degraded every join skips the read, and the next successful merge
    // clears the backoff so joins read again.
    let (live, store, http, _) = fixture();
    let reads = Arc::clone(&store.creator_for_reads);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.store.creator_for_error = Some(StoreError::Unavailable);
    const FIRST: u64 = 901;
    const SECOND: u64 = 902;
    assert!(!worker.revalidate_creator(FIRST).await);
    assert!(!worker.revalidate_creator(SECOND).await);
    assert_eq!(
        reads.load(Ordering::Relaxed),
        1,
        "joins into other channels must skip the read while the store is degraded"
    );
    // Recovery: the store is healthy again and a merge ran, so the next
    // join into an unseen channel reads once more.
    worker.store.creator_for_error = None;
    worker
        .store
        .creators
        .lock()
        .unwrap()
        .push(CreatorChannel::new(GUILD, FIRST));
    worker.creators_refresh_due = true;
    worker.reload_creators(0).await;
    assert!(worker.creators.contains_key(&FIRST));
    assert!(!worker.revalidate_creator(SECOND).await);
    assert_eq!(
        reads.load(Ordering::Relaxed),
        2,
        "a successful merge must clear the backoff"
    );
}

#[tokio::test]
async fn revalidate_bounds_a_hanging_store_read() {
    // A saturated pool must not hold the join inbox: the read resolves at
    // the timeout, and the repeat join pays no second read.
    let (live, store, http, _) = fixture();
    let reads = Arc::clone(&store.creator_for_reads);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.store.creator_for_delay = Some(Duration::from_secs(30));
    const STRANGER: u64 = 903;
    const OTHER: u64 = 904;
    let started = tokio::time::Instant::now();
    assert!(!worker.revalidate_creator(STRANGER).await);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a hanging read must resolve at the 1 s bound, took {:?}",
        started.elapsed()
    );
    assert!(!worker.revalidate_creator(OTHER).await);
    assert_eq!(
        reads.load(Ordering::Relaxed),
        1,
        "a timed-out read must back further joins off"
    );
}

#[tokio::test]
async fn actor_join_revalidates_a_stale_creator_view() {
    // The inbox read-through is the production path: a Join sent through the
    // actor loop must merge a store row the in-memory view lost.
    let (live, store, http, _) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.creators.clear();
    let ticket = worker
        .live
        .voice_update(MEMBER, Some(CREATOR), Some(false))
        .expect("ticket for a joined channel");
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move {
        let backpressure = ActorInboxBackpressure::default();
        run_actor_with_limits(
            &mut worker,
            rx,
            Duration::from_millis(100),
            ActorLimits::default(),
            &backpressure,
        )
        .await;
        worker
    });
    tx.send(ActorCommand::Join {
        ticket,
        display: "new room".to_owned(),
        seed: 7,
        created_at: NOW.to_owned(),
        received: tokio::time::Instant::now(),
    })
    .unwrap();
    // The Status round-trip proves the Join was handled: the inbox is FIFO.
    let (reply, reply_rx) = oneshot::channel();
    tx.send(ActorCommand::Status(reply)).unwrap();
    reply_rx.await.unwrap();
    drop(tx);
    let worker = handle.await.unwrap();
    assert!(
        worker.creators.contains_key(&CREATOR),
        "a Join through the actor must revalidate a stale creator view"
    );
}

#[tokio::test]
async fn refresh_creators_command_flags_a_debounced_merge() {
    // Channel create/update/delete frames must reach the worker as a merge
    // flag: without this arm the gateway wiring is dead.
    let (live, store, http, _) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    assert!(!worker.creators_refresh_due);
    apply_command(
        &mut worker,
        ActorCommand::RefreshCreators,
        0,
        &ActorInboxBackpressure::default(),
    );
    assert!(worker.creators_refresh_due);
}

#[tokio::test(start_paused = true)]
async fn maintenance_tick_runs_the_creator_refresh() {
    // The maintenance branch must call reload_creators: it stamps the read
    // clock and clears the miss cache, which reload_name_settings never does.
    let (live, store, http, _) = fixture();
    let reads = Arc::clone(&store.creator_for_reads);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    const STRANGER: u64 = 999;
    assert!(!worker.revalidate_creator(STRANGER).await);
    assert_eq!(reads.load(Ordering::Relaxed), 1);
    assert!(worker.creators_read_ms.is_none());
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move {
        let backpressure = ActorInboxBackpressure::default();
        run_actor_with_limits(
            &mut worker,
            rx,
            Duration::from_millis(100),
            ActorLimits::default(),
            &backpressure,
        )
        .await;
        worker
    });
    for _ in 0..5 {
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
    }
    let (reply, reply_rx) = oneshot::channel();
    tx.send(ActorCommand::Status(reply)).unwrap();
    reply_rx.await.unwrap();
    drop(tx);
    let mut worker = handle.await.unwrap();
    assert!(
        worker.creators_read_ms.is_some(),
        "the maintenance tick must run reload_creators"
    );
    assert!(!worker.revalidate_creator(STRANGER).await);
    assert_eq!(
        reads.load(Ordering::Relaxed),
        2,
        "the merge must clear the miss cache"
    );
}

#[tokio::test]
async fn reload_creators_debounces_channel_event_refreshes() {
    // A bulk reorder's CHANNEL_UPDATE echo storm flags one re-read, not one
    // per frame: refreshes inside the debounce window are skipped.
    let (live, store, http, _) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.reload_creators(0).await;
    worker
        .store
        .creators
        .lock()
        .unwrap()
        .push(CreatorChannel::new(GUILD, 201));
    worker.creators_refresh_due = true;
    worker.reload_creators(5_000).await;
    assert!(
        !worker.creators.contains_key(&201),
        "a flagged refresh inside the debounce window must wait"
    );
    worker.creators_refresh_due = true;
    worker.reload_creators(10_000).await;
    assert!(worker.creators.contains_key(&201));
}

#[tokio::test]
async fn reload_creators_converges_rows_on_the_five_minute_interval() {
    // With no channel events flagging a merge, the interval still converges
    // rows written outside the worker.
    let (live, store, http, _) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.store.creators.lock().unwrap()[0].name_template = "old".to_owned();
    worker.reload_creators(0).await;
    assert_eq!(worker.creators[&CREATOR].name_template, "old");
    worker.store.creators.lock().unwrap()[0].name_template = "new".to_owned();
    worker.reload_creators(299_999).await;
    assert_eq!(
        worker.creators[&CREATOR].name_template, "old",
        "the interval must gate the periodic merge"
    );
    worker.reload_creators(300_000).await;
    assert_eq!(worker.creators[&CREATOR].name_template, "new");
}

#[tokio::test]
async fn reload_creators_keeps_the_view_on_a_failed_read() {
    // A transient store error is not a deletion: the old map stays so joins
    // keep flowing until the next refresh.
    let (live, store, http, _) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.creators.clear();
    worker.store.creators_error = Some(StoreError::Unavailable);
    worker.reload_creators(300_000).await;
    assert!(
        !worker.creators.contains_key(&CREATOR),
        "a failed read must not invent rows"
    );
    worker.store.creators_error = None;
    worker.reload_creators(600_000).await;
    assert!(worker.creators.contains_key(&CREATOR));
}

#[tokio::test]
async fn creator_map_age_keeps_climbing_through_a_store_outage() {
    // The age gauge counts from the last *successful* merge, never from a
    // failed attempt: during a store outage every refresh fails, yet the
    // age must sail past the 5 min refresh interval instead of sawing back
    // toward 0 on each attempt (creators_read_ms stamps attempts only to
    // pace retries, and must not feed the gauge).
    let (live, store, http, _) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.reload_creators(0).await;
    assert_eq!(worker.creators_merged_ms, Some(0));
    worker.store.creators_error = Some(StoreError::Unavailable);
    worker.reload_creators(300_000).await;
    worker.reload_creators(600_000).await;
    assert_eq!(
        worker.creators_merged_ms,
        Some(0),
        "failed reads must not move the merge stamp"
    );
    assert_eq!(
        creator_map_age_secs(worker.creators_merged_ms, 600_000),
        600,
        "the age must climb past the refresh interval while merges fail"
    );
    worker.store.creators_error = None;
    worker.reload_creators(900_000).await;
    assert_eq!(creator_map_age_secs(worker.creators_merged_ms, 900_000), 0);
}

#[tokio::test]
async fn reload_creators_converges_after_a_gateway_reconnect() {
    // A gateway reconnect (GuildCreate/Resumed) bumps the live generation but
    // sends no channel event, so no RefreshCreators flag is set and the
    // five-minute interval has not elapsed. A generation change must trigger
    // a debounced merge so rows written while down converge within about
    // 10 s instead of waiting up to 5 minutes.
    let (live, store, http, _) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.reload_creators(0).await;
    assert!(worker.creators.contains_key(&CREATOR));
    // A creator row written outside the worker while the gateway was down.
    worker
        .store
        .creators
        .lock()
        .unwrap()
        .push(CreatorChannel::new(GUILD, 201));
    // The reconnect republishes the snapshot (new generation, new channel).
    assert!(worker.live.publish(snapshot(&[201], vec![])));
    // Debounced (10 s since the last read) but no flag and no interval: the
    // reconnect alone must converge the new row.
    worker.reload_creators(10_000).await;
    assert!(
        worker.creators.contains_key(&201),
        "a gateway reconnect must merge rows written while down"
    );
}

#[tokio::test]
async fn reload_creators_debounces_a_generation_merge_after_a_reconnect() {
    // A generation change inside the debounce window must not read the
    // store: without the debounce a reconnect during a store outage would
    // re-read about 4 times a second per guild on the 250 ms tick.
    let (live, store, http, _) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.reload_creators(0).await;
    worker
        .store
        .creators
        .lock()
        .unwrap()
        .push(CreatorChannel::new(GUILD, 201));
    // The reconnect republishes the snapshot (new generation, new channel).
    assert!(worker.live.publish(snapshot(&[201], vec![])));
    worker.reload_creators(5_000).await;
    assert!(
        !worker.creators.contains_key(&201),
        "a generation change inside the debounce window must wait"
    );
    worker.reload_creators(10_000).await;
    assert!(
        worker.creators.contains_key(&201),
        "the debounced tick must merge the row after a reconnect"
    );
}

#[tokio::test]
async fn reload_creators_retries_a_failed_generation_merge_on_the_next_tick() {
    // A reconnect whose first merge read fails must leave the stored
    // generation alone: stamping it before the read would park the retry on
    // the five-minute interval, exactly the outage case the trigger targets.
    let (live, store, http, _) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.reload_creators(0).await;
    worker
        .store
        .creators
        .lock()
        .unwrap()
        .push(CreatorChannel::new(GUILD, 201));
    // The reconnect republishes the snapshot (new generation, new channel).
    assert!(worker.live.publish(snapshot(&[201], vec![])));
    let generation_before = worker.creators_generation;
    worker.store.creators_error = Some(StoreError::Unavailable);
    worker.reload_creators(10_000).await;
    assert!(
        !worker.creators.contains_key(&201),
        "a failed generation merge must not invent rows"
    );
    assert_eq!(
        worker.creators_generation, generation_before,
        "a failed read must leave the generation alone so the next tick retries"
    );
    worker.store.creators_error = None;
    worker.reload_creators(20_000).await;
    assert!(
        worker.creators.contains_key(&201),
        "the next debounced tick must retry the generation merge"
    );
}

#[tokio::test]
async fn reload_creators_settles_generation_after_a_successful_merge() {
    let (live, store, http, _) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.reload_creators(0).await;
    worker
        .store
        .creators
        .lock()
        .unwrap()
        .push(CreatorChannel::new(GUILD, 201));
    assert!(worker.live.publish(snapshot(&[201], vec![])));
    worker.reload_creators(10_000).await;
    assert!(worker.creators.contains_key(&201));

    worker
        .store
        .creators
        .lock()
        .unwrap()
        .push(CreatorChannel::new(GUILD, 202));
    worker.reload_creators(20_000).await;
    assert!(
        !worker.creators.contains_key(&202),
        "a successful generation merge must settle the generation until the next interval"
    );
}

#[test]
fn creator_map_age_climbs_between_merges_and_starts_at_zero() {
    // Observability v1: the creator-map age gauge climbs on the actor-uptime
    // clock between merges (seconds, truncated) and reads 0 before the first
    // merge; a backwards clock saturates instead of wrapping.
    assert_eq!(creator_map_age_secs(None, 0), 0);
    assert_eq!(creator_map_age_secs(None, 600_000), 0);
    assert_eq!(creator_map_age_secs(Some(20_000), 20_000), 0);
    assert_eq!(creator_map_age_secs(Some(20_000), 20_999), 0);
    assert_eq!(creator_map_age_secs(Some(20_000), 81_000), 61);
    assert_eq!(creator_map_age_secs(Some(90_000), 20_000), 0);
}

#[test]
fn snapshot_queue_age_climbs_from_the_stored_enqueue_stamp() {
    // Sibling-clock queue age: whole seconds since the snapshot's oldest
    // first-enqueue stamp; an empty snapshot reads 0, sub-second remainders
    // truncate, and a stamp in the future saturates instead of wrapping.
    let now = std::time::Instant::now();
    assert_eq!(snapshot_queue_age_secs(None, now), 0);
    assert_eq!(snapshot_queue_age_secs(Some(now), now), 0);
    assert_eq!(
        snapshot_queue_age_secs(Some(now - Duration::from_secs(400)), now),
        400
    );
    assert_eq!(
        snapshot_queue_age_secs(
            Some(now - Duration::from_secs(400) - Duration::from_millis(999)),
            now
        ),
        400
    );
    assert_eq!(
        snapshot_queue_age_secs(Some(now + Duration::from_secs(5)), now),
        0
    );
}

#[test]
fn snapshot_not_ready_age_matches_the_live_view_semantics() {
    // The sibling's not-ready age mirrors `not_ready_secs`: 0 while the
    // live view is ready or the transport is down (`None` stamp),
    // climbing whole seconds otherwise, saturating on a future stamp.
    let now = tokio::time::Instant::now();
    assert_eq!(snapshot_not_ready_age_secs(None, false, now), 0);
    assert_eq!(snapshot_not_ready_age_secs(Some(now), true, now), 0);
    assert_eq!(snapshot_not_ready_age_secs(Some(now), false, now), 0);
    assert_eq!(
        snapshot_not_ready_age_secs(
            Some(now - Duration::from_secs(400) - Duration::from_millis(999)),
            false,
            now
        ),
        400
    );
    assert_eq!(
        snapshot_not_ready_age_secs(Some(now + Duration::from_secs(5)), false, now),
        0
    );
}

#[tokio::test(start_paused = true)]
async fn watchdog_creator_map_age_climbs_from_a_frozen_snapshot() {
    // While a dispatch is hung the main task takes no new snapshots, so the
    // watchdog sibling must derive the age from the stored merge stamp on
    // its own clock: the same frozen snapshot yields a climbing age.
    let (live, store, http, _) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.reload_creators(0).await;
    let stall = StallShared {
        guild: worker.live.guild_id,
        generation: next_voice_actor_generation(),
        retired: AtomicBool::new(false),
        http: Arc::clone(&worker.http),
        live: worker.live.clone(),
        start: tokio::time::Instant::now(),
        state: Mutex::new(StallState {
            last_progress: tokio::time::Instant::now(),
            urgent: 0,
            deferred: 0,
            oldest_enqueued_at: None,
            halted: false,
            creators_generation: 0,
            creators_merged_ms: None,
        }),
    };
    stall.snapshot(&worker, tokio::time::Instant::now());
    let snap = *stall.state.lock().unwrap();
    assert_eq!(snap.creators_merged_ms, Some(0));
    assert_eq!(stall.snapshot_creator_map_age_secs(&snap), 0);
    // No new snapshot (the hung-dispatch case): the age still climbs.
    tokio::time::advance(Duration::from_secs(400)).await;
    assert_eq!(stall.snapshot_creator_map_age_secs(&snap), 400);
}

#[tokio::test(start_paused = true)]
async fn watchdog_queue_age_climbs_from_a_frozen_snapshot() {
    // While a dispatch is hung the main task takes no new snapshots, so the
    // watchdog sibling must derive the queue age from the stored
    // first-enqueue stamp on its own clock: the same frozen snapshot yields
    // a climbing age. The std clock ignores paused tokio time, so the test
    // reads the stored stamp at a fabricated later `now`, then pins the
    // full chain end to end through observe().
    const QUEUE_GUILD: u64 = 9_100_006;
    let (live, store, http, _) = fixture_for_guild(QUEUE_GUILD);
    let worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker
        .queue
        .enqueue(QUEUE_GUILD, RoomAction::DeleteRoom { channel_id: 501 });
    let generation = next_voice_actor_generation();
    let stall = StallShared {
        guild: QUEUE_GUILD,
        generation,
        retired: AtomicBool::new(false),
        http: Arc::clone(&worker.http),
        live: worker.live.clone(),
        start: tokio::time::Instant::now(),
        state: Mutex::new(StallState {
            last_progress: tokio::time::Instant::now(),
            urgent: 0,
            deferred: 0,
            oldest_enqueued_at: None,
            halted: false,
            creators_generation: 0,
            creators_merged_ms: None,
        }),
    };
    stall.snapshot(&worker, tokio::time::Instant::now());
    let snap = *stall.state.lock().unwrap();
    assert_eq!((snap.urgent, snap.deferred), (1, 0));
    assert_eq!(
        snap.oldest_enqueued_at,
        worker.queue.oldest_pending_at(QUEUE_GUILD),
        "the snapshot keeps the queue's own first-enqueue stamp"
    );
    let stamp = snap
        .oldest_enqueued_at
        .expect("the snapshot keeps the enqueue stamp");
    // No new snapshot (the hung-dispatch case): the age still climbs.
    assert_eq!(snapshot_queue_age_secs(Some(stamp), stamp), 0);
    assert_eq!(
        snapshot_queue_age_secs(Some(stamp), stamp + Duration::from_secs(400)),
        400
    );
    // End to end through observe(): a 400 s-old stamp lands as a climbing
    // queue age under the shared generation. (The std clock ignores paused
    // tokio time, so the old stamp is fabricated, not advanced.)
    {
        let mut state = stall.state.lock().unwrap();
        state.oldest_enqueued_at = Some(std::time::Instant::now() - Duration::from_secs(400));
    }
    let mut last_fire = None;
    stall.observe(Duration::from_secs(3600), &mut last_fire);
    assert_eq!(
        metrics::global().voice_actor_contribution(QUEUE_GUILD),
        Some((generation, false, [1, 0], 400)),
        "observe() publishes the climbing queue age from the frozen snapshot"
    );
    metrics::global().remove_voice_actor(QUEUE_GUILD, generation);
}

#[tokio::test(start_paused = true)]
async fn watchdog_not_ready_age_climbs_from_a_frozen_snapshot() {
    // The sibling reads readiness from the shared live view, not the
    // snapshot: a reconnect that stamps the not-ready clock after the last
    // snapshot still yields a climbing age through a hung dispatch, and
    // observe() lands it under the shared generation.
    const NOT_READY_GUILD: u64 = 9_100_005;
    let (live, store, http, _) = fixture_for_guild(NOT_READY_GUILD);
    let worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let generation = next_voice_actor_generation();
    let stall = StallShared {
        guild: NOT_READY_GUILD,
        generation,
        retired: AtomicBool::new(false),
        http: Arc::clone(&worker.http),
        live: worker.live.clone(),
        start: tokio::time::Instant::now(),
        state: Mutex::new(StallState {
            last_progress: tokio::time::Instant::now(),
            urgent: 0,
            deferred: 0,
            oldest_enqueued_at: None,
            halted: false,
            creators_generation: 0,
            creators_merged_ms: None,
        }),
    };
    // Snapshot first, while the view is ready: the hung-dispatch case takes
    // no further snapshots.
    stall.snapshot(&worker, tokio::time::Instant::now());
    // Transport loss then reconnect while the dispatch is hung: the gateway
    // task stamps the not-ready clock on the live view after the last
    // snapshot.
    worker.live.transport_lost();
    worker.live.gateway_connected(tokio::time::Instant::now());
    assert_eq!(worker.live.not_ready_secs(tokio::time::Instant::now()), 0);
    // No new snapshot (the hung-dispatch case): the age still climbs.
    tokio::time::advance(Duration::from_secs(400)).await;
    let mut last_fire = None;
    stall.observe(Duration::from_secs(3600), &mut last_fire);
    assert_eq!(
        metrics::global().voice_actor_live_not_ready_secs(NOT_READY_GUILD),
        Some(400),
        "the sibling republishes the climbing not-ready age"
    );
    assert_eq!(
        metrics::global()
            .voice_actor_contribution(NOT_READY_GUILD)
            .map(|(_, _, _, age)| age),
        Some(0),
        "an empty snapshot queue still publishes age 0"
    );
    metrics::global().remove_voice_actor(NOT_READY_GUILD, generation);
}

#[tokio::test(start_paused = true)]
async fn watchdog_not_ready_age_tracks_a_live_republish_to_ready() {
    // The sibling reads readiness from the shared live view: a GUILD_CREATE
    // republish that flips the view ready after the last snapshot must
    // clear the age, not keep climbing from a stale not-ready copy past the
    // 120 s page bound.
    const REPUBLISH_GUILD: u64 = 9_100_007;
    let (live, store, http, _) = fixture_for_guild(REPUBLISH_GUILD);
    let worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.live.transport_lost();
    worker.live.gateway_connected(tokio::time::Instant::now());
    let generation = next_voice_actor_generation();
    let stall = StallShared {
        guild: REPUBLISH_GUILD,
        generation,
        retired: AtomicBool::new(false),
        http: Arc::clone(&worker.http),
        live: worker.live.clone(),
        start: tokio::time::Instant::now(),
        state: Mutex::new(StallState {
            last_progress: tokio::time::Instant::now(),
            urgent: 0,
            deferred: 0,
            oldest_enqueued_at: None,
            halted: false,
            creators_generation: 0,
            creators_merged_ms: None,
        }),
    };
    stall.snapshot(&worker, tokio::time::Instant::now());
    // The gateway task republishes the view ready while the dispatch is
    // hung: no new snapshot follows.
    assert!(worker
        .live
        .publish(snapshot_for_guild(REPUBLISH_GUILD, &[], vec![])));
    tokio::time::advance(Duration::from_secs(400)).await;
    let mut last_fire = None;
    stall.observe(Duration::from_secs(3600), &mut last_fire);
    assert_eq!(
        metrics::global().voice_actor_live_not_ready_secs(REPUBLISH_GUILD),
        Some(0),
        "a live republish to ready clears the sibling-published age"
    );
    metrics::global().remove_voice_actor(REPUBLISH_GUILD, generation);
}

#[test]
fn watchdog_due_needs_a_queue_and_an_expired_window() {
    let start = tokio::time::Instant::now();
    let window = Duration::from_secs(120);
    assert!(!watchdog_due(start, start, false, window));
    assert!(!watchdog_due(start, start, true, window));
    assert!(watchdog_due(start, start + window, true, window));
    assert!(!watchdog_due(start, start + window, false, window));
    assert!(!watchdog_due(
        start,
        start + window - Duration::from_nanos(1),
        true,
        window
    ));
}

#[tokio::test(start_paused = true)]
async fn halt_reprobe_clears_the_credential_latch_without_a_restart() {
    let (live, store, http, _) = fixture();
    store
        .settle_errors
        .lock()
        .unwrap()
        .push_back(StoreError::CredentialRefused);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    assert!(worker.halted());
    let recoveries_before = global_series("two_bot_voice_halt_recoveries_total");
    let (tx, rx) = mpsc::unbounded_channel();
    let limits = ActorLimits {
        halt_reprobe_interval: Duration::from_secs(5),
        watchdog_stuck_after: Duration::from_secs(3600),
        ..ActorLimits::default()
    };
    let handle = tokio::spawn(async move {
        let backpressure = ActorInboxBackpressure::default();
        run_actor_with_limits(
            &mut worker,
            rx,
            Duration::from_millis(100),
            limits,
            &backpressure,
        )
        .await;
    });
    // Pump the actor once so its probe baseline starts at t=0 (a Status
    // round-trip also proves the halted actor is still alive to answer).
    let (status_reply, status_rx) = oneshot::channel();
    tx.send(ActorCommand::Status(status_reply)).unwrap();
    assert!(status_rx.await.unwrap().halted);
    tokio::time::advance(Duration::from_secs(6)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        global_series("two_bot_voice_halt_recoveries_total"),
        recoveries_before + 1
    );
    // Worker-direct proof the latch cleared: the halted gauge is
    // process-global and other actor tests publish it concurrently, so an
    // exact gauge read would flake under parallel runs.
    let (status_reply, status_rx) = oneshot::channel();
    tx.send(ActorCommand::Status(status_reply)).unwrap();
    assert!(!status_rx.await.unwrap().halted);
    drop(tx);
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn halt_reprobe_keeps_a_discord_401_latch_set() {
    // A move refused with 401 halts with a Discord cause. The store stays
    // healthy, but the re-probe must not report recovery: the writer's auth
    // latch never clears in-process, so this halt needs a token fix and a
    // restart.
    let (live, store, http, _) = fixture();
    http.move_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::Unauthorized);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    assert!(worker.halted());
    assert!(worker.halted_by_discord());
    // Worker-direct evidence only below: the halted gauge and the recovery
    // counter are process-global, and the sibling store-halt test publishes
    // both while running in parallel in this binary.
    let (tx, rx) = mpsc::unbounded_channel();
    let limits = ActorLimits {
        halt_reprobe_interval: Duration::from_secs(5),
        watchdog_stuck_after: Duration::from_secs(3600),
        ..ActorLimits::default()
    };
    let handle = tokio::spawn(async move {
        let backpressure = ActorInboxBackpressure::default();
        run_actor_with_limits(
            &mut worker,
            rx,
            Duration::from_millis(100),
            limits,
            &backpressure,
        )
        .await;
    });
    // Pump the actor once so its probe baseline starts at t=0 (a Status
    // round-trip also proves the halted actor is still alive to answer).
    let (status_reply, status_rx) = oneshot::channel();
    tx.send(ActorCommand::Status(status_reply)).unwrap();
    assert!(status_rx.await.unwrap().halted);
    tokio::time::advance(Duration::from_secs(6)).await;
    tokio::task::yield_now().await;
    // No recovery: a full re-probe interval passes and the latch stays set.
    // (A store re-probe would have cleared a store halt and answered
    // unhalted here, as the sibling test proves.)
    let (status_reply, status_rx) = oneshot::channel();
    tx.send(ActorCommand::Status(status_reply)).unwrap();
    assert!(status_rx.await.unwrap().halted);
    drop(tx);
    handle.await.unwrap();
}

#[tokio::test]
async fn halted_compensation_settle_requeues_the_delete_after_recovery() {
    // CredentialRefused mid-compensation parks the worker with the hold
    // unsettled. Recovery must retry the delete (re-queued by reconcile),
    // not count the hold against admission caps until a restart.
    let (live, mut store, http, _) = fixture();
    store.persist_error = Some(StoreError::Unavailable);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    // Create lands on Discord but the persist fails: the hold is kept and a
    // compensation delete is queued immediately (no move is queued).
    dispatch(&mut worker, 0).await;
    assert_eq!(
        worker
            .compensation_reservations
            .get(&500)
            .map(String::as_str),
        Some("r1")
    );
    assert!(worker.deletes.contains(&500));
    // The Discord delete lands but the settle is refused: halted with the
    // hold retained and the delete marker dropped for re-queue.
    worker
        .store
        .settle_errors
        .lock()
        .unwrap()
        .push_back(StoreError::CredentialRefused);
    dispatch(&mut worker, 1).await;
    assert!(worker.halted());
    assert!(!worker.halted_by_discord());
    assert!(worker.store.settled.lock().unwrap().is_empty());
    assert_eq!(
        worker
            .compensation_reservations
            .get(&500)
            .map(String::as_str),
        Some("r1")
    );
    assert!(!worker.deletes.contains(&500));
    // Recovery clears the latch; reconcile re-queues the delete; the next
    // dispatch settles the held reservation.
    worker.clear_halt();
    worker.reconcile();
    let (urgent, deferred) = worker.queue.pending_counts(GUILD);
    assert!(
        urgent + deferred > 0,
        "reconcile re-queues the unsettled delete"
    );
    dispatch(&mut worker, 2).await;
    assert!(!worker.store.settled.lock().unwrap().is_empty());
    assert!(worker.compensation_reservations.is_empty());
    assert!(!worker.deletes.contains(&500));
}

#[tokio::test(start_paused = true)]
async fn watchdog_fires_while_a_dispatch_is_hung_mid_write() {
    // The incident this PR chases (silent worker, no errors, healthy
    // gateway) was a stall inside `dispatch_one`: the tick arm never ran,
    // so no watchdog and no gauge moved. The sibling observes the shared
    // snapshot on its own timer and must fire while the write is held.
    let _serial = STUCK_COUNTER_SERIAL.lock().await;
    let (live, store, mut http, trace) = owned_room();
    let gate = Arc::new(LimitGate::default());
    http.limit_gate = Some(gate.clone());
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let super::limit::LimitReply::Queued { .. } =
        worker.apply_limit(MEMBER, false, LimitCommand::Limit(LimitArg::Value(3)))
    else {
        panic!("the limit must queue");
    };
    // A second action stays queued while the limit PATCH is held: the hung
    // dispatch popped the limit, so without this the queue reads empty and
    // the watchdog correctly stays silent. Deferred drains after urgent, so
    // the limit is the one that hangs.
    worker.queue.enqueue(
        GUILD,
        RoomAction::RenameRoom {
            channel_id: 500,
            name: "den".to_owned(),
        },
    );
    let http_probe = Arc::clone(&worker.http);
    let stuck_before = global_series("two_bot_voice_watchdog_stucks_total");
    let (tx, rx) = mpsc::unbounded_channel();
    let limits = ActorLimits {
        watchdog_stuck_after: Duration::from_secs(5),
        halt_reprobe_interval: Duration::from_secs(3600),
        ..ActorLimits::default()
    };
    let handle = tokio::spawn(async move {
        let backpressure = ActorInboxBackpressure::default();
        run_actor_with_limits(
            &mut worker,
            rx,
            Duration::from_millis(100),
            limits,
            &backpressure,
        )
        .await;
    });
    // Drive the actor until the limit write is held mid-flight. The clock
    // only moves here, so the waiter must be registered before advancing.
    let mut sent = Box::pin(gate.sent.notified());
    let mut ticks = 0;
    loop {
        tokio::select! {
            biased;
            () = &mut sent => break,
            () = tokio::task::yield_now() => {
                tokio::time::advance(Duration::from_millis(100)).await;
            }
        }
        ticks += 1;
        assert!(ticks < 200, "the limit write never went live");
    }
    // One full stuck window passes with the dispatch hung: exactly one fire,
    // and the held write is still live (Tokio clock, so deterministic).
    // The sibling's REST-age republish is pinned per-instance: the count
    // must rise while the write is held, so deleting the republish call
    // fails this test (the global gauge itself stays unasserted).
    let republished_before = http_probe.republish_calls.load(Ordering::Relaxed);
    tokio::time::advance(Duration::from_secs(6)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        global_series("two_bot_voice_watchdog_stucks_total"),
        stuck_before + 1
    );
    assert!(
        http_probe.republish_calls.load(Ordering::Relaxed) > republished_before,
        "the sibling republishes the REST age while the write is held"
    );
    let held_age_secs = http_probe
        .limit_in_flight
        .lock()
        .unwrap()
        .map(|started| started.elapsed().as_secs());
    assert!(
        held_age_secs.unwrap_or(0) >= 5,
        "the hung write stays observable mid-flight"
    );
    // Release the PATCH: the held write lands (the worker moved into the
    // actor, so prove the drain through the shared trace, not the worker).
    gate.release.notify_one();
    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(trace
        .lock()
        .unwrap()
        .iter()
        .any(|entry| entry == "limit:500:3"));
    drop(tx);
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn watchdog_sibling_exits_when_its_actor_dies() {
    // The sibling must not outlive a panicked actor: its stop flag and
    // abort run only after the loop exits normally, but a panic unwinds
    // past them. The sibling holds only a weak ref, so it exits when the
    // actor task drops the last strong ref instead of alarming forever
    // from the dead worker's snapshot.
    let _serial = STUCK_COUNTER_SERIAL.lock().await;
    let (live, store, mut http, _) = owned_room();
    http.before_limit = Some(Arc::new(|| panic!("probe panic: actor dies")));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let super::limit::LimitReply::Queued { .. } =
        worker.apply_limit(MEMBER, false, LimitCommand::Limit(LimitArg::Value(3)))
    else {
        panic!("the limit must queue");
    };
    // A second action stays queued: the snapshot has pending work with no
    // progress, so a leaked sibling would fire the stuck alarm.
    worker.queue.enqueue(
        GUILD,
        RoomAction::RenameRoom {
            channel_id: 500,
            name: "den".to_owned(),
        },
    );
    let (tx, rx) = mpsc::unbounded_channel();
    let limits = ActorLimits {
        watchdog_stuck_after: Duration::from_secs(5),
        halt_reprobe_interval: Duration::from_secs(3600),
        ..ActorLimits::default()
    };
    let handle = tokio::spawn(async move {
        let backpressure = ActorInboxBackpressure::default();
        run_actor_with_limits(
            &mut worker,
            rx,
            Duration::from_millis(100),
            limits,
            &backpressure,
        )
        .await;
    });
    // Drive the actor until the panicking write kills it: the unwind drops
    // the inbox receiver, which closes the sender's side.
    let mut ticks = 0;
    while !tx.is_closed() {
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        ticks += 1;
        assert!(ticks < 200, "the actor never reached the panicking write");
    }
    handle
        .await
        .expect_err("the actor task must die on the panic");
    // Two full stuck windows pass with queued work and no progress: a
    // leaked sibling would fire twice, so the counter must not move.
    let stuck = global_series("two_bot_voice_watchdog_stucks_total");
    tokio::time::advance(Duration::from_secs(6)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        global_series("two_bot_voice_watchdog_stucks_total"),
        stuck,
        "no stuck alarm after the actor died"
    );
    tokio::time::advance(Duration::from_secs(6)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        global_series("two_bot_voice_watchdog_stucks_total"),
        stuck,
        "no stuck alarm on the second window either"
    );
    drop(tx);
}

#[tokio::test(start_paused = true)]
async fn actor_exit_on_inbox_close_retires_its_stall_contribution() {
    // A halted actor publishes its stall contribution under its guild
    // key; when the inbox closes the actor loop ends and the metric
    // guard must retire that contribution, or the process-wide halt
    // gauge stays stuck at 1 for a guild that no longer runs. Unique
    // guild: the contribution map is process-global.
    const RETIRE_GUILD: u64 = 9_100_001;
    let (live, store, http, _) = fixture_for_guild(RETIRE_GUILD);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.halted = true;
    assert_eq!(
        metrics::global().voice_actor_contribution(RETIRE_GUILD),
        None,
        "no contribution before the actor starts"
    );
    let (tx, rx) = mpsc::unbounded_channel();
    let limits = ActorLimits {
        watchdog_stuck_after: Duration::from_secs(3600),
        halt_reprobe_interval: Duration::from_secs(3600),
        ..ActorLimits::default()
    };
    let handle = tokio::spawn(async move {
        let backpressure = ActorInboxBackpressure::default();
        run_actor_with_limits(
            &mut worker,
            rx,
            Duration::from_millis(100),
            limits,
            &backpressure,
        )
        .await;
    });
    // Drive ticks until the running actor's halted contribution lands.
    let mut contribution = None;
    for _ in 0..200 {
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        contribution = metrics::global().voice_actor_contribution(RETIRE_GUILD);
        if contribution.is_some() {
            break;
        }
    }
    let (generation, halted, _, _) =
        contribution.expect("the running actor publishes its stall contribution");
    assert!(generation > 0, "the actor runs under a real generation");
    assert!(halted, "the halted latch is the published contribution");
    drop(tx);
    handle.await.unwrap();
    assert_eq!(
        metrics::global().voice_actor_contribution(RETIRE_GUILD),
        None,
        "inbox close retires the contribution, so the halt signal cannot stick"
    );
}

#[tokio::test(start_paused = true)]
async fn actor_panic_unwind_retires_its_stall_contribution() {
    // Same retirement on the panic path: the unwind skips the loop's
    // shutdown steps, so only the metric guard's drop can retire the
    // contribution. The panicking write records the contribution it
    // sees first, proving the entry existed when the actor died.
    const PANIC_GUILD: u64 = 9_100_002;
    let (live, store, mut http, _) = owned_room_for_guild(PANIC_GUILD);
    let seen_at_panic = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&seen_at_panic);
    http.before_limit = Some(Arc::new(move || {
        *seen.lock().unwrap() = metrics::global().voice_actor_contribution(PANIC_GUILD);
        panic!("probe panic: actor dies");
    }));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let super::limit::LimitReply::Queued { .. } =
        worker.apply_limit(MEMBER, false, LimitCommand::Limit(LimitArg::Value(3)))
    else {
        panic!("the limit must queue");
    };
    worker.queue.enqueue(
        PANIC_GUILD,
        RoomAction::RenameRoom {
            channel_id: 500,
            name: "den".to_owned(),
        },
    );
    let (tx, rx) = mpsc::unbounded_channel();
    let limits = ActorLimits {
        watchdog_stuck_after: Duration::from_secs(3600),
        halt_reprobe_interval: Duration::from_secs(3600),
        ..ActorLimits::default()
    };
    let handle = tokio::spawn(async move {
        let backpressure = ActorInboxBackpressure::default();
        run_actor_with_limits(
            &mut worker,
            rx,
            Duration::from_millis(100),
            limits,
            &backpressure,
        )
        .await;
    });
    let mut ticks = 0;
    while !tx.is_closed() {
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        ticks += 1;
        assert!(ticks < 200, "the actor never reached the panicking write");
    }
    handle
        .await
        .expect_err("the actor task must die on the panic");
    let (generation, _, queue, _) = seen_at_panic
        .lock()
        .unwrap()
        .expect("a tick published the contribution before the panicking dispatch");
    assert!(generation > 0, "the actor runs under a real generation");
    assert!(
        queue.iter().sum::<u64>() > 0,
        "the queued work is in the published contribution: {queue:?}"
    );
    assert_eq!(
        metrics::global().voice_actor_contribution(PANIC_GUILD),
        None,
        "the panic unwind retires the contribution"
    );
    drop(tx);
}

#[tokio::test(start_paused = true)]
async fn watchdog_observe_publishes_nothing_after_retirement() {
    // The retire fence: once the actor's contribution is removed, a
    // late watchdog-sibling observe must not resurrect it. Positive
    // control first: the same observe publishes the frozen snapshot
    // under the incarnation's own generation while it is live.
    const FENCE_GUILD: u64 = 9_100_003;
    let (live, store, http, _) = fixture_for_guild(FENCE_GUILD);
    let worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let generation = next_voice_actor_generation();
    let stall = StallShared {
        guild: FENCE_GUILD,
        generation,
        retired: AtomicBool::new(false),
        http: Arc::clone(&worker.http),
        live: worker.live.clone(),
        start: tokio::time::Instant::now(),
        state: Mutex::new(StallState {
            last_progress: tokio::time::Instant::now(),
            urgent: 2,
            deferred: 3,
            oldest_enqueued_at: Some(std::time::Instant::now() - Duration::from_secs(44)),
            halted: true,
            creators_generation: 0,
            creators_merged_ms: None,
        }),
    };
    let mut last_fire = None;
    stall.observe(Duration::from_secs(3600), &mut last_fire);
    assert_eq!(
        metrics::global().voice_actor_contribution(FENCE_GUILD),
        Some((generation, true, [2, 3], 44)),
        "the live sibling publishes under the shared generation"
    );
    // Retire exactly as the metric guard does, then observe again.
    stall
        .retired
        .store(true, std::sync::atomic::Ordering::Relaxed);
    metrics::global().remove_voice_actor(FENCE_GUILD, generation);
    stall.observe(Duration::from_secs(3600), &mut last_fire);
    assert_eq!(
        metrics::global().voice_actor_contribution(FENCE_GUILD),
        None,
        "a retired sibling cannot resurrect the removed contribution"
    );
}

#[tokio::test(start_paused = true)]
async fn metric_guard_drop_sets_the_retired_fence() {
    // The setter half of the fence: dropping a real metric guard must
    // mark the shared stall state retired (so a sibling already holding
    // a strong ref cannot re-publish after the removal) and retire the
    // contribution. The watchdog test above sets the flag by hand and
    // only pins the observe side; this test goes through the guard.
    const GUARD_GUILD: u64 = 9_100_004;
    let (live, store, http, _) = fixture_for_guild(GUARD_GUILD);
    let worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let generation = next_voice_actor_generation();
    let stall = Arc::new(StallShared {
        guild: GUARD_GUILD,
        generation,
        retired: AtomicBool::new(false),
        http: Arc::clone(&worker.http),
        live: worker.live.clone(),
        start: tokio::time::Instant::now(),
        state: Mutex::new(StallState {
            last_progress: tokio::time::Instant::now(),
            urgent: 1,
            deferred: 0,
            oldest_enqueued_at: Some(std::time::Instant::now() - Duration::from_secs(7)),
            halted: true,
            creators_generation: 0,
            creators_merged_ms: None,
        }),
    });
    let mut last_fire = None;
    stall.observe(Duration::from_secs(3600), &mut last_fire);
    assert_eq!(
        metrics::global().voice_actor_contribution(GUARD_GUILD),
        Some((generation, true, [1, 0], 7)),
        "the live sibling publishes before the guard drops"
    );
    drop(VoiceActorMetricGuard {
        actor: GUARD_GUILD,
        generation,
        stall: Arc::downgrade(&stall),
    });
    assert!(
        stall.retired.load(std::sync::atomic::Ordering::Relaxed),
        "the guard's drop sets the retired fence"
    );
    assert_eq!(
        metrics::global().voice_actor_contribution(GUARD_GUILD),
        None,
        "the guard's drop retires the contribution"
    );
    stall.observe(Duration::from_secs(3600), &mut last_fire);
    assert_eq!(
        metrics::global().voice_actor_contribution(GUARD_GUILD),
        None,
        "a sibling holding a strong ref cannot resurrect it after the guard dropped"
    );
}

#[tokio::test]
async fn halted_delete_provenance_refusal_requeues_the_delete_after_recovery() {
    // The ordinary-delete provenance read refuses mid-dispatch: the worker
    // halts with the delete marker left, so recovery must re-queue the
    // delete instead of stranding the live room until a restart.
    let (live, store, http, trace) = fixture();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.live.set_empty_grace(Duration::ZERO);
    worker.queue_delete(500, false);
    worker.reconcile();
    assert_eq!(worker.queue.pending_counts(GUILD), (1, 0));
    worker
        .store
        .rooms_errors
        .lock()
        .unwrap()
        .push_back(StoreError::CredentialRefused);
    worker.dispatch_one(0).await;
    assert!(worker.halted());
    assert!(!worker.halted_by_discord());
    assert!(
        worker.deletes.contains(&500),
        "halt leaves the dedup marker"
    );
    // Recovery drops the marker; reconcile re-derives the delete from the
    // still-live empty room and the retry forgets the row.
    worker.clear_halt();
    assert!(!worker.deletes.contains(&500));
    worker.reconcile();
    let (urgent, deferred) = worker.queue.pending_counts(GUILD);
    assert!(
        urgent + deferred > 0,
        "reconcile re-queues the stranded delete"
    );
    worker.dispatch_one(1).await;
    assert!(!worker.halted());
    assert!(!worker.store.rooms.lock().unwrap().contains_key(&500));
    assert!(!worker.deletes.contains(&500));
    assert!(trace
        .lock()
        .unwrap()
        .iter()
        .any(|entry| entry == "delete:500"));
}

#[tokio::test]
async fn halted_delete_forget_refusal_retries_forget_after_recovery() {
    // The Discord delete lands but the row forget refuses: the worker halts
    // with the marker left. Recovery must retry the forget, not leave the
    // row tracked until a restart.
    let (live, store, http, _) = fixture();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.live.set_empty_grace(Duration::ZERO);
    worker.queue_delete(500, false);
    worker.reconcile();
    worker
        .store
        .forget_errors
        .lock()
        .unwrap()
        .push_back(StoreError::CredentialRefused);
    worker.dispatch_one(0).await;
    assert!(worker.halted());
    assert!(worker.deletes.contains(&500));
    assert!(
        !worker
            .live
            .inner
            .read()
            .unwrap()
            .channels
            .contains_key(&500),
        "the Discord delete already happened"
    );
    worker.clear_halt();
    worker.reconcile();
    let (urgent, deferred) = worker.queue.pending_counts(GUILD);
    assert!(
        urgent + deferred > 0,
        "reconcile re-queues the stranded delete"
    );
    worker.dispatch_one(1).await;
    assert!(!worker.halted());
    assert!(!worker.store.rooms.lock().unwrap().contains_key(&500));
    assert!(!worker.deletes.contains(&500));
}

#[tokio::test]
async fn halted_owner_prepare_refusal_requeues_the_handoff_after_recovery() {
    // Succession queues the handoff, then grant preparation refuses: the
    // worker halts with the ownership marker left. Recovery must re-queue
    // the handoff instead of stranding the repair until a restart.
    let (live, store, http, trace) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker
        .live
        .voice_update_at(MEMBER, None, Some(false), 1_000);
    worker.reconcile();
    assert!(
        worker.ownership_queued.contains(&500),
        "succession queues the handoff"
    );
    worker
        .store
        .prepare_errors
        .lock()
        .unwrap()
        .push_back(StoreError::CredentialRefused);
    worker.dispatch_one(2_000).await;
    assert!(worker.halted());
    assert!(
        worker.ownership_queued.contains(&500),
        "halt leaves the handoff marker"
    );
    worker.clear_halt();
    assert!(!worker.ownership_queued.contains(&500));
    worker.dispatch_one(3_000).await;
    assert!(!worker.halted());
    assert!(trace
        .lock()
        .unwrap()
        .iter()
        .any(|entry| entry.starts_with("update_ownership:500")));
    assert!(!worker.ownership_queued.contains(&500));
}

#[tokio::test]
async fn halted_owner_update_refusal_requeues_the_handoff_after_recovery() {
    // Preparation succeeds but the ownership write refuses: same stranded
    // shape on the later halted branch.
    let (live, store, http, trace) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker
        .live
        .voice_update_at(MEMBER, None, Some(false), 1_000);
    worker.reconcile();
    assert!(
        worker.ownership_queued.contains(&500),
        "succession queues the handoff"
    );
    worker
        .store
        .ownership_errors
        .lock()
        .unwrap()
        .push_back(StoreError::CredentialRefused);
    worker.dispatch_one(2_000).await;
    assert!(worker.halted());
    assert!(
        worker.ownership_queued.contains(&500),
        "halt leaves the handoff marker"
    );
    worker.clear_halt();
    worker.dispatch_one(3_000).await;
    assert!(!worker.halted());
    assert!(trace
        .lock()
        .unwrap()
        .iter()
        .any(|entry| entry.starts_with("update_ownership:500")));
    assert!(!worker.ownership_queued.contains(&500));
}

#[tokio::test(start_paused = true)]
async fn watchdog_fires_while_a_backed_off_queue_makes_no_progress() {
    // Exact counter asserts: serialized against the hung-dispatch sibling
    // test (see `STUCK_COUNTER_SERIAL`).
    let _serial = STUCK_COUNTER_SERIAL.lock().await;
    let (live, store, http, _) = fixture();
    http.move_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::RateLimited {
            retry_after_ms: 30_000,
            global: true,
        });
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 0).await;
    let (urgent, deferred) = worker.queue.pending_counts(GUILD);
    assert!(urgent + deferred > 0, "the 429 keeps the move queued");
    let stuck_before = global_series("two_bot_voice_watchdog_stucks_total");
    let (tx, rx) = mpsc::unbounded_channel();
    let limits = ActorLimits {
        watchdog_stuck_after: Duration::from_secs(5),
        halt_reprobe_interval: Duration::from_secs(3600),
        ..ActorLimits::default()
    };
    let handle = tokio::spawn(async move {
        let backpressure = ActorInboxBackpressure::default();
        run_actor_with_limits(
            &mut worker,
            rx,
            Duration::from_millis(100),
            limits,
            &backpressure,
        )
        .await;
    });
    // Liveness first: the actor answers Status probes while stalled.
    let (reply, reply_rx) = oneshot::channel();
    tx.send(ActorCommand::Status(reply)).unwrap();
    let status = reply_rx.await.unwrap();
    assert!(!status.halted);
    // One window passes with the queue backed off: exactly one fire.
    tokio::time::advance(Duration::from_secs(6)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        global_series("two_bot_voice_watchdog_stucks_total"),
        stuck_before + 1
    );
    // A second full window fires once more (at most once per interval).
    tokio::time::advance(Duration::from_secs(6)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        global_series("two_bot_voice_watchdog_stucks_total"),
        stuck_before + 2
    );
    drop(tx);
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn global_429_requeues_and_resumes_without_losing_the_move() {
    let (live, store, http, trace) = fixture();
    http.move_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::RateLimited {
            retry_after_ms: 1_000,
            global: true,
        });
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 0).await;
    // Still queued (not dead-lettered) while the backoff holds.
    let (urgent, deferred) = worker.queue.pending_counts(GUILD);
    assert_eq!((urgent, deferred), (1, 0));
    assert!(worker.queue.oldest_pending_age(GUILD).is_some());
    assert!(!worker.dispatch_one(500).await);
    tokio::time::advance(Duration::from_secs(2)).await;
    dispatch(&mut worker, 2_000).await;
    // The move landed (the first `move` trace entry is the 429 attempt, the
    // second the success). The success queues a deferred post-move respace;
    // draining it leaves the queue empty.
    assert_eq!(
        *trace.lock().unwrap(),
        ["create", "persist:500", "move:300:500", "move:300:500"]
    );
    assert_eq!(worker.queue.pending_counts(GUILD), (0, 1));
    dispatch(&mut worker, 2_000).await;
    assert_eq!(worker.queue.pending_counts(GUILD), (0, 0));
    assert_eq!(worker.queue.oldest_pending_age(GUILD), None);
}

#[test]
fn voice_claims_custom_id_matches_sink_shapes() {
    use two_bot_core::voice_custom_id::{join_custom_id, name_modal_custom_id};
    use two_bot_core::voice_private::JoinDecision;

    let room = 777_000_111_222_333_444u64;
    let request = 888_000_111_222_333_444u64;
    let bare = join_custom_id(JoinDecision::Approve, room, request);
    // Production runtime shape: codec core plus the 32-hex worker epoch.
    let runtime = format!("{bare}:0123456789abcdef0123456789abcdef");
    assert!(voice_claims_custom_id(&bare), "bare join button");
    assert!(
        voice_claims_custom_id(&runtime),
        "epoch-suffixed join button"
    );
    assert!(
        voice_claims_custom_id(&name_modal_custom_id(room)),
        "name modal"
    );
    assert!(
        voice_claims_custom_id("votekick:7000000111222333444:yes"),
        "ballot yes"
    );
    assert!(
        voice_claims_custom_id("votekick:7000000111222333444:no"),
        "ballot no"
    );
    let short_epoch = format!("{bare}:0123456789abcdef0123456789abcde");
    let upper_epoch = format!("{bare}:0123456789ABCDEF0123456789ABCDEF");
    let nonhex_epoch = format!("{bare}:zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz");
    for unowned in [
        "two:stale-widget:deadbeef",
        "two:voice:join-approve:notanid:7",
        "two:voice:join-approve:500",
        short_epoch.as_str(),
        upper_epoch.as_str(),
        nonhex_epoch.as_str(),
        "votekick:7000000111222333444:maybe",
        "votekick:notanid:yes",
        "votekick:7000000111222333444",
    ] {
        assert!(!voice_claims_custom_id(unowned), "unowned: {unowned}");
    }
}
