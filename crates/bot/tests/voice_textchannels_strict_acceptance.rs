//! Protected acceptance: malformed `/textchannels` shapes refuse before any
//! store read or write, Discord call, audit, or live-actor update.
//!
//! Consumer: server admins toggling companion text channels per creator with
//! `/textchannels channel enabled name viewer-role`. User value: a mistyped
//! or forged option can never silently half-apply (wrong-typed options
//! dropped, duplicates last-wins) or read creator row 0; it is refused with
//! the store untouched and a copyable correction.
//!
//! Scope: this file pins `/textchannels` only. The five sibling V8 shapes
//! (`/position`, `/group`, `/inheritpermissions`, `/defaultlimit`,
//! `/alwaysprivate`) are pinned by the protected setup-strict handoff on
//! branch `test/voice-setup-option-shapes-acceptance`
//! (`crates/bot/tests/voice_setup_option_shape_acceptance.rs`); this file
//! reuses its refusal contract verbatim so all six per-creator shapes refuse
//! identically. The Builder for this card implements `/textchannels` strict
//! decode only and must never edit this file; test corrections go to the
//! Test Author.
//!
//! Baseline RED: the `textchannels` decoder drops wrong-typed options
//! (`_ => {}`), last-wins duplicates, and leaves `channel_id = 0` when
//! `channel` is missing or wrong-typed, yet the handler still performs
//! `store.creator_for(guild_id, 0)`. A wrong-typed `enabled`/`name`/
//! `viewer-role` with a valid `channel` can therefore save a truncated row
//! with a success receipt.
//!
//! Builder contract (this file is preserved verbatim):
//!
//! - `channel` must be `Channel`, `enabled` must be `Boolean`, `name` must
//!   be `String`, `viewer-role` must be `Role`. A present option with the
//!   wrong wire type refuses.
//! - An option name sent twice refuses in any order, adjacency, and value
//!   mix (different, reversed, identical, valid/wrong, wrong/valid).
//! - A missing `channel` refuses (it is required); omitted `enabled`/
//!   `name`/`viewer-role` keep today's keep-stored semantics.
//! - Unknown option names stay ignored (status quo), including duplicates
//!   of unknown names and nested known names beneath an unknown option.
//! - Every malformed shape above refuses BEFORE any store read or write
//!   (the `calls` log stays empty, including under `fail_reads`), with no
//!   creator mutation, no Discord call, and no live-actor update. The
//!   refusal is one private (ephemeral) reply naming the offending option
//!   plus a copyable `/textchannels <field>:` invocation, bounded,
//!   mention-safe, and echo-free (see `refused`).
//! - Preserved semantics: valid sets save byte-identically with today's
//!   receipts, omitted optionals keep stored values, a non-creator channel
//!   keeps its validation refusal after reads, the foreign guild row is
//!   untouched, and the registry gains no option and no new required flag.
//!
//! No production/staging credentials or effects; in-memory `Store`/
//! `Discord` doubles only, no disposable database needed. Failing on
//! baseline is the honest RED: truncated saves with success receipts,
//! reads before refusal, and last-wins duplicates.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::json;
use twilight_model::{
    application::interaction::{
        application_command::{CommandDataOption, CommandOptionValue},
        Interaction,
    },
    channel::{
        message::{component::Component, MessageFlags},
        permission_overwrite::PermissionOverwrite,
        Channel,
    },
    guild::{Permissions, Role},
    http::interaction::{InteractionResponse, InteractionResponseType},
    id::Id,
};
use two_bot::voice_rooms::{
    handle_voice_interaction, BotAccess, EventVoiceBinding, GuildSnapshot, NoticeTarget,
    RoomPersistence, RoomWrites, StoreError, VoiceRuntime, WriteGuard,
};
use two_bot_core::{
    voice_access::AccessControls,
    voice_config::VoiceConfiguration,
    voice_create_admission::CreateAdmissionConfig,
    voice_logging::LoggingSettings,
    voice_private::PrivacyRecord,
    voice_rooms::{CreatorChannel, PermissionSource, RoomPosition, TextCompanion, VoiceRoom},
    voice_text_channel::TextChannelPlan,
    voice_vote_kick_audit::KickAuditRow,
};
use two_bot_cutover::voice_rooms::{CreateClaim, JoinGrantIntent, OwnerGrantIntent};
use two_bot_discord::voice_rooms::{MessageRef, RoomChannelAttributes, RoomHttpError};

const GUILD: u64 = 100;
const CREATOR: u64 = 200;
const MEMBER: u64 = 300;
const SOURCE: u64 = 201;
const FOREIGN_GUILD: u64 = 101;
const FOREIGN_CHANNEL: u64 = 202;
const CREATED: u64 = 500;
const POISON: &str = "raw-error credential sentinel @everyone <@300>";

