//! V3 `/name` runtime: the panel, the custom-name modal and the restore
//! button, written from `docs/voice-rooms.md` §V3.
//!
//! `/name` replies (ephemeral) with a panel carrying two buttons. **Custom
//! name** opens a modal; submitting it sets a per-room override, with template
//! tokens allowed. **Restore template** clears the override and returns the
//! room to its template name. Every id is the request-bound `two:voice:`
//! component id from [`two_bot_core::voice_custom_id`], parsed back as
//! untrusted input.
//!
//! The decisions are pure and live in [`two_bot_core::voice_room_name`]; this
//! module gathers the facts (the room's naming context from the live guild
//! snapshot, the guild's "unique names" setting and named lists from the saved
//! configuration), asks the guild worker to apply the decision, and builds the
//! Discord responses. The worker owns every state change, so authorization is
//! checked against the room's current owner on every click, and a stale
//! button from a previous owner or for a deleted room is answered, never
//! applied.
//!
//! Presence data (games, streams) and privacy are not tracked by this runtime
//! yet, so tokens built from them render in their "nothing to show" state.
//! `##` numbers a room by its position among its creator's tracked rooms until
//! room numbers are persisted.

use super::*;
use twilight_model::{
    application::interaction::modal::ModalInteractionComponent,
    channel::message::component::{Label, TextInput, TextInputStyle},
};
use two_bot_core::{
    voice_conditions::ConditionFacts,
    voice_custom_id::{name_custom_custom_id, name_modal_custom_id, name_restore_custom_id},
    voice_name_filter::{filter_channel_name, NameFilterContext},
    voice_naming::RoomContext,
    voice_room_name::{
        decide_custom_name, decide_template_name, NameChecks, RenderFacts, MAX_CUSTOM_NAME_CHARS,
        NAME_INPUT_ID,
    },
};

/// One `/name` step. The same value is the parsed interaction and the
/// worker request, so a click is never reinterpreted between the two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum NameInteraction {
    /// `/name`: show the panel for the room the invoker is in.
    Panel,
    /// The panel's Custom name button: open the modal for this room.
    OpenModal { room_id: Snowflake },
    /// The panel's Restore template button.
    Restore { room_id: Snowflake },
    /// The modal submit. `text` is empty when the input is missing.
    Submit { room_id: Snowflake, text: String },
}

/// Parse a `/name` panel click or modal submit. `None` for anything else, so
/// other components stay with their owners.
pub(super) fn name_component_action(interaction: &Interaction) -> Option<NameInteraction> {
    interaction_guild(interaction)?;
    match interaction.data.as_ref()? {
        InteractionData::MessageComponent(data)
            if interaction.kind == InteractionType::MessageComponent =>
        {
            match parse_voice_custom_id(&data.custom_id)? {
                VoiceAction::NameCustom { room_id } => Some(NameInteraction::OpenModal { room_id }),
                VoiceAction::NameRestore { room_id } => Some(NameInteraction::Restore { room_id }),
                _ => None,
            }
        }
        InteractionData::ModalSubmit(data) if interaction.kind == InteractionType::ModalSubmit => {
            match parse_voice_custom_id(&data.custom_id)? {
                VoiceAction::NameModal { room_id } => Some(NameInteraction::Submit {
                    room_id,
                    text: find_text_input(&data.components).unwrap_or_default(),
                }),
                _ => None,
            }
        }
        _ => None,
    }
}

/// The submitted value of the name input, wherever the modal nests it.
fn find_text_input(components: &[ModalInteractionComponent]) -> Option<String> {
    components.iter().find_map(|component| match component {
        ModalInteractionComponent::TextInput(input) if input.custom_id == NAME_INPUT_ID => {
            Some(input.value.clone())
        }
        ModalInteractionComponent::Label(label) => {
            find_text_input(std::slice::from_ref(&*label.component))
        }
        ModalInteractionComponent::ActionRow(row) => find_text_input(&row.components),
        _ => None,
    })
}

/// Display names the worker renders `@@owner@@` from. The gateway sink fills
/// it from the cache (the invoker plus everyone in voice); a member missing
/// here renders as "member", never as an ID.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NameDirectory {
    names: HashMap<Snowflake, String>,
}

impl NameDirectory {
    pub fn insert(&mut self, member: Snowflake, display: String) {
        self.names.insert(member, display);
    }

