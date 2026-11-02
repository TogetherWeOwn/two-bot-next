//! Strict slash-option decode plus synthetic message build for
//! `/automod-preview` (docs/automod-preview-spec.md §§3.1, 5; slice 3c).
//!
//! Pure core: no I/O, no clock, no config read. The Discord handler maps
//! twilight option values into [`PreviewOptionValue`] and calls
//! [`decode_preview_input`]; unknown, duplicated, or wrong-typed options
//! refuse before any config read, rate consumption, or evaluation. Refusals
//! carry fixed messages only — never the submitted text or any normalized,
//! matched, or digested form of it.
//!
//! Commas separate attachment entries, so no single entry can contain one:
//! `safe.exe,txt` as an option value is unambiguously two bare names. An
//! empty entry (leading, trailing, or doubled comma) refuses instead of
//! silently dropping, and [`build_preview_message`] re-validates every name
//! (including a comma check) so a comma-bearing name can never reach the
//! matcher through this path.

use crate::automod::{normalize_content, AutomodMessage};

/// Slash bounds from spec §3.1: text is 1–2,000 UTF-16 units, `mentions` is a
/// synthetic count 0–100, `attachments` is one ≤1,000-unit option carrying at
/// most 10 bare names.
pub const PREVIEW_TEXT_MAX_UTF16: usize = 2000;
pub const PREVIEW_MENTIONS_MAX: i64 = 100;
pub const PREVIEW_ATTACHMENTS_OPTION_MAX_UTF16: usize = 1000;
pub const PREVIEW_ATTACHMENTS_MAX_FILES: usize = 10;
pub const PREVIEW_FILENAME_MAX_UTF16: usize = 255;

/// One neutral slash-option value. The handler maps twilight's
/// `CommandOptionValue` onto this shape (`String` → `Text`,
/// `Integer` → `Integer`, anything else → `Other`); core never sees Discord
/// types, so these decoders stay testable offline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewOptionValue<'a> {
    Text(&'a str),
    Integer(i64),
    Other,
}

/// Strict-decode refusal. Every [`message`](PreviewInputRefusal::message) is a
/// fixed string naming only closed-vocabulary option names — never submitted
/// text, a filename, or an unknown option name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewInputRefusal {
    UnknownOption,
    DuplicateOption,
    WrongType,
    MissingText,
    TextBounds,
    MentionsBounds,
    AttachmentsBounds,
    TooManyAttachments,
    EmptyFilename,
    BadFilename,
    BlankText,
}

impl PreviewInputRefusal {
    /// Fixed refusal line for the ephemeral reply.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::UnknownOption => {
                "Unknown option for /automod-preview; supply only text, mentions, or attachments."
            }
            Self::DuplicateOption => "Supply each /automod-preview option at most once.",
            Self::WrongType => {
                "Option types are fixed: text and attachments are text, mentions is a whole number."
            }
            Self::MissingText => "Supply text to preview (1-2000 characters).",
            Self::TextBounds => "Preview text must be 1-2000 characters.",
            Self::MentionsBounds => "Mentions must be a whole number 0-100.",
            Self::AttachmentsBounds => {
                "Attachments must total at most 1000 characters with at most 10 filenames."
            }
            Self::TooManyAttachments => "Supply at most 10 attachment filenames.",
            Self::EmptyFilename => {
                "Attachment filenames must be non-empty; check commas separate exactly the intended names."
            }
            Self::BadFilename => {
                "Attachment filenames must be bare names without slashes or control characters."
            }
            Self::BlankText => {
                "Blank text alone has no postable form; add mentions or attachments, or supply visible text."
            }
        }
    }
}

/// Strictly decoded `/automod-preview` options, ready for gating and
/// evaluation. `text_len_utf16` feeds the content-free audit length bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedPreviewInput {
    pub text: String,
    pub text_len_utf16: u32,
    pub mention_count: usize,
    pub attachment_names: Vec<String>,
}

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn valid_filename(name: &str) -> bool {
    let len = utf16_len(name);
    (1..=PREVIEW_FILENAME_MAX_UTF16).contains(&len)
        && !name.bytes().any(|b| b == b'/' || b == b'\\')
        && !name.chars().any(|c| c.is_control())
}

/// Strictly decode one attachments option value into bare filenames. Empty
/// entries refuse (no silent boundary normalization); entries with slashes
/// or control characters refuse. Commas cannot survive inside an entry —
/// they are the separator — and the message builder re-checks that.
pub fn parse_attachment_list(value: &str) -> Result<Vec<String>, PreviewInputRefusal> {
    if utf16_len(value) > PREVIEW_ATTACHMENTS_OPTION_MAX_UTF16 {
        return Err(PreviewInputRefusal::AttachmentsBounds);
    }
    let names: Vec<&str> = value.split(',').collect();
    if names.len() > PREVIEW_ATTACHMENTS_MAX_FILES {
        return Err(PreviewInputRefusal::TooManyAttachments);
    }
    names
        .into_iter()
        .map(|name| {
            if name.is_empty() {
                Err(PreviewInputRefusal::EmptyFilename)
            } else if valid_filename(name) {
                Ok(name.to_owned())
            } else {
                Err(PreviewInputRefusal::BadFilename)
            }
        })
        .collect()
}