#[derive(Default)]
struct Observed {
    calls: Vec<String>,
    creators: BTreeMap<(u64, u64), CreatorChannel>,
    discord_creates: Vec<(u64, String, RoomChannelAttributes)>,
    saved: Vec<CreatorChannel>,
    fail_reads: bool,
}

type State = Arc<Mutex<Observed>>;

#[derive(Clone)]
struct Store(State);

impl Store {
    fn read(&self, name: &str, guild: u64) -> Result<(), StoreError> {
        let mut state = self.0.lock().expect("observation lock");
        state.calls.push(format!("{name}:{guild}"));
        if state.fail_reads {
            Err(StoreError::CredentialRefused)
        } else {
            Ok(())
        }
    }

    fn unexpected(&self, name: &str) -> StoreError {
        self.0
            .lock()
            .expect("observation lock")
            .calls
            .push(name.into());
        StoreError::Unavailable
    }
}

// The worker bootstrap uses real empty successful reads. Unrelated writes fail
// closed and remain observable, rather than being silently swallowed by a fake.
macro_rules! store_refuses {
    ($name:ident ($($arg:ident: $ty:ty),* $(,)?) -> $out:ty) => {
        async fn $name(&self, $($arg: $ty),*) -> Result<$out, StoreError> {
            Err(self.unexpected(stringify!($name)))
        }
    };
}

impl RoomPersistence for Store {
    async fn creators(&self, guild: u64) -> Result<Vec<CreatorChannel>, StoreError> {
        self.read("creators", guild)?;
        Ok(self
            .0
            .lock()
            .expect("observation lock")
            .creators
            .values()
            .filter(|c| c.guild_id == guild)
            .cloned()
            .collect())
    }
    async fn creator_for(
        &self,
        guild: u64,
        channel: u64,
    ) -> Result<Option<CreatorChannel>, StoreError> {
        self.read("creator_for", guild)?;
        Ok(self
            .0
            .lock()
            .expect("observation lock")
            .creators
            .get(&(guild, channel))
            .cloned())
    }
    async fn add_creator(&self, creator: &CreatorChannel) -> Result<(), StoreError> {
        let mut state = self.0.lock().expect("observation lock");
        state.calls.push(format!(
            "add_creator:{}:{}",
            creator.guild_id, creator.channel_id
        ));
        state.saved.push(creator.clone());
        state
            .creators
            .insert((creator.guild_id, creator.channel_id), creator.clone());
        Ok(())
    }
    async fn access_controls(&self, guild: u64) -> Result<AccessControls, StoreError> {
        self.read("access_controls", guild)?;
        Ok(AccessControls::default())
    }
    async fn rooms(&self, guild: u64) -> Result<Vec<VoiceRoom>, StoreError> {
        self.read("rooms", guild)?;
        Ok(vec![])
    }
    async fn pending_owner_grants(&self, guild: u64) -> Result<Vec<u64>, StoreError> {
        self.read("pending_owner_grants", guild)?;
        Ok(vec![])
    }
    async fn custom_names(&self, guild: u64) -> Result<Vec<(u64, String)>, StoreError> {
        self.read("custom_names", guild)?;
        Ok(vec![])
    }
    async fn logging_settings(&self, guild: u64) -> Result<LoggingSettings, StoreError> {
        self.read("logging_settings", guild)?;
        Ok(LoggingSettings::default())
    }
    async fn config_snapshot(&self, guild: u64) -> Result<VoiceConfiguration, StoreError> {
        self.read("config_snapshot", guild)?;
        Ok(VoiceConfiguration {
            version: two_bot_core::voice_config::VOICE_CONFIG_VERSION,
            guild_id: guild.to_string(),
            creators: vec![],
            templates: vec![],
            aliases: vec![],
            lists: vec![],
            logging: None,
            settings: two_bot_core::voice_config::GuildSettings {
                creation_enabled: true,
                unique_names: false,
                no_game_label: "No game".into(),
                force_single_game: false,
                count_members_without_activity: false,
                time_zone: "UTC".into(),
                text_channel_name: "voice-chat".into(),
                text_viewer_role_id: None,
                command_role_id: None,
                command_roles: vec![],
            },
        })
    }
    async fn orphaned_create_channels(&self, guild: u64) -> Result<Vec<(String, u64)>, StoreError> {
        self.read("orphaned_create_channels", guild)?;
        Ok(vec![])
    }
    async fn companions(&self, guild: u64) -> Result<Vec<TextCompanion>, StoreError> {
        self.read("companions", guild)?;
        Ok(vec![])
    }
    async fn privacy(&self, guild: u64) -> Result<BTreeMap<u64, PrivacyRecord>, StoreError> {
        self.read("privacy", guild)?;
        Ok(BTreeMap::new())
    }
    async fn join_grants(&self, guild: u64) -> Result<BTreeMap<u64, BTreeSet<u64>>, StoreError> {
        self.read("join_grants", guild)?;
        Ok(BTreeMap::new())
    }
    async fn event_voice_guild_bindings(
        &self,
        guild: u64,
    ) -> Result<Vec<EventVoiceBinding>, StoreError> {
        self.read("event_voice_guild_bindings", guild)?;
        Ok(vec![])
    }
    store_refuses!(persist(_room: &VoiceRoom) -> ());
    store_refuses!(prepare_owner_grants(_room: &VoiceRoom, _previous: u64) -> OwnerGrantIntent);
    store_refuses!(update_ownership(_room: &VoiceRoom, _revision: &str) -> bool);
    store_refuses!(save_custom_name(_guild: u64, _channel: u64, _name: Option<&str>) -> bool);
    store_refuses!(save_access_controls(_guild: u64, _controls: &AccessControls) -> ());
    store_refuses!(save_logging_settings(_guild: u64, _settings: &LoggingSettings) -> ());
    store_refuses!(
        config_apply(
            _guild: u64,
            _config: &VoiceConfiguration,
            _expected: &VoiceConfiguration,
        ) -> ()
    );
    store_refuses!(forget(_guild: u64, _channel: u64) -> ());
    store_refuses!(
        claim_create(
            _guild: u64,
            _user: u64,
            _leaving: &[u64],
            _config: &CreateAdmissionConfig,
            _now: i64,
        ) -> CreateClaim
    );
    store_refuses!(bind_create_channel(_guild: u64, _reservation: &str, _channel: u64) -> ());
    store_refuses!(persist_create(_reservation: &str, _room: &VoiceRoom) -> ());
    store_refuses!(settle_create(_reservation: &str) -> bool);
    store_refuses!(add_companion(_companion: &TextCompanion) -> bool);
    store_refuses!(remove_companion(_guild: u64, _room: u64) -> Option<TextCompanion>);
    store_refuses!(record_kick_audit(_rows: &[KickAuditRow]) -> ());
    store_refuses!(save_privacy(_guild: u64, _room: u64, _record: &PrivacyRecord) -> bool);
    store_refuses!(save_join_grant(_guild: u64, _room: u64, _member: u64) -> JoinGrantIntent);
    store_refuses!(remove_join_grant(_guild: u64, _room: u64, _member: u64) -> ());
    store_refuses!(record_event_voice_join(_channel: u64, _member: u64) -> ());
    store_refuses!(record_event_voice_leave(_channel: u64, _member: u64) -> ());
    store_refuses!(
        flush_event_voice_presence(
            _event: &str,
            _instance: time::OffsetDateTime,
            _members: &[u64],
        ) -> u64
    );
    store_refuses!(
        close_event_voice_spans_absent(
            _event: &str,
            _instance: time::OffsetDateTime,
            _present: &[u64],
        ) -> u64
    );
}