    fn get(&self, member: Snowflake) -> &str {
        self.names.get(&member).map_or("member", String::as_str)
    }

    pub(super) fn knows(&self, member: Snowflake) -> bool {
        self.names.contains_key(&member)
    }

    fn retain(&mut self, mut keep: impl FnMut(Snowflake) -> bool) {
        self.names.retain(|member, _| keep(*member));
    }

    fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut entries: Vec<_> = self.names.iter().collect();
        entries.sort_unstable();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        entries.hash(&mut hasher);
        hasher.finish()
    }
}

/// Display names for one `/name` interaction, from the gateway cache. Any
/// other interaction gets an empty directory, so the walk over everyone in
/// voice runs only for `/name`.
pub(super) fn name_directory_from_cache(
    cache: &DefaultInMemoryCache,
    interaction: &Interaction,
) -> NameDirectory {
    let mut directory = NameDirectory::default();
    let is_name = name_component_action(interaction).is_some()
        || matches!(parse_voice_command(interaction), Some(VoiceCommand::Name));
    let Some(guild_id) = interaction_guild(interaction).filter(|_| is_name) else {
        return directory;
    };
    if let Some(users) = cache.guild_voice_states(Id::new(guild_id)) {
        for user in users.iter() {
            directory.insert(user.get(), display_name(cache, guild_id, user.get()));
        }
    }
    if let Some(member) = invoker_member_id(interaction) {
        directory.insert(member, display_name(cache, guild_id, member));
    }
    directory
}

/// The invoker's display name from the interaction itself: nickname, then
/// global name, then username. Covers a runtime without a cache.
fn invoker_display(interaction: &Interaction) -> Option<(Snowflake, String)> {
    let member = interaction.member.as_ref();
    let user = member
        .and_then(|member| member.user.as_ref())
        .or(interaction.user.as_ref())?;
    let display = member
        .and_then(|member| member.nick.clone())
        .filter(|nick| !nick.is_empty())
        .or_else(|| user.global_name.clone().filter(|name| !name.is_empty()))
        .unwrap_or_else(|| user.name.clone());
    Some((user.id.get(), display))
}

/// The guild settings `/name` reads: the "unique names" toggle, the named
/// random lists and the "no game" label.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct NameSettings {
    pub unique_names: bool,
    pub no_game_label: String,
    pub lists: HashMap<String, Vec<String>>,
}

impl NameSettings {
    pub(super) fn from_config(config: &VoiceConfiguration) -> Self {
        Self {
            unique_names: config.settings.unique_names,
            no_game_label: config.settings.no_game_label.clone(),
            lists: config
                .lists
                .iter()
                .map(|list| (list.name.clone(), list.choices.clone()))
                .collect(),
        }
    }
}

/// One `/name` step handed to the guild worker.
pub(super) struct NameCommand {
    pub actor_id: Snowflake,
    pub is_admin: bool,
    pub request: NameInteraction,
    pub settings: NameSettings,
    pub directory: NameDirectory,
    pub policy: Arc<AutomodPolicy>,
}

/// What the worker decided; the handler turns it into the Discord response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum NameReply {
    /// An ephemeral refusal. Nothing changed.
    Refused(String),
    /// Show the panel for this room.
    Panel { room_id: Snowflake, text: String },
    /// Open the modal for this room, pre-filled with the current override.
    Modal {
        room_id: Snowflake,
        prefill: Option<String>,
    },
    /// The change is applied; the text says what happens next.
    Applied(String),
}

const PAUSED: &str = "Voice rooms are paused: Discord refused the bot credential. \
                      Fix the token, then restart the bot.";
const NOT_WARM: &str = "The voice worker isn't warmed up yet — try again in a moment.";
const NOT_IN_ROOM: &str = "You need to be in a temporary voice room to use /name.";
const NOT_A_ROOM: &str = "That voice channel isn't a temporary room I manage.";
const ROOM_GONE: &str = "That room no longer exists. Run /name again in a current room.";
const NOT_OWNER: &str = "Only the room's owner (or a server admin) can rename it.";
const CHANNEL_UNSEEN: &str = "I can't see that channel right now. Try again in a moment.";
const RENAME_NOTE: &str =
    "Discord limits renames to about two every ten minutes, so it may take a moment to show.";

const NAME_SETTINGS_RELOAD_MS: u64 = 300_000;
const NAME_SETTINGS_RETRY_MS: u64 = 60_000;

