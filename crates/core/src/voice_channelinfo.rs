//! Pure V7c `/channelinfo` resolution core, derived from `docs/voice-rooms.md`
//! §V7 only.
//!
//! The Discord panel (parent card TOG-10113) shows a room's owner, detected
//! game and why the room has its name, with "All variables" and "Preview in
//! other states" buttons. This module is the pure core behind those buttons:
//! it resolves every template variable to its current value, renders the
//! room's configured templates under the six canonical preview states, and
//! answers the owner-or-admin inspection gate. It performs no I/O, holds no
//! Discord, store, clock or database types, and never leaks member names,
//! presence details or IDs beyond what [`render`](crate::voice_naming::render)
//! already exposes: callers pass aggregated room facts (the resolved owner
//! display name, the resolved game title, headcounts), never rosters.
//!
//! Nickname resolution (`/nick`) and alias storage belong to V7a; command
//! access gates belong to V10b; the Discord panel wiring belongs to TOG-10113.
//! This core consumes the already-resolved owner display name and game title
//! and reuses [`crate::voice_naming`] under the V5 passthrough policy for
//! every token value and preview, so button output always matches the room
//! name the engine actually renders. V6 conditional/styling syntax therefore
//! previews literally here, exactly as the V5 engine renders it.

use crate::voice_naming::{
    parse, render, ChannelKind, Evaluation, PassthroughExtensions, RoomContext,
};
use crate::Snowflake;

/// Every `@@token@@` and numbering spelling resolved by
/// [`resolve_variables`], in "All variables" display order: numbering, people
/// and counts, game/stream/party, time and random. Derived room state
/// (`FULL`, `PRIVATE`, `LOCKED`, `occupant_bucket`, `room_number`) follows
/// these in the same order.
const TOKEN_VARIABLES: &[&str] = &[
    "##",
    "$#",
    "$0#",
    "$00#",
    "+#",
    "@@nato@@",
    "@@owner@@",
    "@@creator@@",
    "@@original_creator@@",
    "@@num@@",
    "@@num_others@@",
    "@@num_live@@",
    "@@limit@@",
    "@@slots@@",
    "@@game_name@@",
    "@@stream_name@@",
    "@@num_playing@@",
    "@@party_size@@",
    "@@party_state@@",
    "@@party_details@@",
    "@@weekday@@",
    "@@month@@",
    "@@hour@@",
    "@@random_emoji@@",
];

/// Number of entries [`resolve_variables`] always returns: every token above
/// plus `FULL`, `PRIVATE`, `LOCKED`, `occupant_bucket` and `room_number`.
pub const VARIABLE_COUNT: usize = TOKEN_VARIABLES.len() + 5;

/// Canonical preview states for "Preview in other states", in button order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PreviewState {
    SoloNoGame,
    InGame,
    Full,
    Locked,
    Private,
    Streaming,
}

impl PreviewState {
    /// All six states, in button order.
    pub const ALL: [PreviewState; 6] = [
        PreviewState::SoloNoGame,
        PreviewState::InGame,
        PreviewState::Full,
        PreviewState::Locked,
        PreviewState::Private,
        PreviewState::Streaming,
    ];

    /// Stable button label for this state.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            PreviewState::SoloNoGame => "solo-no-game",
            PreviewState::InGame => "in-game",
            PreviewState::Full => "full",
            PreviewState::Locked => "locked",
            PreviewState::Private => "private",
            PreviewState::Streaming => "streaming",
        }
    }
}

/// Which configured template a preview rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TemplateKind {
    Name,
    Status,
}

impl TemplateKind {
    /// Stable label for this template slot.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            TemplateKind::Name => "name",
            TemplateKind::Status => "status",
        }
    }
}