#[derive(Clone)]
struct Discord(State);

impl Discord {
    fn unexpected(&self, name: &str) -> RoomHttpError {
        self.0
            .lock()
            .expect("observation lock")
            .calls
            .push(format!("discord:{name}"));
        RoomHttpError::InvalidRequest
    }
}

macro_rules! discord_refuses {
    ($name:ident ($($arg:ident: $ty:ty),* $(,)?) -> $out:ty) => {
        async fn $name(&self, $($arg: $ty),*) -> Result<$out, RoomHttpError> {
            Err(self.unexpected(stringify!($name)))
        }
    };
}

impl RoomWrites for Discord {
    async fn create(
        &self,
        guild: u64,
        name: &str,
        attributes: &RoomChannelAttributes,
        guard: WriteGuard,
    ) -> Result<Channel, RoomHttpError> {
        assert!(guard(), "healthy control must have live write authority");
        let mut state = self.0.lock().expect("observation lock");
        state.calls.push("discord:create".into());
        state
            .discord_creates
            .push((guild, name.into(), attributes.clone()));
        Ok(channel(CREATED, guild))
    }
    fn is_unauthorized(&self) -> bool {
        self.0
            .lock()
            .expect("observation lock")
            .calls
            .push("discord:is_unauthorized".into());
        false
    }
    discord_refuses!(
        move_member(_guild: u64, _member: u64, _channel: u64, _guard: WriteGuard) -> ()
    );
    discord_refuses!(disconnect(_guild: u64, _member: u64, _guard: WriteGuard) -> ());
    discord_refuses!(deny_connect(_channel: u64, _member: u64, _guard: WriteGuard) -> ());
    discord_refuses!(delete(_channel: u64, _guard: WriteGuard) -> ());
    discord_refuses!(rename(_channel: u64, _name: &str) -> ());
    discord_refuses!(set_user_limit(_channel: u64, _limit: u32, _guard: WriteGuard) -> ());
    discord_refuses!(download_attachment(_url: &str, _max: usize) -> Vec<u8>);
    discord_refuses!(
        create_companion(_plan: &TextChannelPlan, _bot: u64, _guard: WriteGuard) -> Channel
    );
    discord_refuses!(grant_companion_view(_channel: u64, _member: u64, _guard: WriteGuard) -> ());
    discord_refuses!(revoke_companion_view(_channel: u64, _member: u64, _guard: WriteGuard) -> ());
    discord_refuses!(reorder_channels(_guild: u64, _positions: &[(u64, u64)]) -> ());
    discord_refuses!(set_voice_status(_channel: u64, _status: &str) -> ());
    discord_refuses!(
        put_overwrite(_channel: u64, _overwrite: PermissionOverwrite, _guard: WriteGuard) -> ()
    );
    discord_refuses!(
        create_join_channel(
            _guild: u64,
            _name: &str,
            _parent: Option<u64>,
            _position: Option<u64>,
            _guard: WriteGuard,
        ) -> Channel
    );
    discord_refuses!(delete_overwrite(_channel: u64, _member: u64, _guard: WriteGuard) -> ());
    discord_refuses!(
        send_component_message(
            _channel: u64,
            _content: &str,
            _mention: Option<u64>,
            _components: &[Component],
        ) -> MessageRef
    );
    discord_refuses!(
        edit_component_message(
            _message: MessageRef,
            _content: &str,
            _components: &[Component],
        ) -> ()
    );
    discord_refuses!(send_notice(_target: NoticeTarget, _content: &str, _role: Option<u64>) -> ());
}