/// The facts an automatic template name was rendered from: the room is
/// re-rendered only when one of them changes, so an idle room costs nothing
/// and Discord's rename budget is spent on real changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NameSignature {
    owner_name: String,
    original_creator_name: String,
    member_count: u32,
    owner_present: bool,
    user_limit: u32,
    room_number: u32,
    members: Vec<String>,
}

/// Everything one name decision needs from the live guild.
struct NameFacts {
    context: RoomContext,
    conditions: ConditionFacts,
    fallback: String,
    filter: NameFilterContext,
    other_names: Vec<String>,
}

impl<S: RoomPersistence, H: RoomWrites> GuildRoomWorker<S, H> {
    /// Apply one `/name` step. Authorization is the room's current owner or
    /// an admin, checked here on every step: the panel and its buttons carry
    /// only the room id, so a stale click from a previous owner is refused.
    pub(super) fn apply_name(&mut self, command: NameCommand, now_ms: u64) -> NameReply {
        if self.halted {
            return NameReply::Refused(PAUSED.to_owned());
        }
        let room_id = {
            let live = self.live.read_state();
            if !live.ready {
                return NameReply::Refused(NOT_WARM.to_owned());
            }
            match &command.request {
                NameInteraction::Panel => {
                    match live
                        .members
                        .get(&command.actor_id)
                        .and_then(|member| member.channel_id)
                    {
                        Some(channel) => channel,
                        None => return NameReply::Refused(NOT_IN_ROOM.to_owned()),
                    }
                }
                NameInteraction::OpenModal { room_id }
                | NameInteraction::Restore { room_id }
                | NameInteraction::Submit { room_id, .. } => *room_id,
            }
        };
        let Some(room) = self.rooms.get(&room_id).cloned() else {
            let text = match command.request {
                NameInteraction::Panel => NOT_A_ROOM,
                _ => ROOM_GONE,
            };
            return NameReply::Refused(text.to_owned());
        };
        if room.owner_id != command.actor_id && !command.is_admin {
            return NameReply::Refused(NOT_OWNER.to_owned());
        }
        match &command.request {
            NameInteraction::Panel => NameReply::Panel {
                room_id,
                text: self.panel_text(room_id),
            },
            NameInteraction::OpenModal { .. } => NameReply::Modal {
                room_id,
                prefill: self.custom_names.get(&room_id).cloned(),
            },
            NameInteraction::Submit { text, .. } => {
                let facts = self.name_facts(&room, &command);
                let checks = NameChecks {
                    policy: &command.policy,
                    filter: &facts.filter,
                    unique_names: command.settings.unique_names,
                    other_voice_names: &facts.other_names,
                };
                let render = RenderFacts {
                    context: &facts.context,
                    conditions: &facts.conditions,
                    fallback_name: &facts.fallback,
                };
                match decide_custom_name(text, &render, &checks) {
                    Ok(custom) => {
                        self.commit_name(room_id, Some(custom.stored), &custom.channel_name, now_ms)
                    }
                    Err(refusal) => NameReply::Refused(refusal.to_string()),
                }
            }
            NameInteraction::Restore { .. } => {
                let facts = self.name_facts(&room, &command);
                let checks = NameChecks {
                    policy: &command.policy,
                    filter: &facts.filter,
                    unique_names: false,
                    other_voice_names: &facts.other_names,
                };
                let render = RenderFacts {
                    context: &facts.context,
                    conditions: &facts.conditions,
                    fallback_name: &facts.fallback,
                };
                let template = self
                    .creators
                    .get(&room.creator_channel_id)
                    .map_or("", |creator| creator.name_template.as_str());
                match decide_template_name(template, &render, &checks) {
                    Ok(name) => self.commit_name(room_id, None, &name, now_ms),
                    Err(refusal) => NameReply::Refused(refusal.to_string()),
                }
            }
        }
    }