/// Pure input for one room inspection. `room` carries the aggregated facts
/// the naming engine renders (owner display name already resolved through the
/// V7a nick core, game title already resolved through aliases); no roster,
/// presence list or member IDs travel with it. `private`/`locked` are the
/// room's V3 privacy/lock flags; `name_template` and `status_template` are the
/// room's configured templates verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelInfoContext {
    pub room: RoomContext,
    pub private: bool,
    pub locked: bool,
    pub name_template: String,
    pub status_template: Option<String>,
}

/// One "All variables" row: the variable spelling and its current value.
/// Token values are the raw substitutions (no trim, truncation or fallback),
/// so empty states surface as empty strings (`@@slots@@` is blank when the
/// room is unlimited, `@@stream_name@@` when nobody is live).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableEntry {
    pub name: &'static str,
    pub value: String,
}

/// Every template variable with its current value, in display order. Always
/// holds exactly [`VARIABLE_COUNT`] entries with unique names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VariableMap {
    entries: Vec<VariableEntry>,
}

impl VariableMap {
    /// Number of entries (always [`VARIABLE_COUNT`]).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the map holds no entries (never true for resolved maps).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entries in display order.
    pub fn iter(&self) -> impl Iterator<Item = &VariableEntry> {
        self.entries.iter()
    }

    /// Current value for one variable spelling, or `None` when unknown.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| entry.value.as_str())
    }
}

/// One "Preview in other states" row: the state, which configured template
/// was rendered, and the rendered name. Rendering reuses
/// [`render`](crate::voice_naming::render), so the V5 fallback contract
/// holds: previews are never empty and never over 100 characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatePreview {
    pub state: PreviewState,
    pub template_kind: TemplateKind,
    pub template: String,
    pub rendered: String,
}

/// Resolve every template variable in V5/V6 scope to its current value for
/// the room, plus derived room state (`FULL`, `PRIVATE`, `LOCKED`,
/// `occupant_bucket`, `room_number`).
///
/// Token values reuse the naming engine's own substitution pass, so they
/// match what the room name actually shows. `FULL` needs a limit
/// (`user_limit != 0 && member_count >= user_limit`, mirroring V6);
/// `PRIVATE` is always false on standalone channels (mirroring V6) and
/// otherwise follows the room's privacy flag; `LOCKED` is the opaque V3 lock
/// flag the runtime sets when the limit was applied as a headcount lock;
/// `occupant_bucket` is the privacy-preserving headcount class
/// (`empty`/`solo`/`duo`/`group`).
#[must_use]
pub fn resolve_variables(ctx: &ChannelInfoContext) -> VariableMap {
    let ext = PassthroughExtensions;
    let mut entries = Vec::with_capacity(VARIABLE_COUNT);
    for probe in TOKEN_VARIABLES {
        let value = Evaluation::new(&ctx.room, &ext).evaluate(&parse(probe));
        entries.push(VariableEntry {
            name: *probe,
            value,
        });
    }
    let full = ctx.room.user_limit != 0 && ctx.room.member_count >= ctx.room.user_limit;
    // V6: PRIVATE is always false on standalone channels.
    let private = ctx.private && ctx.room.channel_kind == ChannelKind::Temporary;
    for (name, value) in [
        ("FULL", full.to_string()),
        ("PRIVATE", private.to_string()),
        ("LOCKED", ctx.locked.to_string()),
        (
            "occupant_bucket",
            occupant_bucket(ctx.room.member_count).to_string(),
        ),
        ("room_number", ctx.room.room_number.to_string()),
    ] {
        entries.push(VariableEntry { name, value });
    }
    VariableMap { entries }
}