fn channel(id: u64, guild: u64) -> Channel {
    serde_json::from_value(json!({
        "id": id.to_string(), "guild_id": guild.to_string(), "type": 2,
        "name": "lobby", "permission_overwrites": [], "user_limit": 8
    }))
    .expect("channel fixture")
}

fn role(id: u64, permissions: Permissions) -> Role {
    serde_json::from_value(json!({
        "id": id.to_string(), "name": "fixture", "color": 0, "hoist": false,
        "managed": false, "mentionable": false, "position": 0, "flags": 0,
        "colors": {"primary_color": 0, "secondary_color": null, "tertiary_color": null},
        "permissions": permissions.bits().to_string()
    }))
    .expect("role fixture")
}

fn seeded_creator() -> CreatorChannel {
    CreatorChannel {
        guild_id: GUILD,
        channel_id: CREATOR,
        name_template: "@@owner@@ room ##".into(),
        permission_source: PermissionSource::Channel(SOURCE),
        permission_channel_id: Some(SOURCE),
        default_limit: Some(7),
        private_default: true,
        text_channels: true,
        text_channel_name: Some("room-chat".into()),
        text_viewer_role_id: Some(901),
        position: RoomPosition::Above,
        first_room_number: 9,
        group_by_category: true,
    }
}

struct Fixture {
    runtime: VoiceRuntime<Store, Discord>,
    state: State,
}

impl Fixture {
    async fn new() -> Self {
        let state = State::default();
        {
            let mut observed = state.lock().expect("observation lock");
            observed.creators.insert((GUILD, CREATOR), seeded_creator());
            observed.creators.insert(
                (FOREIGN_GUILD, FOREIGN_CHANNEL),
                CreatorChannel::new(FOREIGN_GUILD, FOREIGN_CHANNEL),
            );
        }
        let shared = state.clone();
        let runtime = VoiceRuntime::new(
            move || (Store(shared.clone()), Discord(shared.clone())),
            Duration::from_secs(3600),
            true,
        );
        let permissions = Permissions::VIEW_CHANNEL
            | Permissions::CONNECT
            | Permissions::MANAGE_CHANNELS
            | Permissions::MOVE_MEMBERS
            | Permissions::MANAGE_ROLES;
        assert!(runtime.publish_snapshot(
            GUILD,
            GuildSnapshot {
                channels: vec![channel(CREATOR, GUILD), channel(SOURCE, GUILD)],
                members: vec![],
                bot: BotAccess {
                    member_id: 999,
                    guild_owner_id: 998,
                    system_channel_id: None,
                    member_roles: vec![Id::new(900)],
                    roles: vec![
                        role(GUILD, Permissions::empty()),
                        role(900, permissions),
                        role(
                            902,
                            Permissions::MANAGE_CHANNELS | Permissions::VIEW_CHANNEL
                        )
                    ],
                },
                member_roles: HashMap::new(),
            }
        ));
        // Let the immediately-ready first maintenance tick complete as well as
        // hydration. A status message alone can run before that initial tick.
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if state
                    .lock()
                    .expect("observation lock")
                    .calls
                    .iter()
                    .any(|call| call == "event_voice_guild_bindings:100")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first maintenance repair observed");
        assert!(
            runtime.worker_status(GUILD).await.is_some(),
            "worker hydrated and maintenance complete"
        );
        state.lock().expect("observation lock").calls.clear();
        Self { runtime, state }
    }

