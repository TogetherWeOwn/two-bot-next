//! Pure V3 `/name` decisions derived from `docs/voice-rooms.md` §V3 and §V5.
//!
//! `/name` opens a panel to set a custom room name (template tokens allowed)
//! or restore the template name. This module decides what a typed name or a
//! restore means; it performs no I/O and holds no Discord, store or clock
//! types. The runtime supplies the facts: the room's current naming context,
//! the guild's automod policy, the "unique names" setting and the names of
//! the guild's other voice channels (the room itself already excluded).
//!
//! A custom name is checked in this order:
//!
//! 1. The raw text must be non-empty and at most
//!    [`MAX_CUSTOM_NAME_CHARS`] characters, so the stored override is bounded.
//! 2. The raw text passes the room-name sanitizer and automod name filter
//!    ([`crate::voice_name_filter::filter_channel_name`]). Filtering the
//!    template text, not just today's render, means a blocked word hiding in
//!    a conditional branch that is not active yet is refused now rather than
//!    appearing on a later re-render.
//! 3. The text renders through the full V6 template engine
//!    ([`crate::voice_template::resolve_room_name`]) and the render passes the
//!    same filter, because tokens such as `@@owner@@` or `@@game_name@@` pull
//!    in member-controlled text.
//! 4. When the guild's "unique names" setting is on and the text is a literal
//!    name (no template syntax), the folded name must not match another voice
//!    channel ([`crate::voice_room_controls::name_conflicts`]). A name built
//!    from tokens changes as the room does, so a one-time collision check
//!    would prove nothing; numbering tokens exist to keep those apart.
//!
//! The stored override is the trimmed raw text, never the render: the
//! runtime re-renders it as the room changes and clears it on restore.

use crate::automod::{AutomodFilter, AutomodPolicy};
use crate::voice_conditions::ConditionFacts;
use crate::voice_name_filter::{
    filter_channel_name, NameError, NameFilterContext, MAX_CHANNEL_NAME_CHARS,
};
use crate::voice_naming::{parse, RoomContext, Segment};
use crate::voice_room_controls::name_conflicts;
use crate::voice_template::resolve_room_name;

/// Longest stored custom-name text, in characters. Matches the modal input
/// limit and the Discord channel-name ceiling, so a stored override always
/// fits the column the runtime keeps it in.
pub const MAX_CUSTOM_NAME_CHARS: usize = MAX_CHANNEL_NAME_CHARS;

/// Component id of the modal's text input.
pub const NAME_INPUT_ID: &str = "name";

/// Why a typed name or a restore was refused. Display text is the ephemeral
/// reply and never echoes the rejected name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameRefusal {
    /// Empty once whitespace and formatting are removed.
    Empty,
    /// Over [`MAX_CUSTOM_NAME_CHARS`] characters.
    TooLong,
    /// The automod name filter fired.
    Blocked { filter: AutomodFilter },
    /// The "unique names" setting is on and another voice channel already
    /// uses this literal name.
    Taken,
    /// Restore only: the creator's own template renders a name the filter
    /// blocks, so there is no allowed template name to restore.
    TemplateBlocked,
}

impl std::fmt::Display for NameRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("That name is empty once formatting is removed."),
            Self::TooLong => write!(
                f,
                "Room names are at most {MAX_CUSTOM_NAME_CHARS} characters."
            ),
            Self::Blocked { filter } => write!(
                f,
                "That name is not allowed here ({}).",
                filter.as_str().replace('_', " ")
            ),
            Self::Taken => {
                f.write_str("Another voice channel already uses that name. Pick a different one.")
            }
            Self::TemplateBlocked => f.write_str(
                "This server's name filter blocks the room's template name, so it cannot be \
                 restored. Ask an admin to fix the creator channel's name template.",
            ),
        }
    }
}

impl std::error::Error for NameRefusal {}

impl From<NameError> for NameRefusal {
    fn from(error: NameError) -> Self {
        match error {
            NameError::Empty => Self::Empty,
            NameError::TooLong { .. } => Self::TooLong,
            NameError::Blocked { filter } => Self::Blocked { filter },
        }
    }
}

/// What the name is rendered against: the room's current naming context and
/// condition facts, plus the name used when a render comes out empty.
#[derive(Debug, Clone, Copy)]
pub struct RenderFacts<'a> {
    pub context: &'a RoomContext,
    pub conditions: &'a ConditionFacts,
    /// Name a render falls back to when it is empty (never empty itself).
    pub fallback_name: &'a str,
}

/// The guild-level rules a name must pass.
#[derive(Debug, Clone, Copy)]
pub struct NameChecks<'a> {
    pub policy: &'a AutomodPolicy,
    /// Identifies the filtered text for the automod matcher; the member is
    /// the one who typed the name.
    pub filter: &'a NameFilterContext,
    /// The guild's "unique names" setting.
    pub unique_names: bool,
    /// Names of the guild's other voice channels. The caller excludes the
    /// room being renamed, so an unchanged name never conflicts with itself.
    pub other_voice_names: &'a [String],
}

/// An accepted custom name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomName {
    /// The text to store: the trimmed raw input, template syntax intact.
    pub stored: String,
    /// The channel name it renders to right now: sanitized, at most
    /// [`MAX_CHANNEL_NAME_CHARS`] characters, never empty.
    pub channel_name: String,
}

/// True when `raw` carries no template syntax at all: no tokens, numbering,
/// plurals, choices, resting blocks, conditionals or styling.
#[must_use]
pub fn is_literal_name(raw: &str) -> bool {
    parse(raw)
        .0
        .iter()
        .all(|segment| matches!(segment, Segment::Text(_)))
}

/// Decide a typed custom name. See the module docs for the check order.
pub fn decide_custom_name(
    raw: &str,
    render: &RenderFacts<'_>,
    checks: &NameChecks<'_>,
) -> Result<CustomName, NameRefusal> {
    let text = raw.trim();
    if text.is_empty() {
        return Err(NameRefusal::Empty);
    }
    if text.chars().count() > MAX_CUSTOM_NAME_CHARS {
        return Err(NameRefusal::TooLong);
    }
    // Template text first, so a blocked word in any branch is refused now.
    filter_channel_name(text, checks.policy, checks.filter)?;
    let rendered = resolve_room_name(
        text,
        render.context,
        render.conditions,
        render.fallback_name,
    );
    let channel_name = filter_channel_name(&rendered, checks.policy, checks.filter)?;
    if is_literal_name(text)
        && name_conflicts(&channel_name, checks.other_voice_names, checks.unique_names)
    {
        return Err(NameRefusal::Taken);
    }
    Ok(CustomName {
        stored: text.to_owned(),
        channel_name,
    })
}

/// Decide the template name `/name` restore returns the room to: the
/// creator's template rendered against the room, or `render.fallback_name`
/// when the template is blank. The result passes the same sanitizer and
/// filter as a custom name; there is no uniqueness check, because a template
/// name is never a typed literal.
pub fn decide_template_name(
    template: &str,
    render: &RenderFacts<'_>,
    checks: &NameChecks<'_>,
) -> Result<String, NameRefusal> {
    let rendered = resolve_room_name(
        template,
        render.context,
        render.conditions,
        render.fallback_name,
    );
    filter_channel_name(&rendered, checks.policy, checks.filter)
        .map_err(|_| NameRefusal::TemplateBlocked)
}
