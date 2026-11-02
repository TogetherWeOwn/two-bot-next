//! Protected snapshot/output vocabulary, not a production API requirement.
//! The editable bridge converts these facts to the real pure board API.

use twilight_model::{
    channel::permission_overwrite::PermissionOverwrite,
    guild::Role,
    id::{marker::RoleMarker, Id},
};
use two_bot_core::{
    voice_alias::AliasTable, voice_naming::GameOptions, voice_presence::MemberPresence,
};

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub ready: bool,
    pub generation: u64,
    pub as_of_unix: i64,
    pub guild_id: u64,
    pub guild_owner_id: u64,
    pub viewer_id: u64,
    // None is an absent member-role snapshot, not an ordinary member with no roles.
    pub viewer_roles: Option<Vec<Id<RoleMarker>>>,
    pub roles: Vec<Role>,
    pub presence_enabled: bool,
    pub aliases: AliasTable,
    pub game_options: GameOptions,
    pub rooms: Vec<Room>,
}

#[derive(Debug, Clone)]
pub struct Room {
    pub channel_id: u64,
    pub name: String,
    pub tracked: bool,
    pub owner_id: u64,
    pub user_limit: Option<u16>,
    pub overwrites: Vec<PermissionOverwrite>,
    pub occupants: Vec<Occupant>,
}

#[derive(Debug, Clone)]
pub struct Occupant {
    pub member_id: u64,
    pub is_bot: bool,
    pub presence: Option<MemberPresence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Ready,
    CatchingUp,
    // The baseline has no board. Never mistake this deliberate RED bridge for
    // a production refusal or for evidence of working permission filtering.
    MissingFeature,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub channel_id: u64,
    pub humans: usize,
    pub seats: String,
    // The bounded display label, before renderer escaping; None means absent.
    pub game: Option<String>,
    pub members_playing: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct Reply {
    pub generation: u64,
    pub state: State,
    // Entries actually included in content, not an unrendered intermediate list.
    pub entries: Vec<Entry>,
    pub omitted: usize,
    pub content: String,
}