/// Strictly decode the three `/automod-preview` options. Unknown names,
/// duplicates, and wrong types refuse before anything else; bounds follow;
/// whitespace-only text refuses unless `mentions` or `attachments` give it a
/// postable form (mention-only and attachment-only posts exist live; blank
/// content itself never repeat-counts).
pub fn decode_preview_input(
    options: &[(&str, PreviewOptionValue<'_>)],
) -> Result<DecodedPreviewInput, PreviewInputRefusal> {
    let mut text: Option<&str> = None;
    let mut mentions: Option<i64> = None;
    let mut attachments: Option<&str> = None;
    for (name, value) in options {
        match *name {
            "text" => {
                if text.is_some() {
                    return Err(PreviewInputRefusal::DuplicateOption);
                }
                match value {
                    PreviewOptionValue::Text(found) => text = Some(found),
                    _ => return Err(PreviewInputRefusal::WrongType),
                }
            }
            "mentions" => {
                if mentions.is_some() {
                    return Err(PreviewInputRefusal::DuplicateOption);
                }
                match value {
                    PreviewOptionValue::Integer(found) => mentions = Some(*found),
                    _ => return Err(PreviewInputRefusal::WrongType),
                }
            }
            "attachments" => {
                if attachments.is_some() {
                    return Err(PreviewInputRefusal::DuplicateOption);
                }
                match value {
                    PreviewOptionValue::Text(found) => attachments = Some(found),
                    _ => return Err(PreviewInputRefusal::WrongType),
                }
            }
            _ => return Err(PreviewInputRefusal::UnknownOption),
        }
    }
    let text = text.ok_or(PreviewInputRefusal::MissingText)?;
    let text_len = utf16_len(text);
    if text_len == 0 || text_len > PREVIEW_TEXT_MAX_UTF16 {
        return Err(PreviewInputRefusal::TextBounds);
    }
    let mentions = mentions.unwrap_or(0);
    if !(0..=PREVIEW_MENTIONS_MAX).contains(&mentions) {
        return Err(PreviewInputRefusal::MentionsBounds);
    }
    let attachment_names = match attachments {
        Some(value) => parse_attachment_list(value)?,
        None => Vec::new(),
    };
    if normalize_content(text).is_empty() && mentions == 0 && attachment_names.is_empty() {
        return Err(PreviewInputRefusal::BlankText);
    }
    Ok(DecodedPreviewInput {
        text: text.to_owned(),
        text_len_utf16: text_len as u32,
        mention_count: mentions as usize,
        attachment_names,
    })
}

/// Namespaced synthetic message id for one interaction. It cannot collide
/// with a Discord snowflake, so the read-only repeat probe never silently
/// drops a live row whose id equals the candidate's (spec §5.1).
#[must_use]
pub fn preview_message_id(interaction_id: u64) -> String {
    format!("preview:{interaction_id}")
}

/// Build the synthetic message for evaluation: author and roles are the
/// invoker's own, `author_is_bot` is false, no mention snowflakes are
/// fabricated (the count rides alongside as data), and the receipt timestamp
/// doubles as the message clock. Attachment names are re-validated —
/// including a comma check — so no caller can smuggle a separator-bearing
/// name past the option decoder into the matcher.
pub fn build_preview_message(
    input: &DecodedPreviewInput,
    guild_id: &str,
    channel_id: &str,
    invoker_id: &str,
    role_ids: &[String],
    interaction_id: u64,
    receipt_ms: u64,
) -> Result<AutomodMessage, PreviewInputRefusal> {
    if input
        .attachment_names
        .iter()
        .any(|name| name.contains(',') || !valid_filename(name))
    {
        return Err(PreviewInputRefusal::BadFilename);
    }
    Ok(AutomodMessage {
        guild_id: guild_id.to_owned(),
        channel_id: channel_id.to_owned(),
        message_id: preview_message_id(interaction_id),
        author_id: invoker_id.to_owned(),
        author_is_bot: false,
        role_ids: role_ids.to_vec(),
        content: input.text.clone(),
        mentioned_user_ids: Vec::new(),
        attachment_names: input.attachment_names.clone(),
        observed_timestamp_ms: receipt_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_id_is_namespaced_off_snowflakes() {
        let id = preview_message_id(123);
        assert_eq!(id, "preview:123");
        assert!(
            id.parse::<u64>().is_err(),
            "must never parse as a Discord snowflake"
        );
    }

    #[test]
    fn refusal_lines_are_fixed_and_content_free() {
        let refusals = [
            PreviewInputRefusal::UnknownOption,
            PreviewInputRefusal::DuplicateOption,
            PreviewInputRefusal::WrongType,
            PreviewInputRefusal::MissingText,
            PreviewInputRefusal::TextBounds,
            PreviewInputRefusal::MentionsBounds,
            PreviewInputRefusal::AttachmentsBounds,
            PreviewInputRefusal::TooManyAttachments,
            PreviewInputRefusal::EmptyFilename,
            PreviewInputRefusal::BadFilename,
            PreviewInputRefusal::BlankText,
        ];
        assert_eq!(refusals.len(), 11);
        for refusal in refusals {
            let line = refusal.message();
            assert!(!line.is_empty());
            for token in ["bannedword", "<@everyone>", "evil.example", ".exe"] {
                assert!(!line.contains(token), "{refusal:?} leaks: {line}");
            }
        }
    }

    #[test]
    fn builder_refuses_comma_bearing_names() {
        let input = DecodedPreviewInput {
            text: "hello".to_owned(),
            text_len_utf16: 5,
            mention_count: 0,
            attachment_names: vec!["safe.exe,txt".to_owned()],
        };
        assert_eq!(
            build_preview_message(&input, "g", "c", "a", &[], 7, 1_000),
            Err(PreviewInputRefusal::BadFilename)
        );
    }
}