    /// V5 automatic naming: render each tracked room's creator template
    /// whenever the facts it depends on change (creation, joins and leaves,
    /// owner handoff, limit) and propose the result on the rename lane, which
    /// coalesces to one pending name per channel within Discord's rename
    /// budget. Rooms with a `/name` override keep it; a blank template keeps
    /// the room's current name; a render the name filter blocks is skipped.
    pub(super) fn refresh_template_names(&mut self, now_ms: u64) {
        if self.halted || !self.live.read_state().ready {
            return;
        }
        // Skip the whole pass while nothing automatic names read has moved:
        // voice transitions, room owners and limits, display names, settings,
        // templates, and forgotten signatures (a forced re-render).
        let inputs = self.name_inputs_fingerprint();
        if self.name_inputs == Some(inputs) {
            return;
        }
        self.name_inputs = Some(inputs);
        // Keep display names only for members who can still appear in a
        // name: everyone in voice plus every room's owner and original
        // creator.
        {
            let live = self.live.read_state();
            let rooms = &self.rooms;
            self.name_directory.retain(|member| {
                live.members
                    .get(&member)
                    .is_some_and(|state| state.channel_id.is_some())
                    || rooms
                        .values()
                        .any(|room| room.owner_id == member || room.original_creator_id == member)
            });
        }
        let mut command = NameCommand {
            actor_id: 0,
            is_admin: true,
            request: NameInteraction::Panel,
            settings: self.name_settings.clone(),
            directory: self.name_directory.clone(),
            policy: Arc::clone(&self.name_policy),
        };
        let rooms: Vec<VoiceRoom> = self.rooms.values().cloned().collect();
        for room in rooms {
            let room_id = room.channel_id;
            if self.custom_names.contains_key(&room_id) {
                continue;
            }
            let Some(template) = self
                .creators
                .get(&room.creator_channel_id)
                .map(|creator| creator.name_template.clone())
                .filter(|template| !template.trim().is_empty())
            else {
                continue;
            };
            // A person whose display name is unknown would render as
            // "member": wait for the name instead of spending a rename.
            if !self.name_directory.knows(room.owner_id)
                || (template.contains("@@original_creator@@")
                    && !self.name_directory.knows(room.original_creator_id))
            {
                continue;
            }
            command.actor_id = room.owner_id;
            command.request = NameInteraction::Restore { room_id };
            let facts = self.name_facts(&room, &command);
            let signature = NameSignature {
                owner_name: facts.context.owner_name.clone(),
                original_creator_name: facts.context.original_creator_name.clone(),
                member_count: facts.context.member_count,
                owner_present: facts.context.owner_present,
                user_limit: facts.context.user_limit,
                room_number: facts.context.room_number,
                members: facts.conditions.member_ids.clone(),
            };
            if self.name_signatures.get(&room_id) == Some(&signature) {
                continue;
            }
            let checks = NameChecks {
                policy: &command.policy,
                filter: &facts.filter,
                unique_names: false,
                other_voice_names: &facts.other_names,
            };
            let render = RenderFacts {
                context: &facts.context,
                conditions: &facts.conditions,
                fallback_name: &facts.fallback,
            };
            match decide_template_name(&template, &render, &checks) {
                Ok(name) => {
                    // A channel the live snapshot cannot see yet is retried
                    // on a later pass.
                    if self.propose_name(room_id, &name, now_ms).is_some() {
                        self.name_signatures.insert(room_id, signature);
                    } else {
                        self.name_inputs = None;
                    }
                }
                Err(_) => {
                    self.name_signatures.insert(room_id, signature);
                }
            }
        }
        let rooms = &self.rooms;
        self.name_signatures
            .retain(|room_id, _| rooms.contains_key(room_id));
    }