    async fn dispatch(&self, name: &str, options: Vec<CommandDataOption>) -> InteractionResponse {
        let interaction = interaction(name, options);
        let responses = Arc::new(Mutex::new(Vec::new()));
        let capture = responses.clone();
        assert!(
            handle_voice_interaction(&self.runtime, &interaction, move |response| {
                capture.lock().expect("response lock").push(response);
                async {}
            })
            .await,
            "/{name} must own its interaction"
        );
        let mut responses = responses.lock().expect("response lock");
        assert_eq!(responses.len(), 1, "/{name} must reply exactly once");
        responses.pop().expect("one response")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.runtime.disconnect();
    }
}

fn interaction(name: &str, options: Vec<CommandDataOption>) -> Interaction {
    let mut interaction: Interaction = serde_json::from_value(json!({
        "id": "2", "application_id": "1", "type": 2, "token": "fixture-only",
        "guild_id": GUILD.to_string(), "authorizing_integration_owners": {},
        "data": {"id": "3", "name": name, "type": 1},
        "member": {
            "user": {"id": MEMBER.to_string(), "username": "member", "discriminator": "0001", "avatar": null},
            "roles": ["902"], "deaf": false, "mute": false,
            "permissions": (Permissions::MANAGE_CHANNELS | Permissions::VIEW_CHANNEL).bits().to_string(),
            "joined_at": "2026-09-30T00:00:00.000000+00:00", "flags": 0
        },
        "entitlements": []
    })).expect("interaction fixture");
    let Some(twilight_model::application::interaction::InteractionData::ApplicationCommand(data)) =
        interaction.data.as_mut()
    else {
        panic!("application command fixture");
    };
    data.options = options;
    interaction
}

fn option(name: &str, value: CommandOptionValue) -> CommandDataOption {
    CommandDataOption {
        name: name.into(),
        value,
    }
}

fn string(value: &str) -> CommandOptionValue {
    CommandOptionValue::String(value.into())
}

fn channel_value(id: u64) -> CommandOptionValue {
    CommandOptionValue::Channel(Id::new(id))
}

struct Field {
    name: &'static str,
    good: CommandOptionValue,
    other: CommandOptionValue,
}

struct Shape {
    command: &'static str,
    fields: Vec<Field>,
    receipt: &'static str,
}

fn shape(command: &'static str) -> Shape {
    assert_eq!(
        command, "textchannels",
        "this file pins /textchannels only; the V8 shapes live in the setup-strict handoff"
    );
    let fields = vec![
        Field {
            name: "channel",
            good: channel_value(CREATOR),
            other: channel_value(SOURCE),
        },
        Field {
            name: "enabled",
            good: CommandOptionValue::Boolean(true),
            other: CommandOptionValue::Boolean(false),
        },
        Field {
            name: "name",
            good: string("lounge"),
            other: string("other-lounge"),
        },
        Field {
            name: "viewer-role",
            good: CommandOptionValue::Role(Id::new(42)),
            other: CommandOptionValue::Role(Id::new(43)),
        },
    ];
    let receipt = "Companion text channels are on for <#200>: named `lounge`, visible to occupants, admins and <@&42>. Applies to rooms created from now on; open rooms keep their settings.";
    Shape {
        command,
        fields,
        receipt,
    }
}

impl Shape {
    fn options(&self) -> Vec<CommandDataOption> {
        self.fields
            .iter()
            .map(|f| option(f.name, f.good.clone()))
            .collect()
    }

    fn saved(&self) -> CreatorChannel {
        assert_eq!(self.command, "textchannels");
        let mut creator = seeded_creator();
        creator.text_channels = true;
        creator.text_channel_name = Some("lounge".into());
        creator.text_viewer_role_id = Some(42);
        creator
    }
}

fn wrong_values() -> Vec<CommandOptionValue> {
    vec![
        string(POISON),
        CommandOptionValue::Integer(7),
        CommandOptionValue::Boolean(true),
        channel_value(CREATOR),
        CommandOptionValue::Number(7.5),
        CommandOptionValue::User(Id::new(MEMBER)),
        CommandOptionValue::Role(Id::new(901)),
        CommandOptionValue::Mentionable(Id::new(902)),
        CommandOptionValue::Attachment(Id::new(777)),
        CommandOptionValue::SubCommand(vec![option("enabled", CommandOptionValue::Boolean(true))]),
        CommandOptionValue::SubCommandGroup(vec![]),
    ]
}

fn private_text(response: &InteractionResponse) -> &str {
    assert_eq!(
        response.kind,
        InteractionResponseType::ChannelMessageWithSource
    );
    let data = response.data.as_ref().expect("private response data");
    assert_eq!(data.flags, Some(MessageFlags::EPHEMERAL));
    data.content.as_deref().expect("private response content")
}

