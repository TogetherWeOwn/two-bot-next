//! Protected acceptance: strict `/logging` option shapes and vote-kick `reason` decoding.
//!
//! Locked contract for `/logging show|level|channel|mention` subcommand shapes plus
//! `/kick reason`, wired through the real `handle_voice_interaction` dispatch with
//! local stateful store/Discord doubles. Every case is behavioral: it dispatches a
//! crafted Twilight interaction and asserts the user-visible reply plus the persistent
//! store, never a decoder symbol, so RED means "today's decode is observably wrong",
//! not "a helper is missing".
//!
//! - A duplicated option name inside one `/logging` subcommand (both orders,
//!   conflicting or identical values, valid/wrong mixes), or more than one top-level
//!   subcommand in a single `/logging` invocation, is a *decode failure*. It refuses
//!   BEFORE any store read (including the command gate), store write, audit write,
//!   Discord effect or logging save, with one bounded ephemeral correction that names
//!   the offending option and carries a copyable corrected invocation (`/logging …`
//!   plus `option:`), echoes no user input (mention-safe), and stays far under
//!   Discord's 2,000-unit cap. No logging row changes; Discord sees exactly one
//!   private response and nothing else.
//! - A present-but-wrong-wire-type `/kick reason` (every non-string variant), or a
//!   duplicated `reason` name (both orders), is a *decode failure* with the same
//!   before-effects refusal shape (`/kick …` plus `reason:`). The public ballot never
//!   posts without the stated reason, and no vote ledger entry is claimed: a
//!   corrected retry with a fresh interaction id starts the vote immediately.
//! - Unknown option names stay ignored: a valid `/logging` subcommand with extra
//!   unknown args (including duplicated unknowns and hostile values) saves
//!   byte-identically, and a valid `/kick` with extra unknown args posts the
//!   byte-identical ballot. A missing *optional* option still means "absent"
//!   (omitted `channel`/`role` clears, omitted `reason` starts the vote with no
//!   `Reason:` line). Existing wrong-type `/logging` refusals still refuse (they
//!   keep today's `Unknown /logging option…` text or upgrade to the new correction;
//!   either is accepted, but `Saved.` is never accepted).
//! - Well-typed valid paths keep their pinned texts byte-identical: `/logging show`
//!   renders the summary without saving, level/channel/mention saves render
//!   `Saved.` plus the summary, and the vote ballot renders the exact public text
//!   with only the target pinged, embeds suppressed, and two enabled buttons.
//!
//! RED map (fails on current `main`, flips GREEN only with a correct
//! implementation): every `logging_duplicate_*`, `logging_extra_subcommands_*`,
//! `kick_wrong_type_reason_*` and `kick_duplicate_reason_*` test. Today's wrong
//! behavior per matrix: duplicated logging options are first-win (they save the
//! first value); trailing subcommands are ignored (only `options.first()` is read);
//! wrong-typed kick `reason` decodes `_ => None` so the public ballot posts without
//! the stated reason; duplicated kick `reason` is first-win (it posts with the first
//! value). GREEN pins (pass today and after): every `*_valid_*`, `*_unknown_*`,
//! `*_omitted_*`, `*_preserved_*` and foreign-guild control.
//!
//! Runnable recipe (disposable test DB only, never staging/production; this target
//! needs no database, secret, Discord token or live service):
//! ```sh
//! cargo fmt --all -- --check
//! python3 scripts/cargo_cache.py run -- test -p two-bot \
//!   --test voice_logging_option_shape_acceptance -- --nocapture --test-threads=1
//! ```
//! On this RED head the `duplicate`/`extra`/`wrong_type` tests fail and every other
//! test in this file passes. After the Builder lands, the whole target must be GREEN.
//!
//! Builder rules: consume this file's branch/SHA unchanged (see the issue's
//! protected-file manifest), never edit this file, keep the existing
//! `parse_logging_subcommands` / `parse_kick_*` unit tests compiling (narrow only
//! what contradicts the new strict contract, with a Test Author follow-up for any
//! concrete conflict), and route any genuine fixture/oracle correction back through
//! the Test Author instead of waiving it.

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
    id::{marker::UserMarker, Id},
};
use two_bot::voice_rooms::{
    handle_voice_interaction, BotAccess, EventVoiceBinding, GuildSnapshot, NoticeTarget,
    RoomPersistence, RoomWrites, StoreError, VoiceMember, VoiceRuntime, WriteGuard,
};
use two_bot_core::{
    voice_access::AccessControls,
    voice_config::VoiceConfiguration,
    voice_create_admission::CreateAdmissionConfig,
    voice_logging::{DetailLevel, LoggingSettings},
    voice_private::PrivacyRecord,
    voice_rooms::{CreatorChannel, NewRoomSpec, TextCompanion, VoiceRoom},
    voice_text_channel::TextChannelPlan,
    voice_vote_kick_audit::KickAuditRow,
};
use two_bot_cutover::voice_rooms::{CreateClaim, JoinGrantIntent, OwnerGrantIntent};
use two_bot_discord::voice_rooms::{MessageRef, RoomChannelAttributes, RoomHttpError};

const GUILD: u64 = 100;
const MEMBER: u64 = 300;
const VOTER_A: u64 = 301;
const VOTER_B: u64 = 302;
const TARGET: u64 = 303;
const VOTER_C: u64 = 304;
const ROOM: u64 = 500;
const OTHER_ROOM: u64 = 501;
const LOG_TEXT_CHANNEL: u64 = 5;
const LOG_TEXT_CHANNEL_OTHER: u64 = 6;
const MENTION_ROLE: u64 = 9;
const MENTION_ROLE_OTHER: u64 = 11;
const ACTOR_ROLE: u64 = 902;
const FOREIGN_GUILD: u64 = 101;
const POISON: &str = "raw-error credential sentinel @everyone <@300>";

