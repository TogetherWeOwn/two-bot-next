//! Full V6 voice-room name templates, written from `docs/voice-rooms.md` §V6
//! only: the V5 naming engine plus conditionals and styling.
//!
//! [`TemplateExtensions`] is the V6 [`ExtensionPolicy`]. `{{cond ?? yes // no}}`
//! nodes go to the condition evaluator in [`crate::voice_conditions`], and
//! `""mode:text""` nodes go to the styling library in [`crate::voice_style`].
//! The engine's pipeline is unchanged: conditionals, then tokens, then
//! styling, then trim, truncation to [`MAX_NAME_LEN`] characters and the
//! fallback name. A styled body is styled after its tokens and nested
//! conditionals render, so `""upper:@@owner@@""` upper-cases the owner's name;
//! truncation counts characters after styling, so a Unicode font never splits
//! a code point.
//!
//! Random picks keep their positions. A styled body reserves its picks
//! whatever its modes do, and a conditional reserves the picks in both
//! branches, so a condition that flips never re-rolls a later `[[a/b]]`. The
//! `rand` case mode is seeded from the room's stored seed alone, so it is
//! stable across renames and membership changes.
//!
//! This module performs no I/O. Resolving member roles, activities and
//! streams into [`ConditionFacts`] belongs to the runtime that renders a room.
//!
//! ```
//! use two_bot_core::voice_conditions::ConditionFacts;
//! use two_bot_core::voice_naming::RoomContext;
//! use two_bot_core::voice_template::resolve_room_name;
//!
//! let room = RoomContext {
//!     owner_name: "Alex".into(),
//!     member_count: 1,
//!     owner_present: true,
//!     ..RoomContext::default()
//! };
//! let template = r#"{{LIVE ??""upper:@@owner@@"" is live//@@owner@@'s room}}"#;
//! let live = ConditionFacts {
//!     owner_live_discord: true,
//!     ..ConditionFacts::default()
//! };
//! assert_eq!(resolve_room_name(template, &room, &live, "Hangout"), "ALEX is live");
//! let idle = ConditionFacts::default();
//! assert_eq!(resolve_room_name(template, &room, &idle, "Hangout"), "Alex's room");
//! ```
//!
//! [`MAX_NAME_LEN`]: crate::voice_naming::MAX_NAME_LEN

use crate::voice_conditions::{ConditionFacts, Conditions};
use crate::voice_naming::{
    parse, render_text, resolve_room_name_with, Evaluation, ExtensionPolicy, RoomContext,
    Template, MAX_TEMPLATE_BYTES,
};
use crate::voice_style::{apply_chain, parse_modes};

/// Separates the `rand` case stream from the random-choice dice, which are
/// seeded from the same stored room seed.
const RAND_CASE_DOMAIN: u64 = 0x5241_4e44_4341_5345;

/// The V6 extension policy over one room's [`ConditionFacts`]: real
/// conditionals and real styling. One value serves one render at a time.
#[derive(Debug)]
pub struct TemplateExtensions<'f> {
    conditions: Conditions<'f>,
}

impl<'f> TemplateExtensions<'f> {
    /// Evaluate conditions against these room facts and style with every
    /// V6 mode.
    #[must_use]
    pub fn new(facts: &'f ConditionFacts) -> Self {
        Self {
            conditions: Conditions::new(facts),
        }
    }
}

impl ExtensionPolicy for TemplateExtensions<'_> {
    fn conditional(&self, source: &str, evaluation: &mut Evaluation<'_, Self>) -> String {
        self.conditions.evaluate(source, evaluation)
    }

    fn styled(
        &self,
        modes: &str,
        body: &Template,
        _source: &str,
        evaluation: &mut Evaluation<'_, Self>,
    ) -> String {
        let text = evaluation.evaluate(body);
        let seed = evaluation.context().seed ^ RAND_CASE_DOMAIN;
        apply_chain(&parse_modes(modes), &text, seed)
    }
}

/// Resolve a room's channel name with full V6 behaviour.
///
/// Same contract as [`crate::voice_naming::resolve_room_name`]: a blank or
/// oversized template keeps `raw_name`, an empty render falls back to
/// `raw_name`, and the result is never empty and never over
/// [`MAX_NAME_LEN`] characters.
///
/// [`MAX_NAME_LEN`]: crate::voice_naming::MAX_NAME_LEN
#[must_use]
pub fn resolve_room_name(
    template: &str,
    ctx: &RoomContext,
    facts: &ConditionFacts,
    raw_name: &str,
) -> String {
    resolve_room_name_with(template, ctx, raw_name, &TemplateExtensions::new(facts))
}

/// Render a free-text template (a voice status line) with full V6
/// behaviour: trimmed, at most `max_chars` characters, and empty for a blank
/// or oversized template or an empty render. No channel-name fallback.
#[must_use]
pub fn resolve_text(
    template: &str,
    ctx: &RoomContext,
    facts: &ConditionFacts,
    max_chars: usize,
) -> String {
    if template.trim().is_empty() || template.len() > MAX_TEMPLATE_BYTES {
        return String::new();
    }
    render_text(
        &parse(template),
        ctx,
        &TemplateExtensions::new(facts),
        max_chars,
    )
}