    /// Cheap hash over every input of [`Self::refresh_template_names`]; no
    /// rendering, filtering or channel walk.
    fn name_inputs_fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let live = self.live.read_state();
        live.next_transition.hash(&mut hasher);
        live.generation.hash(&mut hasher);
        let mut rooms: Vec<_> = self
            .rooms
            .values()
            .map(|room| {
                (
                    room.channel_id,
                    room.owner_id,
                    room.creator_channel_id,
                    live.channels
                        .get(&room.channel_id)
                        .and_then(|channel| channel.user_limit),
                    self.custom_names.contains_key(&room.channel_id),
                )
            })
            .collect();
        rooms.sort_unstable();
        rooms.hash(&mut hasher);
        let mut creators: Vec<_> = self
            .creators
            .values()
            .map(|creator| (creator.channel_id, creator.name_template.as_str()))
            .collect();
        creators.sort_unstable();
        creators.hash(&mut hasher);
        self.name_directory.fingerprint().hash(&mut hasher);
        format!("{:?}", self.name_settings).hash(&mut hasher);
        self.name_signatures.len().hash(&mut hasher);
        hasher.finish()
    }

    /// Re-read the guild's naming settings every five minutes, or a minute
    /// after a failed read, so writers other than `/import` (the operator
    /// CLI) reach automatic names and a transient error does not stick.
    pub(super) async fn reload_name_settings(&mut self, now_ms: u64) {
        let interval = if self.name_settings_loaded {
            NAME_SETTINGS_RELOAD_MS
        } else {
            NAME_SETTINGS_RETRY_MS
        };
        if self
            .name_settings_read_ms
            .is_some_and(|last| now_ms.saturating_sub(last) < interval)
        {
            return;
        }
        self.name_settings_read_ms = Some(now_ms);
        if let Ok(config) = self.store.config_snapshot(self.live.guild_id).await {
            self.name_settings = NameSettings::from_config(&config);
            self.name_settings_loaded = true;
        }
    }

    /// Propose the new channel name first: a channel the live snapshot cannot
    /// see changes nothing. Then record the override and queue its
    /// persistence; the rename itself rides the deferred rename lane.
    fn commit_name(
        &mut self,
        room_id: Snowflake,
        custom_name: Option<String>,
        channel_name: &str,
        now_ms: u64,
    ) -> NameReply {
        let Some(outcome) = self.propose_name(room_id, channel_name, now_ms) else {
            return NameReply::Refused(CHANNEL_UNSEEN.to_owned());
        };
        let restored = custom_name.is_none();
        match &custom_name {
            Some(text) => {
                self.custom_names.insert(room_id, text.clone());
            }
            None => {
                self.custom_names.remove(&room_id);
            }
        }
        self.queue.enqueue(
            self.live.guild_id,
            RoomAction::SetCustomName {
                channel_id: room_id,
                custom_name,
            },
        );
        NameReply::Applied(match (restored, outcome) {
            (false, ProposeOutcome::Queued { .. }) => {
                format!("Renaming this room to **{channel_name}**. {RENAME_NOTE}")
            }
            (false, ProposeOutcome::Unchanged) => {
                format!(
                    "This room is already named **{channel_name}**. Saved it as the custom name."
                )
            }
            (true, ProposeOutcome::Queued { .. }) => {
                format!("Restored the template name: **{channel_name}**. {RENAME_NOTE}")
            }
            (true, ProposeOutcome::Unchanged) => {
                format!("This room already uses its template name, **{channel_name}**.")
            }
        })
    }

    fn panel_text(&self, room_id: Snowflake) -> String {
        let live = self.live.read_state();
        let current = live
            .channels
            .get(&room_id)
            .and_then(|channel| channel.name.as_deref())
            .unwrap_or("(unknown)");
        let mode = match self.custom_names.get(&room_id) {
            Some(text) => format!("custom name `{}`", text.replace('`', "'")),
            None => "the template name".to_owned(),
        };
        format!(
            "**Room name**\nCurrent name: **{current}**\nThis room uses {mode}.\n\
             **Custom name** sets your own; template tokens such as @@owner@@ work. \
             **Restore template** goes back to the template name."
        )
    }

    /// The room's naming context from the live snapshot. Display names come
    /// from the directory; a member missing there renders as "member".
    fn name_facts(&self, room: &VoiceRoom, command: &NameCommand) -> NameFacts {
        let live = self.live.read_state();
        let occupants = live.occupants(room.channel_id);
        let user_limit = live
            .channels
            .get(&room.channel_id)
            .and_then(|channel| channel.user_limit)
            .unwrap_or(0);
        let first = self
            .creators
            .get(&room.creator_channel_id)
            .map_or(1, |creator| creator.first_room_number.max(1)) as u64;
        let rank = self
            .rooms
            .values()
            .filter(|other| {
                other.creator_channel_id == room.creator_channel_id
                    && other.channel_id < room.channel_id
            })
            .count() as u64;
        let label = command.settings.no_game_label.trim();
        let filter = NameFilterContext {
            guild_id: self.live.guild_id.to_string(),
            channel_id: room.channel_id.to_string(),
            user_id: command.actor_id.to_string(),
        };
        let owner_name = command.directory.get(room.owner_id);
        // The V1 name is the fallback for a blank template or an empty
        // render. A display name the filter blocks never becomes one.
        let mut fallback = room_name(owner_name);
        if filter_channel_name(&fallback, &command.policy, &filter).is_err() {
            fallback = room_name("member");
        }
        NameFacts {
            context: RoomContext {
                room_number: u32::try_from(first + rank).unwrap_or(u32::MAX),
                owner_name: owner_name.to_owned(),
                original_creator_name: command.directory.get(room.original_creator_id).to_owned(),
                member_count: u32::try_from(occupants.len()).unwrap_or(u32::MAX),
                owner_present: occupants.contains(&room.owner_id),
                user_limit,
                game_name: if label.is_empty() { "General" } else { label }.to_owned(),
                seed: room.name_seed,
                timestamp: i64::try_from(unix_now_ms() / 1000).unwrap_or(0),
                named_lists: command.settings.lists.clone(),
                ..RoomContext::default()
            },
            conditions: ConditionFacts {
                owner_id: Some(room.owner_id.to_string()),
                member_ids: occupants.iter().map(ToString::to_string).collect(),
                ..ConditionFacts::default()
            },
            fallback,
            filter,
            other_names: live
                .channels
                .values()
                .filter(|channel| {
                    channel.id.get() != room.channel_id
                        && matches!(
                            channel.kind,
                            ChannelType::GuildVoice | ChannelType::GuildStageVoice
                        )
                })
                .filter_map(|channel| channel.name.clone())
                .collect(),
        }
    }

    /// Persist the override through the urgent lane: the in-memory entry is
    /// the truth, and a stale action (the override moved on, or the room was
    /// forgotten) persists nothing.
    pub(super) async fn dispatch_custom_name(
        &mut self,
        action: QueuedAction,
        channel_id: Snowflake,
        custom_name: Option<String>,
        now_ms: u64,
        started: Instant,
    ) {
        if !self.rooms.contains_key(&channel_id)
            || self.custom_names.get(&channel_id) != custom_name.as_ref()
        {
            self.queue.mark_succeeded(&action);
            return;
        }
        match self
            .store
            .save_custom_name(self.live.guild_id, channel_id, custom_name.as_deref())
            .await
        {
            Ok(true) => {
                self.queue.mark_succeeded(&action);
            }
            Ok(false) => {
                // Tracked but no database row (deleted out-of-band): a write
                // that cannot land is never retried. The override stays in
                // the worker and surfaces as a persistence failure.
                self.record(LifecycleFailure::Persistence {
                    channel_id: Some(channel_id),
                    error: StoreError::Conflict,
                });
                self.queue.mark_succeeded(&action);
            }
            Err(StoreError::CredentialRefused) => {
                self.record(LifecycleFailure::Persistence {
                    channel_id: Some(channel_id),
                    error: StoreError::CredentialRefused,
                });
                self.halted = true;
                self.queue.mark_succeeded(&action);
            }
            Err(error) => {
                self.record(LifecycleFailure::Persistence {
                    channel_id: Some(channel_id),
                    error,
                });
                self.mark_failed_observed(
                    action,
                    "voice-room persistence unavailable".to_owned(),
                    elapsed_ms(now_ms, started),
                );
            }
        }
    }
}