const OLD_INVALID_TEXT: &str = "Unknown /logging option. Use show, level, channel or mention.";
const READ_FAILURE_TEXT: &str =
    "Could not read the logging settings. Nothing was changed; try again.";
const SAVE_FAILURE_TEXT: &str =
    "Could not save the logging settings. Nothing was changed; try again.";
const GATE_FAILURE_TEXT: &str = "Voice-room settings are unavailable right now. Try again shortly.";
const CHANNEL_FALLBACK: &str =
    "not set (falls back to the server's system channel, then a DM, then the creator channel's chat)";

#[derive(Default)]
struct Observed {
    calls: Vec<String>,
    logging: BTreeMap<u64, LoggingSettings>,
    saved_logging: Vec<(u64, LoggingSettings)>,
    rooms: BTreeMap<u64, VoiceRoom>,
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
        Ok(vec![])
    }
    async fn creator_for(
        &self,
        guild: u64,
        _channel: u64,
    ) -> Result<Option<CreatorChannel>, StoreError> {
        self.read("creator_for", guild)?;
        Ok(None)
    }
    async fn add_creator(&self, _creator: &CreatorChannel) -> Result<(), StoreError> {
        Err(self.unexpected("add_creator"))
    }
    async fn access_controls(&self, guild: u64) -> Result<AccessControls, StoreError> {
        self.read("access_controls", guild)?;
        Ok(AccessControls::default())
    }
    async fn rooms(&self, guild: u64) -> Result<Vec<VoiceRoom>, StoreError> {
        self.read("rooms", guild)?;
        Ok(self
            .0
            .lock()
            .expect("observation lock")
            .rooms
            .values()
            .filter(|room| room.guild_id == guild)
            .cloned()
            .collect())
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
        Ok(self
            .0
            .lock()
            .expect("observation lock")
            .logging
            .get(&guild)
            .copied()
            .unwrap_or_default())
    }
    async fn save_logging_settings(
        &self,
        guild: u64,
        settings: &LoggingSettings,
    ) -> Result<(), StoreError> {
        let mut state = self.0.lock().expect("observation lock");
        state.calls.push(format!("save_logging_settings:{guild}"));
        state.logging.insert(guild, *settings);
        state.saved_logging.push((guild, *settings));
        Ok(())
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
    store_refuses!(config_apply(_guild: u64, _config: &VoiceConfiguration, _expected: &VoiceConfiguration) -> ());
    store_refuses!(forget(_guild: u64, _channel: u64) -> ());
    store_refuses!(claim_create(_guild: u64, _user: u64, _leaving: &[u64], _config: &CreateAdmissionConfig, _now: i64) -> CreateClaim);
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
    store_refuses!(flush_event_voice_presence(_event: &str, _instance: time::OffsetDateTime, _members: &[u64]) -> u64);
    store_refuses!(close_event_voice_spans_absent(_event: &str, _instance: time::OffsetDateTime, _present: &[u64]) -> u64);
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
    fn is_unauthorized(&self) -> bool {
        self.0
            .lock()
            .expect("observation lock")
            .calls
            .push("discord:is_unauthorized".into());
        false
    }
    discord_refuses!(create(_guild: u64, _name: &str, _attributes: &RoomChannelAttributes, _guard: WriteGuard) -> Channel);
    discord_refuses!(move_member(_guild: u64, _member: u64, _channel: u64, _guard: WriteGuard) -> ());
    discord_refuses!(disconnect(_guild: u64, _member: u64, _guard: WriteGuard) -> ());
    discord_refuses!(deny_connect(_channel: u64, _member: u64, _guard: WriteGuard) -> ());
    discord_refuses!(delete(_channel: u64, _guard: WriteGuard) -> ());
    discord_refuses!(rename(_channel: u64, _name: &str) -> ());
    discord_refuses!(set_user_limit(_channel: u64, _limit: u32, _guard: WriteGuard) -> ());
    discord_refuses!(download_attachment(_url: &str, _max: usize) -> Vec<u8>);
    discord_refuses!(create_companion(_plan: &TextChannelPlan, _bot: u64, _guard: WriteGuard) -> Channel);
    discord_refuses!(grant_companion_view(_channel: u64, _member: u64, _guard: WriteGuard) -> ());
    discord_refuses!(revoke_companion_view(_channel: u64, _member: u64, _guard: WriteGuard) -> ());
    discord_refuses!(reorder_channels(_guild: u64, _positions: &[(u64, u64)]) -> ());
    discord_refuses!(set_voice_status(_channel: u64, _status: &str) -> ());
    discord_refuses!(put_overwrite(_channel: u64, _overwrite: PermissionOverwrite, _guard: WriteGuard) -> ());
    discord_refuses!(create_join_channel(_guild: u64, _name: &str, _parent: Option<u64>, _position: Option<u64>, _guard: WriteGuard) -> Channel);
    discord_refuses!(delete_overwrite(_channel: u64, _member: u64, _guard: WriteGuard) -> ());
    discord_refuses!(send_component_message(_channel: u64, _content: &str, _mention: Option<u64>, _components: &[Component]) -> MessageRef);
    discord_refuses!(edit_component_message(_message: MessageRef, _content: &str, _components: &[Component]) -> ());
    discord_refuses!(send_notice(_target: NoticeTarget, _content: &str, _role: Option<u64>) -> ());
}

fn voice_channel(id: u64, guild: u64) -> Channel {
    serde_json::from_value(json!({
        "id": id.to_string(), "guild_id": guild.to_string(), "type": 2,
        "name": "room", "permission_overwrites": [], "user_limit": 8
    }))
    .expect("voice channel fixture")
}

fn text_channel(id: u64, guild: u64) -> Channel {
    serde_json::from_value(json!({
        "id": id.to_string(), "guild_id": guild.to_string(), "type": 0,
        "name": "logs", "permission_overwrites": [], "nsfw": false
    }))
    .expect("text channel fixture")
}

