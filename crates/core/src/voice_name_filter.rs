//! Pure legacy temp-voice room-name sanitize and automod filter core.
//!
//! Ports legacy two-bot `src/tempVoice/nameFilter.ts` (`sanitize`,
//! `filterChannelName`, `renderNameTemplate`) and the create-path retry in
//! `src/tempVoice/service.ts` (`onGeneratorJoin`) as pure functions over plain
//! data. A rendered name is filtered through the existing automod matcher
//! ([`crate::automod::match_automod`]) as a synthetic message, so a word
//! blocked in chat is blocked in a channel name by construction.
//!
//! Only the content-shaped filters apply: [`crate::automod::AutomodFilter`]
//! `BadWords`, `InviteLink` and `ExternalLink`. A name cannot mention anybody,
//! carry an attachment, or repeat itself, so the name-scoped policy neutralises
//! `MentionSpam` (even a guild running `mention_limit` 0), `RepeatedMessage`
//! and `AttachmentType` rather than ignoring them afterwards. Exemptions are
//! not consulted: legacy feeds the synthetic message straight to
//! `matchAutomod` with no bypass-role or exempt-channel check.
//!
//! Deliberately out of scope: the room-lifecycle runtime (caps, claims, Discord
//! creates/moves/deletes, audit rows, logging), the V5 template engine
//! ([`crate::voice_naming`], which uses `@@owner@@` syntax — this module keeps
//! the legacy `{username}`/`{count}`/`{seq}` placeholders), and persistence.
//! The caller supplies the guild's automod policy, persists the returned name,
//! and audits [`NAME_BLOCKED_AUDIT_REASON`] on refusal.

use unicode_normalization::UnicodeNormalization;

use crate::automod::{match_automod, AutomodFilter, AutomodMessage, AutomodPolicy, RepeatTracker};

/// Discord's channel-name length bounds, in Unicode scalar values. Legacy
/// measures UTF-16 code units; astral characters (emoji) count one here where
/// legacy counts two, so this core is marginally more permissive for them.
pub const MIN_CHANNEL_NAME_CHARS: usize = 1;
/// Discord's channel-name length ceiling, in Unicode scalar values.
pub const MAX_CHANNEL_NAME_CHARS: usize = 100;
/// Legacy `renderNameTemplate` fallback when the sanitized render is empty.
pub const FALLBACK_TEMPLATE_NAME: &str = "voice channel";
/// Stable audit/machine reason when the bare template is also blocked.
pub const NAME_BLOCKED_AUDIT_REASON: &str = "name_blocked";

/// Who asked for the name, mirroring legacy `filterChannelName`'s `context`.
/// The generator channel is the filter context: it is the channel the member
/// is sitting in, and there is no generated channel yet to name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameFilterContext {
    pub guild_id: String,
    pub channel_id: String,
    pub user_id: String,
}

/// Typed `filterChannelName` refusals. Display messages repeat the legacy
/// user-facing sentences verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameError {
    /// Sanitized name is empty.
    Empty,
    /// Sanitized name is over [`MAX_CHANNEL_NAME_CHARS`] scalars.
    TooLong {
        /// Scalar count of the rejected name.
        len: usize,
    },
    /// The automod matcher fired one of the name filters.
    Blocked {
        /// Which content filter fired.
        filter: AutomodFilter,
    },
}

impl std::fmt::Display for NameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("That name is empty once formatting is removed."),
            Self::TooLong { .. } => write!(
                f,
                "Channel names are at most {MAX_CHANNEL_NAME_CHARS} characters."
            ),
            Self::Blocked { filter } => write!(
                f,
                "That name is not allowed here ({}).",
                filter.as_str().replace('_', " ")
            ),
        }
    }
}

impl std::error::Error for NameError {}

impl NameError {
    /// The blocking filter, if this error is a filter block.
    #[must_use]
    pub fn filter(self) -> Option<AutomodFilter> {
        match self {
            Self::Blocked { filter } => Some(filter),
            Self::Empty | Self::TooLong { .. } => None,
        }
    }
}

/// Legacy `/\\s/gu` whitespace, enumerated so the collapse matches JS exactly
/// rather than Rust's `White_Space`: U+0085 NEL splits in Rust but not in JS,
/// so it survives here as in legacy; U+FEFF splits in JS but not in Rust, so
/// it is listed explicitly.
fn is_legacy_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