async fn refused(
    shape: &Shape,
    field: &Field,
    options: Vec<CommandDataOption>,
    fail_reads: bool,
) -> Vec<String> {
    let fixture = Fixture::new().await;
    let before = fixture
        .state
        .lock()
        .expect("observation lock")
        .creators
        .clone();
    fixture.state.lock().expect("observation lock").fail_reads = fail_reads;
    let response = fixture.dispatch(shape.command, options).await;
    let text = private_text(&response);
    let observed = fixture.state.lock().expect("observation lock");
    let mut failures = vec![];
    if !observed.calls.is_empty() {
        failures.push(format!("read/effect before refusal: {:?}", observed.calls));
    }
    if observed.creators != before
        || !observed.saved.is_empty()
        || !observed.discord_creates.is_empty()
    {
        failures.push("malformed input changed creator/Discord state".into());
    }
    if !text.contains(field.name) {
        failures.push(format!("correction does not name {}: {text}", field.name));
    }
    let invocation = format!("/{} ", shape.command);
    if !text.contains(&invocation) || !text.contains(&format!("{}:", field.name)) {
        failures.push(format!(
            "no copyable corrected invocation for {}: {text}",
            field.name
        ));
    }
    if text.chars().count() > 2000
        || text.contains(POISON)
        || [
            "credential",
            "Unavailable",
            "CredentialRefused",
            "sqlx",
            "postgres://",
            "@everyone",
            "<@300>",
        ]
        .iter()
        .any(|s| text.contains(s))
    {
        failures.push(format!("unbounded or disclosing correction: {text}"));
    }
    failures
}