fn role_with(id: u64, permissions: Permissions, position: u16, mentionable: bool) -> Role {
    serde_json::from_value(json!({
        "id": id.to_string(), "name": "fixture", "color": 0, "hoist": false,
        "managed": false, "mentionable": mentionable, "position": position, "flags": 0,
        "colors": {"primary_color": 0, "secondary_color": null, "tertiary_color": null},
        "permissions": permissions.bits().to_string()
    }))
    .expect("role fixture")
}

fn seed_room(channel_id: u64) -> VoiceRoom {
    VoiceRoom::from_spec(
        NewRoomSpec {
            guild_id: GUILD,
            creator_channel_id: 200,
            owner_id: MEMBER,
            seed: 7,
            created_at: "2026-09-30T00:00:00.000000+00:00".into(),
        },
        channel_id,
    )
}

fn occupant(member_id: u64) -> VoiceMember {
    VoiceMember {
        member_id,
        channel_id: ROOM,
        bot: Some(false),
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
            observed.logging.insert(GUILD, LoggingSettings::default());
            observed
                .logging
                .insert(FOREIGN_GUILD, LoggingSettings::default());
            observed.rooms.insert(ROOM, seed_room(ROOM));
            observed.rooms.insert(OTHER_ROOM, seed_room(OTHER_ROOM));
        }
        let shared = state.clone();
        let runtime = VoiceRuntime::new(
            move || (Store(shared.clone()), Discord(shared.clone())),
            Duration::from_secs(3600),
            true,
        );
        let members = vec![
            occupant(VOTER_A),
            occupant(VOTER_B),
            occupant(TARGET),
            occupant(VOTER_C),
        ];
        let member_roles: HashMap<u64, Vec<u64>> = members
            .iter()
            .map(|member| (member.member_id, Vec::new()))
            .collect();
        assert!(runtime.publish_snapshot(
            GUILD,
            GuildSnapshot {
                channels: vec![
                    voice_channel(ROOM, GUILD),
                    voice_channel(OTHER_ROOM, GUILD),
                    text_channel(LOG_TEXT_CHANNEL, GUILD),
                    text_channel(LOG_TEXT_CHANNEL_OTHER, GUILD),
                ],
                members,
                bot: BotAccess {
                    member_id: 999,
                    guild_owner_id: 998,
                    system_channel_id: None,
                    member_roles: vec![Id::new(900)],
                    roles: vec![
                        role_with(
                            GUILD,
                            Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES,
                            0,
                            false,
                        ),
                        role_with(
                            900,
                            Permissions::VIEW_CHANNEL
                                | Permissions::CONNECT
                                | Permissions::MANAGE_CHANNELS
                                | Permissions::MOVE_MEMBERS
                                | Permissions::MANAGE_ROLES,
                            0,
                            false,
                        ),
                        role_with(
                            ACTOR_ROLE,
                            Permissions::VIEW_CHANNEL | Permissions::MANAGE_CHANNELS,
                            2,
                            false,
                        ),
                        role_with(MENTION_ROLE, Permissions::empty(), 1, true),
                        role_with(MENTION_ROLE_OTHER, Permissions::empty(), 1, true),
                    ],
                },
                member_roles,
            }
        ));
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

    async fn dispatch(&self, interaction: &Interaction) -> InteractionResponse {
        let responses = Arc::new(Mutex::new(Vec::new()));
        let capture = responses.clone();
        assert!(
            handle_voice_interaction(&self.runtime, interaction, move |response| {
                capture.lock().expect("response lock").push(response);
                async {}
            })
            .await,
            "voice must own its interaction"
        );
        let mut responses = responses.lock().expect("response lock");
        assert_eq!(responses.len(), 1, "exactly one reply per invocation");
        responses.pop().expect("one response")
    }

    async fn dispatch_logging(&self, subcommands: Vec<CommandDataOption>) -> InteractionResponse {
        self.dispatch(&interaction_with("logging", subcommands, MEMBER, 2, GUILD))
            .await
    }

    async fn dispatch_kick(
        &self,
        options: Vec<CommandDataOption>,
        invoker: u64,
        interaction_id: u64,
    ) -> InteractionResponse {
        self.dispatch(&interaction_with(
            "kick",
            options,
            invoker,
            interaction_id,
            GUILD,
        ))
        .await
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.runtime.disconnect();
    }
}

fn interaction_with(
    name: &str,
    options: Vec<CommandDataOption>,
    invoker: u64,
    interaction_id: u64,
    guild: u64,
) -> Interaction {
    let mut interaction: Interaction = serde_json::from_value(json!({
        "id": interaction_id.to_string(), "application_id": "1", "type": 2, "token": "fixture-only",
        "guild_id": guild.to_string(), "authorizing_integration_owners": {},
        "data": {"id": "3", "name": name, "type": 1},
        "member": {
            "user": {"id": invoker.to_string(), "username": "member", "discriminator": "0001", "avatar": null},
            "roles": [ACTOR_ROLE.to_string()], "deaf": false, "mute": false,
            "permissions": (Permissions::MANAGE_CHANNELS | Permissions::VIEW_CHANNEL).bits().to_string(),
            "joined_at": "2026-09-30T00:00:00.000000+00:00", "flags": 0
        },
        "entitlements": []
    }))
    .expect("interaction fixture");
    let Some(twilight_model::application::interaction::InteractionData::ApplicationCommand(data)) =
        interaction.data.as_mut()
    else {
        panic!("application command fixture");
    };
    data.options = options;
    interaction
}

fn opt(name: &str, value: CommandOptionValue) -> CommandDataOption {
    CommandDataOption {
        name: name.into(),
        value,
    }
}

fn str_opt(name: &str, value: &str) -> CommandDataOption {
    opt(name, CommandOptionValue::String(value.into()))
}