impl<S, H> VoiceRuntime<S, H>
where
    S: RoomPersistence + Send + 'static,
    H: RoomWrites + Send + 'static,
{
    /// Run one `/name` step through the guild actor. `None` when the guild
    /// has no live actor yet or its inbox already drained.
    async fn run_name(&self, guild: Snowflake, command: NameCommand) -> Option<NameReply> {
        let actor = self.live_actor(guild)?;
        let (reply, inbox) = oneshot::channel();
        actor.tx.send(ActorCommand::Name { command, reply }).ok()?;
        inbox.await.ok()
    }
}

/// Handle one `/name` step: the slash command, a panel click or a modal
/// submit. `gated` is false only for the slash command, whose guild role gate
/// already ran in the shared handler. Always replies exactly once and
/// returns true.
pub(super) async fn handle_name_interaction<S, H, F>(
    runtime: &VoiceRuntime<S, H>,
    interaction: &Interaction,
    guild_id: Snowflake,
    names: &NameDirectory,
    request: NameInteraction,
    gated: bool,
    reply: impl FnOnce(InteractionResponse) -> F + Send,
) -> bool
where
    S: RoomPersistence + Send + 'static,
    H: RoomWrites + Send + 'static,
    F: Future<Output = ()> + Send,
{
    // The admin override comes from guild-level role facts, never from the
    // channel-scoped interaction permissions a room owner's own grant inflates.
    let guild_permissions = runtime.guild_permissions(interaction);
    let is_admin = is_voice_admin(guild_permissions);
    let Some(actor_id) = invoker_member_id(interaction) else {
        reply(ephemeral_response(
            "I couldn't tell who invoked /name — try again.",
        ))
        .await;
        return true;
    };
    let (store, _) = runtime.make_pair();
    if gated {
        let member = access_member(interaction, guild_permissions);
        if let Some(denial) = command_gate(&store, guild_id, &member, "name").await {
            reply(denial).await;
            return true;
        }
    }
    // The settings decide a name (uniqueness, named lists), so they are read
    // only for the steps that decide one, and an unreadable store refuses
    // rather than guessing: a missed "unique names" check would mint a
    // duplicate.
    let settings = if matches!(
        request,
        NameInteraction::Submit { .. } | NameInteraction::Restore { .. }
    ) {
        match store.config_snapshot(guild_id).await {
            Ok(config) => NameSettings::from_config(&config),
            Err(_) => {
                reply(ephemeral_response(
                    "Voice settings are unavailable right now. Nothing was changed; try again.",
                ))
                .await;
                return true;
            }
        }
    } else {
        NameSettings::default()
    };
    let mut directory = names.clone();
    if let Some((member, display)) = invoker_display(interaction) {
        if !directory.names.contains_key(&member) {
            directory.insert(member, display);
        }
    }
    let command = NameCommand {
        actor_id,
        is_admin,
        request,
        settings,
        directory,
        policy: Arc::clone(&runtime.name_policy),
    };
    let response = match runtime.run_name(guild_id, command).await {
        None => ephemeral_response(NOT_WARM),
        Some(NameReply::Refused(text) | NameReply::Applied(text)) => ephemeral_response(&text),
        Some(NameReply::Panel { room_id, text }) => panel_response(room_id, &text),
        Some(NameReply::Modal { room_id, prefill }) => modal_response(room_id, prefill.as_deref()),
    };
    reply(response).await;
    true
}