/// Legacy `sanitize`: NFKC, C0 controls plus DEL become spaces, `@` and
/// backtick are stripped, legacy-whitespace runs collapse to one ASCII space,
/// ends trimmed. Full-width `@` (U+FF20) normalises to `@` first, so it is
/// stripped too. Zero-width characters survive (they are not whitespace); the
/// automod matcher still sees through them. This function is a fixed point:
/// sanitizing its own output changes nothing.
#[must_use]
pub fn sanitize_channel_name(raw: &str) -> String {
    let normalized: String = raw.nfkc().collect();
    let mut stripped = String::with_capacity(normalized.len());
    for c in normalized.chars() {
        if matches!(c, '\u{0000}'..='\u{001F}' | '\u{007F}') {
            stripped.push(' ');
        } else if c == '@' || c == '`' {
            // Stripped, not rejected: a user typing `@everyone` wants a name.
        } else {
            stripped.push(c);
        }
    }
    // Mirrors legacy `.replace(/\\s+/gu, ' ').trim()`: split on runs, drop
    // empties, rejoin.
    stripped
        .split(is_legacy_whitespace)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Legacy `filterChannelName`: sanitize, enforce the 1–100 scalar bounds, then
/// run the automod matcher under a name-scoped policy. Returns the sanitized
/// name on acceptance.
pub fn filter_channel_name(
    raw: &str,
    policy: &AutomodPolicy,
    context: &NameFilterContext,
) -> Result<String, NameError> {
    filter_text(raw, policy, context, MAX_CHANNEL_NAME_CHARS)
}

/// Longest voice channel status Discord accepts.
pub const MAX_VOICE_STATUS_CHARS: usize = 500;

/// [`filter_channel_name`] for a voice status line: the same sanitizer and
/// automod checks with Discord's 500-character status bound.
pub fn filter_voice_status(
    raw: &str,
    policy: &AutomodPolicy,
    context: &NameFilterContext,
) -> Result<String, NameError> {
    filter_text(raw, policy, context, MAX_VOICE_STATUS_CHARS)
}

fn filter_text(
    raw: &str,
    policy: &AutomodPolicy,
    context: &NameFilterContext,
    max_chars: usize,
) -> Result<String, NameError> {
    let name = sanitize_channel_name(raw);
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    let len = name.chars().count();
    if len > max_chars {
        return Err(NameError::TooLong { len });
    }
    // `match_automod` returns the FIRST filter that fires, so the inapplicable
    // ones are neutralised rather than ignored afterwards: a guild running
    // mention limit 0 would otherwise short-circuit on `mention_spam` and
    // never reach the invite-link check.
    let name_scoped = AutomodPolicy {
        mention_limit: usize::MAX,
        repeated_message_count: u32::MAX,
        blocked_attachment_extensions: Vec::new(),
        ..policy.clone()
    };
    let message = AutomodMessage {
        guild_id: context.guild_id.clone(),
        channel_id: context.channel_id.clone(),
        message_id: format!("temp-voice-name:{}", context.channel_id),
        author_id: context.user_id.clone(),
        author_is_bot: false,
        role_ids: Vec::new(),
        content: name.clone(),
        mentioned_user_ids: Vec::new(),
        attachment_names: Vec::new(),
        observed_timestamp_ms: 0,
    };
    let mut repeats = RepeatTracker::default();
    match match_automod(&message, &name_scoped, &mut repeats) {
        Some(
            filter @ (AutomodFilter::BadWords
            | AutomodFilter::InviteLink
            | AutomodFilter::ExternalLink),
        ) => Err(NameError::Blocked { filter }),
        _ => Ok(name),
    }
}

/// Legacy `renderNameTemplate`: substitute `{username}`, `{count}` and `{seq}`
/// (in that order, so a username containing `{count}` still expands), sanitize
/// the result, fall back to [`FALLBACK_TEMPLATE_NAME`] when empty, and
/// truncate to [`MAX_CHANNEL_NAME_CHARS`] scalars.
#[must_use]
pub fn render_name_template(template: &str, username: &str, count: u32, seq: u32) -> String {
    let rendered = template
        .replace("{username}", username)
        .replace("{count}", &count.to_string())
        .replace("{seq}", &seq.to_string());
    let trimmed = sanitize_channel_name(&rendered);
    if trimmed.is_empty() {
        return FALLBACK_TEMPLATE_NAME.to_string();
    }
    truncate_chars(&trimmed, MAX_CHANNEL_NAME_CHARS)
}

/// A name the create path may mint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRoomName {
    /// Sanitized name to create the channel with.
    pub name: String,
    /// True when the username was stripped to pass the filter.
    pub username_stripped: bool,
}

/// The guild's own template violates its own automod policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedRoomName {
    /// User-facing sentence: `That channel name is not allowed here. <cause>`.
    pub reason: String,
    /// Stable machine reason for the audit row.
    pub audit_reason: &'static str,
    /// The bare-template rejection (filter, length or empty).
    pub error: NameError,
}

impl BlockedRoomName {
    /// The blocking filter, if the bare template was filter-blocked.
    #[must_use]
    pub fn filter(&self) -> Option<AutomodFilter> {
        self.error.filter()
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let end = s.char_indices().nth(max_chars).map_or(s.len(), |(i, _)| i);
    s[..end].to_string()
}

/// Legacy `onGeneratorJoin` name decision: filter the rendered template; when
/// it is blocked, retry with an empty username and use that name instead.
/// Only a template that is itself blocked refuses — an operator
/// misconfiguration to fix, not a member to punish — with audit reason
/// [`NAME_BLOCKED_AUDIT_REASON`]. Creating anyway would mint a forbidden name,
/// so refusal is loud rather than laundered.
pub fn resolve_create_name(
    template: &str,
    username: &str,
    count: u32,
    seq: u32,
    policy: &AutomodPolicy,
    context: &NameFilterContext,
) -> Result<ResolvedRoomName, BlockedRoomName> {
    let rendered = render_name_template(template, username, count, seq);
    if let Ok(name) = filter_channel_name(&rendered, policy, context) {
        return Ok(ResolvedRoomName {
            name,
            username_stripped: false,
        });
    }
    let bare = render_name_template(template, "", count, seq);
    match filter_channel_name(&bare, policy, context) {
        Ok(name) => Ok(ResolvedRoomName {
            name,
            username_stripped: true,
        }),
        Err(error) => Err(BlockedRoomName {
            reason: format!("That channel name is not allowed here. {error}"),
            audit_reason: NAME_BLOCKED_AUDIT_REASON,
            error,
        }),
    }
}