fn sub(name: &str, args: Vec<CommandDataOption>) -> CommandDataOption {
    opt(name, CommandOptionValue::SubCommand(args))
}

fn channel_opt(name: &str, id: u64) -> CommandDataOption {
    opt(name, CommandOptionValue::Channel(Id::new(id)))
}

fn role_opt(name: &str, id: u64) -> CommandDataOption {
    opt(name, CommandOptionValue::Role(Id::new(id)))
}

fn user_opt(name: &str, id: u64) -> CommandDataOption {
    opt(name, CommandOptionValue::User(Id::new(id)))
}

fn private_text(response: &InteractionResponse) -> &str {
    assert_eq!(
        response.kind,
        InteractionResponseType::ChannelMessageWithSource
    );
    let data = response.data.as_ref().expect("private response data");
    assert_eq!(data.flags, Some(MessageFlags::EPHEMERAL));
    assert!(
        data.components.as_ref().is_none_or(Vec::is_empty),
        "a refusal carries no buttons"
    );
    data.content.as_deref().expect("private response content")
}

fn public_ballot_text(response: &InteractionResponse, vote_id: u64) -> &str {
    assert_eq!(
        response.kind,
        InteractionResponseType::ChannelMessageWithSource
    );
    let data = response.data.as_ref().expect("ballot data");
    assert_eq!(data.flags, Some(MessageFlags::SUPPRESS_EMBEDS));
    assert_eq!(
        data.allowed_mentions
            .as_ref()
            .map(|mentions| mentions.users.clone())
            .unwrap_or_default(),
        [Id::<UserMarker>::new(TARGET)]
    );
    assert!(data
        .allowed_mentions
        .as_ref()
        .is_some_and(|mentions| { mentions.parse.is_empty() && mentions.roles.is_empty() }));
    assert!(data.embeds.as_ref().is_none_or(Vec::is_empty));
    let components = data.components.as_ref().expect("ballot buttons");
    assert_eq!(components.len(), 1, "ballot is one button row");
    let Component::ActionRow(row) = &components[0] else {
        panic!("ballot row must be an action row");
    };
    assert_eq!(row.components.len(), 2, "Yes plus No");
    for (component, ballot) in row.components.iter().zip(["yes", "no"]) {
        let Component::Button(button) = component else {
            panic!("ballot row must hold buttons only");
        };
        assert!(!button.disabled, "an open ballot stays votable");
        assert_eq!(
            button.custom_id.clone().unwrap_or_default(),
            format!("votekick:{vote_id}:{ballot}"),
            "button addresses this vote"
        );
    }
    data.content.as_deref().expect("ballot content")
}

/// A strict-decode refusal names the offending option, carries a copyable
/// corrected invocation, echoes nothing hostile, and stays bounded.
fn assert_safe_correction(text: &str, field: &str, command: &str) {
    assert!(text.contains(field), "correction names {field}: {text:?}");
    assert!(
        text.contains(&format!("/{command} ")) && text.contains(&format!("{field}:")),
        "copyable corrected invocation for {field}: {text:?}"
    );
    assert!(
        text.chars().count() <= 2000,
        "refusal stays bounded: {text:?}"
    );
    for leak in [
        POISON,
        "credential",
        "CredentialRefused",
        "Unavailable",
        "sqlx",
        "postgres://",
        "@everyone",
        "<@300>",
    ] {
        assert!(
            !text.contains(leak),
            "refusal echoes no input or store detail ({leak:?}): {text:?}"
        );
    }
    assert_ne!(text, READ_FAILURE_TEXT, "refused before any store read");
    assert_ne!(text, SAVE_FAILURE_TEXT, "refused before any store write");
    assert_ne!(text, GATE_FAILURE_TEXT, "refused before the gate read");
}

fn logging_summary_text(level: &str, channel: &str, mention: &str) -> String {
    format!("Log level: {level}\nLog channel: {channel}\nMentioned on errors: {mention}")
}

fn default_summary() -> String {
    logging_summary_text("brief", CHANNEL_FALLBACK, "none")
}

/// Dispatch malformed `/logging` subcommands and collect every contract failure:
/// any store/Discord call, any logging state change, or a reply that is not the
/// strict correction shape.
async fn refused_logging(
    subcommands: Vec<CommandDataOption>,
    field: &str,
    fail_reads: bool,
) -> Vec<String> {
    let fixture = Fixture::new().await;
    let before = fixture
        .state
        .lock()
        .expect("observation lock")
        .logging
        .clone();
    fixture.state.lock().expect("observation lock").fail_reads = fail_reads;
    let response = fixture.dispatch_logging(subcommands).await;
    let text = private_text(&response).to_owned();
    let observed = fixture.state.lock().expect("observation lock");
    let mut failures = vec![];
    if !observed.calls.is_empty() {
        failures.push(format!("read/effect before refusal: {:?}", observed.calls));
    }
    if observed.logging != before || !observed.saved_logging.is_empty() {
        failures.push("malformed input changed logging state".into());
    }
    if text.starts_with("Saved.") {
        failures.push(format!("duplicate saved instead of refusing: {text:?}"));
    }
    if !text.contains(field) || !text.contains("/logging ") || !text.contains(&format!("{field}:"))
    {
        failures.push(format!("no copyable correction for {field}: {text:?}"));
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
        failures.push(format!("unbounded or disclosing correction: {text:?}"));
    }
    for store_text in [READ_FAILURE_TEXT, SAVE_FAILURE_TEXT, GATE_FAILURE_TEXT] {
        if text == store_text {
            failures.push(format!("store touch disclosed as: {text:?}"));
        }
    }
    failures
}