/// The ephemeral panel: current state plus the Custom name and Restore
/// template buttons, bound to the room.
pub(super) fn panel_response(room_id: Snowflake, text: &str) -> InteractionResponse {
    let button = |label: &str, style: ButtonStyle, custom_id: String| {
        Component::Button(Button {
            id: None,
            custom_id: Some(custom_id),
            disabled: false,
            emoji: None,
            label: Some(label.to_owned()),
            style,
            url: None,
            sku_id: None,
        })
    };
    InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(InteractionResponseData {
            content: Some(text.to_owned()),
            components: Some(vec![Component::ActionRow(ActionRow {
                id: None,
                components: vec![
                    button(
                        "Custom name",
                        ButtonStyle::Primary,
                        name_custom_custom_id(room_id),
                    ),
                    button(
                        "Restore template",
                        ButtonStyle::Secondary,
                        name_restore_custom_id(room_id),
                    ),
                ],
            })]),
            flags: Some(MessageFlags::EPHEMERAL),
            ..Default::default()
        }),
    }
}

/// The custom-name modal, pre-filled with the room's current override. It is
/// an initial callback, so a deferred acknowledgement cannot carry it.
#[allow(deprecated)] // `TextInput::label` is deprecated for `Label`; leave it unset.
pub(super) fn modal_response(room_id: Snowflake, prefill: Option<&str>) -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::Modal,
        data: Some(InteractionResponseData {
            custom_id: Some(name_modal_custom_id(room_id)),
            title: Some("Name your room".to_owned()),
            components: Some(vec![Component::Label(Label {
                id: None,
                label: "Room name".to_owned(),
                description: Some(format!(
                    "Template tokens like @@owner@@ work. Up to {MAX_CUSTOM_NAME_CHARS} characters."
                )),
                component: Box::new(Component::TextInput(TextInput {
                    id: None,
                    custom_id: NAME_INPUT_ID.to_owned(),
                    label: None,
                    max_length: Some(MAX_CUSTOM_NAME_CHARS as u16),
                    min_length: Some(1),
                    placeholder: Some("@@owner@@'s room".to_owned()),
                    required: Some(true),
                    style: TextInputStyle::Short,
                    value: prefill.map(str::to_owned),
                })),
            })]),
            ..Default::default()
        }),
    }
}