async fn duplicated_options_refuse(command: &'static str) {
    let shape = shape(command);
    let mut failures = vec![];
    for field in &shape.fields {
        for (label, first, second) in [
            ("different", field.good.clone(), field.other.clone()),
            ("reversed", field.other.clone(), field.good.clone()),
            ("identical", field.good.clone(), field.good.clone()),
            ("valid/wrong", field.good.clone(), string(POISON)),
            ("wrong/valid", string(POISON), field.good.clone()),
        ] {
            let mut options = shape.options();
            let index = options
                .iter()
                .position(|o| o.name == field.name)
                .expect("field");
            options[index].value = first;
            options.insert(index + 1, option(field.name, second));
            for failure in refused(&shape, field, options, false).await {
                failures.push(format!("/{command} {} {label}: {failure}", field.name));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

async fn wrong_types_refuse(command: &'static str) {
    let shape = shape(command);
    let mut failures = vec![];
    for field in &shape.fields {
        for wrong in wrong_values() {
            if std::mem::discriminant(&wrong) == std::mem::discriminant(&field.good) {
                continue;
            }
            let mut options = shape.options();
            options
                .iter_mut()
                .find(|o| o.name == field.name)
                .expect("field")
                .value = wrong.clone();
            for failure in refused(&shape, field, options, false).await {
                failures.push(format!("/{command} {} {wrong:?}: {failure}", field.name));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

async fn valid_and_unknown_control(command: &'static str) {
    let shape = shape(command);
    for unknown in [
        None,
        Some(string(POISON)),
        Some(CommandOptionValue::SubCommand(vec![
            option("channel", channel_value(FOREIGN_CHANNEL)),
            option("enabled", string(POISON)),
        ])),
    ] {
        let fixture = Fixture::new().await;
        let mut options = shape.options();
        if let Some(unknown) = unknown {
            // Unknown names (including duplicates) remain ignored. Nested known
            // names beneath an unknown option must not be promoted into fields.
            options.insert(0, option("future-option", unknown.clone()));
            options.push(option("future-option", unknown));
        }
        let response = fixture.dispatch(command, options).await;
        assert_eq!(
            private_text(&response),
            shape.receipt,
            "/{command} valid receipt"
        );
        {
            let observed = fixture.state.lock().expect("observation lock");
            assert_eq!(
                observed.saved,
                vec![shape.saved()],
                "/{command} saved exact creator"
            );
            assert_eq!(
                observed.creators.get(&(FOREIGN_GUILD, FOREIGN_CHANNEL)),
                Some(&CreatorChannel::new(FOREIGN_GUILD, FOREIGN_CHANNEL)),
                "foreign guild unchanged"
            );
            assert!(
                !observed
                    .calls
                    .iter()
                    .any(|call| call == "record_kick_audit" || call == "claim_create"),
                "textchannels does not enter room audit/admission"
            );
            assert!(
                observed.discord_creates.is_empty(),
                "settings do not mutate live Discord"
            );
        }
        // Repeating the valid set is byte-identical: same receipt, same row.
        let repeat = fixture.dispatch(command, shape.options()).await;
        assert_eq!(
            private_text(&repeat),
            shape.receipt,
            "/{command} repeat receipt byte-identical"
        );
        let observed = fixture.state.lock().expect("observation lock");
        assert_eq!(
            observed.saved,
            vec![shape.saved(), shape.saved()],
            "/{command} repeat saves the same row"
        );
    }
}

macro_rules! acceptance {
    ($duplicates:ident, $types:ident, $control:ident, $command:literal) => {
        #[tokio::test]
        async fn $duplicates() {
            duplicated_options_refuse($command).await;
        }
        #[tokio::test]
        async fn $types() {
            wrong_types_refuse($command).await;
        }
        #[tokio::test]
        async fn $control() {
            valid_and_unknown_control($command).await;
        }
    };
}

acceptance!(
    textchannels_duplicates_refuse_before_reads,
    textchannels_wrong_types_refuse_before_reads,
    textchannels_valid_and_unknown_are_unchanged,
    "textchannels"
);

#[tokio::test]
async fn textchannels_missing_channel_refuses_before_any_read() {
    let shape = shape("textchannels");
    let channel = shape
        .fields
        .iter()
        .find(|f| f.name == "channel")
        .expect("channel field");
    // Bare invocation plus every channel-less subset: the required channel
    // is absent, so strict decode refuses before the creator_for(guild, 0)
    // read the baseline still performs.
    let cases: Vec<(&str, Vec<CommandDataOption>)> = vec![
        ("bare", vec![]),
        (
            "enabled only",
            vec![option("enabled", CommandOptionValue::Boolean(true))],
        ),
        ("name only", vec![option("name", string("lounge"))]),
        (
            "viewer-role only",
            vec![option("viewer-role", CommandOptionValue::Role(Id::new(42)))],
        ),
        (
            "all but channel",
            vec![
                option("enabled", CommandOptionValue::Boolean(true)),
                option("name", string("lounge")),
                option("viewer-role", CommandOptionValue::Role(Id::new(42))),
            ],
        ),
    ];
    let mut failures = vec![];
    for (label, options) in &cases {
        for failure in refused(&shape, channel, options.clone(), false).await {
            failures.push(format!("missing channel {label}: {failure}"));
        }
        for failure in refused(&shape, channel, options.clone(), true).await {
            failures.push(format!("missing channel {label} poisoned: {failure}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn malformed_shapes_do_not_disclose_store_failure_or_consume_a_corrected_attempt() {
    let mut failures = vec![];
    for command in ["textchannels"] {
        let shape = shape(command);
        let field = shape.fields.last().expect("field");
        let mut malformed = shape.options();
        malformed.push(option(field.name, field.other.clone()));
        failures.extend(refused(&shape, field, malformed.clone(), true).await);
        let fixture = Fixture::new().await;
        let rejected = fixture.dispatch(command, malformed.clone()).await;
        let corrected = fixture.dispatch(command, shape.options()).await;
        assert_eq!(
            private_text(&corrected),
            shape.receipt,
            "same-member immediate correction must be admitted"
        );
        let rejected_again = fixture.dispatch(command, malformed).await;
        if serde_json::to_value(&rejected).expect("reply JSON")
            != serde_json::to_value(&rejected_again).expect("reply JSON")
        {
            failures.push(format!(
                "/{command} refusal changed after corrected attempt"
            ));
        }
        let observed = fixture.state.lock().expect("observation lock");
        if observed.saved != vec![shape.saved()] {
            failures.push(format!(
                "/{command} malformed attempts were admitted: {:?}",
                observed.saved
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn omitted_optional_fields_preserve_existing_receipts_and_state() {
    // Seeded creator: text on, name `room-chat`, viewer role 901. Only
    // `channel` is required: all-omitted is a read (no save), while a
    // partially-omitted update keeps the stored values and saves once.
    let on_with_stored = "Companion text channels are on for <#200>: named `room-chat`, visible to occupants, admins and <@&901>. Applies to rooms created from now on; open rooms keep their settings.";
    let cases: Vec<(
        Vec<CommandDataOption>,
        &'static str,
        Option<CreatorChannel>,
    )> = vec![
        (
            vec![option("channel", channel_value(CREATOR))],
            on_with_stored,
            None,
        ),
        (
            vec![
                option("channel", channel_value(CREATOR)),
                option("enabled", CommandOptionValue::Boolean(false)),
            ],
            "Companion text channels are off for <#200>. Rooms already open keep theirs; new rooms get none.",
            Some({
                let mut c = seeded_creator();
                c.text_channels = false;
                c
            }),
        ),
        (
            vec![
                option("channel", channel_value(CREATOR)),
                option("name", string("lounge")),
            ],
            "Companion text channels are on for <#200>: named `lounge`, visible to occupants, admins and <@&901>. Applies to rooms created from now on; open rooms keep their settings.",
            Some({
                let mut c = seeded_creator();
                c.text_channel_name = Some("lounge".into());
                c
            }),
        ),
        (
            vec![
                option("channel", channel_value(CREATOR)),
                option("viewer-role", CommandOptionValue::Role(Id::new(42))),
            ],
            "Companion text channels are on for <#200>: named `room-chat`, visible to occupants, admins and <@&42>. Applies to rooms created from now on; open rooms keep their settings.",
            Some({
                let mut c = seeded_creator();
                c.text_viewer_role_id = Some(42);
                c
            }),
        ),
    ];
    for (options, receipt, expected) in &cases {
        let fixture = Fixture::new().await;
        let response = fixture.dispatch("textchannels", options.clone()).await;
        assert_eq!(
            private_text(&response),
            *receipt,
            "/textchannels omission receipt"
        );
        let observed = fixture.state.lock().expect("observation lock");
        assert_eq!(
            observed.saved,
            expected.iter().cloned().collect::<Vec<_>>(),
            "/textchannels omission state"
        );
        assert!(
            observed.discord_creates.is_empty(),
            "omission saves touch no Discord"
        );
    }
}

#[tokio::test]
async fn malformed_options_refuse_even_before_guild_membership_is_warm() {
    for command in ["textchannels"] {
        let shape = shape(command);
        let mut options = shape.options();
        let field = &shape.fields[0];
        options.push(option(field.name, field.good.clone()));
        let warm = Fixture::new().await;
        let expected = warm.dispatch(command, options.clone()).await;
        let state = State::default();
        let shared = state.clone();
        let cold = Fixture {
            runtime: VoiceRuntime::new(
                move || (Store(shared.clone()), Discord(shared.clone())),
                Duration::from_secs(3600),
                true,
            ),
            state,
        };
        let response = cold.dispatch(command, options).await;
        let text = private_text(&response);
        assert!(
            text.contains(field.name) && text.contains(&format!("/{command} ")),
            "malformed payload must produce a correction, not a membership/warming refusal: {text}"
        );
        assert_eq!(
            serde_json::to_value(response).expect("reply JSON"),
            serde_json::to_value(expected).expect("reply JSON"),
            "malformed correction cannot depend on cached membership"
        );
        assert!(cold
            .state
            .lock()
            .expect("observation lock")
            .calls
            .is_empty());
    }
}

#[tokio::test]
async fn healthy_channel_scope_does_not_edit_a_foreign_creator() {
    // A well-typed but non-creator channel keeps its validation refusal
    // after reads: strict decode must not swallow it, and nothing saves.
    let fixture = Fixture::new().await;
    let response = fixture
        .dispatch(
            "textchannels",
            vec![
                option("channel", channel_value(SOURCE)),
                option("enabled", CommandOptionValue::Boolean(true)),
            ],
        )
        .await;
    assert_eq!(
        private_text(&response),
        "That channel is not a voice-room creator. Pick one made with /create."
    );
    let observed = fixture.state.lock().expect("observation lock");
    assert!(observed.saved.is_empty());
    assert!(
        observed.calls.iter().any(|call| call == "creator_for:100"),
        "validation still reads the row: {:?}",
        observed.calls
    );
    assert_eq!(
        observed.creators.get(&(FOREIGN_GUILD, FOREIGN_CHANNEL)),
        Some(&CreatorChannel::new(FOREIGN_GUILD, FOREIGN_CHANNEL)),
        "foreign guild unchanged"
    );
}

#[test]
fn registry_keeps_the_textchannels_shape_without_new_required_options() {
    let defs = two_bot_core::voice_rooms::voice_commands();
    let textchannels = defs
        .iter()
        .find(|def| def.name == "textchannels")
        .expect("textchannels stays published");
    let names: Vec<&str> = textchannels
        .options
        .iter()
        .map(|opt| opt.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["channel", "enabled", "name", "viewer-role"],
        "no new /textchannels option"
    );
    let kind = |name: &str| -> u8 {
        textchannels
            .options
            .iter()
            .find(|opt| opt.name == name)
            .unwrap_or_else(|| panic!("{name} stays published"))
            .kind
    };
    // Discord wire types: Channel 7, Boolean 5, String 3, Role 8.
    assert_eq!(kind("channel"), 7);
    assert_eq!(kind("enabled"), 5);
    assert_eq!(kind("name"), 3);
    assert_eq!(kind("viewer-role"), 8);
    let required = |name: &str| -> Option<bool> {
        textchannels
            .options
            .iter()
            .find(|opt| opt.name == name)
            .unwrap_or_else(|| panic!("{name} stays published"))
            .required
    };
    assert_eq!(required("channel"), Some(true), "channel stays required");
    assert_ne!(
        required("enabled"),
        Some(true),
        "enabled stays optional so omission keeps the stored value"
    );
    assert_ne!(
        required("name"),
        Some(true),
        "name stays optional so omission keeps the stored value"
    );
    assert_ne!(
        required("viewer-role"),
        Some(true),
        "viewer-role stays optional so omission keeps the stored value"
    );
}