/// Extra-subcommand variant: the correction must carry the `/logging` invocation
/// and stay bounded/safe, but it may name either action (or a generic action
/// word), so no single field name is required.
async fn refused_logging_extra(subcommands: Vec<CommandDataOption>) -> Vec<String> {
    let fixture = Fixture::new().await;
    let before = fixture
        .state
        .lock()
        .expect("observation lock")
        .logging
        .clone();
    let response = fixture.dispatch_logging(subcommands).await;
    let text = private_text(&response).to_owned();
    let observed = fixture.state.lock().expect("observation lock");
    let mut failures = vec![];
    if !observed.calls.is_empty() {
        failures.push(format!("read/effect before refusal: {:?}", observed.calls));
    }
    if observed.logging != before || !observed.saved_logging.is_empty() {
        failures.push("malformed input changed logging state".into());
    }
    if text.starts_with("Saved.") || text == default_summary() {
        failures.push(format!(
            "extra subcommand applied instead of refusing: {text:?}"
        ));
    }
    if !text.contains("/logging ") {
        failures.push(format!("no copyable /logging invocation: {text:?}"));
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
        failures.push(format!("unbounded or disclosing correction: {text:?}"));
    }
    for store_text in [READ_FAILURE_TEXT, SAVE_FAILURE_TEXT, GATE_FAILURE_TEXT] {
        if text == store_text {
            failures.push(format!("store touch disclosed as: {text:?}"));
        }
    }
    failures
}

fn wrong_values() -> Vec<CommandOptionValue> {
    vec![
        CommandOptionValue::String(POISON.into()),
        CommandOptionValue::Integer(7),
        CommandOptionValue::Boolean(true),
        CommandOptionValue::Channel(Id::new(LOG_TEXT_CHANNEL)),
        CommandOptionValue::Number(7.5),
        CommandOptionValue::User(Id::new(MEMBER)),
        CommandOptionValue::Role(Id::new(MENTION_ROLE)),
        CommandOptionValue::Attachment(Id::new(777)),
        CommandOptionValue::SubCommand(vec![opt("enabled", CommandOptionValue::Boolean(true))]),
        CommandOptionValue::SubCommandGroup(vec![]),
    ]
}