/// Render the room's configured templates under the six canonical states, in
/// [`PreviewState::ALL`] order. Each state yields a name-template preview
/// plus a status-template preview when one is configured. Room identity
/// (number, owner, seed, clock, named lists, fallback, channel kind) is
/// preserved so random picks stay stable; only the facts that define each
/// state change:
///
/// - solo-no-game: one member, no game, nothing playing, no parties, not
///   live, unlimited, open, unlocked.
/// - in-game: three members playing the room's current game (or `Apex` when
///   it has none), no parties, not live, unlimited, open, unlocked.
/// - full: headcount at the room's limit (or 4 when unlimited), otherwise the
///   current facts, open, unlocked.
/// - locked: same headcount-at-limit shape with the lock flag set.
/// - private: the current facts with privacy set.
/// - streaming: the current facts with the owner live (keeping the current
///   stream title, or `Live build` when it has none).
#[must_use]
pub fn preview_states(ctx: &ChannelInfoContext) -> Vec<StatePreview> {
    let mut out = Vec::with_capacity(PreviewState::ALL.len() * 2);
    for state in PreviewState::ALL {
        let room = preview_inputs(ctx, state);
        let rendered = render(&parse(&ctx.name_template), &room, &PassthroughExtensions);
        out.push(StatePreview {
            state,
            template_kind: TemplateKind::Name,
            template: ctx.name_template.clone(),
            rendered,
        });
        if let Some(status) = &ctx.status_template {
            let rendered = render(&parse(status), &room, &PassthroughExtensions);
            out.push(StatePreview {
                state,
                template_kind: TemplateKind::Status,
                template: status.clone(),
                rendered,
            });
        }
    }
    out
}

/// Owner-or-admin inspection gate: the room owner may always inspect their
/// own room, admins may inspect any room, everyone else is refused. Pure ID
/// comparison; zero IDs are never authorized.
#[must_use]
pub fn may_inspect(viewer_id: Snowflake, viewer_is_admin: bool, owner_id: Snowflake) -> bool {
    if viewer_id == 0 || owner_id == 0 {
        return false;
    }
    viewer_is_admin || viewer_id == owner_id
}

/// Privacy-preserving headcount class: `empty` (0), `solo` (1), `duo` (2),
/// `group` (3+). Buckets never name members.
fn occupant_bucket(member_count: u32) -> &'static str {
    match member_count {
        0 => "empty",
        1 => "solo",
        2 => "duo",
        _ => "group",
    }
}

/// Altered facts for one preview state; see [`preview_states`]. Privacy and
/// lock travel in the [`StatePreview::state`] label: V5 has no privacy or
/// lock token, so the engine renders those states from the headcount and
/// limit facts the runtime actually changes (at-limit counts, live flags).
fn preview_inputs(ctx: &ChannelInfoContext, state: PreviewState) -> RoomContext {
    let mut room = ctx.room.clone();
    match state {
        PreviewState::SoloNoGame => {
            room.member_count = 1;
            room.owner_present = true;
            room.live_count = 0;
            room.user_limit = 0;
            room.game_name.clear();
            room.stream_title.clear();
            room.members_playing = 0;
            room.parties.clear();
        }
        PreviewState::InGame => {
            room.member_count = 3;
            room.owner_present = true;
            room.live_count = 0;
            room.user_limit = 0;
            if room.game_name.is_empty() {
                room.game_name = "Apex".to_string();
            }
            room.stream_title.clear();
            room.members_playing = 3;
            room.parties.clear();
        }
        PreviewState::Full | PreviewState::Locked => {
            // A V3 headcount lock sets the limit to the current headcount, so
            // `full` and `locked` render identically under V5 tokens; the
            // state label carries the distinction.
            let limit = if ctx.room.user_limit == 0 {
                4
            } else {
                ctx.room.user_limit
            };
            room.user_limit = limit;
            room.member_count = limit;
            room.owner_present = true;
        }
        PreviewState::Private => {
            // Privacy has no V5 token; the preview shows the current facts
            // unchanged so the panel can state the name is unaffected.
        }
        PreviewState::Streaming => {
            if room.member_count == 0 {
                room.member_count = 2;
                room.owner_present = true;
            }
            room.live_count = 1;
            if room.stream_title.is_empty() {
                room.stream_title = "Live build".to_string();
            }
        }
    }
    room
}