// ---------------------------------------------------------------------------
// `/logging`: duplicated options refuse before any effect (RED on main).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn logging_duplicate_level_refuses_before_effects() {
    let mut failures = vec![];
    for (label, first, second) in [
        ("different", "full", "off"),
        ("reversed", "off", "full"),
        ("identical", "full", "full"),
        ("valid/hostile", "full", POISON),
        ("hostile/valid", POISON, "full"),
    ] {
        let args = vec![str_opt("level", first), str_opt("level", second)];
        for failure in refused_logging(vec![sub("level", args)], "level", false).await {
            failures.push(format!("level {label}: {failure}"));
        }
    }
    // Name-count wins over type-check: a duplicated name refuses even when one
    // value is also mistyped.
    for (label, first, second) in [
        (
            "valid/wrong-type",
            CommandOptionValue::String("full".into()),
            CommandOptionValue::Integer(7),
        ),
        (
            "wrong-type/valid",
            CommandOptionValue::Integer(7),
            CommandOptionValue::String("full".into()),
        ),
    ] {
        let args = vec![opt("level", first), opt("level", second)];
        for failure in refused_logging(vec![sub("level", args)], "level", false).await {
            failures.push(format!("level {label}: {failure}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn logging_duplicate_channel_refuses_before_effects() {
    let mut failures = vec![];
    for (label, first, second) in [
        ("different", LOG_TEXT_CHANNEL, LOG_TEXT_CHANNEL_OTHER),
        ("reversed", LOG_TEXT_CHANNEL_OTHER, LOG_TEXT_CHANNEL),
        ("identical", LOG_TEXT_CHANNEL, LOG_TEXT_CHANNEL),
    ] {
        let args = vec![
            channel_opt("channel", first),
            channel_opt("channel", second),
        ];
        for failure in refused_logging(vec![sub("channel", args)], "channel", false).await {
            failures.push(format!("channel {label}: {failure}"));
        }
    }
    for (label, first, second) in [
        (
            "valid/hostile-type",
            CommandOptionValue::Channel(Id::new(LOG_TEXT_CHANNEL)),
            CommandOptionValue::String(POISON.into()),
        ),
        (
            "hostile-type/valid",
            CommandOptionValue::String(POISON.into()),
            CommandOptionValue::Channel(Id::new(LOG_TEXT_CHANNEL)),
        ),
    ] {
        let args = vec![opt("channel", first), opt("channel", second)];
        for failure in refused_logging(vec![sub("channel", args)], "channel", false).await {
            failures.push(format!("channel {label}: {failure}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn logging_duplicate_mention_refuses_before_effects() {
    let mut failures = vec![];
    for (label, first, second) in [
        ("different", MENTION_ROLE, MENTION_ROLE_OTHER),
        ("reversed", MENTION_ROLE_OTHER, MENTION_ROLE),
        ("identical", MENTION_ROLE, MENTION_ROLE),
    ] {
        let args = vec![role_opt("role", first), role_opt("role", second)];
        for failure in refused_logging(vec![sub("mention", args)], "role", false).await {
            failures.push(format!("mention {label}: {failure}"));
        }
    }
    for (label, first, second) in [
        (
            "valid/hostile-type",
            CommandOptionValue::Role(Id::new(MENTION_ROLE)),
            CommandOptionValue::String(POISON.into()),
        ),
        (
            "hostile-type/valid",
            CommandOptionValue::String(POISON.into()),
            CommandOptionValue::Role(Id::new(MENTION_ROLE)),
        ),
    ] {
        let args = vec![opt("role", first), opt("role", second)];
        for failure in refused_logging(vec![sub("mention", args)], "role", false).await {
            failures.push(format!("mention {label}: {failure}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn logging_extra_subcommands_refuse_before_effects() {
    let level_full = || sub("level", vec![str_opt("level", "full")]);
    let channel = || sub("channel", vec![channel_opt("channel", LOG_TEXT_CHANNEL)]);
    let mention = || sub("mention", vec![role_opt("role", MENTION_ROLE)]);
    let show = || sub("show", vec![]);
    let bogus = || sub("bogus", vec![]);
    let mut failures = vec![];
    // Each pair is dispatched in both orders: only the first is read today, so
    // the second is silently dropped and the first saves or shows.
    for (label, first, second) in [
        ("level/channel", level_full(), channel()),
        ("channel/level", channel(), level_full()),
        ("level/show", level_full(), show()),
        ("show/level", show(), level_full()),
        ("mention/channel", mention(), channel()),
        ("channel/mention", channel(), mention()),
        ("level/level", level_full(), level_full()),
        ("show/show", show(), show()),
        ("level/bogus", level_full(), bogus()),
        ("bogus/level", bogus(), level_full()),
    ] {
        for failure in refused_logging_extra(vec![first, second]).await {
            failures.push(format!("extra {label}: {failure}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ---------------------------------------------------------------------------
// `/logging`: valid, unknown-ignored, omitted and preserved controls (GREEN).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn logging_valid_shapes_save_and_show_byte_identical() {
    // Show renders the default summary without saving.
    let fixture = Fixture::new().await;
    let response = fixture.dispatch_logging(vec![sub("show", vec![])]).await;
    assert_eq!(private_text(&response), default_summary());
    {
        let observed = fixture.state.lock().expect("observation lock");
        assert!(
            observed.saved_logging.is_empty(),
            "show saves nothing: {:?}",
            observed.saved_logging
        );
        assert!(
            observed
                .calls
                .iter()
                .any(|call| call == "logging_settings:100"),
            "show reads once: {:?}",
            observed.calls
        );
        assert!(
            !observed.calls.iter().any(|call| call.contains("discord:")),
            "show touches no Discord: {:?}",
            observed.calls
        );
    }

    // Level, channel and mention each save exactly their field.
    for (subs, level, channel, mention) in [
        (
            vec![sub("level", vec![str_opt("level", " FULL ")])],
            DetailLevel::Full,
            None,
            None,
        ),
        (
            vec![sub(
                "channel",
                vec![channel_opt("channel", LOG_TEXT_CHANNEL)],
            )],
            DetailLevel::Brief,
            Some(LOG_TEXT_CHANNEL),
            None,
        ),
        (
            vec![sub("mention", vec![role_opt("role", MENTION_ROLE)])],
            DetailLevel::Brief,
            None,
            Some(MENTION_ROLE),
        ),
    ] {
        let fixture = Fixture::new().await;
        let response = fixture.dispatch_logging(subs).await;
        let text = private_text(&response).to_owned();
        assert!(text.starts_with("Saved.\n"), "valid saves: {text:?}");
        let observed = fixture.state.lock().expect("observation lock");
        assert_eq!(observed.saved_logging.len(), 1);
        assert_eq!(
            observed.saved_logging[0].1,
            LoggingSettings {
                level,
                channel_id: channel,
                mention_role_id: mention,
            }
        );
        assert_eq!(
            observed.logging.get(&FOREIGN_GUILD),
            Some(&LoggingSettings::default()),
            "foreign guild unchanged"
        );
        assert!(
            !observed.calls.iter().any(|call| call.contains("discord:")),
            "valid save touches no Discord: {:?}",
            observed.calls
        );
        assert!(
            !observed
                .calls
                .iter()
                .any(|call| call == "record_kick_audit"),
            "logging never audits kicks: {:?}",
            observed.calls
        );
    }

    // Clearing the channel and mention keeps the level.
    let fixture = Fixture::new().await;
    fixture
        .dispatch_logging(vec![sub("level", vec![str_opt("level", "full")])])
        .await;
    fixture
        .dispatch_logging(vec![sub(
            "channel",
            vec![channel_opt("channel", LOG_TEXT_CHANNEL)],
        )])
        .await;
    fixture
        .dispatch_logging(vec![sub("mention", vec![role_opt("role", MENTION_ROLE)])])
        .await;
    let response = fixture.dispatch_logging(vec![sub("channel", vec![])]).await;
    assert!(private_text(&response).starts_with("Saved.\n"));
    let response = fixture.dispatch_logging(vec![sub("mention", vec![])]).await;
    assert!(private_text(&response).starts_with("Saved.\n"));
    let observed = fixture.state.lock().expect("observation lock");
    assert_eq!(
        observed.logging.get(&GUILD),
        Some(&LoggingSettings {
            level: DetailLevel::Full,
            channel_id: None,
            mention_role_id: None,
        })
    );
}

#[tokio::test]
async fn logging_unknown_options_are_ignored() {
    let hostile_unknown = || {
        vec![
            opt("future-option", CommandOptionValue::String(POISON.into())),
            opt(
                "future-option",
                CommandOptionValue::SubCommand(vec![
                    channel_opt("channel", LOG_TEXT_CHANNEL_OTHER),
                    str_opt("level", POISON),
                ]),
            ),
        ]
    };
    // Each valid shape keeps its byte-identical receipt with unknown args
    // present (including duplicated unknowns and nested known names beneath
    // an unknown option, which must not be promoted into fields).
    for (label, mut args, shape) in [
        ("show", hostile_unknown(), "show"),
        (
            "level",
            {
                let mut args = vec![str_opt("level", "full")];
                args.extend(hostile_unknown());
                args
            },
            "level",
        ),
        (
            "channel",
            {
                let mut args = vec![channel_opt("channel", LOG_TEXT_CHANNEL)];
                args.extend(hostile_unknown());
                args
            },
            "channel",
        ),
        (
            "mention",
            {
                let mut args = vec![role_opt("role", MENTION_ROLE)];
                args.extend(hostile_unknown());
                args
            },
            "mention",
        ),
    ] {
        let fixture = Fixture::new().await;
        // Unknowns bookend the known args so position cannot matter.
        args.insert(0, opt("future-option", CommandOptionValue::Integer(7)));
        let response = fixture.dispatch_logging(vec![sub(shape, args)]).await;
        let text = private_text(&response).to_owned();
        if shape == "show" {
            assert_eq!(text, default_summary(), "{label} with unknowns");
        } else {
            assert!(
                text.starts_with("Saved.\n"),
                "{label} with unknowns: {text:?}"
            );
        }
        assert!(!text.contains(POISON), "{label} echoes no unknown value");
        let observed = fixture.state.lock().expect("observation lock");
        if shape == "show" {
            assert!(
                observed.saved_logging.is_empty(),
                "{label} show saves nothing"
            );
        } else {
            assert_eq!(observed.saved_logging.len(), 1, "{label} saves once");
        }
    }
}

#[tokio::test]
async fn logging_existing_wrong_types_still_refuse() {
    // Shapes that already refuse today keep refusing: either today's Invalid
    // text byte-identical or the new strict correction. `Saved.` is never
    // accepted, and nothing is persisted.
    let cases: Vec<(Vec<CommandDataOption>, &str)> = vec![
        (vec![sub("level", vec![])], "level"),
        (
            vec![sub(
                "level",
                vec![opt("level", CommandOptionValue::Integer(7))],
            )],
            "level",
        ),
        (
            vec![sub("channel", vec![role_opt("channel", MENTION_ROLE)])],
            "channel",
        ),
        (
            vec![sub(
                "channel",
                vec![opt("channel", CommandOptionValue::Integer(7))],
            )],
            "channel",
        ),
        (
            vec![sub("mention", vec![channel_opt("role", LOG_TEXT_CHANNEL)])],
            "role",
        ),
        (
            vec![sub(
                "mention",
                vec![opt("role", CommandOptionValue::Integer(7))],
            )],
            "role",
        ),
        (vec![sub("bogus", vec![])], "bogus"),
        (vec![], "logging"),
    ];
    for (subs, field) in cases {
        let fixture = Fixture::new().await;
        let before = fixture
            .state
            .lock()
            .expect("observation lock")
            .logging
            .clone();
        let response = fixture.dispatch_logging(subs).await;
        let text = private_text(&response).to_owned();
        assert!(
            !text.starts_with("Saved."),
            "{field}: wrong type must not save: {text:?}"
        );
        if text != OLD_INVALID_TEXT {
            // An upgraded strict correction is accepted as long as it meets
            // the shape contract (names the field, or the command for a bare
            // invocation).
            if field == "logging" || field == "bogus" {
                assert!(
                    text.contains("/logging "),
                    "{field}: upgraded correction keeps the invocation: {text:?}"
                );
            } else {
                assert_safe_correction(&text, field, "logging");
            }
        }
        let observed = fixture.state.lock().expect("observation lock");
        assert_eq!(observed.logging, before, "{field}: nothing persisted");
        assert!(observed.saved_logging.is_empty(), "{field}: nothing saved");
    }

    // Unknown level text keeps its validation refusal byte-identical.
    let fixture = Fixture::new().await;
    let response = fixture
        .dispatch_logging(vec![sub("level", vec![str_opt("level", "loud")])])
        .await;
    assert_eq!(
        private_text(&response),
        "Unknown detail level \"loud\"; expected off, brief or full. Nothing was changed."
    );
}

#[tokio::test]
async fn logging_malformed_neither_discloses_nor_consumes_a_corrected_attempt() {
    // A poisoned store still yields the correction, never a store failure.
    let malformed = vec![sub(
        "level",
        vec![str_opt("level", "full"), str_opt("level", "off")],
    )];
    for failure in refused_logging(malformed.clone(), "level", true).await {
        panic!("poisoned read: {failure}");
    }

    // Malformed, corrected, malformed on one runtime: only the corrected write
    // lands, immediately, and the refusal is stable.
    let fixture = Fixture::new().await;
    let rejected = fixture.dispatch_logging(malformed.clone()).await;
    let corrected = fixture
        .dispatch_logging(vec![sub("level", vec![str_opt("level", "full")])])
        .await;
    assert!(
        private_text(&corrected).starts_with("Saved.\n"),
        "same-member immediate correction must be admitted"
    );
    let rejected_again = fixture.dispatch_logging(malformed).await;
    assert_eq!(
        serde_json::to_value(&rejected).expect("reply JSON"),
        serde_json::to_value(&rejected_again).expect("reply JSON"),
        "refusal is stable across a corrected attempt"
    );
    let observed = fixture.state.lock().expect("observation lock");
    assert_eq!(
        observed.saved_logging,
        vec![(
            GUILD,
            LoggingSettings {
                level: DetailLevel::Full,
                channel_id: None,
                mention_role_id: None,
            }
        )],
        "malformed attempts were never admitted"
    );
}

#[tokio::test]
async fn logging_foreign_channel_still_refuses_without_saving() {
    // Existing target validation is preserved: a channel outside this server
    // refuses with the validation text, not a save.
    let fixture = Fixture::new().await;
    let before = fixture
        .state
        .lock()
        .expect("observation lock")
        .logging
        .clone();
    let response = fixture
        .dispatch_logging(vec![sub("channel", vec![channel_opt("channel", 99)])])
        .await;
    let text = private_text(&response).to_owned();
    assert!(
        text.contains("Nothing was changed") && !text.starts_with("Saved."),
        "foreign channel refuses: {text:?}"
    );
    let observed = fixture.state.lock().expect("observation lock");
    assert_eq!(observed.logging, before);
    assert!(observed.saved_logging.is_empty());
}

// ---------------------------------------------------------------------------
// `/kick reason`: wrong-type and duplicate reasons refuse (RED on main).
// ---------------------------------------------------------------------------

/// Dispatch a malformed `/kick` and collect every contract failure. The refusal
/// must be an ephemeral correction before any store/Discord call, and a fresh
/// valid retry must immediately start the vote (proving no ledger entry was
/// claimed and no ballot was posted for the malformed attempt).
async fn refused_kick_reason(options: Vec<CommandDataOption>) -> Vec<String> {
    let fixture = Fixture::new().await;
    let mut failures = vec![];
    let response = fixture.dispatch_kick(options, VOTER_A, 2).await;
    let text = private_text(&response).to_owned();
    {
        let observed = fixture.state.lock().expect("observation lock");
        if !observed.calls.is_empty() {
            failures.push(format!("read/effect before refusal: {:?}", observed.calls));
        }
        if !observed.saved_logging.is_empty() {
            failures.push("kick refusal touched logging state".into());
        }
    }
    if !text.contains("reason") || !text.contains("/kick ") || !text.contains("reason:") {
        failures.push(format!("no copyable correction for reason: {text:?}"));
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
        failures.push(format!("unbounded or disclosing correction: {text:?}"));
    }
    if text.contains("started a vote") || text.contains("Reason:") || text.contains("<@303>") {
        failures.push(format!("refusal looks like a ballot: {text:?}"));
    }
    // The corrected retry starts the vote: no ledger entry was claimed.
    let retry = fixture
        .dispatch_kick(
            vec![user_opt("member", TARGET), str_opt("reason", "too loud")],
            VOTER_A,
            3,
        )
        .await;
    let ballot = public_ballot_text(&retry, 3).to_owned();
    if !ballot.contains("Reason: too loud") {
        failures.push(format!("retry did not start a reasoned vote: {ballot:?}"));
    }
    failures
}

#[tokio::test]
async fn kick_wrong_type_reason_refuses_before_ballot_or_ledger() {
    let mut failures = vec![];
    for wrong in wrong_values() {
        if matches!(wrong, CommandOptionValue::String(_)) {
            continue;
        }
        // A hostile string is covered by the duplicate tests; here the wire
        // type itself is wrong, so the string variant is the valid control.
        let options = vec![user_opt("member", TARGET), opt("reason", wrong.clone())];
        for failure in refused_kick_reason(options).await {
            failures.push(format!("reason {wrong:?}: {failure}"));
        }
    }
    // A hostile string in the wrong-type position is still a string (valid
    // type); it renders sanitized on the ballot and is pinned by the VK-04
    // suite, not refused here.
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn kick_duplicate_reason_refuses_before_ballot_or_ledger() {
    let mut failures = vec![];
    for (label, first, second) in [
        ("different", "too loud", "after hours"),
        ("reversed", "after hours", "too loud"),
        ("identical", "too loud", "too loud"),
        ("valid/hostile", "too loud", POISON),
        ("hostile/valid", POISON, "too loud"),
    ] {
        let options = vec![
            user_opt("member", TARGET),
            str_opt("reason", first),
            str_opt("reason", second),
        ];
        for failure in refused_kick_reason(options).await {
            failures.push(format!("reason {label}: {failure}"));
        }
    }
    // Name-count wins over type-check.
    for (label, first, second) in [
        (
            "valid/wrong-type",
            CommandOptionValue::String("too loud".into()),
            CommandOptionValue::Integer(7),
        ),
        (
            "wrong-type/valid",
            CommandOptionValue::Integer(7),
            CommandOptionValue::String("too loud".into()),
        ),
    ] {
        let options = vec![
            user_opt("member", TARGET),
            opt("reason", first),
            opt("reason", second),
        ];
        for failure in refused_kick_reason(options).await {
            failures.push(format!("reason {label}: {failure}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ---------------------------------------------------------------------------
// `/kick`: omitted, valid and unknown-ignored controls (GREEN).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn kick_omitted_reason_starts_the_vote_without_a_reason_line() {
    for options in [
        vec![user_opt("member", TARGET)],
        vec![user_opt("target", TARGET)],
    ] {
        let fixture = Fixture::new().await;
        let response = fixture.dispatch_kick(options, VOTER_A, 2).await;
        let content = public_ballot_text(&response, 2).to_owned();
        assert_eq!(
            content,
            "<@303> — <@301> started a vote to disconnect them from this voice room.\nVote with the buttons: 2/3 needed. Not voting counts as No. The vote ends in 2 minutes.",
            "omitted reason posts a reasonless ballot by choice, not by drop"
        );
        assert!(
            !content.lines().any(|line| line.starts_with("Reason:")),
            "no reason means no Reason line: {content:?}"
        );
    }
}

#[tokio::test]
async fn kick_valid_reason_posts_the_byte_identical_ballot() {
    let fixture = Fixture::new().await;
    let response = fixture
        .dispatch_kick(
            vec![user_opt("member", TARGET), str_opt("reason", "too loud")],
            VOTER_A,
            2,
        )
        .await;
    assert_eq!(
        public_ballot_text(&response, 2),
        "<@303> — <@301> started a vote to disconnect them from this voice room.\nReason: too loud\nVote with the buttons: 2/3 needed. Not voting counts as No. The vote ends in 2 minutes."
    );
}

#[tokio::test]
async fn kick_unknown_options_are_ignored() {
    let fixture = Fixture::new().await;
    let response = fixture
        .dispatch_kick(
            vec![
                opt("future-option", CommandOptionValue::Integer(7)),
                user_opt("member", TARGET),
                str_opt("reason", "too loud"),
                opt("future-option", CommandOptionValue::String(POISON.into())),
                opt(
                    "future-option",
                    CommandOptionValue::SubCommand(vec![str_opt("reason", POISON)]),
                ),
            ],
            VOTER_A,
            2,
        )
        .await;
    assert_eq!(
        public_ballot_text(&response, 2),
        "<@303> — <@301> started a vote to disconnect them from this voice room.\nReason: too loud\nVote with the buttons: 2/3 needed. Not voting counts as No. The vote ends in 2 minutes.",
        "unknown options (including nested reason) stay ignored"
    );
}
